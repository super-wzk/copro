use crate::error::*;
use crate::response::*;
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;

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

pub type ModelStream<'a> =
    Pin<Box<dyn Stream<Item = ModelResult<AssistantContentEvent>> + Send + 'a>>;
