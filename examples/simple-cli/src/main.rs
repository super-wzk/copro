use copro_agent::{
    AgentCheckpoint, AgentControl, AgentEvent, AgentHistory, AgentOutcome, AgentTurnConfig,
    InputMessage, ToolExecutionPolicy, ToolRouter, start_turn,
};
use copro_api::message::{ImageContent, InputContent, OutputContent, ToolResultStatus};
use copro_api::stream::{Model, OutputContentDelta};
use copro_harness::request::RequestPipeline;
use copro_harness::skills::{SkillRequestInjector, SkillRuntime, SkillToolRouter};
use copro_harness::tool;
use copro_harness::tools::{CompositeToolRouter, LocalToolRouter};
use copro_provider_openai::{
    OpenAiResponsesModelConfig, OpenAiResponsesProvider, OpenAiResponsesProviderConfig,
};
use copro_workspace::WorkspaceToolRouter;
use futures_util::StreamExt;
use futures_util::io::AsyncWriteExt;
use std::env;
use std::error::Error as StdError;
use std::io::{self, Write};
use std::sync::Arc;
use vfs::async_vfs::{AsyncMemoryFS, AsyncVfsPath};

mod skills;
mod tools;

use skills::ExampleSkillStore;
use tools::{calculator, datetime};

const SYSTEM_PROMPT: &str = "You are a helpful assistant. Answer concisely.";

#[tokio::main]
async fn main() -> Result<(), Box<dyn StdError>> {
    let provider = OpenAiResponsesProvider::new(OpenAiResponsesProviderConfig {
        api_key: env_var("OPENAI_API_KEY"),
        api_base: env_var("OPENAI_API_BASE"),
        organization: env_var("OPENAI_ORGANIZATION"),
        project: env_var("OPENAI_PROJECT"),
    });

    let model = provider.model(
        "gpt-5.5",
        OpenAiResponsesModelConfig {
            parallel_tool_calls: Some(true),
            reasoning_effort: None,
            reasoning_summary: Some("auto".to_string()),
            ..OpenAiResponsesModelConfig::default()
        },
    )?;

    let root = AsyncMemoryFS::new().into();
    setup_workspace(&root).await;

    let local_tools: Arc<dyn ToolRouter> = Arc::new(LocalToolRouter::new(vec![
        tool!(
            "calculator",
            "Evaluate a simple arithmetic expression. Supports +, -, *, /, and parentheses.",
            calculator,
            policy = ToolExecutionPolicy::Parallel,
        ),
        tool!(
            "datetime",
            "Get the current date and time, optionally adjusted by a timezone offset.",
            datetime,
            policy = ToolExecutionPolicy::Parallel,
        ),
    ]));
    let workspace_tools = Arc::new(WorkspaceToolRouter::new(root));
    let skill_runtime = Arc::new(SkillRuntime::new(Arc::new(ExampleSkillStore::new(
        env::current_dir()?.join("examples/simple-cli/skills"),
    ))));
    let skill_tools = Arc::new(SkillToolRouter::new(Arc::clone(&skill_runtime)));
    let tool_router: Arc<dyn ToolRouter> = Arc::new(CompositeToolRouter::new(vec![
        local_tools,
        workspace_tools.clone(),
        skill_tools,
    ]));
    let request_pipeline =
        RequestPipeline::new(vec![Arc::new(SkillRequestInjector::new(skill_runtime))]);

    let mut history = AgentHistory::default();
    let config = AgentTurnConfig::default();
    history.push_input(InputMessage::System(vec![text(SYSTEM_PROMPT)]));

    println!("copro CLI — type /quit to exit, /clear to reset\n");

    loop {
        print!("> ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim().to_string();

        if input.is_empty() {
            continue;
        }
        if input == "/quit" {
            break;
        }
        if input == "/clear" {
            history = AgentHistory::default();
            workspace_tools.clear_cache();
            history.push_input(InputMessage::System(vec![text(SYSTEM_PROMPT)]));
            println!("[conversation cleared]\n");
            continue;
        }

        history.push_input(InputMessage::User(vec![text(&input)]));
        let rollback = history.clone();

        match drive_turn(
            history,
            config.clone(),
            Arc::clone(&model),
            Arc::clone(&tool_router),
            &request_pipeline,
        )
        .await
        {
            Ok(updated) => history = updated,
            Err(error) => {
                history = rollback;
                eprintln!("[error] {error}");
            }
        }
        println!();
    }

    Ok(())
}

async fn drive_turn(
    history: AgentHistory,
    config: AgentTurnConfig,
    model: Arc<dyn Model>,
    tools: Arc<dyn ToolRouter>,
    request_pipeline: &RequestPipeline,
) -> Result<AgentHistory, Box<dyn StdError>> {
    let run = start_turn(history, config, model, tools);
    let mut started_assistant = false;
    let mut streaming_thinking = false;

    loop {
        let point = run.step_until_control().await?;
        for event in point.events().iter().cloned() {
            handle_agent_event(event, &mut started_assistant, &mut streaming_thinking)?;
        }

        let finished = matches!(point.pending_outcome(), AgentOutcome::TurnFinished);
        match point.checkpoint() {
            AgentCheckpoint::RequestBuilt(report) => {
                let AgentOutcome::RequestBuilt(mut request) = report.outcome.clone() else {
                    unreachable!("request checkpoint must carry a request outcome")
                };
                request_pipeline.prepare_request(&mut request).await?;
                point.control(AgentControl::ReplaceRequest(request)).await?;
            }
            _ => {
                point.continue_turn().await?;
            }
        }

        if finished {
            let mut stream = run.clone().events();
            while let Some(result) = stream.next().await {
                handle_agent_event(result?, &mut started_assistant, &mut streaming_thinking)?;
            }
            if streaming_thinking {
                eprintln!();
            }
            break;
        }
    }

    Ok(run.into_history().await)
}

fn handle_agent_event(
    event: AgentEvent,
    started_assistant: &mut bool,
    streaming_thinking: &mut bool,
) -> Result<(), Box<dyn StdError>> {
    match event {
        AgentEvent::ModelDelta { delta, .. } => match delta {
            OutputContentDelta::Text(text) => {
                if *streaming_thinking {
                    eprintln!();
                    *streaming_thinking = false;
                }
                if !*started_assistant {
                    print!("assistant: ");
                    *started_assistant = true;
                }
                print!("{text}");
                io::stdout().flush()?;
            }
            OutputContentDelta::Thinking(text) => {
                if !*streaming_thinking {
                    eprint!("[thinking] ");
                    *streaming_thinking = true;
                }
                eprint!("{text}");
                io::stderr().flush()?;
            }
            OutputContentDelta::Image(_) => {}
            OutputContentDelta::ToolCall { .. } => {}
        },
        AgentEvent::AssistantCommitted { content, .. } => {
            if *streaming_thinking {
                eprintln!();
                *streaming_thinking = false;
            }
            for item in content {
                match item {
                    OutputContent::ToolCall(tool_call) => {
                        eprintln!("[tool call] {}({:?})", tool_call.name, tool_call.arguments)
                    }
                    OutputContent::Image(image) => eprintln!("[image output] {image:?}"),
                    _ => {}
                }
            }
        }
        AgentEvent::ToolStarted {
            tool: tool_call, ..
        } => {
            if *streaming_thinking {
                eprintln!();
                *streaming_thinking = false;
            }
            eprintln!("[tool started] {}", tool_call.name);
        }
        AgentEvent::ToolResultCommitted { result, .. } => {
            if *streaming_thinking {
                eprintln!();
                *streaming_thinking = false;
            }
            let label = match result.status {
                ToolResultStatus::Success => "output",
                ToolResultStatus::Error => "error",
            };
            eprintln!(
                "[tool {label}] {}: {}",
                result.name,
                input_content_text(&result.content)
            );
        }
        _ => {}
    }

    Ok(())
}

async fn setup_workspace(root: &AsyncVfsPath) {
    write_file(root, "README.md", b"# Demo\n\nA simple CLI demo.\n").await;
    write_file(root, "src/main.rs", b"fn main() {}\n").await;
    write_file(root, "tests/lib.rs", b"#[test]\nfn ok() {}\n").await;
    write_file(root, ".gitignore", b"*.log\n").await;

    write_file(root, "test.png", include_bytes!("../assets/test.png")).await;
}

async fn write_file(root: &AsyncVfsPath, path: &str, bytes: &[u8]) {
    let path = root.join(path).unwrap();
    path.parent().create_dir_all().await.unwrap();
    path.create_file()
        .await
        .unwrap()
        .write_all(bytes)
        .await
        .unwrap();
}

fn text(t: impl Into<String>) -> InputContent {
    InputContent::Text(t.into())
}

fn input_content_text(content: &[InputContent]) -> String {
    content
        .iter()
        .map(|content| match content {
            InputContent::Text(text) => text.clone(),
            InputContent::Image(ImageContent::Url { url }) => format!("[image: {url}]"),
            InputContent::Image(ImageContent::Data { mime_type, data }) => {
                format!("[image: {mime_type}, {} bytes]", data.len())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn env_var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}
