use copro_api::message::{InputContent, InputMessage};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

const MAX_PENDING_INPUT_ROWS: usize = 3;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PendingInputs {
    items: Vec<PendingInput>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingInput {
    delivery: PendingDelivery,
    content: Vec<InputContent>,
}

impl PendingInput {
    #[cfg(test)]
    pub(crate) fn new(delivery: PendingDelivery, content: Vec<InputContent>) -> Self {
        Self { delivery, content }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingDelivery {
    Steer,
    Queue,
}

impl PendingInputs {
    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn items(&self) -> &[PendingInput] {
        &self.items
    }

    pub(crate) fn push(&mut self, message: InputMessage, delivery: PendingDelivery) {
        if let InputMessage::User(content) = message {
            self.items.push(PendingInput { delivery, content });
        }
    }

    pub(crate) fn commit_next(
        &mut self,
        message: &InputMessage,
        delivery: PendingDelivery,
    ) -> bool {
        let InputMessage::User(expected) = message else {
            return false;
        };
        let Some(index) = self
            .items
            .iter()
            .position(|pending| pending.delivery == delivery && pending.content == *expected)
        else {
            return false;
        };
        self.items.remove(index);
        true
    }

    pub(crate) fn requeue_next_steer(&mut self, message: &InputMessage) -> bool {
        let InputMessage::User(expected) = message else {
            return false;
        };
        let Some(pending) = self.items.iter_mut().find(|pending| {
            pending.delivery == PendingDelivery::Steer && pending.content == *expected
        }) else {
            return false;
        };
        pending.delivery = PendingDelivery::Queue;
        true
    }

    pub(crate) fn render_height(&self) -> u16 {
        self.render_lines().len() as u16
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.is_empty() || self.is_empty() {
            return;
        }

        frame.render_widget(
            Paragraph::new(self.render_lines()).style(pending_area_style()),
            area,
        );
    }

    fn render_lines(&self) -> Vec<Line<'static>> {
        if self.items.len() <= MAX_PENDING_INPUT_ROWS {
            return self.items.iter().map(pending_input_line).collect();
        }

        let visible = MAX_PENDING_INPUT_ROWS.saturating_sub(1);
        let mut lines = self
            .items
            .iter()
            .take(visible)
            .map(pending_input_line)
            .collect::<Vec<_>>();
        lines.push(Line::from(Span::styled(
            format!("+{} pending", self.items.len().saturating_sub(visible)),
            pending_overflow_style(),
        )));
        lines
    }
}

fn pending_input_line(pending: &PendingInput) -> Line<'static> {
    let label = match pending.delivery {
        PendingDelivery::Steer => "steer",
        PendingDelivery::Queue => "queue",
    };
    Line::from(vec![
        Span::styled(label.to_string(), pending_label_style(pending.delivery)),
        Span::raw(" "),
        Span::styled(
            pending_input_preview(&pending.content),
            pending_text_style(),
        ),
    ])
}

fn pending_input_preview(content: &[InputContent]) -> String {
    content
        .iter()
        .map(|item| match item {
            InputContent::Text(text) => text.replace(['\n', '\r'], " "),
            InputContent::Image(_) => "[image]".to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn pending_area_style() -> Style {
    Style::default().fg(Color::Gray).bg(Color::Rgb(31, 34, 40))
}

fn pending_label_style(delivery: PendingDelivery) -> Style {
    match delivery {
        PendingDelivery::Steer => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        PendingDelivery::Queue => Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
    }
}

fn pending_text_style() -> Style {
    Style::default().fg(Color::Gray)
}

fn pending_overflow_style() -> Style {
    Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC)
}
