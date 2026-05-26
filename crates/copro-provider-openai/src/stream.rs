use std::collections::{BTreeMap, BTreeSet};

use async_openai::types::responses::ResponseStreamEvent;
use base64::Engine;
use copro_core::error::{ModelError, ModelResult};
use copro_core::message::ImageContent;
use copro_core::response::{FinishReason, Usage};
use copro_core::stream::{OutputContentDelta, OutputStreamEvent};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum StreamContentKind {
    Image,
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

    fn image(output_index: u32) -> Self {
        Self {
            output_index,
            content_index: 0,
            kind: StreamContentKind::Image,
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
pub(crate) struct OpenAiEventMapper {
    index_by_key: BTreeMap<StreamKey, usize>,
    next_index: usize,
    saw_tool_call: bool,
    streamed_tool_arguments: BTreeSet<StreamKey>,
}

impl OpenAiEventMapper {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn map_event(
        &mut self,
        event: ResponseStreamEvent,
    ) -> ModelResult<Vec<OutputStreamEvent>> {
        match event {
            ResponseStreamEvent::ResponseOutputTextDelta(event) => Ok(vec![self.delta(
                StreamKey::text(event.output_index, event.content_index),
                OutputContentDelta::Text { text: event.delta },
            )]),
            ResponseStreamEvent::ResponseReasoningSummaryTextDelta(event) => Ok(vec![self.delta(
                StreamKey::thinking(event.output_index, event.summary_index),
                OutputContentDelta::Thinking { text: event.delta },
            )]),
            ResponseStreamEvent::ResponseReasoningTextDelta(event) => Ok(vec![self.delta(
                StreamKey::thinking(event.output_index, event.content_index),
                OutputContentDelta::Thinking { text: event.delta },
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
                    OutputContentDelta::ToolCall {
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
                    OutputContentDelta::ToolCall {
                        id: None,
                        name: event.name,
                        arguments,
                    },
                )])
            }
            ResponseStreamEvent::ResponseImageGenerationCallPartialImage(event) => {
                Ok(vec![self.delta(
                    StreamKey::image(event.output_index),
                    OutputContentDelta::Image {
                        image: decode_openai_image_base64(&event.partial_image_b64)?,
                    },
                )])
            }
            ResponseStreamEvent::ResponseCompleted(event) => {
                Ok(vec![OutputStreamEvent::Finished {
                    reason: self.finish_reason(FinishReason::Stop),
                    usage: event.response.usage.map(|usage| Usage {
                        input_tokens: Some(usage.input_tokens.into()),
                        output_tokens: Some(usage.output_tokens.into()),
                    }),
                }])
            }
            ResponseStreamEvent::ResponseIncomplete(event) => {
                Ok(vec![OutputStreamEvent::Finished {
                    reason: self.finish_reason(FinishReason::Length),
                    usage: event.response.usage.map(|usage| Usage {
                        input_tokens: Some(usage.input_tokens.into()),
                        output_tokens: Some(usage.output_tokens.into()),
                    }),
                }])
            }
            ResponseStreamEvent::ResponseFailed(event) => {
                Err(crate::error::response_error(event.response))
            }
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
    ) -> ModelResult<Vec<OutputStreamEvent>> {
        let item = serde_json::to_value(item).map_err(|error| {
            ModelError::protocol(format!("failed to serialize OpenAI output item: {error}"))
        })?;
        if done && let Some(image) = image_from_item(&item)? {
            return Ok(vec![self.delta(
                StreamKey::image(output_index),
                OutputContentDelta::Image { image },
            )]);
        }

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
            OutputContentDelta::ToolCall {
                id: tool_call.id,
                name: tool_call.name,
                arguments,
            },
        )])
    }

    fn delta(&mut self, key: StreamKey, delta: OutputContentDelta) -> OutputStreamEvent {
        let content_index = self.content_index(key);
        OutputStreamEvent::Delta {
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

fn image_from_item(item: &Value) -> ModelResult<Option<ImageContent>> {
    if item.get("type").and_then(Value::as_str) != Some("image_generation_call") {
        return Ok(None);
    }

    let Some(image_base64) = item.get("result").and_then(Value::as_str) else {
        return Ok(None);
    };

    decode_openai_image_base64(image_base64).map(Some)
}

fn decode_openai_image_base64(image_base64: &str) -> ModelResult<ImageContent> {
    let data = base64::engine::general_purpose::STANDARD
        .decode(image_base64)
        .map_err(|error| {
            ModelError::protocol(format!("failed to decode OpenAI image output: {error}"))
        })?;

    Ok(ImageContent::Data {
        mime_type: "image/png".to_string(),
        data,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::responses::ResponseImageGenCallPartialImageEvent;
    use base64::Engine as _;
    use copro_core::stream::OutputStreamEvent;
    use serde_json::json;

    #[test]
    fn maps_openai_partial_image_event() {
        let image_base64 = base64::engine::general_purpose::STANDARD.encode([1_u8, 2, 3]);
        let mut mapper = OpenAiEventMapper::new();

        let events = mapper
            .map_event(
                ResponseStreamEvent::ResponseImageGenerationCallPartialImage(
                    ResponseImageGenCallPartialImageEvent {
                        sequence_number: 0,
                        output_index: 0,
                        item_id: "image_call".to_string(),
                        partial_image_index: 0,
                        partial_image_b64: image_base64,
                    },
                ),
            )
            .unwrap();

        assert_eq!(
            events,
            vec![OutputStreamEvent::Delta {
                content_index: 0,
                delta: OutputContentDelta::Image {
                    image: ImageContent::Data {
                        mime_type: "image/png".to_string(),
                        data: vec![1, 2, 3],
                    },
                },
            }]
        );
    }

    #[test]
    fn maps_openai_final_image_item() {
        let image_base64 = base64::engine::general_purpose::STANDARD.encode([4_u8, 5, 6]);
        let image = image_from_item(&json!({
            "type": "image_generation_call",
            "id": "image_call",
            "status": "completed",
            "result": image_base64,
        }))
        .unwrap();

        assert_eq!(
            image,
            Some(ImageContent::Data {
                mime_type: "image/png".to_string(),
                data: vec![4, 5, 6],
            })
        );
    }
}
