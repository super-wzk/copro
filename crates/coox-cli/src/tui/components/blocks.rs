use std::sync::LazyLock;

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
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Style as SyntectStyle, Theme, ThemeSet},
    parsing::{SyntaxReference, SyntaxSet},
};

use crate::tui::state::{AssistantItem, BlockKind, BlockState, ToolBlockState};

const MAX_COLLAPSED_LINES: usize = 14;
pub const BLOCK_PADDING: Padding = Padding::new(1, 1, 1, 1);
static CODE_HIGHLIGHTER: LazyLock<CodeHighlighter> = LazyLock::new(CodeHighlighter::new);

#[derive(Clone, Debug)]
pub enum BlockSegment {
    Line(BlockLine),
    Image(ImageContent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FenceNode {
    MarkdownText(String),
    Markdown {
        children: Vec<FenceNode>,
    },
    Code {
        language: Option<String>,
        text: String,
    },
}

struct CodeHighlighter {
    syntax_set: SyntaxSet,
    theme: Theme,
}

impl CodeHighlighter {
    fn new() -> Self {
        let syntax_set = SyntaxSet::load_defaults_nonewlines();
        let mut themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .remove("base16-ocean.dark")
            .expect("syntect default themes include base16-ocean.dark");

        Self { syntax_set, theme }
    }

    fn highlight_lines(
        &self,
        language: Option<&str>,
        raw_lines: &[String],
        fallback_style: Style,
    ) -> Option<Vec<Line<'static>>> {
        let syntax = self.syntax_for_language(language?)?;
        let mut highlighter = HighlightLines::new(syntax, &self.theme);
        let mut lines = Vec::with_capacity(raw_lines.len());

        for line in raw_lines {
            if line.is_empty() {
                lines.push(Line::from(Span::styled(String::new(), fallback_style)));
                continue;
            }

            let ranges = highlighter.highlight_line(line, &self.syntax_set).ok()?;
            let spans = ranges
                .into_iter()
                .map(|(style, text)| {
                    Span::styled(
                        text.to_string(),
                        syntect_style_to_ratatui(style, fallback_style),
                    )
                })
                .collect::<Vec<_>>();
            lines.push(Line::from(spans));
        }

        Some(lines)
    }

    fn syntax_for_language(&self, language: &str) -> Option<&SyntaxReference> {
        let language = language_token(language)?;
        std::iter::once(language)
            .chain(language_aliases(language).iter().copied())
            .find_map(|token| self.syntax_set.find_syntax_by_token(token))
    }
}

fn language_token(language: &str) -> Option<&str> {
    let token = language
        .trim()
        .split(|ch: char| ch.is_whitespace() || ch == ',' || ch == '{')
        .next()
        .unwrap_or_default()
        .trim_matches('.');
    (!token.is_empty()).then_some(token)
}

fn language_aliases(language: &str) -> &'static [&'static str] {
    match language.to_ascii_lowercase().as_str() {
        "shell" | "shell-script" | "sh" | "zsh" => &["bash"],
        "js" | "jsx" => &["javascript"],
        "ts" | "tsx" => &["typescript"],
        "jsonc" => &["json"],
        "yml" => &["yaml"],
        "md" => &["markdown"],
        _ => &[],
    }
}

fn plain_text_lines(raw_lines: &[String], style: Style) -> Vec<Line<'static>> {
    raw_lines
        .iter()
        .map(|line| Line::from(Span::styled(line.clone(), style)))
        .collect()
}

fn syntect_style_to_ratatui(style: SyntectStyle, fallback_style: Style) -> Style {
    let mut output = fallback_style.fg(Color::Rgb(
        style.foreground.r,
        style.foreground.g,
        style.foreground.b,
    ));

    if style.font_style.contains(FontStyle::BOLD) {
        output = output.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        output = output.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        output = output.add_modifier(Modifier::UNDERLINED);
    }

    output
}

fn parse_fence_tree(text: &str) -> Vec<FenceNode> {
    let lines = text.lines().collect::<Vec<_>>();
    let mut cursor = 0;
    parse_markdown_fence_nodes(&lines, &mut cursor, true)
}

fn parse_markdown_fence_nodes(lines: &[&str], cursor: &mut usize, is_root: bool) -> Vec<FenceNode> {
    let mut nodes = Vec::new();
    let mut text_lines = Vec::new();

    while let Some(line) = lines.get(*cursor) {
        if let Some(language) = fence_language(line) {
            if language.is_none() && !is_root {
                *cursor += 1;
                break;
            }

            flush_markdown_text(&mut nodes, &mut text_lines);
            *cursor += 1;

            if language.as_deref().is_some_and(is_markdown_language) {
                let children = parse_markdown_fence_nodes(lines, cursor, false);
                nodes.push(FenceNode::Markdown { children });
            } else {
                nodes.push(parse_code_fence_node(lines, cursor, language));
            }
            continue;
        }

        text_lines.push((*line).to_string());
        *cursor += 1;
    }

    flush_markdown_text(&mut nodes, &mut text_lines);
    nodes
}

fn parse_code_fence_node(
    lines: &[&str],
    cursor: &mut usize,
    language: Option<String>,
) -> FenceNode {
    let mut text_lines = Vec::new();

    while let Some(line) = lines.get(*cursor) {
        if fence_language(line).is_some_and(|language| language.is_none()) {
            *cursor += 1;
            break;
        }

        text_lines.push((*line).to_string());
        *cursor += 1;
    }

    FenceNode::Code {
        language,
        text: text_lines.join("\n"),
    }
}

fn flush_markdown_text(nodes: &mut Vec<FenceNode>, text_lines: &mut Vec<String>) {
    if text_lines.is_empty() {
        return;
    }

    nodes.push(FenceNode::MarkdownText(text_lines.join("\n")));
    text_lines.clear();
}

fn fence_language(line: &str) -> Option<Option<String>> {
    let rest = line.trim().strip_prefix("```")?;
    if rest.starts_with('`') {
        return None;
    }

    let language = rest.trim();
    Some((!language.is_empty()).then(|| language.to_string()))
}

fn is_markdown_language(language: &str) -> bool {
    language_token(language)
        .map(|token| matches!(token.to_ascii_lowercase().as_str(), "markdown" | "md"))
        .unwrap_or(false)
}

#[derive(Clone, Debug)]
pub struct BlockLine {
    line: Line<'static>,
    trim: bool,
}

impl BlockLine {
    pub fn new(line: Line<'static>) -> Self {
        Self { line, trim: true }
    }

    pub fn preserve_whitespace(line: Line<'static>) -> Self {
        Self { line, trim: false }
    }

    pub fn line(&self) -> &Line<'static> {
        &self.line
    }

    pub fn trim(&self) -> bool {
        self.trim
    }
}

impl BlockSegment {
    fn line(line: Line<'static>) -> Self {
        Self::Line(BlockLine::new(line))
    }

    fn preformatted_line(line: Line<'static>) -> Self {
        Self::Line(BlockLine::preserve_whitespace(line))
    }

    pub fn into_line(self) -> Line<'static> {
        match self {
            Self::Line(line) => line.line,
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
        BlockKind::Command { text, is_error } => {
            render_text_segments(text, command_text_style(*is_error))
        }
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
        BlockKind::Command { is_error, .. } => command_block_style(*is_error),
        BlockKind::Tool(tool) => tool_block_style(tool),
    }
}

fn render_assistant_items(items: &[AssistantItem]) -> Vec<BlockSegment> {
    let mut segments = Vec::new();

    for item in items {
        match item {
            AssistantItem::Text(text) => segments.extend(render_assistant_text_segments(text)),
            AssistantItem::Image(image) => segments.push(BlockSegment::Image(image.clone())),
        }
    }

    non_empty(segments)
}

fn render_assistant_text_segments(text: &str) -> Vec<BlockSegment> {
    render_fence_nodes(&parse_fence_tree(text))
}

fn render_fence_nodes(nodes: &[FenceNode]) -> Vec<BlockSegment> {
    let mut segments = Vec::new();

    for node in nodes {
        let node_segments = match node {
            FenceNode::MarkdownText(text) => render_markdown_preview_segments(text),
            FenceNode::Markdown { children } => render_fence_nodes(children),
            FenceNode::Code { language, text } => {
                render_highlighted_text_segments(text, language.as_deref(), Style::default())
            }
        };

        if node_segments.is_empty() {
            continue;
        }
        if !segments.is_empty() && !segments.last().is_some_and(block_segment_is_blank) {
            segments.push(BlockSegment::line(Line::from(String::new())));
        }
        segments.extend(node_segments);
    }

    segments
}

fn block_segment_is_blank(segment: &BlockSegment) -> bool {
    match segment {
        BlockSegment::Line(line) => line.line().spans.iter().all(|span| span.content.is_empty()),
        BlockSegment::Image(_) => false,
    }
}

fn render_markdown_preview_segments(text: &str) -> Vec<BlockSegment> {
    if text.is_empty() {
        return Vec::new();
    }

    MarkdownPreview::new(text)
        .styles(markdown_styles())
        .lines()
        .into_iter()
        .map(BlockSegment::line)
        .collect()
}

fn render_tool_segments(tool: &ToolBlockState) -> Vec<BlockSegment> {
    let mut segments = Vec::new();
    let name = if tool.name.is_empty() {
        "unknown"
    } else {
        tool.name.as_str()
    };

    segments.push(BlockSegment::line(Line::from(Span::styled(
        name.to_string(),
        tool_header_text_style(),
    ))));

    if !tool.arguments.is_empty() {
        segments.extend(render_text_segments(&tool.arguments, Style::default()));
    }

    if let Some(result) = &tool.result {
        if segments.len() > 1 {
            segments.push(BlockSegment::line(Line::from(Span::styled(
                "─".to_string(),
                tool_divider_text_style(),
            ))));
        }
        segments.extend(render_preformatted_input_content(
            &result.content,
            Style::default(),
        ));
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
        return vec![BlockSegment::line(Line::from(Span::styled(
            String::new(),
            style,
        )))];
    }

    text.lines()
        .map(|line| Line::from(Span::styled(line.to_string(), style)))
        .map(BlockSegment::line)
        .collect()
}

fn render_preformatted_input_content(content: &[InputContent], style: Style) -> Vec<BlockSegment> {
    let mut segments = Vec::new();

    for item in content {
        match item {
            InputContent::Text(text) => {
                segments.extend(render_preformatted_text_segments(text, style));
            }
            InputContent::Image(image) => segments.push(BlockSegment::Image(image.clone())),
        }
    }

    non_empty(segments)
}

fn render_preformatted_text_segments(text: &str, style: Style) -> Vec<BlockSegment> {
    if text.is_empty() {
        return vec![BlockSegment::preformatted_line(Line::from(Span::styled(
            String::new(),
            style,
        )))];
    }

    text.lines()
        .map(|line| Line::from(Span::styled(line.to_string(), style)))
        .map(BlockSegment::preformatted_line)
        .collect()
}

fn render_highlighted_text_segments(
    text: &str,
    language: Option<&str>,
    style: Style,
) -> Vec<BlockSegment> {
    if text.is_empty() {
        return vec![BlockSegment::preformatted_line(Line::from(Span::styled(
            String::new(),
            style,
        )))];
    }

    let raw_lines = text.lines().map(str::to_string).collect::<Vec<_>>();
    let lines = CODE_HIGHLIGHTER
        .highlight_lines(language, &raw_lines, style)
        .unwrap_or_else(|| plain_text_lines(&raw_lines, style));

    lines
        .into_iter()
        .map(BlockSegment::preformatted_line)
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

    let preserve_whitespace = segments.iter().any(|segment| match segment {
        BlockSegment::Line(line) => !line.trim(),
        BlockSegment::Image(_) => false,
    });
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

    visible
        .into_iter()
        .map(|line| {
            if preserve_whitespace {
                BlockSegment::preformatted_line(line)
            } else {
                BlockSegment::line(line)
            }
        })
        .collect()
}

fn non_empty(segments: Vec<BlockSegment>) -> Vec<BlockSegment> {
    if segments.is_empty() {
        vec![BlockSegment::line(Line::from(String::new()))]
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

fn command_block_style(is_error: bool) -> Style {
    if is_error {
        Style::default().fg(Color::White).bg(Color::Rgb(52, 34, 34))
    } else {
        Style::default().fg(Color::Gray).bg(Color::Rgb(28, 32, 36))
    }
}

fn command_text_style(is_error: bool) -> Style {
    if is_error {
        Style::default().fg(Color::LightRed)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn tool_divider_text_style() -> Style {
    Style::default().fg(Color::Gray)
}

fn markdown_styles() -> MarkdownStyles {
    MarkdownStyles {
        text: Style::default(),
        heading: heading_style(),
        code: code_style(),
        quote_marker: quote_marker_style(),
        quote: quote_style(),
        bullet_marker: bullet_marker_style(),
    }
}

fn heading_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn code_style() -> Style {
    Style::default().fg(Color::Cyan)
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
    fn assistant_fence_tree_previews_markdown_and_highlights_code() {
        let mut state = AppState::default();
        state.apply_delta(OutputContentDelta::Text(
            "```markdown\n# Title\n\n- item\n\n```rust\nfn main() {}\n```\n```".to_string(),
        ));

        let lines = render_lines(&state.blocks()[0]);

        assert_eq!(
            lines.iter().map(line_text).collect::<Vec<_>>(),
            vec!["Title", "", "• item", "", "fn main() {}"]
        );
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .any(|span| span.content.contains("fn") && span.style.fg.is_some())
        );
    }

    #[test]
    fn assistant_markdown_inline_code_is_styled() {
        let mut state = AppState::default();
        state.apply_delta(OutputContentDelta::Text("Use `copro-agent`.".to_string()));

        let lines = render_lines(&state.blocks()[0]);
        let code_span = lines[0]
            .spans
            .iter()
            .find(|span| span.content == "copro-agent")
            .expect("inline code span");

        assert_eq!(line_text(&lines[0]), "Use copro-agent.");
        assert_eq!(code_span.style.fg, Some(Color::Cyan));
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

    #[test]
    fn read_tool_result_keeps_line_number_separator_visible() {
        let mut state = AppState::default();
        state.apply_delta(OutputContentDelta::ToolCall {
            id: Some("call_1".to_string()),
            name: Some("read".to_string()),
            arguments: "{\"path\":\"Cargo.toml\"}".to_string(),
        });
        state.apply_tool_result(ToolResult {
            call_id: ToolCallId::new("call_1"),
            name: "read".to_string(),
            status: ToolResultStatus::Success,
            content: vec![InputContent::Text(
                "Cargo.toml\n1: [workspace]\n2: members = [".to_string(),
            )],
        });

        let text = render_text(&state.blocks()[0]);

        assert_eq!(
            text,
            vec![
                "read",
                "{\"path\":\"Cargo.toml\"}",
                "─",
                "Cargo.toml",
                "1: [workspace]",
                "2: members = [",
            ]
        );
        assert!(!text.iter().any(|line| line.contains('\t')));
    }

    fn render_text(block: &BlockState) -> Vec<String> {
        render_lines(block)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
    }

    fn render_lines(block: &BlockState) -> Vec<Line<'static>> {
        render_block_lines(block)
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }
}
