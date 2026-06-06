use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, WidgetRef, Wrap},
};

use pulldown_cmark::{Event, Parser, Tag, TagEnd};

#[derive(Debug, Clone, PartialEq, Eq)]
enum MarkdownBlock {
    Paragraph { lines: Vec<Vec<MarkdownSpan>> },
    Heading { lines: Vec<Vec<MarkdownSpan>> },
    Code { lines: Vec<String> },
    Quote { lines: Vec<Vec<MarkdownSpan>> },
    Bullet { lines: Vec<Vec<MarkdownSpan>> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MarkdownSpan {
    Text(String),
    Code(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkdownBlockKind {
    Paragraph,
    Heading,
    Quote,
    Bullet,
}

impl MarkdownBlockKind {
    fn block(self, lines: Vec<Vec<MarkdownSpan>>) -> MarkdownBlock {
        match self {
            Self::Paragraph => MarkdownBlock::Paragraph { lines },
            Self::Heading => MarkdownBlock::Heading { lines },
            Self::Quote => MarkdownBlock::Quote { lines },
            Self::Bullet => MarkdownBlock::Bullet { lines },
        }
    }
}

fn parse_markdown_blocks(markdown: &str) -> Vec<MarkdownBlock> {
    fn active_kind(quote_depth: usize, item_depth: usize) -> MarkdownBlockKind {
        if item_depth > 0 {
            MarkdownBlockKind::Bullet
        } else if quote_depth > 0 {
            MarkdownBlockKind::Quote
        } else {
            MarkdownBlockKind::Paragraph
        }
    }

    fn current_line(current_lines: &mut Vec<Vec<MarkdownSpan>>) -> &mut Vec<MarkdownSpan> {
        if current_lines.is_empty() {
            current_lines.push(Vec::new());
        }
        current_lines
            .last_mut()
            .expect("current line was just inserted")
    }

    fn append_span(current_lines: &mut Vec<Vec<MarkdownSpan>>, span: MarkdownSpan) {
        let line = current_line(current_lines);
        match (line.last_mut(), span) {
            (Some(MarkdownSpan::Text(existing)), MarkdownSpan::Text(text)) => {
                existing.push_str(&text);
            }
            (Some(MarkdownSpan::Code(existing)), MarkdownSpan::Code(text)) => {
                existing.push_str(&text);
            }
            (_, span) => line.push(span),
        }
    }

    fn start_block(
        current_kind: &mut Option<MarkdownBlockKind>,
        current_lines: &mut Vec<Vec<MarkdownSpan>>,
        kind: MarkdownBlockKind,
    ) {
        current_kind.get_or_insert(kind);
        current_line(current_lines);
    }

    fn append_text(
        current_lines: &mut Vec<Vec<MarkdownSpan>>,
        current_kind: &mut Option<MarkdownBlockKind>,
        quote_depth: usize,
        item_depth: usize,
        text: &str,
    ) {
        current_kind.get_or_insert_with(|| active_kind(quote_depth, item_depth));
        append_span(current_lines, MarkdownSpan::Text(text.to_string()));
    }

    fn append_code(
        current_lines: &mut Vec<Vec<MarkdownSpan>>,
        current_kind: &mut Option<MarkdownBlockKind>,
        quote_depth: usize,
        item_depth: usize,
        text: &str,
    ) {
        current_kind.get_or_insert_with(|| active_kind(quote_depth, item_depth));
        append_span(current_lines, MarkdownSpan::Code(text.to_string()));
    }

    fn flush_current(
        blocks: &mut Vec<MarkdownBlock>,
        current_lines: &mut Vec<Vec<MarkdownSpan>>,
        current_kind: &mut Option<MarkdownBlockKind>,
    ) {
        if let Some(kind) = current_kind.take() {
            blocks.push(kind.block(std::mem::take(current_lines)));
        }
    }

    fn code_lines(code_block: &str) -> Vec<String> {
        code_block.lines().map(str::to_string).collect()
    }

    let mut blocks = Vec::new();
    let mut current_lines = Vec::new();
    let mut current_kind = None;
    let mut quote_depth = 0usize;
    let mut item_depth = 0usize;
    let mut in_code_block = false;
    let mut code_block = String::new();

    for event in Parser::new(markdown) {
        match event {
            Event::Start(Tag::BlockQuote(_)) => {
                quote_depth += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                flush_current(&mut blocks, &mut current_lines, &mut current_kind);
                quote_depth = quote_depth.saturating_sub(1);
            }
            Event::Start(Tag::Item) => {
                item_depth += 1;
            }
            Event::End(TagEnd::Item) => {
                flush_current(&mut blocks, &mut current_lines, &mut current_kind);
                item_depth = item_depth.saturating_sub(1);
            }
            Event::Start(Tag::Paragraph) => {
                start_block(
                    &mut current_kind,
                    &mut current_lines,
                    active_kind(quote_depth, item_depth),
                );
            }
            Event::End(TagEnd::Paragraph) => {
                flush_current(&mut blocks, &mut current_lines, &mut current_kind);
            }
            Event::Start(Tag::Heading { .. }) => {
                start_block(
                    &mut current_kind,
                    &mut current_lines,
                    MarkdownBlockKind::Heading,
                );
            }
            Event::End(TagEnd::Heading(_)) => {
                flush_current(&mut blocks, &mut current_lines, &mut current_kind);
            }
            Event::Start(Tag::CodeBlock(_)) => {
                flush_current(&mut blocks, &mut current_lines, &mut current_kind);
                in_code_block = true;
                code_block.clear();
            }
            Event::End(TagEnd::CodeBlock) => {
                blocks.push(MarkdownBlock::Code {
                    lines: code_lines(&code_block),
                });
                in_code_block = false;
                code_block.clear();
            }
            Event::Text(text) => {
                if in_code_block {
                    code_block.push_str(&text);
                } else {
                    append_text(
                        &mut current_lines,
                        &mut current_kind,
                        quote_depth,
                        item_depth,
                        &text,
                    );
                }
            }
            Event::Code(text) => {
                append_code(
                    &mut current_lines,
                    &mut current_kind,
                    quote_depth,
                    item_depth,
                    &text,
                );
            }
            Event::SoftBreak | Event::HardBreak => {
                if in_code_block {
                    code_block.push('\n');
                } else if current_kind.is_some() {
                    current_lines.push(Vec::new());
                }
            }
            _ => {}
        }
    }

    flush_current(&mut blocks, &mut current_lines, &mut current_kind);

    if blocks.is_empty() && !markdown.is_empty() {
        blocks.push(MarkdownBlock::Paragraph {
            lines: vec![vec![MarkdownSpan::Text(markdown.to_string())]],
        });
    }

    blocks
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkdownStyles {
    pub text: Style,
    pub heading: Style,
    pub code: Style,
    pub quote_marker: Style,
    pub quote: Style,
    pub bullet_marker: Style,
}

impl Default for MarkdownStyles {
    fn default() -> Self {
        Self {
            text: Style::default(),
            heading: Style::default().add_modifier(Modifier::BOLD),
            code: Style::default().fg(Color::Cyan),
            quote_marker: Style::default().fg(Color::DarkGray),
            quote: Style::default().fg(Color::Gray),
            bullet_marker: Style::default().fg(Color::Cyan),
        }
    }
}

fn markdown_lines(markdown: &str, styles: MarkdownStyles) -> Vec<Line<'static>> {
    let blocks = parse_markdown_blocks(markdown);
    let mut lines = Vec::new();

    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            let previous = &blocks[index - 1];
            for _ in 0..markdown_block_gap(previous, block) {
                lines.push(Line::from(String::new()));
            }
        }
        lines.extend(markdown_block_lines(block, styles));
    }

    lines
}

pub struct MarkdownPreview<'a> {
    markdown: &'a str,
    styles: MarkdownStyles,
    wrap: bool,
}

impl<'a> MarkdownPreview<'a> {
    pub fn new(markdown: &'a str) -> Self {
        Self {
            markdown,
            styles: MarkdownStyles::default(),
            wrap: true,
        }
    }

    pub fn styles(mut self, styles: MarkdownStyles) -> Self {
        self.styles = styles;
        self
    }

    pub fn wrap(mut self, wrap: bool) -> Self {
        self.wrap = wrap;
        self
    }

    pub fn lines(&self) -> Vec<Line<'static>> {
        markdown_lines(self.markdown, self.styles)
    }
}

impl WidgetRef for MarkdownPreview<'_> {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        let paragraph = Paragraph::new(self.lines());
        if self.wrap {
            paragraph.wrap(Wrap { trim: true }).render(area, buf);
        } else {
            paragraph.render(area, buf);
        }
    }
}

fn markdown_block_gap(previous: &MarkdownBlock, next: &MarkdownBlock) -> usize {
    match (previous, next) {
        (MarkdownBlock::Bullet { .. }, MarkdownBlock::Bullet { .. })
        | (MarkdownBlock::Quote { .. }, MarkdownBlock::Quote { .. }) => 0,
        _ => 1,
    }
}

fn markdown_block_lines(block: &MarkdownBlock, styles: MarkdownStyles) -> Vec<Line<'static>> {
    match block {
        MarkdownBlock::Paragraph { lines } => span_lines(lines, styles.text, styles.code),
        MarkdownBlock::Heading { lines } => span_lines(lines, styles.heading, styles.code),
        MarkdownBlock::Code { lines } => lines
            .iter()
            .map(|line| Line::from(Span::styled(line.clone(), styles.code)))
            .collect(),
        MarkdownBlock::Quote { lines } => {
            marker_lines(lines, "│ ", styles.quote_marker, styles.quote, styles.code)
        }
        MarkdownBlock::Bullet { lines } => {
            marker_lines(lines, "• ", styles.bullet_marker, styles.text, styles.code)
        }
    }
}

fn span_lines(
    lines: &[Vec<MarkdownSpan>],
    text_style: Style,
    code_style: Style,
) -> Vec<Line<'static>> {
    lines
        .iter()
        .map(|spans| Line::from(markdown_spans(spans, text_style, code_style)))
        .collect()
}

fn marker_lines(
    lines: &[Vec<MarkdownSpan>],
    marker: &str,
    marker_style: Style,
    text_style: Style,
    code_style: Style,
) -> Vec<Line<'static>> {
    lines
        .iter()
        .map(|spans| {
            Line::from(
                [
                    vec![Span::styled(marker.to_string(), marker_style)],
                    markdown_spans(spans, text_style, code_style),
                ]
                .concat(),
            )
        })
        .collect()
}

fn markdown_spans(
    spans: &[MarkdownSpan],
    text_style: Style,
    code_style: Style,
) -> Vec<Span<'static>> {
    spans
        .iter()
        .map(|span| match span {
            MarkdownSpan::Text(text) => Span::styled(text.clone(), text_style),
            MarkdownSpan::Code(text) => Span::styled(text.clone(), code_style),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, widgets::FrameExt};

    #[test]
    fn markdown_preview_lines_styles_structural_markdown() {
        let lines = MarkdownPreview::new("> quoted\n\n- item\n\n```rust\nfn main() {}\n```")
            .styles(MarkdownStyles::default())
            .lines();

        assert_eq!(
            lines.iter().map(line_text).collect::<Vec<_>>(),
            vec!["│ quoted", "", "• item", "", "fn main() {}"]
        );
    }

    #[test]
    fn parse_markdown_blocks_keeps_heading_and_paragraph_separate() {
        let blocks = parse_markdown_blocks("# Title\n\nBody");

        assert_eq!(
            blocks,
            vec![
                MarkdownBlock::Heading {
                    lines: vec![vec![MarkdownSpan::Text("Title".to_string())]],
                },
                MarkdownBlock::Paragraph {
                    lines: vec![vec![MarkdownSpan::Text("Body".to_string())]],
                },
            ]
        );
    }

    #[test]
    fn parse_markdown_blocks_preserves_inline_emphasis_link_and_code_text() {
        let blocks = parse_markdown_blocks("Hello *strong* [link](https://example.com) `code`");

        assert_eq!(
            blocks,
            vec![MarkdownBlock::Paragraph {
                lines: vec![vec![
                    MarkdownSpan::Text("Hello strong link ".to_string()),
                    MarkdownSpan::Code("code".to_string()),
                ]],
            }]
        );
    }

    #[test]
    fn markdown_preview_inserts_gaps_between_body_and_next_heading() {
        let lines = MarkdownPreview::new("# Copro\n\nCopro is a Rust workspace.\n\n## Crates")
            .styles(MarkdownStyles::default())
            .lines();

        assert_eq!(
            lines.iter().map(line_text).collect::<Vec<_>>(),
            vec!["Copro", "", "Copro is a Rust workspace.", "", "Crates"]
        );
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            lines[4].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn markdown_preview_styles_heading_and_inline_code() {
        let lines = MarkdownPreview::new("# Title\n\nUse `copro-agent`.")
            .styles(MarkdownStyles::default())
            .lines();

        assert_eq!(
            lines.iter().map(line_text).collect::<Vec<_>>(),
            vec!["Title", "", "Use copro-agent."]
        );
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        let code_span = lines[2]
            .spans
            .iter()
            .find(|span| span.content == "copro-agent")
            .expect("inline code span");
        assert_eq!(code_span.style.fg, Some(Color::Cyan));
    }

    #[test]
    fn markdown_preview_renders_parsed_lines() {
        let backend = TestBackend::new(16, 3);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| frame.render_widget_ref(MarkdownPreview::new("- item"), frame.area()))
            .expect("render markdown preview");

        let line = (0..terminal.backend().buffer().area.width)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect::<String>();
        assert!(line.contains("• item"));
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }
}
