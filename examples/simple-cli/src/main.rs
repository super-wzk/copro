use copro_agent::{
    Agent, AgentControlPoint, AgentEvent, AgentOutcome, ToolExecutionPolicy, ToolRouter,
};
use copro_api::message::{InputContent, Message, OutputContent, ToolResultStatus};
use copro_api::stream::OutputContentDelta;
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
    let tool_router =
        CompositeToolRouter::new(vec![local_tools, workspace_tools.clone(), skill_tools]);
    let agent = Agent::new(model, Arc::new(tool_router));
    let skill_request = SkillRequestInjector::new(skill_runtime);

    agent
        .push_message(Message::System(vec![text(SYSTEM_PROMPT)]))
        .await?;

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
            agent.clear_messages().await?;
            workspace_tools.clear_cache();
            agent
                .push_message(Message::System(vec![text(SYSTEM_PROMPT)]))
                .await?;
            println!("[conversation cleared]\n");
            continue;
        }

        agent
            .push_message(Message::User(vec![text(&input)]))
            .await?;

        if let Err(error) = run_turn(&agent, &skill_request).await {
            eprintln!("[error] {error}");
        }
        println!();
    }

    Ok(())
}

async fn run_turn(
    agent: &Agent,
    skill_request: &SkillRequestInjector,
) -> Result<(), Box<dyn StdError>> {
    let run = agent.start_run().await?;
    let mut started_assistant = false;
    let mut streaming_thinking = false;

    loop {
        let point = run.step_until_control().await?;
        for event in point.events().iter().cloned() {
            handle_agent_event(event, &mut started_assistant, &mut streaming_thinking)?;
        }

        let finished = matches!(point.pending_outcome(), AgentOutcome::TurnFinished);
        match point {
            AgentControlPoint::RequestBuilt(point) => {
                let mut request = point.request().clone();
                skill_request.prepare_request(&mut request).await?;
                run.apply_control(point.replace_request(request)).await?;
            }
            point => {
                run.apply_control(point.continue_run()).await?;
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

    Ok(())
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

    let png = &[
        0x89u8, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xfc,
        0xcf, 0xc0, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x05, 0xfe, 0xd8, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    write_file(root, "assets/logo.png", png).await;
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
        .filter_map(|content| match content {
            InputContent::Text(text) => Some(text.as_str()),
            InputContent::Image(_) => None,
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
