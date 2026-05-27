use copro_agent::{Agent, AgentEvent};
use copro_core::message::{InputContent, Message};
use copro_core::provider::ProviderRegistry;
use copro_provider_openai::{
    OpenAiResponsesModelConfig, OpenAiResponsesProvider, OpenAiResponsesProviderConfig,
};
use futures_util::StreamExt;
use std::env;
use std::io::{self, Write};
use std::sync::Arc;

use copro_core::tool::ErasedTool;

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

    let tools: Vec<Arc<dyn ErasedTool>> = vec![Arc::new(Calculator), Arc::new(DateTimeTool)];
    let agent = Agent::new(registry).with_tools(tools);

    let mut messages: Vec<Message> = vec![Message::System {
        content: vec![text(SYSTEM_PROMPT)],
    }];

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
            messages.truncate(1); // keep system prompt
            println!("[conversation cleared]\n");
            continue;
        }

        messages.push(Message::User {
            content: vec![text(&input)],
        });

        let mut stream = agent.run_stream("gpt-5.5", messages.clone());
        let mut assistant_text = String::new();
        let mut has_error = false;
        let mut started_assistant = false;
        let mut streaming_thinking = false;

        while let Some(result) = stream.next().await {
            match result {
                Ok(event) => match event {
                    // Text / thinking are already deltas from Agent::run_stream,
                    // so print and flush each chunk immediately.
                    AgentEvent::Text(delta) => {
                        if streaming_thinking {
                            eprintln!();
                            streaming_thinking = false;
                        }
                        if !started_assistant {
                            print!("assistant: ");
                            started_assistant = true;
                        }
                        print!("{delta}");
                        io::stdout().flush()?;
                        assistant_text.push_str(&delta);
                    }
                    AgentEvent::Thinking(delta) => {
                        if !streaming_thinking {
                            eprint!("[thinking] ");
                            streaming_thinking = true;
                        }
                        eprint!("{delta}");
                        io::stderr().flush()?;
                    }
                    AgentEvent::ToolCall {
                        name, arguments, ..
                    } => {
                        if streaming_thinking {
                            eprintln!();
                            streaming_thinking = false;
                        }
                        eprintln!("[tool call] {name}({arguments:?})");
                    }
                    AgentEvent::ToolOutput { name, result, .. } => {
                        if streaming_thinking {
                            eprintln!();
                            streaming_thinking = false;
                        }
                        eprintln!("[tool output] {name}: {result}");
                    }
                    AgentEvent::Finished { reason } => {
                        if streaming_thinking {
                            eprintln!();
                            streaming_thinking = false;
                        }
                        if started_assistant {
                            println!();
                        }
                        eprintln!("[finish: {reason:?}]");
                    }
                },
                Err(e) => {
                    if streaming_thinking {
                        eprintln!();
                    }
                    eprintln!("[error] {e}");
                    has_error = true;
                    break;
                }
            }
        }

        if !has_error && !assistant_text.is_empty() {
            messages.push(Message::Assistant {
                content: vec![copro_core::message::OutputContent::Text {
                    text: assistant_text,
                }],
            });
        }
        println!();
    }

    Ok(())
}

fn text(t: impl Into<String>) -> InputContent {
    InputContent::Text { text: t.into() }
}

fn env_var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}
