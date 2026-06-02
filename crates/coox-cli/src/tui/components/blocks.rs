use coox_tui::components::{
    fold::{FoldHint, FoldedText},
    image::ImageSource,
    markdown::{MarkdownPreview, MarkdownStyles},
};
use copro_api::message::{ImageContent, InputContent, ToolResultStatus};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Padding,
};

use crate::tui::state::{AssistantItem, BlockKind, BlockState, ToolBlockState};

const MAX_COLLAPSED_LINES: usize = 14;
pub const BLOCK_PADDING: Padding = Padding::new(1, 1, 1, 1);

#[derive(Clone, Debug)]
pub enum BlockSegment {
    Line(Line<'static>),
    Image(ImageContent),
}

impl BlockSegment {
    pub fn into_line(self) -> Line<'static> {
        match self {
            Self::Line(line) => line,
            Self::Image(image) => image_placeholder_line(&image),
        }
    }
}

pub fn render_block_lines(block: &BlockState) -> Vec<Line<'static>> {
    render_block_segments(block)
        .into_iter()
        .map(BlockSegment::into_line)
        .collect()
}

pub fn render_block_segments(block: &BlockState) -> Vec<BlockSegment> {
    let raw = match block.kind() {
        BlockKind::User { content } => render_input_content(content, Style::default()),
        BlockKind::Thinking { text } => render_text_segments(text, thinking_style()),
        BlockKind::Assistant { items } => render_assistant_items(items),
        BlockKind::Error { text } => render_text_segments(text, error_text_style()),
        BlockKind::Tool(tool) => render_tool_segments(tool),
    };
    apply_block_fold(block, raw)
}

pub fn block_container_style(block: &BlockState) -> Style {
    match block.kind() {
        BlockKind::User { .. } => user_style(),
        BlockKind::Thinking { .. } => thinking_style(),
        BlockKind::Assistant { .. } => Style::default(),
        BlockKind::Error { .. } => error_block_style(),
        BlockKind::Tool(tool) => tool_block_style(tool),
    }
}

fn render_assistant_items(items: &[AssistantItem]) -> Vec<BlockSegment> {
    let mut segments = Vec::new();

    for item in items {
        match item {
            AssistantItem::Text(text) => {
                segments.extend(
                    MarkdownPreview::new(text)
                        .styles(markdown_styles())
                        .lines()
                        .into_iter()
                        .map(BlockSegment::Line),
                );
            }
            AssistantItem::Image(image) => segments.push(BlockSegment::Image(image.clone())),
        }
    }

    non_empty(segments)
}

fn render_tool_segments(tool: &ToolBlockState) -> Vec<BlockSegment> {
    let mut segments = Vec::new();
    let name = if tool.name.is_empty() {
        "unknown"
    } else {
        tool.name.as_str()
    };

    segments.push(BlockSegment::Line(Line::from(Span::styled(
        name.to_string(),
        tool_header_text_style(),
    ))));

    if !tool.arguments.is_empty() {
        segments.extend(render_text_segments(&tool.arguments, Style::default()));
    }

    if let Some(result) = &tool.result {
        if segments.len() > 1 {
            segments.push(BlockSegment::Line(Line::from(Span::styled(
                "─".to_string(),
                tool_divider_text_style(),
            ))));
        }
        segments.extend(render_input_content(&result.content, Style::default()));
    }

    non_empty(segments)
}

fn render_input_content(content: &[InputContent], style: Style) -> Vec<BlockSegment> {
    let mut segments = Vec::new();

    for item in content {
        match item {
            InputContent::Text(text) => segments.extend(render_text_segments(text, style)),
            InputContent::Image(image) => segments.push(BlockSegment::Image(image.clone())),
        }
    }

    non_empty(segments)
}

fn render_text_segments(text: &str, style: Style) -> Vec<BlockSegment> {
    if text.is_empty() {
        return vec![BlockSegment::Line(Line::from(Span::styled(
            String::new(),
            style,
        )))];
    }

    text.lines()
        .map(|line| Line::from(Span::styled(line.to_string(), style)))
        .map(BlockSegment::Line)
        .collect()
}

fn image_placeholder_line(image: &ImageContent) -> Line<'static> {
    Line::from(Span::styled(image_placeholder_text(image), image_style()))
}

pub(crate) fn image_placeholder_text(image: &ImageContent) -> String {
    image_source(image).placeholder_text()
}

pub(crate) fn image_source(image: &ImageContent) -> ImageSource {
    match image {
        ImageContent::Url { url } => ImageSource::url(url.clone()),
        ImageContent::Data { mime_type, data } => {
            ImageSource::data(mime_type.clone(), data.clone())
        }
    }
}

fn apply_block_fold(block: &BlockState, segments: Vec<BlockSegment>) -> Vec<BlockSegment> {
    if !block.is_foldable() || block.is_expanded() {
        return segments;
    }

    let lines = segments
        .into_iter()
        .map(BlockSegment::into_line)
        .collect::<Vec<_>>();
    let visible = FoldedText::new(lines, MAX_COLLAPSED_LINES)
        .hint(FoldHint::new(
            "Ctrl+O expand all · {count} lines hidden",
            fold_hint_style(),
        ))
        .lines();

    visible.into_iter().map(BlockSegment::Line).collect()
}

fn non_empty(segments: Vec<BlockSegment>) -> Vec<BlockSegment> {
    if segments.is_empty() {
        vec![BlockSegment::Line(Line::from(String::new()))]
    } else {
        segments
    }
}

pub fn user_style() -> Style {
    Style::default().fg(Color::Gray).bg(Color::Rgb(70, 70, 70))
}

fn thinking_style() -> Style {
    Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC)
}

fn tool_block_style(tool: &ToolBlockState) -> Style {
    match tool.result.as_ref().map(|result| &result.status) {
        Some(ToolResultStatus::Success) => tool_success_style(),
        Some(ToolResultStatus::Error) => tool_error_style(),
        None => tool_pending_style(),
    }
}

fn tool_header_text_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

fn tool_pending_style() -> Style {
    Style::default().fg(Color::Gray).bg(Color::Rgb(24, 45, 48))
}

fn tool_success_style() -> Style {
    Style::default().fg(Color::White).bg(Color::Rgb(45, 55, 45))
}

fn tool_error_style() -> Style {
    Style::default().fg(Color::White).bg(Color::Rgb(60, 45, 45))
}

fn error_block_style() -> Style {
    Style::default().fg(Color::White).bg(Color::Rgb(70, 35, 35))
}

fn error_text_style() -> Style {
    Style::default().fg(Color::LightRed)
}

fn tool_divider_text_style() -> Style {
    Style::default().fg(Color::Gray)
}

fn markdown_styles() -> MarkdownStyles {
    MarkdownStyles {
        text: Style::default(),
        code: code_style(),
        quote_marker: quote_marker_style(),
        quote: quote_style(),
        bullet_marker: bullet_marker_style(),
    }
}

fn code_style() -> Style {
    Style::default().fg(Color::Cyan).bg(Color::Black)
}

fn quote_marker_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn quote_style() -> Style {
    Style::default().fg(Color::Gray)
}

fn bullet_marker_style() -> Style {
    Style::default().fg(Color::Cyan)
}

fn image_style() -> Style {
    Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC)
}

fn fold_hint_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::state::AppState;
    use copro_api::message::{InputMessage, ToolCallId, ToolResult};
    use copro_api::stream::OutputContentDelta;

    #[test]
    fn user_block_has_no_user_label() {
        let mut state = AppState::default();
        state.push_input(InputMessage::User(vec![InputContent::Text(
            "fix unicode cursor".to_string(),
        )]));

        let text = render_text(&state.blocks()[0]);

        assert_eq!(text, vec!["fix unicode cursor"]);
        assert!(!text.iter().any(|line| line.contains("user")));
    }

    #[test]
    fn assistant_block_has_no_label_and_renders_markdown_bullet() {
        let mut state = AppState::default();
        state.apply_delta(OutputContentDelta::Text("- first".to_string()));

        let text = render_text(&state.blocks()[0]);

        assert_eq!(text, vec!["• first"]);
        assert!(!text.iter().any(|line| line.contains("assistant")));
    }

    #[test]
    fn error_block_renders_text_without_stack_trace_label() {
        let mut state = AppState::default();
        state.push_error("client error: missing api key");

        let text = render_text(&state.blocks()[0]);

        assert_eq!(text, vec!["client error: missing api key"]);
        assert!(!text.iter().any(|line| line.contains("stack")));
    }

    #[test]
    fn tool_header_is_tool_name_only_without_call_id() {
        let mut state = AppState::default();
        state.apply_delta(OutputContentDelta::ToolCall {
            id: Some("call_123".to_string()),
            name: Some("rg".to_string()),
            arguments: "{}".to_string(),
        });

        let text = render_text(&state.blocks()[0]);

        assert_eq!(text[0], "rg");
        assert!(!text[0].contains("call_123"));
        assert!(!text[0].contains("tool call"));
        assert!(!text[0].contains("tool result"));
    }

    #[test]
    fn folded_block_includes_local_ctrl_o_hint() {
        let mut state = AppState::default();
        let text = (0..16)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        state.apply_delta(OutputContentDelta::Thinking(text));
        state.toggle_all_folds();

        let text = render_text(&state.blocks()[0]);

        assert_eq!(text.len(), 15);
        assert_eq!(text[0], "line 0");
        assert_eq!(text[13], "line 13");
        assert_eq!(text[14], "Ctrl+O expand all · 2 lines hidden");
    }

    #[test]
    fn tool_result_renders_in_same_block_after_arguments() {
        let mut state = AppState::default();
        state.apply_delta(OutputContentDelta::ToolCall {
            id: Some("call_1".to_string()),
            name: Some("bash".to_string()),
            arguments: "{\"cmd\":\"pwd\"}".to_string(),
        });
        state.apply_tool_result(ToolResult {
            call_id: ToolCallId::new("call_1"),
            name: "bash".to_string(),
            status: ToolResultStatus::Success,
            content: vec![InputContent::Text("/tmp/project".to_string())],
        });

        let text = render_text(&state.blocks()[0]);

        assert_eq!(text, vec!["bash", "{\"cmd\":\"pwd\"}", "─", "/tmp/project"]);
    }

    fn render_text(block: &BlockState) -> Vec<String> {
        render_block_lines(block)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }
}
