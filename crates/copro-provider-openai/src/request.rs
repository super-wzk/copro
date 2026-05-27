use base64::Engine;
use copro_core::error::{Error, Result};
use copro_core::message::{InputContent, Message, OutputContent, ToolResultStatus};
use copro_core::tool::{HostedToolSpec, ToolChoice, ToolDefinition};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::config::{OpenAiResponsesModelConfig, OpenAiResponsesRequestOptions};

pub(crate) fn build_response_body(
    model_id: &str,
    model_config: &OpenAiResponsesModelConfig,
    request: copro_core::request::GenerateRequest,
) -> Result<Value> {
    let request_options = request.options.extra::<OpenAiResponsesRequestOptions>()?;
    let mut body = Map::new();
    body.insert("model".to_string(), Value::String(model_id.to_string()));
    body.insert(
        "input".to_string(),
        Value::Array(build_input_items(request.messages)?),
    );
    body.insert("stream".to_string(), Value::Bool(true));

    insert_optional_json(&mut body, "temperature", request.options.temperature);
    insert_optional_json(&mut body, "max_output_tokens", request.options.max_tokens);

    let tools = build_request_tools(request.tools, request.hosted_tools)?;
    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
    }
    insert_optional_value(
        &mut body,
        "tool_choice",
        request.tool_choice.map(build_tool_choice),
    );

    insert_model_config(&mut body, model_config);
    insert_extra_body(&mut body, &request_options.extra_body);

    Ok(Value::Object(body))
}

fn insert_model_config(body: &mut Map<String, Value>, model_config: &OpenAiResponsesModelConfig) {
    insert_optional_json(body, "store", model_config.store);
    insert_optional_json(
        body,
        "parallel_tool_calls",
        model_config.parallel_tool_calls,
    );
    insert_optional_value(body, "reasoning", build_reasoning(model_config));
    insert_extra_body(body, &model_config.extra_body);
}

fn insert_optional_json<T>(body: &mut Map<String, Value>, key: &str, value: Option<T>)
where
    T: Serialize,
{
    if let Some(value) = value {
        body.insert(key.to_string(), json!(value));
    }
}

fn insert_optional_value(body: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        body.insert(key.to_string(), value);
    }
}

fn insert_extra_body(body: &mut Map<String, Value>, extra_body: &Map<String, Value>) {
    for (key, value) in extra_body {
        if is_protected_response_body_key(key) {
            continue;
        }
        body.insert(key.clone(), value.clone());
    }
}

fn is_protected_response_body_key(key: &str) -> bool {
    matches!(key, "input" | "model" | "stream")
}

fn build_input_items(messages: Vec<Message>) -> Result<Vec<Value>> {
    let mut items = Vec::new();
    for message in messages {
        items.extend(build_message_items(message)?);
    }
    Ok(items)
}

fn build_message_items(message: Message) -> Result<Vec<Value>> {
    match message {
        Message::System { content } => Ok(vec![message_item("system", input_content(content)?)]),
        Message::Developer { content } => {
            Ok(vec![message_item("developer", input_content(content)?)])
        }
        Message::User { content } => Ok(vec![message_item("user", input_content(content)?)]),
        Message::Assistant { content } => build_output_items(content),
        Message::Tool {
            call_id,
            name: _,
            status,
            content,
        } => {
            let mut output = tool_output_content(content)?;
            if status == ToolResultStatus::Error {
                output = format!("Error: {output}");
            }
            Ok(vec![json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": output,
            })])
        }
    }
}

fn message_item(role: &str, content: Vec<Value>) -> Value {
    json!({
        "type": "message",
        "role": role,
        "content": content,
    })
}

fn input_content(content: Vec<InputContent>) -> Result<Vec<Value>> {
    content.into_iter().map(input_content_part).collect()
}

fn input_content_part(content: InputContent) -> Result<Value> {
    match content {
        InputContent::Text { text } => Ok(json!({
            "type": "input_text",
            "text": text,
        })),
        InputContent::Image { mime_type, data } => {
            let image = base64::engine::general_purpose::STANDARD.encode(data);
            Ok(json!({
                "type": "input_image",
                "image_url": format!("data:{mime_type};base64,{image}"),
            }))
        }
    }
}

fn build_output_items(content: Vec<OutputContent>) -> Result<Vec<Value>> {
    let mut message_content = Vec::new();
    let mut items = Vec::new();

    for content in content {
        match content {
            OutputContent::Text { text } => message_content.push(json!({
                "type": "output_text",
                "text": text,
            })),
            OutputContent::Thinking { .. } => {
                return Err(Error::client(
                    "OpenAI Responses provider cannot replay generic thinking content",
                ));
            }
            OutputContent::Image { .. } => {
                return Err(Error::client(
                    "OpenAI Responses provider cannot replay generic image output",
                ));
            }
            OutputContent::ToolCall {
                id,
                name,
                arguments,
            } => items.push(json!({
                "type": "function_call",
                "call_id": id,
                "name": name,
                "arguments": Value::Object(arguments).to_string(),
                "status": "completed",
            })),
        }
    }

    if !message_content.is_empty() {
        items.insert(0, message_item("assistant", message_content));
    }

    Ok(items)
}

fn tool_output_content(content: Vec<InputContent>) -> Result<String> {
    let mut text = Vec::new();
    for content in content {
        match content {
            InputContent::Text { text: part } => text.push(part),
            InputContent::Image { .. } => {
                return Err(Error::client(
                    "OpenAI function call outputs only support text content",
                ));
            }
        }
    }
    Ok(text.join("\n"))
}

fn build_request_tools(
    function_tools: Vec<ToolDefinition>,
    hosted_tools: Vec<HostedToolSpec>,
) -> Result<Vec<Value>> {
    let mut tools = build_function_tools(function_tools);

    for tool in hosted_tools {
        tools.push(build_hosted_tool(tool)?);
    }

    Ok(tools)
}

fn build_function_tools(tools: Vec<ToolDefinition>) -> Vec<Value> {
    tools
        .into_iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
            })
        })
        .collect()
}

fn build_hosted_tool(tool: HostedToolSpec) -> Result<Value> {
    let kind = tool.kind.trim().to_string();
    if kind.is_empty() {
        return Err(Error::client("hosted tool kind cannot be empty"));
    }

    let mut parameters = tool.parameters;
    parameters.insert("type".to_string(), Value::String(kind));
    Ok(Value::Object(parameters))
}

fn build_tool_choice(tool_choice: ToolChoice) -> Value {
    match tool_choice {
        ToolChoice::Auto => Value::String("auto".to_string()),
        ToolChoice::None => Value::String("none".to_string()),
        ToolChoice::Required => Value::String("required".to_string()),
        ToolChoice::Specific { name } => json!({
            "type": "function",
            "name": name,
        }),
    }
}

fn build_reasoning(model_config: &OpenAiResponsesModelConfig) -> Option<Value> {
    let mut reasoning = Map::new();

    if let Some(effort) = &model_config.reasoning_effort {
        reasoning.insert("effort".to_string(), Value::String(effort.clone()));
    }
    if let Some(summary) = &model_config.reasoning_summary {
        reasoning.insert("summary".to_string(), Value::String(summary.clone()));
    }

    (!reasoning.is_empty()).then_some(Value::Object(reasoning))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        OpenAiImageGenerationTool, OpenAiResponsesModelConfig, OpenAiResponsesRequestOptions,
    };
    use copro_core::{
        message::{InputContent, Message},
        request::GenerateRequestOptions,
        tool::{ToolChoice, ToolDefinition},
    };
    use serde_json::{Map, json};

    fn empty_options() -> GenerateRequestOptions {
        GenerateRequestOptions::default()
    }

    #[test]
    fn builds_basic_text_request() {
        let request = copro_core::request::GenerateRequest {
            messages: vec![Message::User {
                content: vec![InputContent::Text {
                    text: "hello".to_string(),
                }],
            }],
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            tool_choice: None,
            options: GenerateRequestOptions {
                temperature: Some(0.2),
                max_tokens: Some(128),
                ..empty_options()
            },
        };

        let body = build_response_body(
            "gpt-4.1-mini",
            &OpenAiResponsesModelConfig::default(),
            request,
        )
        .unwrap();

        assert_eq!(body["model"], "gpt-4.1-mini");
        assert_eq!(body["stream"], true);
        assert!((body["temperature"].as_f64().unwrap() - 0.2).abs() < 0.000001);
        assert_eq!(body["max_output_tokens"], 128);
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn builds_developer_message_request() {
        let request = copro_core::request::GenerateRequest {
            messages: vec![Message::Developer {
                content: vec![InputContent::Text {
                    text: "follow these instructions".to_string(),
                }],
            }],
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            tool_choice: None,
            options: empty_options(),
        };

        let body = build_response_body(
            "gpt-4.1-mini",
            &OpenAiResponsesModelConfig::default(),
            request,
        )
        .unwrap();

        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(
            body["input"][0]["content"][0]["text"],
            "follow these instructions"
        );
    }

    #[test]
    fn builds_function_tools_and_choice() {
        let request = copro_core::request::GenerateRequest {
            messages: vec![Message::User {
                content: vec![InputContent::Text {
                    text: "weather".to_string(),
                }],
            }],
            tools: vec![ToolDefinition {
                name: "weather".to_string(),
                description: "Get weather".to_string(),
                parameters: json!({"type":"object"}),
            }],
            hosted_tools: Vec::new(),
            tool_choice: Some(ToolChoice::Specific {
                name: "weather".to_string(),
            }),
            options: empty_options(),
        };

        let body = build_response_body(
            "gpt-4.1-mini",
            &OpenAiResponsesModelConfig::default(),
            request,
        )
        .unwrap();

        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "weather");
        assert_eq!(body["tool_choice"]["type"], "function");
        assert_eq!(body["tool_choice"]["name"], "weather");
    }

    #[test]
    fn maps_model_config_into_response_body() {
        let model_config = OpenAiResponsesModelConfig {
            store: Some(false),
            parallel_tool_calls: Some(true),
            reasoning_effort: Some("medium".to_string()),
            reasoning_summary: Some("auto".to_string()),
            extra_body: Map::from_iter([("metadata".to_string(), json!({"trace_id": "req_123"}))]),
        };
        let request = copro_core::request::GenerateRequest {
            messages: vec![Message::User {
                content: vec![InputContent::Text {
                    text: "hello".to_string(),
                }],
            }],
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            tool_choice: None,
            options: empty_options(),
        };

        let body = build_response_body("gpt-5", &model_config, request).unwrap();

        assert_eq!(body["store"], false);
        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(body["metadata"]["trace_id"], "req_123");
    }

    #[test]
    fn extra_body_cannot_override_required_response_fields() {
        let model_config = OpenAiResponsesModelConfig {
            extra_body: Map::from_iter([
                ("input".to_string(), json!([])),
                ("model".to_string(), json!("wrong-model")),
                ("stream".to_string(), json!(false)),
                ("metadata".to_string(), json!({"allowed": true})),
            ]),
            ..OpenAiResponsesModelConfig::default()
        };
        let request = copro_core::request::GenerateRequest {
            messages: vec![Message::User {
                content: vec![InputContent::Text {
                    text: "hello".to_string(),
                }],
            }],
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            tool_choice: None,
            options: empty_options(),
        };

        let body = build_response_body("gpt-4.1-mini", &model_config, request).unwrap();

        assert_eq!(body["model"], "gpt-4.1-mini");
        assert_eq!(body["stream"], true);
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["metadata"]["allowed"], true);
    }

    #[test]
    fn maps_request_extra_body() {
        let model_config = OpenAiResponsesModelConfig {
            extra_body: Map::from_iter([(
                "metadata".to_string(),
                json!({"source": "model-config"}),
            )]),
            ..OpenAiResponsesModelConfig::default()
        };
        let mut options = empty_options();
        options
            .insert_extra(OpenAiResponsesRequestOptions {
                extra_body: Map::from_iter([
                    ("metadata".to_string(), json!({"source": "request"})),
                    ("model".to_string(), json!("wrong-model")),
                ]),
            })
            .unwrap();
        let request = copro_core::request::GenerateRequest {
            messages: vec![Message::User {
                content: vec![InputContent::Text {
                    text: "hello".to_string(),
                }],
            }],
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            tool_choice: None,
            options,
        };

        let body = build_response_body("gpt-4.1-mini", &model_config, request).unwrap();

        assert_eq!(body["model"], "gpt-4.1-mini");
        assert_eq!(body["metadata"]["source"], "request");
    }

    #[test]
    fn maps_hosted_response_tools() {
        let request = copro_core::request::GenerateRequest {
            messages: vec![Message::User {
                content: vec![InputContent::Text {
                    text: "draw a cat".to_string(),
                }],
            }],
            tools: Vec::new(),
            hosted_tools: vec![
                OpenAiImageGenerationTool {
                    partial_images: Some(2),
                }
                .try_into()
                .unwrap(),
            ],
            tool_choice: Some(ToolChoice::Required),
            options: empty_options(),
        };

        let body = build_response_body("gpt-5.5", &OpenAiResponsesModelConfig::default(), request)
            .unwrap();

        assert_eq!(body["tools"][0]["type"], "image_generation");
        assert_eq!(body["tools"][0]["partial_images"], 2);
        assert_eq!(body["tool_choice"], "required");
    }

    #[test]
    fn maps_tool_output_messages() {
        let items = build_message_items(Message::Tool {
            call_id: "call_123".to_string(),
            name: "weather".to_string(),
            status: ToolResultStatus::Success,
            content: vec![InputContent::Text {
                text: "sunny".to_string(),
            }],
        })
        .unwrap();

        assert_eq!(items[0]["type"], "function_call_output");
        assert_eq!(items[0]["call_id"], "call_123");
        assert_eq!(items[0]["output"], "sunny");
    }
}
