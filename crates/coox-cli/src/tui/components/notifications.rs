use std::{
    fmt,
    time::{Duration, Instant},
};

use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Padding, Paragraph},
};
use tui_overlay::{Anchor, Easing, Overlay, OverlayState, Slide};
use unicode_width::UnicodeWidthStr;

const NOTIFICATION_TTL: Duration = Duration::from_millis(1_800);
const NOTIFICATION_ANIMATION: Duration = Duration::from_millis(120);
const NOTIFICATION_MIN_WIDTH: u16 = 14;
const NOTIFICATION_MAX_WIDTH: u16 = 44;
const NOTIFICATION_HEIGHT: u16 = 3;

pub struct NotificationCenter {
    current: Option<Notification>,
    overlay_state: OverlayState,
}

impl NotificationCenter {
    pub fn new() -> Self {
        Self {
            current: None,
            overlay_state: OverlayState::new()
                .with_duration(NOTIFICATION_ANIMATION)
                .with_easing(Easing::EaseOut),
        }
    }

    pub fn push(&mut self, kind: NotificationKind, message: impl Into<String>) {
        self.push_at(kind, message, Instant::now());
    }

    fn push_at(&mut self, kind: NotificationKind, message: impl Into<String>, now: Instant) {
        self.current = Some(Notification {
            kind,
            message: message.into(),
            expires_at: now + NOTIFICATION_TTL,
        });
        self.overlay_state.open();
        self.overlay_state.tick(NOTIFICATION_ANIMATION);
    }

    pub fn tick(&mut self, now: Instant) -> bool {
        if self
            .current
            .as_ref()
            .is_some_and(|notification| now >= notification.expires_at)
        {
            self.current = None;
            return true;
        }

        false
    }

    pub fn next_deadline(&self, now: Instant) -> Option<Duration> {
        self.current
            .as_ref()
            .map(|notification| notification.expires_at.saturating_duration_since(now))
    }

    pub fn current_message(&self) -> Option<&str> {
        self.current
            .as_ref()
            .map(|notification| notification.message.as_str())
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let Some(notification) = &self.current else {
            return;
        };
        if area.width == 0 || area.height == 0 {
            return;
        }

        let width = notification_width(notification, area.width);
        let height = NOTIFICATION_HEIGHT.min(area.height);
        let overlay = Overlay::new()
            .anchor(Anchor::BottomRight)
            .slide(Slide::Bottom)
            .width(Constraint::Length(width))
            .height(Constraint::Length(height));

        frame.render_stateful_widget(overlay, area, &mut self.overlay_state);
        let Some(inner) = self.overlay_state.inner_area() else {
            return;
        };

        frame.render_widget(Clear, inner);
        frame.render_widget(notification_view(notification), inner);
    }
}

impl Default for NotificationCenter {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for NotificationCenter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NotificationCenter")
            .field("current", &self.current)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Notification {
    kind: NotificationKind,
    message: String,
    expires_at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationKind {
    Info,
    Success,
    Warning,
    Error,
}

fn notification_width(notification: &Notification, area_width: u16) -> u16 {
    let text_width = UnicodeWidthStr::width(notification.message.as_str()) as u16;
    let label_width = notification.kind.label().len() as u16;
    let width = text_width
        .saturating_add(label_width)
        .saturating_add(5)
        .clamp(NOTIFICATION_MIN_WIDTH, NOTIFICATION_MAX_WIDTH);

    width.min(area_width)
}

fn notification_view(notification: &Notification) -> Paragraph<'static> {
    Paragraph::new(Line::from(vec![
        Span::styled(
            notification.kind.label().to_string(),
            notification.kind.label_style(),
        ),
        Span::raw(" "),
        Span::raw(notification.message.clone()),
    ]))
    .style(notification.kind.body_style())
    .block(
        Block::new()
            .borders(Borders::ALL)
            .border_style(notification.kind.border_style())
            .padding(Padding::horizontal(1)),
    )
}

impl NotificationKind {
    fn label(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Success => "ok",
            Self::Warning => "wait",
            Self::Error => "error",
        }
    }

    fn label_style(self) -> Style {
        self.border_style().add_modifier(Modifier::BOLD)
    }

    fn body_style(self) -> Style {
        match self {
            Self::Info => Style::default().fg(Color::Gray).bg(Color::Rgb(28, 32, 36)),
            Self::Success => Style::default().fg(Color::White).bg(Color::Rgb(35, 50, 38)),
            Self::Warning => Style::default().fg(Color::White).bg(Color::Rgb(52, 45, 30)),
            Self::Error => Style::default().fg(Color::White).bg(Color::Rgb(58, 35, 35)),
        }
    }

    fn border_style(self) -> Style {
        match self {
            Self::Info => Style::default().fg(Color::DarkGray),
            Self::Success => Style::default().fg(Color::Green),
            Self::Warning => Style::default().fg(Color::Yellow),
            Self::Error => Style::default().fg(Color::LightRed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_expires_after_ttl() {
        let now = Instant::now();
        let mut center = NotificationCenter::new();

        center.push_at(NotificationKind::Warning, "busy", now);

        assert_eq!(center.current_message(), Some("busy"));
        assert!(!center.tick(now + NOTIFICATION_TTL - Duration::from_millis(1)));
        assert!(center.tick(now + NOTIFICATION_TTL));
        assert_eq!(center.current_message(), None);
    }
}
