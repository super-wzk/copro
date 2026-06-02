use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, WidgetRef, Wrap},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldHint {
    template: String,
    style: Style,
}

impl FoldHint {
    pub fn new(template: impl Into<String>, style: Style) -> Self {
        Self {
            template: template.into(),
            style,
        }
    }

    pub fn text(&self, hidden_count: usize) -> String {
        self.template.replace("{count}", &hidden_count.to_string())
    }

    pub fn line(&self, hidden_count: usize) -> Line<'static> {
        Line::from(Span::styled(self.text(hidden_count), self.style))
    }
}

impl Default for FoldHint {
    fn default() -> Self {
        Self::new("{count} lines hidden", Style::default().fg(Color::DarkGray))
    }
}

fn fold_line_widgets(
    lines: Vec<Line<'static>>,
    max_visible: usize,
    hint: FoldHint,
) -> Vec<Line<'static>> {
    let hidden_count = lines.len().saturating_sub(max_visible);
    let mut visible = lines.into_iter().take(max_visible).collect::<Vec<_>>();

    if hidden_count > 0 {
        visible.push(hint.line(hidden_count));
    }

    visible
}

pub struct FoldedText {
    lines: Vec<Line<'static>>,
    max_visible: usize,
    hint: FoldHint,
    wrap: bool,
}

impl FoldedText {
    pub fn new(lines: Vec<Line<'static>>, max_visible: usize) -> Self {
        Self {
            lines,
            max_visible,
            hint: FoldHint::default(),
            wrap: true,
        }
    }

    pub fn hint(mut self, hint: FoldHint) -> Self {
        self.hint = hint;
        self
    }

    pub fn wrap(mut self, wrap: bool) -> Self {
        self.wrap = wrap;
        self
    }

    pub fn lines(&self) -> Vec<Line<'static>> {
        fold_line_widgets(self.lines.clone(), self.max_visible, self.hint.clone())
    }
}

impl WidgetRef for FoldedText {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        let paragraph = Paragraph::new(self.lines());
        if self.wrap {
            paragraph.wrap(Wrap { trim: true }).render(area, buf);
        } else {
            paragraph.render(area, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, widgets::FrameExt};

    #[test]
    fn folded_text_lines_keeps_head_and_counts_hidden_lines() {
        let lines = vec![
            Line::from("one"),
            Line::from("two"),
            Line::from("three"),
            Line::from("four"),
        ];
        let folded = FoldedText::new(lines, 2)
            .hint(FoldHint::new("hidden: {count}", Style::default()))
            .lines();

        assert_eq!(line_text(&folded[0]), "one");
        assert_eq!(line_text(&folded[1]), "two");
        assert_eq!(line_text(&folded[2]), "hidden: 2");
    }

    #[test]
    fn short_lines_do_not_report_hidden_lines() {
        let lines = vec![Line::from("one")];

        let folded = FoldedText::new(lines, 3).lines();

        assert_eq!(folded.len(), 1);
        assert_eq!(line_text(&folded[0]), "one");
    }

    #[test]
    fn folded_text_renders_hint_when_content_is_longer_than_limit() {
        let backend = TestBackend::new(16, 3);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let lines = vec![Line::from("one"), Line::from("two"), Line::from("three")];

        terminal
            .draw(|frame| {
                frame.render_widget_ref(
                    FoldedText::new(lines, 1)
                        .hint(FoldHint::new("{count} hidden", Style::default())),
                    frame.area(),
                )
            })
            .expect("render folded text");

        let rendered = (0..terminal.backend().buffer().area.height)
            .map(|y| {
                (0..terminal.backend().buffer().area.width)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<String>();
        assert!(rendered.contains("one"));
        assert!(rendered.contains("2 hidden"));
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }
}
