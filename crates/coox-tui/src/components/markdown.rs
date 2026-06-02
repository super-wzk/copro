use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, WidgetRef, Wrap},
};

use pulldown_cmark::{Event, Parser, Tag, TagEnd};

#[derive(Debug, Clone, PartialEq, Eq)]
enum MarkdownLine {
    Text(String),
    Code(String),
    Quote(String),
    Bullet(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Text,
    Quote,
    Bullet,
}

impl LineKind {
    fn line(self, text: String) -> MarkdownLine {
        match self {
            Self::Text => MarkdownLine::Text(text),
            Self::Quote => MarkdownLine::Quote(text),
            Self::Bullet => MarkdownLine::Bullet(text),
        }
    }
}

fn preview_markdown(markdown: &str) -> Vec<MarkdownLine> {
    fn active_kind(quote_depth: usize, item_depth: usize) -> LineKind {
        if item_depth > 0 {
            LineKind::Bullet
        } else if quote_depth > 0 {
            LineKind::Quote
        } else {
            LineKind::Text
        }
    }

    fn append_text(
        current: &mut String,
        current_kind: &mut Option<LineKind>,
        quote_depth: usize,
        item_depth: usize,
        text: &str,
    ) {
        current_kind.get_or_insert_with(|| active_kind(quote_depth, item_depth));
        current.push_str(text);
    }

    fn flush_current(
        lines: &mut Vec<MarkdownLine>,
        current: &mut String,
        current_kind: &mut Option<LineKind>,
    ) {
        if let Some(kind) = current_kind.take() {
            lines.push(kind.line(std::mem::take(current)));
        }
    }

    let mut lines = Vec::new();
    let mut current = String::new();
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
                flush_current(&mut lines, &mut current, &mut current_kind);
                quote_depth = quote_depth.saturating_sub(1);
            }
            Event::Start(Tag::Item) => {
                item_depth += 1;
            }
            Event::End(TagEnd::Item) => {
                flush_current(&mut lines, &mut current, &mut current_kind);
                item_depth = item_depth.saturating_sub(1);
            }
            Event::Start(Tag::Paragraph) => {
                current_kind.get_or_insert_with(|| active_kind(quote_depth, item_depth));
            }
            Event::End(TagEnd::Paragraph) => {
                flush_current(&mut lines, &mut current, &mut current_kind);
            }
            Event::Start(Tag::Heading { .. }) => {
                current_kind.get_or_insert_with(|| active_kind(quote_depth, item_depth));
            }
            Event::End(TagEnd::Heading(_)) => {
                flush_current(&mut lines, &mut current, &mut current_kind);
            }
            Event::Start(Tag::CodeBlock(_)) => {
                flush_current(&mut lines, &mut current, &mut current_kind);
                in_code_block = true;
                code_block.clear();
            }
            Event::End(TagEnd::CodeBlock) => {
                for line in code_block.lines() {
                    lines.push(MarkdownLine::Code(line.to_string()));
                }
                in_code_block = false;
                code_block.clear();
            }
            Event::Text(text) => {
                if in_code_block {
                    code_block.push_str(&text);
                } else {
                    append_text(
                        &mut current,
                        &mut current_kind,
                        quote_depth,
                        item_depth,
                        &text,
                    );
                }
            }
            Event::Code(text) => {
                append_text(
                    &mut current,
                    &mut current_kind,
                    quote_depth,
                    item_depth,
                    &text,
                );
            }
            Event::SoftBreak | Event::HardBreak => {
                if in_code_block {
                    code_block.push('\n');
                } else {
                    flush_current(&mut lines, &mut current, &mut current_kind);
                }
            }
            _ => {}
        }
    }

    flush_current(&mut lines, &mut current, &mut current_kind);

    if lines.is_empty() && !markdown.is_empty() {
        lines.push(MarkdownLine::Text(markdown.to_string()));
    }

    lines
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkdownStyles {
    pub text: Style,
    pub code: Style,
    pub quote_marker: Style,
    pub quote: Style,
    pub bullet_marker: Style,
}

impl Default for MarkdownStyles {
    fn default() -> Self {
        Self {
            text: Style::default(),
            code: Style::default().fg(Color::Cyan),
            quote_marker: Style::default().fg(Color::DarkGray),
            quote: Style::default().fg(Color::Gray),
            bullet_marker: Style::default().fg(Color::Cyan),
        }
    }
}

fn markdown_lines(markdown: &str, styles: MarkdownStyles) -> Vec<Line<'static>> {
    preview_markdown(markdown)
        .into_iter()
        .map(|line| markdown_line(line, styles))
        .collect()
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

fn markdown_line(line: MarkdownLine, styles: MarkdownStyles) -> Line<'static> {
    match line {
        MarkdownLine::Text(text) => Line::from(Span::styled(text, styles.text)),
        MarkdownLine::Code(text) => Line::from(Span::styled(text, styles.code)),
        MarkdownLine::Quote(text) => Line::from(vec![
            Span::styled("│ ".to_string(), styles.quote_marker),
            Span::styled(text, styles.quote),
        ]),
        MarkdownLine::Bullet(text) => Line::from(vec![
            Span::styled("• ".to_string(), styles.bullet_marker),
            Span::raw(text),
        ]),
    }
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

        assert_eq!(line_text(&lines[0]), "│ quoted");
        assert_eq!(line_text(&lines[1]), "• item");
        assert_eq!(line_text(&lines[2]), "fn main() {}");
    }

    #[test]
    fn preview_markdown_keeps_heading_and_paragraph_separate() {
        let lines = preview_markdown("# Title\n\nBody");

        assert_eq!(
            lines,
            vec![
                MarkdownLine::Text("Title".to_string()),
                MarkdownLine::Text("Body".to_string()),
            ]
        );
    }

    #[test]
    fn preview_markdown_preserves_inline_emphasis_link_and_code_text() {
        let lines = preview_markdown("Hello *strong* [link](https://example.com) `code`");

        assert_eq!(
            lines,
            vec![MarkdownLine::Text("Hello strong link code".to_string())]
        );
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
