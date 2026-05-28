use copro_agent::{Agent, AgentEvent, ToolRouter};
use copro_api::message::{InputContent, Message, OutputContent, ToolResultStatus};
use copro_api::stream::OutputContentDelta;
use copro_harness::skills::{SkillHook, SkillRuntime, SkillToolRouter};
use copro_harness::tools::{CompositeToolRouter, LocalToolRouter, tool_fn};
use copro_provider_openai::{
    OpenAiResponsesModelConfig, OpenAiResponsesProvider, OpenAiResponsesProviderConfig,
};
use futures_util::StreamExt;
use std::env;
use std::io::{self, Write};
use std::sync::Arc;

mod skills;
mod tools;

use skills::ExampleSkillStore;
use tools::{calculator, datetime};

const SYSTEM_PROMPT: &str = "You are a helpful assistant. Answer concisely.";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let provider = OpenAiResponsesProvider::new(OpenAiResponsesProviderConfig {
        api_key: env_var("OPENAI_API_KEY"),
        api_base: env_var("OPENAI_API_BASE"),
        organization: env_var("OPENAI_ORGANIZATION"),
        project: env_var("OPENAI_PROJECT"),
    });

    let model = provider.model(
        "gpt-5.5",
        OpenAiResponsesModelConfig {
            reasoning_effort: Some("xhigh".to_string()),
            reasoning_summary: Some("auto".to_string()),
            ..OpenAiResponsesModelConfig::default()
        },
    )?;
    let local_tools: Arc<dyn ToolRouter> = Arc::new(LocalToolRouter::new(vec![
        tool_fn(
            "calculator",
            "Evaluate a simple arithmetic expression. Supports +, -, *, /, and parentheses.",
            calculator,
        ),
        tool_fn(
            "datetime",
            "Get the current date and time, optionally adjusted by a timezone offset.",
            datetime,
        ),
    ]));
    let skill_runtime = Arc::new(SkillRuntime::new(Arc::new(ExampleSkillStore::new(
        env::current_dir()?.join("examples/simple-cli/skills"),
    ))));
    let skill_tools: Arc<dyn ToolRouter> =
        Arc::new(SkillToolRouter::new(Arc::clone(&skill_runtime)));
    let tool_router = CompositeToolRouter::new(vec![local_tools, skill_tools]);
    let mut agent = Agent::new(model, Arc::new(tool_router));
    agent.hooks.push(Arc::new(SkillHook::new(skill_runtime)));

    agent
        .messages
        .push(Message::System(vec![text(SYSTEM_PROMPT)]));

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
            agent.messages.clear();
            agent
                .messages
                .push(Message::System(vec![text(SYSTEM_PROMPT)]));
            println!("[conversation cleared]\n");
            continue;
        }

        agent.messages.push(Message::User(vec![text(&input)]));

        let mut stream = agent.run_stream();
        let mut started_assistant = false;
        let mut streaming_thinking = false;

        while let Some(result) = stream.next().await {
            match result {
                Ok(event) => match event {
                    AgentEvent::OutputDelta(delta) => match delta {
                        OutputContentDelta::Text(text) => {
                            if streaming_thinking {
                                eprintln!();
                                streaming_thinking = false;
                            }
                            if !started_assistant {
                                print!("assistant: ");
                                started_assistant = true;
                            }
                            print!("{text}");
                            io::stdout().flush()?;
                        }
                        OutputContentDelta::Thinking(text) => {
                            if !streaming_thinking {
                                eprint!("[thinking] ");
                                streaming_thinking = true;
                            }
                            eprint!("{text}");
                            io::stderr().flush()?;
                        }
                        OutputContentDelta::Image(_) => {}
                        OutputContentDelta::ToolCall { .. } => {}
                    },
                    AgentEvent::OutputFinished { content, .. } => {
                        if streaming_thinking {
                            eprintln!();
                            streaming_thinking = false;
                        }
                        for item in content {
                            match item {
                                OutputContent::ToolCall(tool_call) => eprintln!(
                                    "[tool call] {}({:?})",
                                    tool_call.name, tool_call.arguments
                                ),
                                OutputContent::Image(image) => {
                                    eprintln!("[image output] {image:?}")
                                }
                                _ => {}
                            }
                        }
                    }
                    AgentEvent::ToolResult(result) => {
                        if streaming_thinking {
                            eprintln!();
                            streaming_thinking = false;
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
                },
                Err(e) => {
                    if streaming_thinking {
                        eprintln!();
                        streaming_thinking = false;
                    }
                    eprintln!("[error] {e}");
                    break;
                }
            }
        }

        if streaming_thinking {
            eprintln!();
        }
        println!();
    }

    Ok(())
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
