use copro_agent::{Agent, AgentEvent};
use copro_core::message::{InputContent, Message, OutputContent, ToolResultStatus};
use copro_core::provider::ProviderRegistry;
use copro_core::stream::OutputContentDelta;
use copro_provider_openai::{
    OpenAiResponsesModelConfig, OpenAiResponsesProvider, OpenAiResponsesProviderConfig,
};
use futures_util::StreamExt;
use std::env;
use std::io::{self, Write};
use std::sync::Arc;

mod tools;
use tools::{Calculator, DateTimeTool};

const SYSTEM_PROMPT: &str = "You are a helpful assistant. Answer concisely.";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let provider = OpenAiResponsesProvider::new(OpenAiResponsesProviderConfig {
        api_key: env_var("OPENAI_API_KEY"),
        api_base: env_var("OPENAI_API_BASE"),
        organization: env_var("OPENAI_ORGANIZATION"),
        project: env_var("OPENAI_PROJECT"),
    });

    let mut registry = ProviderRegistry::new();
    let model = provider.model_definition(
        "gpt-5.5",
        OpenAiResponsesModelConfig {
            reasoning_effort: Some("xhigh".to_string()),
            reasoning_summary: Some("auto".to_string()),
            ..OpenAiResponsesModelConfig::default()
        },
    )?;
    registry.register_provider(provider);
    registry.register_model(model);

    let chat = registry.chat("gpt-5.5")?;
    let mut agent = Agent::new(chat);
    agent.tools = vec![Arc::new(Calculator), Arc::new(DateTimeTool)];

    agent.messages.push(Message::System {
        content: vec![text(SYSTEM_PROMPT)],
    });

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
            agent.messages.push(Message::System {
                content: vec![text(SYSTEM_PROMPT)],
            });
            println!("[conversation cleared]\n");
            continue;
        }

        agent.messages.push(Message::User {
            content: vec![text(&input)],
        });

        let mut stream = agent.run_stream();
        let mut started_assistant = false;
        let mut streaming_thinking = false;

        while let Some(result) = stream.next().await {
            match result {
                Ok(event) => match event {
                    AgentEvent::OutputDelta { delta } => match delta {
                        OutputContentDelta::Text { text } => {
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
                        OutputContentDelta::Thinking { text } => {
                            if !streaming_thinking {
                                eprint!("[thinking] ");
                                streaming_thinking = true;
                            }
                            eprint!("{text}");
                            io::stderr().flush()?;
                        }
                        OutputContentDelta::Image { .. } => {}
                        OutputContentDelta::ToolCall { .. } => {}
                    },
                    AgentEvent::Output { content, .. } => {
                        if streaming_thinking {
                            eprintln!();
                            streaming_thinking = false;
                        }
                        for item in content {
                            match item {
                                OutputContent::ToolCall {
                                    name, arguments, ..
                                } => eprintln!("[tool call] {name}({arguments:?})"),
                                OutputContent::Image { image } => {
                                    eprintln!("[image output] {image:?}")
                                }
                                _ => {}
                            }
                        }
                    }
                    AgentEvent::ToolResult {
                        name,
                        status,
                        content,
                        ..
                    } => {
                        if streaming_thinking {
                            eprintln!();
                            streaming_thinking = false;
                        }
                        let label = match status {
                            ToolResultStatus::Success => "output",
                            ToolResultStatus::Error => "error",
                        };
                        eprintln!("[tool {label}] {name}: {}", input_content_text(&content));
                    }
                    AgentEvent::TurnFinish => {
                        if streaming_thinking {
                            eprintln!();
                            streaming_thinking = false;
                        }
                        if started_assistant {
                            println!();
                        }
                        eprintln!("[turn finish]");
                    }
                },
                Err(e) => {
                    if streaming_thinking {
                        eprintln!();
                    }
                    eprintln!("[error] {e}");
                    break;
                }
            }
        }

        println!();
    }

    Ok(())
}

fn text(t: impl Into<String>) -> InputContent {
    InputContent::Text { text: t.into() }
}

fn input_content_text(content: &[InputContent]) -> String {
    content
        .iter()
        .filter_map(|content| match content {
            InputContent::Text { text } => Some(text.as_str()),
            InputContent::Image { .. } => None,
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
