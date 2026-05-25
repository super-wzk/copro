use crate::error::*;
use crate::message::{AssistantContent, Message};
use crate::response::*;
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
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

#[derive(Debug, Default)]
pub struct AssistantStreamState {
    content: Vec<Option<AssistantContentState>>,
    finished: bool,
}

impl AssistantStreamState {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn collect(mut stream: ModelStream<'_>) -> ModelResult<GenerateResponse> {
        let mut state = Self::new();

        while let Some(event) = stream.next().await {
            if let Some(response) = state.apply(event?)? {
                return Ok(response);
            }
        }

        Err(ModelError::protocol("stream ended before finished event"))
    }

    pub fn apply(&mut self, event: AssistantContentEvent) -> ModelResult<Option<GenerateResponse>> {
        if self.finished {
            return Err(ModelError::protocol("stream already finished"));
        }

        match event {
            AssistantContentEvent::Delta {
                content_index,
                delta,
            } => {
                self.apply_delta(content_index, delta)?;
                Ok(None)
            }
            AssistantContentEvent::Finished { reason, usage } => {
                self.finished = true;
                Ok(Some(GenerateResponse {
                    message: Message::Assistant {
                        content: finish_content(std::mem::take(&mut self.content))?,
                    },
                    finish_reason: reason,
                    usage,
                }))
            }
        }
    }

    fn apply_delta(
        &mut self,
        content_index: usize,
        delta: AssistantContentDetail,
    ) -> ModelResult<()> {
        if self.content.len() <= content_index {
            self.content.resize_with(content_index + 1, || None);
        }
        if let Some(state) = &mut self.content[content_index] {
            state.apply_delta(content_index, delta)
        } else {
            self.content[content_index] = Some(AssistantContentState::from_delta(delta));
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AssistantContentState {
    Thinking(String),
    Text(String),
    ToolCall {
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
}

impl AssistantContentState {
    fn from_delta(delta: AssistantContentDetail) -> Self {
        match delta {
            AssistantContentDetail::Thinking { text } => Self::Thinking(text),
            AssistantContentDetail::Text { text } => Self::Text(text),
            AssistantContentDetail::ToolCall {
                id,
                name,
                arguments,
            } => Self::ToolCall {
                id,
                name,
                arguments,
            },
        }
    }

    fn apply_delta(
        &mut self,
        content_index: usize,
        delta: AssistantContentDetail,
    ) -> ModelResult<()> {
        match self {
            Self::Thinking(text) => {
                let AssistantContentDetail::Thinking { text: delta } = delta else {
                    return Err(ModelError::protocol(format!(
                        "content delta type changed at index {content_index}"
                    )));
                };
                text.push_str(&delta);
            }
            Self::Text(text) => {
                let AssistantContentDetail::Text { text: delta } = delta else {
                    return Err(ModelError::protocol(format!(
                        "content delta type changed at index {content_index}"
                    )));
                };
                text.push_str(&delta);
            }
            Self::ToolCall {
                id,
                name,
                arguments,
            } => {
                let AssistantContentDetail::ToolCall {
                    id: delta_id,
                    name: delta_name,
                    arguments: delta_arguments,
                } = delta
                else {
                    return Err(ModelError::protocol(format!(
                        "content delta type changed at index {content_index}"
                    )));
                };
                if delta_id.is_some() {
                    *id = delta_id;
                }
                if delta_name.is_some() {
                    *name = delta_name;
                }
                arguments.push_str(&delta_arguments);
            }
        }

        Ok(())
    }

    fn finish(self) -> ModelResult<AssistantContent> {
        match self {
            Self::Thinking(text) => Ok(AssistantContent::Thinking { text }),
            Self::Text(text) => Ok(AssistantContent::Text { text }),
            Self::ToolCall {
                id,
                name,
                arguments,
            } => Ok(AssistantContent::ToolCall {
                id: id.ok_or_else(|| ModelError::protocol("tool call is missing id"))?,
                name: name.ok_or_else(|| ModelError::protocol("tool call is missing name"))?,
                arguments: parse_arguments(&arguments)?,
            }),
        }
    }
}

fn finish_content(
    content: Vec<Option<AssistantContentState>>,
) -> ModelResult<Vec<AssistantContent>> {
    content
        .into_iter()
        .enumerate()
        .map(|(index, detail)| {
            let detail = detail
                .ok_or_else(|| ModelError::protocol(format!("missing content at index {index}")))?;
            detail.finish()
        })
        .collect()
}

fn parse_arguments(arguments: &str) -> ModelResult<Map<String, Value>> {
    if arguments.trim().is_empty() {
        return Ok(Map::new());
    }

    match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(arguments)) => Ok(arguments),
        Ok(_) => Err(ModelError::protocol(
            "tool call arguments must be a JSON object",
        )),
        Err(error) => Err(ModelError::protocol(format!(
            "failed to parse tool call arguments: {error}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_collects_text() {
        let mut state = AssistantStreamState::new();

        state
            .apply(AssistantContentEvent::Delta {
                content_index: 0,
                delta: AssistantContentDetail::Text {
                    text: "Hel".to_string(),
                },
            })
            .unwrap();
        state
            .apply(AssistantContentEvent::Delta {
                content_index: 0,
                delta: AssistantContentDetail::Text {
                    text: "lo".to_string(),
                },
            })
            .unwrap();

        let response = state
            .apply(AssistantContentEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            })
            .unwrap()
            .unwrap();

        assert_eq!(
            response.message,
            Message::Assistant {
                content: vec![AssistantContent::Text {
                    text: "Hello".to_string()
                }]
            }
        );
    }
}
