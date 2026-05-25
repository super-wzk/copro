use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::pin::Pin;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolResultStatus {
    Success,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InputContent {
    Text { text: String },
    Image { mime_type: String, data: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AssistantContent {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Map<String, Value>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Message {
    System {
        content: Vec<InputContent>,
    },
    User {
        content: Vec<InputContent>,
    },
    Assistant {
        content: Vec<AssistantContent>,
    },
    Tool {
        call_id: String,
        name: String,
        status: ToolResultStatus,
        content: Vec<InputContent>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderError {
    RateLimit {
        retry_after: Option<Duration>,
        message: String,
    },
    Auth {
        message: String,
    },
    Timeout,
    ServerError {
        message: String,
    },
    Unknown {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Specific { name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    Error,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssistantContentEvent {
    Delta {
        content_index: usize,
        delta: AssistantContentDetail,
    },
    Finished {
        reason: FinishReason,
        usage: Option<Usage>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssistantContentDetail {
    Thinking {
        text: String,
    },
    Text {
        text: String,
    },
    ToolCall {
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerateOptions {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerateRequest {
    pub messages: Vec<Message>,
    pub tools: Option<Vec<ToolDefinition>>,
    pub tool_choice: Option<ToolChoice>,
    pub options: GenerateOptions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerateResponse {
    pub message: Message,
    pub finish_reason: FinishReason,
    pub usage: Option<Usage>,
}

pub type ModelResult<T> = Result<T, ProviderError>;
pub type ModelStream<'a> =
    Pin<Box<dyn Stream<Item = ModelResult<AssistantContentEvent>> + Send + 'a>>;
pub trait ChatModel {
    fn stream(&'_ self, request: GenerateRequest) -> ModelStream<'_>;
}
