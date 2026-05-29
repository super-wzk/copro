use std::collections::{BTreeMap, BTreeSet};

use base64::Engine;
use copro_api::error::{Error, Result};
use copro_api::message::ImageContent;
use copro_api::response::{FinishReason, Usage};
use copro_api::stream::{OutputContentDelta, OutputStreamEvent};
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
    synthetic_output_index_by_item_id: BTreeMap<String, u32>,
    next_synthetic_output_index: u32,
}

impl OpenAiEventMapper {
    pub(crate) fn new() -> Self {
        Self {
            // Keep synthesized indices far away from normal OpenAI output_index
            // values. They are only used when newer stream events omit
            // output_index but include a stable item_id.
            next_synthetic_output_index: 1_000_000_000,
            ..Self::default()
        }
    }

    pub(crate) fn map_event(&mut self, event: Value) -> Result<Vec<OutputStreamEvent>> {
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();

        match event_type {
            "response.output_text.delta" => self.map_text_delta(&event),
            "response.reasoning_summary_text.delta" => self.map_reasoning_summary_delta(&event),
            "response.reasoning_text.delta" => self.map_reasoning_text_delta(&event),
            "response.output_item.added" => self.map_output_item_event(&event, false),
            "response.output_item.done" => self.map_output_item_event(&event, true),
            "response.function_call_arguments.delta" => self.map_function_arguments_delta(&event),
            "response.function_call_arguments.done" => self.map_function_arguments_done(&event),
            "response.image_generation_call.partial_image" => self.map_partial_image(&event),
            "response.created" | "response.in_progress" | "response.queued" => Ok(Vec::new()),
            "response.completed" => Ok(vec![OutputStreamEvent::Finished {
                reason: self.resolve_reason(FinishReason::Stop),
                usage: usage_from_event(&event),
            }]),
            "response.incomplete" => Ok(vec![OutputStreamEvent::Finished {
                reason: self.resolve_reason(FinishReason::Length),
                usage: usage_from_event(&event),
            }]),
            "response.failed" => Err(response_failed_error(&event)),
            "error" => Err(response_error_event(&event)),
            _ => Ok(Vec::new()),
        }
    }

    fn map_text_delta(&mut self, event: &Value) -> Result<Vec<OutputStreamEvent>> {
        let output_index = self.output_index(event, "response.output_text.delta")?;
        let content_index =
            optional_u32_field(event, "content_index", "response.output_text.delta")?.unwrap_or(0);
        let text = string_field(event, "delta", "response.output_text.delta")?;

        Ok(vec![self.delta(
            StreamKey::text(output_index, content_index),
            OutputContentDelta::Text(text),
        )])
    }

    fn map_reasoning_summary_delta(&mut self, event: &Value) -> Result<Vec<OutputStreamEvent>> {
        let output_index = self.output_index(event, "response.reasoning_summary_text.delta")?;
        let summary_index = optional_u32_field(
            event,
            "summary_index",
            "response.reasoning_summary_text.delta",
        )?
        .unwrap_or(0);
        let text = string_field(event, "delta", "response.reasoning_summary_text.delta")?;

        Ok(vec![self.delta(
            StreamKey::thinking(output_index, summary_index),
            OutputContentDelta::Thinking(text),
        )])
    }

    fn map_reasoning_text_delta(&mut self, event: &Value) -> Result<Vec<OutputStreamEvent>> {
        let output_index = self.output_index(event, "response.reasoning_text.delta")?;
        let content_index =
            optional_u32_field(event, "content_index", "response.reasoning_text.delta")?
                .unwrap_or(0);
        let text = string_field(event, "delta", "response.reasoning_text.delta")?;

        Ok(vec![self.delta(
            StreamKey::thinking(output_index, content_index),
            OutputContentDelta::Thinking(text),
        )])
    }

    fn map_output_item_event(
        &mut self,
        event: &Value,
        done: bool,
    ) -> Result<Vec<OutputStreamEvent>> {
        let Some(item) = event.get("item") else {
            return Err(Error::protocol("OpenAI output item event is missing item"));
        };

        if !output_item_can_emit(item, done) {
            return Ok(Vec::new());
        }

        let event_type = if done {
            "response.output_item.done"
        } else {
            "response.output_item.added"
        };
        let output_index = self.output_index(event, event_type)?;
        self.map_output_item_value(output_index, item, done)
    }

    fn map_function_arguments_delta(&mut self, event: &Value) -> Result<Vec<OutputStreamEvent>> {
        let output_index = self.output_index(event, "response.function_call_arguments.delta")?;
        let key = StreamKey::tool_call(output_index);
        self.saw_tool_call = true;
        self.streamed_tool_arguments.insert(key);
        let arguments = string_field(event, "delta", "response.function_call_arguments.delta")?;

        Ok(vec![self.delta(
            key,
            OutputContentDelta::ToolCall {
                id: None,
                name: None,
                arguments,
            },
        )])
    }

    fn map_function_arguments_done(&mut self, event: &Value) -> Result<Vec<OutputStreamEvent>> {
        let output_index = self.output_index(event, "response.function_call_arguments.done")?;
        let key = StreamKey::tool_call(output_index);
        self.saw_tool_call = true;
        let arguments = if self.streamed_tool_arguments.contains(&key) {
            String::new()
        } else {
            string_field(event, "arguments", "response.function_call_arguments.done")?
        };
        let name = event
            .get("name")
            .and_then(Value::as_str)
            .map(ToString::to_string);

        Ok(vec![self.delta(
            key,
            OutputContentDelta::ToolCall {
                id: None,
                name,
                arguments,
            },
        )])
    }

    fn map_partial_image(&mut self, event: &Value) -> Result<Vec<OutputStreamEvent>> {
        let output_index =
            self.output_index(event, "response.image_generation_call.partial_image")?;
        let image_base64 = string_field(
            event,
            "partial_image_b64",
            "response.image_generation_call.partial_image",
        )?;

        Ok(vec![self.delta(
            StreamKey::image(output_index),
            OutputContentDelta::Image(decode_openai_image_base64(&image_base64)?),
        )])
    }

    fn map_output_item_value(
        &mut self,
        output_index: u32,
        item: &Value,
        done: bool,
    ) -> Result<Vec<OutputStreamEvent>> {
        if done && let Some(image) = image_from_item(item)? {
            return Ok(vec![self.delta(
                StreamKey::image(output_index),
                OutputContentDelta::Image(image),
            )]);
        }

        let Some(tool_call) = tool_call_from_item(item) else {
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

    fn output_index(&mut self, event: &Value, event_type: &str) -> Result<u32> {
        if let Some(output_index) = optional_u32_field(event, "output_index", event_type)? {
            return Ok(output_index);
        }

        if let Some(item_id) = event
            .get("item_id")
            .or_else(|| event.pointer("/item/id"))
            .or_else(|| event.pointer("/item/call_id"))
            .and_then(Value::as_str)
        {
            return Ok(self.synthetic_output_index(item_id));
        }

        Ok(self.next_anonymous_output_index())
    }

    fn next_anonymous_output_index(&mut self) -> u32 {
        let index = self.next_synthetic_output_index;
        self.next_synthetic_output_index = self.next_synthetic_output_index.saturating_add(1);
        index
    }

    fn synthetic_output_index(&mut self, item_id: &str) -> u32 {
        if let Some(index) = self.synthetic_output_index_by_item_id.get(item_id) {
            return *index;
        }

        let index = self.next_synthetic_output_index;
        self.next_synthetic_output_index = self.next_synthetic_output_index.saturating_add(1);
        self.synthetic_output_index_by_item_id
            .insert(item_id.to_string(), index);
        index
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

    fn resolve_reason(&self, fallback: FinishReason) -> FinishReason {
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

fn optional_u32_field(event: &Value, field: &str, event_type: &str) -> Result<Option<u32>> {
    let Some(value) = event.get(field) else {
        return Ok(None);
    };
    let Some(value) = value.as_u64() else {
        return Err(Error::protocol(format!(
            "OpenAI stream event `{event_type}` field `{field}` must be an unsigned integer"
        )));
    };
    let value = u32::try_from(value).map_err(|_| {
        Error::protocol(format!(
            "OpenAI stream event `{event_type}` field `{field}` exceeds u32"
        ))
    })?;

    Ok(Some(value))
}

fn string_field(event: &Value, field: &str, event_type: &str) -> Result<String> {
    event
        .get(field)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| {
            Error::protocol(format!(
                "OpenAI stream event `{event_type}` is missing string field `{field}`"
            ))
        })
}

fn output_item_can_emit(item: &Value, done: bool) -> bool {
    tool_call_from_item(item).is_some()
        || (done
            && item.get("type").and_then(Value::as_str) == Some("image_generation_call")
            && item.get("result").and_then(Value::as_str).is_some())
}

fn usage_from_event(event: &Value) -> Option<Usage> {
    let usage = event.get("response")?.get("usage")?;
    Some(Usage {
        input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
    })
}

fn response_failed_error(event: &Value) -> Error {
    if let Some(message) = event
        .pointer("/response/error/message")
        .and_then(Value::as_str)
    {
        return Error::Server {
            message: message.to_string(),
        };
    }

    if let Some(status) = event.pointer("/response/status").and_then(Value::as_str) {
        return Error::Server {
            message: format!("OpenAI response failed with status {status}"),
        };
    }

    Error::Server {
        message: "OpenAI response failed".to_string(),
    }
}

fn response_error_event(event: &Value) -> Error {
    let message = event
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("OpenAI stream error");
    let code = event
        .get("code")
        .and_then(Value::as_str)
        .map(|code| format!(" ({code})"))
        .unwrap_or_default();

    Error::protocol(format!("OpenAI stream error: {message}{code}"))
}

fn image_from_item(item: &Value) -> Result<Option<ImageContent>> {
    if item.get("type").and_then(Value::as_str) != Some("image_generation_call") {
        return Ok(None);
    }

    let Some(image_base64) = item.get("result").and_then(Value::as_str) else {
        return Ok(None);
    };

    decode_openai_image_base64(image_base64).map(Some)
}

fn decode_openai_image_base64(image_base64: &str) -> Result<ImageContent> {
    let data = base64::engine::general_purpose::STANDARD
        .decode(image_base64)
        .map_err(|error| {
            Error::protocol(format!("failed to decode OpenAI image output: {error}"))
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
    use copro_api::stream::OutputStreamEvent;
    use serde_json::json;

    #[test]
    fn maps_openai_partial_image_event() {
        let image_base64 = base64::engine::general_purpose::STANDARD.encode([1_u8, 2, 3]);
        let mut mapper = OpenAiEventMapper::new();

        let events = mapper
            .map_event(json!({
                "type": "response.image_generation_call.partial_image",
                "output_index": 0,
                "item_id": "image_call",
                "partial_image_index": 0,
                "partial_image_b64": image_base64,
            }))
            .unwrap();

        assert_eq!(
            events,
            vec![OutputStreamEvent::Delta {
                content_index: 0,
                delta: OutputContentDelta::Image(ImageContent::Data {
                    mime_type: "image/png".to_string(),
                    data: vec![1, 2, 3],
                }),
            }]
        );
    }

    #[test]
    fn ignores_reasoning_output_item_without_sequence_number() {
        let mut mapper = OpenAiEventMapper::new();

        let events = mapper
            .map_event(json!({
                "type": "response.output_item.added",
                "item": {
                    "content": [],
                    "encrypted_content": "",
                    "id": "rs_BFxBJSl5LEwCIOj8UciaqCixzLOIXjLz",
                    "status": "in_progress",
                    "summary": [],
                    "type": "reasoning"
                }
            }))
            .unwrap();

        assert!(events.is_empty());
    }

    #[test]
    fn maps_completed_event_without_output() {
        let mut mapper = OpenAiEventMapper::new();

        let events = mapper
            .map_event(json!({
                "type": "response.completed",
                "sequence_number": 101,
                "response": {
                    "status": "completed",
                    "usage": {
                        "input_tokens": 149,
                        "output_tokens": 83
                    }
                }
            }))
            .unwrap();

        assert_eq!(
            events,
            vec![OutputStreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: Some(Usage {
                    input_tokens: Some(149),
                    output_tokens: Some(83),
                }),
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
