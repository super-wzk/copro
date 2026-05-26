use copro_core::message::{ImageContent, InputContent, Message, OutputContent};
use copro_core::provider::ModelProvider;
use copro_core::request::{GenerateRequest, GenerateRequestOptions};
use copro_provider_openai::{
    OpenAiImageGenerationTool, OpenAiResponsesModelConfig, OpenAiResponsesProvider,
    OpenAiResponsesProviderConfig,
};
use std::env;

const DEFAULT_MODEL: &str = "gpt-5.5";
const DEFAULT_PROMPT: &str =
    "Generate an image of gray tabby cat hugging an otter with an orange scarf";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(api_key) = env_var("OPENAI_API_KEY") else {
        eprintln!("Set OPENAI_API_KEY to run this example.");
        std::process::exit(1);
    };

    let model_id = env_var("OPENAI_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let user_prompt = prompt_from_args();

    let provider = OpenAiResponsesProvider::new(OpenAiResponsesProviderConfig {
        api_key: Some(api_key),
        api_base: env_var("OPENAI_API_BASE"),
        organization: env_var("OPENAI_ORGANIZATION"),
        project: env_var("OPENAI_PROJECT"),
    });

    let model = provider.chat_model(
        &model_id,
        OpenAiResponsesModelConfig {
            store: Some(false),
            ..OpenAiResponsesModelConfig::default()
        },
    )?;

    let options = GenerateRequestOptions {
        temperature: Some(0.2),
        max_tokens: Some(256),
        ..GenerateRequestOptions::default()
    };
    let image_generation_tool = OpenAiImageGenerationTool {
        partial_images: Some(2),
    }
    .try_into()?;

    let response = model
        .generate(GenerateRequest {
            messages: vec![
                Message::System {
                    content: vec![text(
                        "Use the available image_generation tool to create the requested image. Do not answer with a prompt or describe inability.",
                    )],
                },
                Message::User {
                    content: vec![text(user_prompt.clone())],
                },
            ],
            tools: Vec::new(),
            tool_choice: None,
            hosted_tools: vec![image_generation_tool],
            options,
        })
        .await?;

    println!("model: {model_id}");
    println!("user: {user_prompt}");
    save_and_print_assistant_message(response.message)?;
    println!("finish: {:?}", response.finish_reason);

    if let Some(usage) = response.usage {
        println!(
            "usage: input={:?}, output={:?}",
            usage.input_tokens, usage.output_tokens
        );
    }

    Ok(())
}

fn prompt_from_args() -> String {
    let prompt = env::args().skip(1).collect::<Vec<_>>().join(" ");
    if prompt.trim().is_empty() {
        DEFAULT_PROMPT.to_string()
    } else {
        prompt
    }
}

fn env_var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn text(text: impl Into<String>) -> InputContent {
    InputContent::Text { text: text.into() }
}

fn save_and_print_assistant_message(message: Message) -> Result<(), Box<dyn std::error::Error>> {
    let Message::Assistant { content } = message else {
        println!("assistant: <non-assistant response>");
        return Ok(());
    };

    print!("assistant:");
    let mut wrote_output = false;
    let mut image_count = 0usize;

    for item in content {
        match item {
            OutputContent::Text { text } => {
                if !wrote_output {
                    print!(" ");
                }
                print!("{text}");
                wrote_output = true;
            }
            OutputContent::Thinking { text } => {
                eprintln!("thinking: {text}");
            }
            OutputContent::Image { image } => {
                image_count += 1;
                match image {
                    ImageContent::Data { mime_type, data } => {
                        let path = image_output_path(image_count, &mime_type);
                        std::fs::write(&path, data)?;
                        println!("\nimage {image_count}: saved to {path} ({mime_type})");
                    }
                    ImageContent::Url { url } => {
                        println!("\nimage {image_count}: {url}");
                    }
                }
                wrote_output = true;
            }
            OutputContent::ToolCall {
                id,
                name,
                arguments,
            } => {
                println!("\ntool call: {name}({arguments:?}) [{id}]");
                wrote_output = true;
            }
        }
    }

    if wrote_output {
        println!();
    } else {
        println!(" <no text output>");
    }

    Ok(())
}

fn image_output_path(index: usize, mime_type: &str) -> String {
    let extension = match mime_type {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        _ => "img",
    };

    if index == 1 {
        format!("output.{extension}")
    } else {
        format!("output-{index}.{extension}")
    }
}
