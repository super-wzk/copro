use crate::error::*;
use crate::message::{ImageContent, Message, OutputContent, ToolCall};
use crate::request::GenerateRequest;
use crate::response::*;
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::mem;
use std::pin::Pin;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputStreamEvent {
    Delta {
        content_index: usize,
        delta: OutputContentDelta,
    },
    Finished {
        reason: FinishReason,
        usage: Option<Usage>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputContentDelta {
    Thinking(String),
    Text(String),
    Image(ImageContent),
    ToolCall {
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
}

pub type ModelStream<'a> = Pin<Box<dyn Stream<Item = Result<OutputStreamEvent>> + Send + 'a>>;

/// A live model that can generate responses for requests.
pub trait Model: Send + Sync {
    /// Starts a streaming generation request.
    fn stream(&self, request: GenerateRequest) -> ModelStream<'_>;
}

#[derive(Debug, Default)]
pub struct OutputStreamState {
    content: Vec<Option<OutputContentState>>,
    finished: bool,
}

impl OutputStreamState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, event: OutputStreamEvent) -> Result<Option<GenerateResponse>> {
        if self.finished {
            return Err(Error::protocol("stream already finished"));
        }

        match event {
            OutputStreamEvent::Delta {
                content_index,
                delta,
            } => {
                self.apply_delta(content_index, delta)?;
                Ok(None)
            }
            OutputStreamEvent::Finished { reason, usage } => {
                self.finished = true;
                Ok(Some(GenerateResponse {
                    message: Message::Assistant(finish_output_content(mem::take(
                        &mut self.content,
                    ))?),
                    reason,
                    usage,
                }))
            }
        }
    }

    fn apply_delta(&mut self, content_index: usize, delta: OutputContentDelta) -> Result<()> {
        if self.content.len() <= content_index {
            self.content.resize_with(content_index + 1, || None);
        }
        if let Some(state) = &mut self.content[content_index] {
            state.apply_delta(content_index, delta)
        } else {
            self.content[content_index] = Some(OutputContentState::from_delta(delta));
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OutputContentState {
    Thinking(String),
    Text(String),
    Image(ImageContent),
    ToolCall {
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
}

impl OutputContentState {
    fn from_delta(delta: OutputContentDelta) -> Self {
        match delta {
            OutputContentDelta::Thinking(text) => Self::Thinking(text),
            OutputContentDelta::Text(text) => Self::Text(text),
            OutputContentDelta::Image(image) => Self::Image(image),
            OutputContentDelta::ToolCall {
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

    fn apply_delta(&mut self, content_index: usize, delta: OutputContentDelta) -> Result<()> {
        match self {
            Self::Thinking(text) => {
                let OutputContentDelta::Thinking(delta) = delta else {
                    return Err(Error::protocol(format!(
                        "content delta type changed at index {content_index}"
                    )));
                };
                text.push_str(&delta);
            }
            Self::Text(text) => {
                let OutputContentDelta::Text(delta) = delta else {
                    return Err(Error::protocol(format!(
                        "content delta type changed at index {content_index}"
                    )));
                };
                text.push_str(&delta);
            }
            Self::Image(image) => {
                let OutputContentDelta::Image(delta) = delta else {
                    return Err(Error::protocol(format!(
                        "content delta type changed at index {content_index}"
                    )));
                };
                *image = delta;
            }
            Self::ToolCall {
                id,
                name,
                arguments,
            } => {
                let OutputContentDelta::ToolCall {
                    id: delta_id,
                    name: delta_name,
                    arguments: delta_arguments,
                } = delta
                else {
                    return Err(Error::protocol(format!(
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

    fn finish(self) -> Result<OutputContent> {
        match self {
            Self::Thinking(text) => Ok(OutputContent::Thinking(text)),
            Self::Text(text) => Ok(OutputContent::Text(text)),
            Self::Image(image) => Ok(OutputContent::Image(image)),
            Self::ToolCall {
                id,
                name,
                arguments,
            } => Ok(OutputContent::ToolCall(ToolCall {
                id: id.ok_or_else(|| Error::protocol("tool call is missing id"))?,
                name: name.ok_or_else(|| Error::protocol("tool call is missing name"))?,
                arguments: parse_arguments(&arguments)?,
            })),
        }
    }
}

fn finish_output_content(content: Vec<Option<OutputContentState>>) -> Result<Vec<OutputContent>> {
    content
        .into_iter()
        .enumerate()
        .map(|(index, detail)| {
            let detail = detail
                .ok_or_else(|| Error::protocol(format!("missing content at index {index}")))?;
            detail.finish()
        })
        .collect()
}

fn parse_arguments(arguments: &str) -> Result<Map<String, Value>> {
    if arguments.trim().is_empty() {
        return Ok(Map::new());
    }

    match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(arguments)) => Ok(arguments),
        Ok(_) => Err(Error::protocol("tool call arguments must be a JSON object")),
        Err(error) => Err(Error::protocol(format!(
            "failed to parse tool call arguments: {error}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_collects_text() {
        let mut state = OutputStreamState::new();

        state
            .apply(OutputStreamEvent::Delta {
                content_index: 0,
                delta: OutputContentDelta::Text("Hel".to_string()),
            })
            .unwrap();
        state
            .apply(OutputStreamEvent::Delta {
                content_index: 0,
                delta: OutputContentDelta::Text("lo".to_string()),
            })
            .unwrap();

        let response = state
            .apply(OutputStreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            })
            .unwrap()
            .unwrap();

        assert_eq!(
            response.message,
            Message::Assistant(vec![OutputContent::Text("Hello".to_string())])
        );
    }

    #[test]
    fn image_delta_replaces_previous_image_at_same_index() {
        let mut state = OutputStreamState::new();

        state
            .apply(OutputStreamEvent::Delta {
                content_index: 0,
                delta: OutputContentDelta::Image(ImageContent::Url {
                    url: "data:image/png;base64,partial".to_string(),
                }),
            })
            .unwrap();
        state
            .apply(OutputStreamEvent::Delta {
                content_index: 0,
                delta: OutputContentDelta::Image(ImageContent::Url {
                    url: "data:image/png;base64,final".to_string(),
                }),
            })
            .unwrap();

        let response = state
            .apply(OutputStreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            })
            .unwrap()
            .unwrap();

        assert_eq!(
            response.message,
            Message::Assistant(vec![OutputContent::Image(ImageContent::Url {
                url: "data:image/png;base64,final".to_string(),
            })])
        );
    }

    #[test]
    fn image_deltas_at_different_indices_collect_multiple_images() {
        let mut state = OutputStreamState::new();

        state
            .apply(OutputStreamEvent::Delta {
                content_index: 0,
                delta: OutputContentDelta::Image(ImageContent::Url {
                    url: "data:image/png;base64,first".to_string(),
                }),
            })
            .unwrap();
        state
            .apply(OutputStreamEvent::Delta {
                content_index: 1,
                delta: OutputContentDelta::Image(ImageContent::Data {
                    mime_type: "image/png".to_string(),
                    data: vec![1, 2, 3],
                }),
            })
            .unwrap();

        let response = state
            .apply(OutputStreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            })
            .unwrap()
            .unwrap();

        assert_eq!(
            response.message,
            Message::Assistant(vec![
                OutputContent::Image(ImageContent::Url {
                    url: "data:image/png;base64,first".to_string(),
                }),
                OutputContent::Image(ImageContent::Data {
                    mime_type: "image/png".to_string(),
                    data: vec![1, 2, 3],
                }),
            ])
        );
    }
}
