use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, WidgetRef},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusBarState {
    pub model: String,
    pub context: String,
    pub workspace: String,
}

impl Default for StatusBarState {
    fn default() -> Self {
        Self {
            model: "mock-model".to_string(),
            context: "mock ctx".to_string(),
            workspace: ".".to_string(),
        }
    }
}

pub struct StatusBar<'a> {
    status: &'a StatusBarState,
}

impl<'a> StatusBar<'a> {
    pub fn new(status: &'a StatusBarState) -> Self {
        Self { status }
    }
}

impl WidgetRef for StatusBar<'_> {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        let line = Line::from(vec![
            Span::styled(self.status.model.clone(), Style::default().fg(Color::Cyan)),
            Span::raw(" · "),
            Span::raw(self.status.context.clone()),
            Span::raw(" · "),
            Span::raw(self.status.workspace.clone()),
        ]);

        Paragraph::new(line).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, widgets::FrameExt};

    #[test]
    fn renders_runtime_context_without_activity() {
        let status = StatusBarState {
            model: "gpt-test".to_string(),
            context: "42%".to_string(),
            workspace: "copro".to_string(),
        };
        let backend = TestBackend::new(48, 1);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| frame.render_widget_ref(StatusBar::new(&status), frame.area()))
            .expect("render status");

        let line = (0..terminal.backend().buffer().area.width)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect::<String>();

        assert!(line.contains("gpt-test"));
        assert!(line.contains("42%"));
        assert!(line.contains("copro"));
        assert!(!line.contains("idle"));
        assert!(!line.contains("streaming"));
    }
}
