use copro_api::message::{ImageContent, InputContent, InputMessage, ToolResult};
use copro_api::stream::OutputContentDelta;
use std::collections::HashMap;

#[cfg(test)]
use copro_api::message::{ToolCallId, ToolResultStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockId(u64);

impl BlockId {
    pub fn get(self) -> u64 {
        self.0
    }

    fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppState {
    blocks: Vec<BlockState>,
    content_blocks: HashMap<usize, BlockId>,
    next_block_id: BlockId,
    revision: u64,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            blocks: Vec::new(),
            content_blocks: HashMap::new(),
            next_block_id: BlockId(1),
            revision: 0,
        }
    }
}

impl AppState {
    pub fn blocks(&self) -> &[BlockState] {
        &self.blocks
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn toggle_all_folds(&mut self) {
        let any_collapsed = self
            .blocks
            .iter()
            .any(|block| block.foldable && !block.expanded);
        let next_expanded = any_collapsed;
        let mut any_foldable = false;

        for block in &mut self.blocks {
            if block.foldable {
                any_foldable = true;
                block.expanded = next_expanded;
            }
        }

        if any_foldable {
            self.bump_revision();
        }
    }

    pub fn push_input(&mut self, message: InputMessage) {
        // This first TUI renders only user messages as visible conversation input.
        if let InputMessage::User(content) = message {
            self.content_blocks.clear();
            self.push_block(BlockKind::User { content }, false);
        }
    }

    pub fn push_error(&mut self, text: impl Into<String>) {
        self.push_block(BlockKind::Error { text: text.into() }, false);
    }

    pub fn push_command_output(&mut self, text: impl Into<String>) {
        self.push_block(
            BlockKind::Command {
                text: text.into(),
                is_error: false,
            },
            false,
        );
    }

    pub fn push_command_error(&mut self, text: impl Into<String>) {
        self.push_block(
            BlockKind::Command {
                text: text.into(),
                is_error: true,
            },
            false,
        );
    }

    pub fn clear_conversation(&mut self) {
        if self.blocks.is_empty() {
            return;
        }

        self.blocks.clear();
        self.content_blocks.clear();
        self.bump_revision();
    }

    pub fn apply_delta(&mut self, delta: OutputContentDelta) {
        self.apply_delta_at(0, delta);
    }

    pub fn apply_delta_at(&mut self, content_index: usize, delta: OutputContentDelta) {
        match delta {
            OutputContentDelta::Thinking(text) => self.append_thinking(content_index, text),
            OutputContentDelta::Text(text) => self.append_assistant_text(content_index, text),
            OutputContentDelta::Image(image) => self.append_assistant_image(content_index, image),
            OutputContentDelta::ToolCall {
                id,
                name,
                arguments,
            } => self.append_tool_call(content_index, id, name, arguments),
        }
    }

    pub fn apply_tool_result(&mut self, result: ToolResult) {
        let call_id = result.call_id.as_str().to_string();

        if let Some(tool) = self
            .blocks
            .iter_mut()
            .find_map(|block| match &mut block.kind {
                BlockKind::Tool(tool) if tool.call_id.as_deref() == Some(call_id.as_str()) => {
                    Some(tool)
                }
                _ => None,
            })
        {
            tool.result = Some(result);
            self.content_blocks.clear();
            self.bump_revision();
            return;
        }

        let name = result.name.clone();
        self.push_block(
            BlockKind::Tool(ToolBlockState {
                call_id: Some(call_id),
                name,
                arguments: String::new(),
                result: Some(result),
            }),
            true,
        );
        self.content_blocks.clear();
    }

    fn append_thinking(&mut self, content_index: usize, text: String) {
        if let Some(BlockState {
            kind: BlockKind::Thinking { text: existing },
            ..
        }) = self.content_block_mut(content_index)
        {
            existing.push_str(&text);
            self.bump_revision();
        } else {
            let id = self.push_block(BlockKind::Thinking { text }, true);
            self.content_blocks.insert(content_index, id);
        }
    }

    fn append_assistant_text(&mut self, content_index: usize, text: String) {
        if let Some(BlockState {
            kind: BlockKind::Assistant { items },
            ..
        }) = self.content_block_mut(content_index)
        {
            if let Some(AssistantItem::Text(existing)) = items.last_mut() {
                existing.push_str(&text);
            } else {
                items.push(AssistantItem::Text(text));
            }
            self.bump_revision();
        } else {
            let id = self.push_block(
                BlockKind::Assistant {
                    items: vec![AssistantItem::Text(text)],
                },
                false,
            );
            self.content_blocks.insert(content_index, id);
        }
    }

    fn append_assistant_image(&mut self, content_index: usize, image: ImageContent) {
        if let Some(BlockState {
            kind: BlockKind::Assistant { items },
            ..
        }) = self.content_block_mut(content_index)
        {
            if let Some(AssistantItem::Image(existing)) = items
                .iter_mut()
                .find(|item| matches!(item, AssistantItem::Image(_)))
            {
                *existing = image;
            } else {
                items.push(AssistantItem::Image(image));
            }
            self.bump_revision();
        } else {
            let id = self.push_block(
                BlockKind::Assistant {
                    items: vec![AssistantItem::Image(image)],
                },
                false,
            );
            self.content_blocks.insert(content_index, id);
        }
    }

    fn append_tool_call(
        &mut self,
        content_index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    ) {
        if let Some(BlockState {
            kind: BlockKind::Tool(tool),
            ..
        }) = self.content_block_mut(content_index)
        {
            let same_streamed_call = tool.result.is_none()
                && match (id.as_deref(), tool.call_id.as_deref()) {
                    (Some(incoming), Some(existing)) => incoming == existing,
                    (Some(_), None) | (None, _) => true,
                };

            if same_streamed_call {
                if tool.call_id.is_none() {
                    tool.call_id = id.clone();
                }
                if let Some(name) = &name {
                    tool.name.clone_from(name);
                }
                tool.arguments.push_str(&arguments);
                self.bump_revision();
                return;
            }
        }

        let block_id = self.push_block(
            BlockKind::Tool(ToolBlockState {
                call_id: id,
                name: name.unwrap_or_default(),
                arguments,
                result: None,
            }),
            true,
        );
        self.content_blocks.insert(content_index, block_id);
    }

    fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    fn content_block_mut(&mut self, content_index: usize) -> Option<&mut BlockState> {
        let block_id = self.content_blocks.get(&content_index).copied()?;
        self.blocks.iter_mut().find(|block| block.id == block_id)
    }

    fn push_block(&mut self, kind: BlockKind, foldable: bool) -> BlockId {
        let id = self.next_block_id;
        self.next_block_id = self.next_block_id.next();
        self.blocks.push(BlockState {
            id,
            kind,
            foldable,
            expanded: true,
        });
        self.bump_revision();
        id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockState {
    id: BlockId,
    kind: BlockKind,
    foldable: bool,
    expanded: bool,
}

impl BlockState {
    pub fn id(&self) -> BlockId {
        self.id
    }

    pub fn kind(&self) -> &BlockKind {
        &self.kind
    }

    pub fn is_foldable(&self) -> bool {
        self.foldable
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKind {
    User { content: Vec<InputContent> },
    Thinking { text: String },
    Assistant { items: Vec<AssistantItem> },
    Error { text: String },
    Command { text: String, is_error: bool },
    Tool(ToolBlockState),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantItem {
    Text(String),
    Image(ImageContent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolBlockState {
    pub call_id: Option<String>,
    pub name: String,
    pub arguments: String,
    pub result: Option<ToolResult>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_input_creates_user_block_without_mapping_protocol() {
        let mut state = AppState::default();

        state.push_input(InputMessage::User(vec![InputContent::Text(
            "fix unicode cursor".to_string(),
        )]));

        assert_eq!(state.blocks().len(), 1);
        assert!(matches!(
            state.blocks()[0].kind(),
            BlockKind::User { content }
                if content == &vec![InputContent::Text("fix unicode cursor".to_string())]
        ));
    }

    #[test]
    fn user_input_resets_active_content_index_routing() {
        let mut state = AppState::default();

        state.apply_delta_at(0, OutputContentDelta::Text("first".to_string()));
        state.push_input(InputMessage::User(vec![InputContent::Text(
            "next turn".to_string(),
        )]));
        state.apply_delta_at(0, OutputContentDelta::Text("second".to_string()));

        assert_eq!(state.blocks().len(), 3);
        assert!(matches!(
            state.blocks()[0].kind(),
            BlockKind::Assistant { items }
                if items == &vec![AssistantItem::Text("first".to_string())]
        ));
        assert!(matches!(
            state.blocks()[2].kind(),
            BlockKind::Assistant { items }
                if items == &vec![AssistantItem::Text("second".to_string())]
        ));
    }

    #[test]
    fn push_error_appends_error_block() {
        let mut state = AppState::default();

        state.push_error("request failed: missing api key");

        assert_eq!(state.blocks().len(), 1);
        assert!(matches!(
            state.blocks()[0].kind(),
            BlockKind::Error { text } if text == "request failed: missing api key"
        ));
        assert!(!state.blocks()[0].is_foldable());
    }

    #[test]
    fn error_block_does_not_replace_active_delta_routing() {
        let mut state = AppState::default();

        state.apply_delta_at(0, OutputContentDelta::Text("hello".to_string()));
        state.push_error("network error");
        state.apply_delta_at(0, OutputContentDelta::Text(" world".to_string()));

        assert_eq!(state.blocks().len(), 2);
        assert!(matches!(
            state.blocks()[0].kind(),
            BlockKind::Assistant { items }
                if items == &vec![AssistantItem::Text("hello world".to_string())]
        ));
        assert!(matches!(
            state.blocks()[1].kind(),
            BlockKind::Error { text } if text == "network error"
        ));
    }

    #[test]
    fn thinking_deltas_append_to_current_thinking_block() {
        let mut state = AppState::default();

        state.apply_delta(OutputContentDelta::Thinking("Need inspect".to_string()));
        state.apply_delta(OutputContentDelta::Thinking(" cursor code".to_string()));

        assert_eq!(state.blocks().len(), 1);
        assert!(matches!(
            state.blocks()[0].kind(),
            BlockKind::Thinking { text } if text == "Need inspect cursor code"
        ));
    }

    #[test]
    fn text_deltas_append_to_current_assistant_text_item() {
        let mut state = AppState::default();

        state.apply_delta(OutputContentDelta::Text("hello".to_string()));
        state.apply_delta(OutputContentDelta::Text(" world".to_string()));

        assert!(matches!(
            state.blocks()[0].kind(),
            BlockKind::Assistant { items }
                if items == &vec![AssistantItem::Text("hello world".to_string())]
        ));
    }

    #[test]
    fn image_delta_stays_inside_assistant_block() {
        let mut state = AppState::default();
        let image = ImageContent::Data {
            mime_type: "image/png".to_string(),
            data: vec![1, 2, 3].into(),
        };

        state.apply_delta(OutputContentDelta::Text("hello".to_string()));
        state.apply_delta(OutputContentDelta::Image(image.clone()));

        assert_eq!(state.blocks().len(), 1);
        assert!(matches!(
            state.blocks()[0].kind(),
            BlockKind::Assistant { items }
                if items == &vec![
                    AssistantItem::Text("hello".to_string()),
                    AssistantItem::Image(image.clone()),
                ]
        ));
    }

    #[test]
    fn repeated_image_deltas_replace_current_routed_image() {
        let mut state = AppState::default();
        let image1 = ImageContent::Data {
            mime_type: "image/png".to_string(),
            data: vec![1, 2, 3].into(),
        };
        let image2 = ImageContent::Data {
            mime_type: "image/png".to_string(),
            data: vec![4, 5, 6].into(),
        };

        state.apply_delta_at(0, OutputContentDelta::Image(image1));
        state.apply_delta_at(0, OutputContentDelta::Image(image2.clone()));

        assert_eq!(state.blocks().len(), 1);
        assert!(matches!(
            state.blocks()[0].kind(),
            BlockKind::Assistant { items }
                if items == &vec![AssistantItem::Image(image2.clone())]
        ));
    }

    #[test]
    fn tool_call_deltas_stream_into_one_tool_block() {
        let mut state = AppState::default();

        state.apply_delta(OutputContentDelta::ToolCall {
            id: Some("call_1".to_string()),
            name: Some("rg".to_string()),
            arguments: "{\"pattern\":\"foo\"".to_string(),
        });
        state.apply_delta(OutputContentDelta::ToolCall {
            id: None,
            name: None,
            arguments: ",\"path\":\"src\"}".to_string(),
        });

        assert!(matches!(
            state.blocks()[0].kind(),
            BlockKind::Tool(tool)
                if tool.call_id.as_deref() == Some("call_1")
                    && tool.name == "rg"
                    && tool.arguments == "{\"pattern\":\"foo\",\"path\":\"src\"}"
        ));
    }

    #[test]
    fn tool_call_metadata_can_stream_across_deltas() {
        let mut state = AppState::default();

        state.apply_delta(OutputContentDelta::ToolCall {
            id: Some("call_1".to_string()),
            name: None,
            arguments: String::new(),
        });
        state.apply_delta(OutputContentDelta::ToolCall {
            id: None,
            name: Some("rg".to_string()),
            arguments: String::new(),
        });
        state.apply_delta(OutputContentDelta::ToolCall {
            id: None,
            name: None,
            arguments: "{}".to_string(),
        });

        assert_eq!(state.blocks().len(), 1);
        assert!(matches!(
            state.blocks()[0].kind(),
            BlockKind::Tool(tool)
                if tool.call_id.as_deref() == Some("call_1")
                    && tool.name == "rg"
                    && tool.arguments == "{}"
        ));
    }

    #[test]
    fn new_tool_call_id_starts_new_tool_block() {
        let mut state = AppState::default();

        state.apply_delta(OutputContentDelta::ToolCall {
            id: Some("call_1".to_string()),
            name: Some("rg".to_string()),
            arguments: "{}".to_string(),
        });
        state.apply_delta(OutputContentDelta::ToolCall {
            id: Some("call_2".to_string()),
            name: Some("cat".to_string()),
            arguments: "{\"path\":\"README.md\"}".to_string(),
        });

        assert_eq!(state.blocks().len(), 2);
        assert!(matches!(
            state.blocks()[0].kind(),
            BlockKind::Tool(tool) if tool.call_id.as_deref() == Some("call_1")
        ));
        assert!(matches!(
            state.blocks()[1].kind(),
            BlockKind::Tool(tool)
                if tool.call_id.as_deref() == Some("call_2")
                    && tool.name == "cat"
                    && tool.arguments == "{\"path\":\"README.md\"}"
        ));
    }

    #[test]
    fn interleaved_tool_calls_merge_by_content_index() {
        let mut state = AppState::default();

        state.apply_delta_at(
            0,
            OutputContentDelta::ToolCall {
                id: Some("call_1".to_string()),
                name: None,
                arguments: String::new(),
            },
        );
        state.apply_delta_at(
            1,
            OutputContentDelta::ToolCall {
                id: Some("call_2".to_string()),
                name: None,
                arguments: String::new(),
            },
        );
        state.apply_delta_at(
            0,
            OutputContentDelta::ToolCall {
                id: None,
                name: Some("rg".to_string()),
                arguments: String::new(),
            },
        );
        state.apply_delta_at(
            1,
            OutputContentDelta::ToolCall {
                id: None,
                name: Some("cat".to_string()),
                arguments: String::new(),
            },
        );
        state.apply_delta_at(
            1,
            OutputContentDelta::ToolCall {
                id: None,
                name: None,
                arguments: "{\"path\":\"README.md\"}".to_string(),
            },
        );
        state.apply_delta_at(
            0,
            OutputContentDelta::ToolCall {
                id: None,
                name: None,
                arguments: "{\"pattern\":\"foo\"}".to_string(),
            },
        );

        assert_eq!(state.blocks().len(), 2);
        assert!(matches!(
            state.blocks()[0].kind(),
            BlockKind::Tool(tool)
                if tool.call_id.as_deref() == Some("call_1")
                    && tool.name == "rg"
                    && tool.arguments == "{\"pattern\":\"foo\"}"
        ));
        assert!(matches!(
            state.blocks()[1].kind(),
            BlockKind::Tool(tool)
                if tool.call_id.as_deref() == Some("call_2")
                    && tool.name == "cat"
                    && tool.arguments == "{\"path\":\"README.md\"}"
        ));
    }

    #[test]
    fn tool_result_merges_by_call_id() {
        let mut state = AppState::default();
        state.apply_delta(OutputContentDelta::ToolCall {
            id: Some("call_1".to_string()),
            name: Some("rg".to_string()),
            arguments: "{}".to_string(),
        });

        state.apply_tool_result(ToolResult {
            call_id: ToolCallId::new("call_1"),
            name: "rg".to_string(),
            status: ToolResultStatus::Success,
            content: vec![InputContent::Text("match".to_string())],
        });

        assert!(matches!(
            state.blocks()[0].kind(),
            BlockKind::Tool(tool)
                if tool.result.as_ref().is_some_and(|result| {
                    result.content == vec![InputContent::Text("match".to_string())]
                })
        ));
    }

    #[test]
    fn unmatched_tool_result_creates_tool_block() {
        let mut state = AppState::default();

        state.apply_tool_result(ToolResult {
            call_id: ToolCallId::new("missing"),
            name: "bash".to_string(),
            status: ToolResultStatus::Error,
            content: vec![InputContent::Text("failed".to_string())],
        });

        assert_eq!(state.blocks().len(), 1);
        assert!(matches!(
            state.blocks()[0].kind(),
            BlockKind::Tool(tool)
                if tool.call_id.as_deref() == Some("missing")
                    && tool.name == "bash"
                    && tool.result.is_some()
        ));
    }

    #[test]
    fn ctrl_o_toggles_all_foldable_blocks() {
        let mut state = AppState::default();
        state.apply_delta(OutputContentDelta::Thinking("long thinking".to_string()));
        state.apply_delta(OutputContentDelta::Text("answer".to_string()));

        assert!(state.blocks().iter().any(|block| block.is_foldable()));
        assert!(state.blocks().iter().any(|block| !block.is_foldable()));

        state.toggle_all_folds();
        assert!(
            state
                .blocks()
                .iter()
                .filter(|block| block.is_foldable())
                .all(|block| !block.is_expanded())
        );
        assert!(
            state
                .blocks()
                .iter()
                .filter(|block| !block.is_foldable())
                .all(|block| block.is_expanded())
        );

        state.toggle_all_folds();
        assert!(
            state
                .blocks()
                .iter()
                .filter(|block| block.is_foldable())
                .all(|block| block.is_expanded())
        );
        assert!(
            state
                .blocks()
                .iter()
                .filter(|block| !block.is_foldable())
                .all(|block| block.is_expanded())
        );
    }
}
