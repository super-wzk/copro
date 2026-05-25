use async_openai::Client;
use async_openai::config::OpenAIConfig;
use async_openai::error::OpenAIError;
use async_openai::types::responses::ResponseStreamEvent;
use base64::Engine;
use copro_core::error::{ModelError, ModelResult};
use copro_core::message::{AssistantContent, InputContent, Message, ToolResultStatus};
use copro_core::model::{
    ChatModel, InputModality, ModelCapabilities, ModelFeature, ModelFuture, ModelInfo,
};
use copro_core::provider::{ErasedModelProvider, ModelProvider, ProviderFactory};
use copro_core::request::GenerateRequest;
use copro_core::response::{FinishReason, Usage};
use copro_core::stream::{AssistantContentDetail, AssistantContentEvent, ModelStream};
use copro_core::tool::{ToolChoice, ToolDefinition};
use futures_util::StreamExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, rename_all = "camelCase")]
pub struct OpenAiResponsesProviderConfig {
    pub api_key: Option<String>,
    pub api_base: Option<String>,
    pub organization: Option<String>,
    pub project: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, rename_all = "camelCase")]
pub struct OpenAiResponsesModelConfig {
    pub store: Option<bool>,
    pub parallel_tool_calls: Option<bool>,
    pub reasoning_effort: Option<String>,
    pub reasoning_summary: Option<String>,
    pub extra_body: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAiResponsesProviderFactory;

impl ProviderFactory for OpenAiResponsesProviderFactory {
    type Config = OpenAiResponsesProviderConfig;

    fn kind(&self) -> &str {
        "openai-responses"
    }

    fn build_provider(&self, config: Self::Config) -> ModelResult<Arc<dyn ErasedModelProvider>> {
        Ok(Arc::new(OpenAiResponsesProvider::new(config)))
    }
}

#[derive(Clone)]
pub struct OpenAiResponsesProvider {
    client: Client<OpenAIConfig>,
}

impl OpenAiResponsesProvider {
    pub fn new(config: OpenAiResponsesProviderConfig) -> Self {
        Self {
            client: Client::with_config(openai_config(config)),
        }
    }
}

impl ModelProvider for OpenAiResponsesProvider {
    type Config = OpenAiResponsesModelConfig;

    fn list_models(&self) -> ModelFuture<'_, Vec<ModelInfo>> {
        let client = self.client.clone();

        Box::pin(async move {
            let response = client.models().list().await.map_err(map_openai_error)?;
            let mut models = response
                .data
                .into_iter()
                .map(|model| ModelInfo {
                    capabilities: infer_capabilities(&model.id),
                    display_name: None,
                    id: model.id,
                })
                .collect::<Vec<_>>();
            models.sort_by(|left, right| left.id.cmp(&right.id));
            Ok(models)
        })
    }

    fn chat_model(&self, model_id: &str, config: Self::Config) -> ModelResult<Arc<dyn ChatModel>> {
        if model_id.trim().is_empty() {
            return Err(ModelError::client("OpenAI model id cannot be empty"));
        }

        Ok(Arc::new(OpenAiResponsesChatModel {
            client: self.client.clone(),
            model_config: config,
            model_id: model_id.to_string(),
        }))
    }
}

#[derive(Clone)]
pub struct OpenAiResponsesChatModel {
    client: Client<OpenAIConfig>,
    model_id: String,
    model_config: OpenAiResponsesModelConfig,
}

impl ChatModel for OpenAiResponsesChatModel {
    fn stream(&self, request: GenerateRequest) -> ModelStream<'_> {
        let timeout = request.options.timeout;
        let body = match build_response_body(&self.model_id, &self.model_config, request) {
            Ok(body) => body,
            Err(error) => return Box::pin(futures_util::stream::once(async move { Err(error) })),
        };
        let client = self.client.clone();

        Box::pin(async_stream::try_stream! {
            let mut stream = create_response_stream(&client, body, timeout).await?;
            let mut mapper = OpenAiEventMapper::new();

            loop {
                let next = next_openai_event(&mut stream, timeout).await?;
                let Some(event) = next else {
                    break;
                };

                for output in mapper.map_event(event)? {
                    yield output;
                }
            }
        })
    }
}

async fn create_response_stream(
    client: &Client<OpenAIConfig>,
    body: Value,
    timeout: Option<Duration>,
) -> ModelResult<async_openai::types::responses::ResponseStream> {
    let responses = client.responses();
    let create = responses.create_stream_byot::<_, ResponseStreamEvent>(body);

    match timeout {
        Some(timeout) => tokio::time::timeout(timeout, create)
            .await
            .map_err(|_| ModelError::Timeout)?
            .map_err(map_openai_error),
        None => create.await.map_err(map_openai_error),
    }
}

async fn next_openai_event(
    stream: &mut async_openai::types::responses::ResponseStream,
    timeout: Option<Duration>,
) -> ModelResult<Option<ResponseStreamEvent>> {
    let next = match timeout {
        Some(timeout) => tokio::time::timeout(timeout, stream.next())
            .await
            .map_err(|_| ModelError::Timeout)?,
        None => stream.next().await,
    };

    next.transpose().map_err(map_openai_error)
}

fn openai_config(config: OpenAiResponsesProviderConfig) -> OpenAIConfig {
    let mut openai_config = OpenAIConfig::new();

    if let Some(api_key) = non_empty(config.api_key) {
        openai_config = openai_config.with_api_key(api_key);
    }
    if let Some(api_base) = non_empty(config.api_base) {
        openai_config = openai_config.with_api_base(api_base);
    }
    if let Some(organization) = non_empty(config.organization) {
        openai_config = openai_config.with_org_id(organization);
    }
    if let Some(project) = non_empty(config.project) {
        openai_config = openai_config.with_project_id(project);
    }

    openai_config
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    })
}

fn build_response_body(
    model_id: &str,
    model_config: &OpenAiResponsesModelConfig,
    request: GenerateRequest,
) -> ModelResult<Value> {
    let mut body = Map::new();
    body.insert("model".to_string(), Value::String(model_id.to_string()));
    body.insert(
        "input".to_string(),
        Value::Array(build_input_items(request.messages)?),
    );
    body.insert("stream".to_string(), Value::Bool(true));

    insert_optional_json(&mut body, "temperature", request.options.temperature);
    insert_optional_json(&mut body, "max_output_tokens", request.options.max_tokens);

    if let Some(tools) = request.tools.filter(|tools| !tools.is_empty()) {
        body.insert("tools".to_string(), Value::Array(build_tools(tools)));
    }
    insert_optional_value(
        &mut body,
        "tool_choice",
        request.tool_choice.map(build_tool_choice),
    );

    insert_model_config(&mut body, model_config);

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

fn insert_extra_body(body: &mut Map<String, Value>, extra_body: &BTreeMap<String, Value>) {
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

fn build_input_items(messages: Vec<Message>) -> ModelResult<Vec<Value>> {
    let mut items = Vec::new();
    for message in messages {
        items.extend(build_message_items(message)?);
    }
    Ok(items)
}

fn build_message_items(message: Message) -> ModelResult<Vec<Value>> {
    match message {
        Message::System { content } => Ok(vec![message_item("system", input_content(content)?)]),
        Message::User { content } => Ok(vec![message_item("user", input_content(content)?)]),
        Message::Assistant { content } => build_assistant_items(content),
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

fn input_content(content: Vec<InputContent>) -> ModelResult<Vec<Value>> {
    content.into_iter().map(input_content_part).collect()
}

fn input_content_part(content: InputContent) -> ModelResult<Value> {
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

fn build_assistant_items(content: Vec<AssistantContent>) -> ModelResult<Vec<Value>> {
    let mut message_content = Vec::new();
    let mut items = Vec::new();

    for content in content {
        match content {
            AssistantContent::Text { text } => message_content.push(json!({
                "type": "output_text",
                "text": text,
            })),
            AssistantContent::Thinking { .. } => {
                return Err(ModelError::client(
                    "OpenAI Responses provider cannot replay generic thinking content",
                ));
            }
            AssistantContent::ToolCall {
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

fn tool_output_content(content: Vec<InputContent>) -> ModelResult<String> {
    let mut text = Vec::new();
    for content in content {
        match content {
            InputContent::Text { text: part } => text.push(part),
            InputContent::Image { .. } => {
                return Err(ModelError::client(
                    "OpenAI function call outputs only support text content",
                ));
            }
        }
    }
    Ok(text.join("\n"))
}

fn build_tools(tools: Vec<ToolDefinition>) -> Vec<Value> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum StreamContentKind {
    Thinking,
    Text,
    ToolCall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct StreamKey {
    output_index: u32,
    content_index: u32,
    kind: StreamContentKind,
}

impl StreamKey {
    fn text(output_index: u32, content_index: u32) -> Self {
        Self {
            output_index,
            content_index,
            kind: StreamContentKind::Text,
        }
    }

    fn thinking(output_index: u32, content_index: u32) -> Self {
        Self {
            output_index,
            content_index,
            kind: StreamContentKind::Thinking,
        }
    }

    fn tool_call(output_index: u32) -> Self {
        Self {
            output_index,
            content_index: 0,
            kind: StreamContentKind::ToolCall,
        }
    }
}

#[derive(Debug, Default)]
struct OpenAiEventMapper {
    index_by_key: BTreeMap<StreamKey, usize>,
    next_index: usize,
    saw_tool_call: bool,
    streamed_tool_arguments: BTreeSet<StreamKey>,
}

impl OpenAiEventMapper {
    fn new() -> Self {
        Self::default()
    }

    fn map_event(&mut self, event: ResponseStreamEvent) -> ModelResult<Vec<AssistantContentEvent>> {
        match event {
            ResponseStreamEvent::ResponseOutputTextDelta(event) => Ok(vec![self.delta(
                StreamKey::text(event.output_index, event.content_index),
                AssistantContentDetail::Text { text: event.delta },
            )]),
            ResponseStreamEvent::ResponseReasoningSummaryTextDelta(event) => Ok(vec![self.delta(
                StreamKey::thinking(event.output_index, event.summary_index),
                AssistantContentDetail::Thinking { text: event.delta },
            )]),
            ResponseStreamEvent::ResponseReasoningTextDelta(event) => Ok(vec![self.delta(
                StreamKey::thinking(event.output_index, event.content_index),
                AssistantContentDetail::Thinking { text: event.delta },
            )]),
            ResponseStreamEvent::ResponseOutputItemAdded(event) => {
                self.map_output_item(event.output_index, &event.item, false)
            }
            ResponseStreamEvent::ResponseOutputItemDone(event) => {
                self.map_output_item(event.output_index, &event.item, true)
            }
            ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(event) => {
                let key = StreamKey::tool_call(event.output_index);
                self.saw_tool_call = true;
                self.streamed_tool_arguments.insert(key);
                Ok(vec![self.delta(
                    key,
                    AssistantContentDetail::ToolCall {
                        id: None,
                        name: None,
                        arguments: event.delta,
                    },
                )])
            }
            ResponseStreamEvent::ResponseFunctionCallArgumentsDone(event) => {
                let key = StreamKey::tool_call(event.output_index);
                self.saw_tool_call = true;
                let arguments = if self.streamed_tool_arguments.contains(&key) {
                    String::new()
                } else {
                    event.arguments
                };
                Ok(vec![self.delta(
                    key,
                    AssistantContentDetail::ToolCall {
                        id: None,
                        name: event.name,
                        arguments,
                    },
                )])
            }
            ResponseStreamEvent::ResponseCompleted(event) => {
                Ok(vec![AssistantContentEvent::Finished {
                    reason: self.finish_reason(FinishReason::Stop),
                    usage: event.response.usage.map(|usage| Usage {
                        input_tokens: Some(usage.input_tokens.into()),
                        output_tokens: Some(usage.output_tokens.into()),
                    }),
                }])
            }
            ResponseStreamEvent::ResponseIncomplete(event) => {
                Ok(vec![AssistantContentEvent::Finished {
                    reason: self.finish_reason(FinishReason::Length),
                    usage: event.response.usage.map(|usage| Usage {
                        input_tokens: Some(usage.input_tokens.into()),
                        output_tokens: Some(usage.output_tokens.into()),
                    }),
                }])
            }
            ResponseStreamEvent::ResponseFailed(event) => Err(response_error(event.response)),
            ResponseStreamEvent::ResponseError(event) => Err(ModelError::protocol(format!(
                "OpenAI stream error: {}{}",
                event.message,
                event
                    .code
                    .map(|code| format!(" ({code})"))
                    .unwrap_or_default()
            ))),
            _ => Ok(Vec::new()),
        }
    }

    fn map_output_item(
        &mut self,
        output_index: u32,
        item: &impl Serialize,
        done: bool,
    ) -> ModelResult<Vec<AssistantContentEvent>> {
        let item = serde_json::to_value(item).map_err(|error| {
            ModelError::protocol(format!("failed to serialize OpenAI output item: {error}"))
        })?;
        let Some(tool_call) = tool_call_from_item(&item) else {
            return Ok(Vec::new());
        };

        let key = StreamKey::tool_call(output_index);
        self.saw_tool_call = true;
        let include_arguments = done && !self.streamed_tool_arguments.contains(&key);
        let arguments = if include_arguments {
            tool_call.arguments
        } else {
            String::new()
        };

        Ok(vec![self.delta(
            key,
            AssistantContentDetail::ToolCall {
                id: tool_call.id,
                name: tool_call.name,
                arguments,
            },
        )])
    }

    fn delta(&mut self, key: StreamKey, delta: AssistantContentDetail) -> AssistantContentEvent {
        let content_index = self.content_index(key);
        AssistantContentEvent::Delta {
            content_index,
            delta,
        }
    }

    fn content_index(&mut self, key: StreamKey) -> usize {
        if let Some(index) = self.index_by_key.get(&key) {
            return *index;
        }

        let index = self.next_index;
        self.next_index += 1;
        self.index_by_key.insert(key, index);
        index
    }

    fn finish_reason(&self, fallback: FinishReason) -> FinishReason {
        if self.saw_tool_call {
            FinishReason::ToolCalls
        } else {
            fallback
        }
    }
}

#[derive(Debug)]
struct ToolCallDelta {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

fn tool_call_from_item(item: &Value) -> Option<ToolCallDelta> {
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return None;
    }

    let id = item
        .get("call_id")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let arguments = match item.get("arguments") {
        Some(Value::String(arguments)) => arguments.clone(),
        Some(arguments) => arguments.to_string(),
        None => String::new(),
    };

    Some(ToolCallDelta {
        id,
        name,
        arguments,
    })
}

fn response_error(response: async_openai::types::responses::Response) -> ModelError {
    if let Some(error) = response.error {
        return ModelError::Server {
            message: error.message,
        };
    }

    ModelError::Server {
        message: format!("OpenAI response failed with status {:?}", response.status),
    }
}

fn map_openai_error(error: OpenAIError) -> ModelError {
    match error {
        OpenAIError::Reqwest(error) if error.is_timeout() => ModelError::Timeout,
        OpenAIError::Reqwest(error) => ModelError::Unknown {
            message: error.to_string(),
        },
        OpenAIError::ApiError(error) => map_api_error(&error),
        OpenAIError::JSONDeserialize(error, content) => ModelError::protocol(format!(
            "failed to deserialize OpenAI response: {error}; content: {content}"
        )),
        OpenAIError::FileSaveError(message) | OpenAIError::FileReadError(message) => {
            ModelError::client(message)
        }
        OpenAIError::InvalidArgument(message) => ModelError::client(message),
        OpenAIError::StreamError(error) => ModelError::Unknown {
            message: error.to_string(),
        },
    }
}

fn map_api_error(error: &async_openai::error::ApiErrorResponse) -> ModelError {
    let message = error.to_string();
    let error_type = error.api_error.r#type.as_deref().unwrap_or_default();
    let code = error.api_error.code.as_deref().unwrap_or_default();
    let status = error.status_code.as_u16();

    if status >= 500 || matches!(error_type, "server_error") || code.starts_with("server_") {
        return ModelError::Server { message };
    }

    if (400..500).contains(&status) {
        return ModelError::Client { message };
    }

    ModelError::Unknown { message }
}

fn infer_capabilities(model_id: &str) -> ModelCapabilities {
    let mut capabilities = ModelCapabilities::default()
        .with_input_modality(InputModality::Text)
        .with_feature(ModelFeature::NativeStreaming)
        .with_feature(ModelFeature::Tools)
        .with_feature(ModelFeature::ToolChoice);

    if is_multimodal_model(model_id) {
        capabilities = capabilities.with_input_modality(InputModality::Image);
    }
    if is_reasoning_model(model_id) {
        capabilities = capabilities.with_feature(ModelFeature::Thinking);
    }

    capabilities
}

fn is_multimodal_model(model_id: &str) -> bool {
    model_id.starts_with("gpt-4.1")
        || model_id.starts_with("gpt-4o")
        || model_id.starts_with("gpt-5")
        || model_id.starts_with("o3")
        || model_id.starts_with("o4")
}

fn is_reasoning_model(model_id: &str) -> bool {
    model_id.starts_with("gpt-5")
        || model_id.starts_with("o1")
        || model_id.starts_with("o3")
        || model_id.starts_with("o4")
}

#[cfg(test)]
mod tests {
    use super::*;
    use copro_core::request::GenerateOptions;

    fn empty_options() -> GenerateOptions {
        GenerateOptions::default()
    }

    #[test]
    fn builds_basic_text_request() {
        let request = GenerateRequest {
            messages: vec![Message::User {
                content: vec![InputContent::Text {
                    text: "hello".to_string(),
                }],
            }],
            tools: None,
            tool_choice: None,
            options: GenerateOptions {
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
    fn builds_function_tools_and_choice() {
        let request = GenerateRequest {
            messages: vec![Message::User {
                content: vec![InputContent::Text {
                    text: "weather".to_string(),
                }],
            }],
            tools: Some(vec![ToolDefinition {
                name: "weather".to_string(),
                description: "Get weather".to_string(),
                parameters: json!({"type":"object"}),
            }]),
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
            extra_body: BTreeMap::from([("metadata".to_string(), json!({"trace_id": "req_123"}))]),
        };
        let request = GenerateRequest {
            messages: vec![Message::User {
                content: vec![InputContent::Text {
                    text: "hello".to_string(),
                }],
            }],
            tools: None,
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
            extra_body: BTreeMap::from([
                ("input".to_string(), json!([])),
                ("model".to_string(), json!("wrong-model")),
                ("stream".to_string(), json!(false)),
                ("metadata".to_string(), json!({"allowed": true})),
            ]),
            ..OpenAiResponsesModelConfig::default()
        };
        let request = GenerateRequest {
            messages: vec![Message::User {
                content: vec![InputContent::Text {
                    text: "hello".to_string(),
                }],
            }],
            tools: None,
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
