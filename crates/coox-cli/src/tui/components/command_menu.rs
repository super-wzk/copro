use crate::command::{SlashCommand, SlashCommandRegistry};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Constraint, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, FrameExt, HighlightSpacing, List, ListItem, ListState, Padding,
        StatefulWidget, StatefulWidgetRef, Widget,
    },
};
use tui_overlay::{Anchor, Overlay, OverlayState, Slide};

const MAX_ROWS: usize = 8;

#[derive(Debug)]
pub struct CommandMenu {
    state: CommandMenuState,
}

#[derive(Debug)]
struct CommandMenuState {
    dismissed: bool,
    overlay_state: OverlayState,
    list_state: ListState,
}

impl CommandMenu {
    pub fn new() -> Self {
        Self {
            state: CommandMenuState::new(),
        }
    }

    pub fn is_open(&self, input: &str) -> bool {
        self.state.is_open(input)
    }

    pub fn input_changed(&mut self) {
        self.state.dismissed = false;
    }

    pub fn dismiss(&mut self) {
        self.state.dismissed = true;
        self.state.overlay_state.close();
    }

    pub fn selected_command(
        &mut self,
        input: &str,
        registry: SlashCommandRegistry,
    ) -> Option<&'static SlashCommand> {
        let matches = matches_for(registry, input);
        self.state.clamp_selected(matches.len());
        self.state
            .selected_index()
            .and_then(|selected| matches.get(selected).copied())
    }

    pub fn select_next(&mut self, input: &str, registry: SlashCommandRegistry) {
        let len = matches_for(registry, input).len();
        if len == 0 {
            self.state.clamp_selected(0);
        } else {
            let selected = self.state.selected_or_zero();
            self.state.select(Some((selected + 1) % len));
        }
    }

    pub fn select_prev(&mut self, input: &str, registry: SlashCommandRegistry) {
        let len = matches_for(registry, input).len();
        if len == 0 {
            self.state.clamp_selected(0);
        } else {
            self.state.clamp_selected(len);
            let selected = self.state.selected_or_zero();
            let selected = if selected == 0 { len - 1 } else { selected - 1 };
            self.state.select(Some(selected));
        }
    }

    pub fn render(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        input: &str,
        registry: SlashCommandRegistry,
    ) {
        frame.render_stateful_widget_ref(
            CommandMenuView { input, registry },
            area,
            &mut self.state,
        );
    }

    pub fn contains(&self, column: u16, row: u16) -> bool {
        self.state
            .overlay_state
            .overlay_rect()
            .is_some_and(|rect| rect.contains(Position::new(column, row)))
    }

    #[cfg(test)]
    fn selected(&self) -> usize {
        self.state.selected_or_zero()
    }

    #[cfg(test)]
    pub(crate) fn overlay_rect(&self) -> Option<Rect> {
        self.state.overlay_state.overlay_rect()
    }

    #[cfg(test)]
    fn list_offset(&self) -> usize {
        self.state.list_state.offset()
    }
}

impl CommandMenuState {
    fn new() -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));

        Self {
            dismissed: false,
            overlay_state: OverlayState::new(),
            list_state,
        }
    }

    fn is_open(&self, input: &str) -> bool {
        is_open_input(input) && !self.dismissed
    }

    fn selected_index(&self) -> Option<usize> {
        self.list_state.selected()
    }

    fn selected_or_zero(&self) -> usize {
        self.selected_index().unwrap_or(0)
    }

    fn select(&mut self, selected: Option<usize>) {
        self.list_state.select(selected);
    }

    fn clamp_selected(&mut self, len: usize) {
        if len == 0 {
            self.select(None);
        } else {
            let selected = self.selected_or_zero().min(len - 1);
            self.select(Some(selected));
        }
    }
}

impl Default for CommandMenu {
    fn default() -> Self {
        Self::new()
    }
}

fn matches_for(registry: SlashCommandRegistry, input: &str) -> Vec<&'static SlashCommand> {
    let query = query_for(input);

    let mut matches = registry
        .iter()
        .filter(|command| command_matches(command, query))
        .collect::<Vec<_>>();
    if !query.is_empty() {
        matches.sort_by_key(|command| command_match_rank(command, query));
    }
    matches
}

fn is_open_input(input: &str) -> bool {
    input.starts_with('/') && !input.starts_with("//") && !input.chars().any(char::is_whitespace)
}

fn query_for(input: &str) -> &str {
    input
        .strip_prefix('/')
        .unwrap_or(input)
        .trim_start_matches('/')
}

fn command_matches(command: &SlashCommand, query: &str) -> bool {
    query.is_empty() || command.spec.name.contains(query)
}

fn command_match_rank(command: &SlashCommand, query: &str) -> (u8, usize) {
    let name = command.spec.name;
    let prefix_rank = if name.starts_with(query) { 0 } else { 1 };
    let match_index = name.find(query).unwrap_or(usize::MAX);
    (prefix_rank, match_index)
}

fn menu_height(len: usize, area_height: u16) -> u16 {
    let rows = len.min(MAX_ROWS) as u16;
    rows.saturating_add(2).min(area_height)
}

struct CommandMenuView<'a> {
    input: &'a str,
    registry: SlashCommandRegistry,
}

impl StatefulWidgetRef for CommandMenuView<'_> {
    type State = CommandMenuState;

    fn render_ref(&self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        if !state.is_open(self.input) || area.is_empty() {
            state.overlay_state.close();
            return;
        }

        let matches = matches_for(self.registry, self.input);
        if matches.is_empty() {
            state.overlay_state.close();
            state.clamp_selected(0);
            return;
        }

        state.clamp_selected(matches.len());
        state.overlay_state.open();

        let height = menu_height(matches.len(), area.height);
        let overlay = Overlay::new()
            .anchor(Anchor::BottomLeft)
            .slide(Slide::Top)
            .width(Constraint::Length(area.width))
            .height(Constraint::Length(height));

        StatefulWidget::render(overlay, area, buf, &mut state.overlay_state);
        let Some(inner) = state.overlay_state.inner_area() else {
            return;
        };

        Widget::render(Clear, inner, buf);
        StatefulWidget::render(menu_view(&matches), inner, buf, &mut state.list_state);
    }
}

fn menu_view(commands: &[&SlashCommand]) -> List<'static> {
    let items = commands.iter().map(|command| {
        ListItem::new(Line::from(vec![
            Span::styled(format!("/{:<12}", command.spec.name), command_style()),
            Span::styled(command.spec.summary.to_string(), command_style()),
        ]))
    });

    List::new(items)
        .style(Style::default().fg(Color::Gray).bg(Color::Rgb(22, 26, 30)))
        .block(
            Block::new()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .padding(Padding::horizontal(1)),
        )
        .highlight_symbol("> ")
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_style(selected_style())
}

fn command_style() -> Style {
    Style::default().fg(Color::Gray)
}

fn selected_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{AppCommand, SessionSnapshot, SlashCommandSpec, SlashError, builtins};
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn open_predicate_excludes_double_slash_and_whitespace() {
        let menu = CommandMenu::new();

        assert!(menu.is_open("/"));
        assert!(menu.is_open("/mo"));
        assert!(!menu.is_open("//mo"));
        assert!(!menu.is_open("/model "));
        assert!(!menu.is_open("hello"));
    }

    #[test]
    fn filters_by_name() {
        let registry = builtins();

        let by_name = matches_for(registry, "/mo");
        let by_infix = matches_for(registry, "/su");
        let by_single_character = matches_for(registry, "/q");
        let empty_query = matches_for(registry, "/");
        let by_r = matches_for(registry, "/r");

        assert_eq!(by_name[0].spec.name, "model");
        assert_eq!(by_infix[0].spec.name, "resume");
        assert_eq!(by_single_character[0].spec.name, "quit");
        assert_eq!(empty_query.len(), builtins::BUILTIN_COMMANDS.len());
        assert_eq!(by_r[0].spec.name, "resume");
    }

    #[test]
    fn selected_clamps_after_refilter() {
        let registry = builtins();
        let mut menu = CommandMenu::new();

        menu.select_next("/", registry);
        menu.select_next("/", registry);
        assert!(menu.selected() > 0);
        let selected = menu
            .selected_command("/mo", registry)
            .expect("selected command");

        assert_eq!(selected.spec.name, "model");
        assert_eq!(menu.selected(), 0);
    }

    #[test]
    fn next_selection_wraps_to_first_match() {
        let registry = builtins();
        let mut menu = CommandMenu::new();
        let len = matches_for(registry, "/").len();

        for _ in 0..len {
            menu.select_next("/", registry);
        }

        assert_eq!(menu.selected(), 0);
    }

    #[test]
    fn previous_selection_wraps_to_last_match() {
        let registry = builtins();
        let mut menu = CommandMenu::new();
        let len = matches_for(registry, "/").len();

        menu.select_prev("/", registry);

        assert_eq!(menu.selected(), len - 1);
    }

    #[test]
    fn dismiss_closes_until_input_changes() {
        let mut menu = CommandMenu::new();

        menu.dismiss();
        assert!(!menu.is_open("/mo"));
        menu.input_changed();
        assert!(menu.is_open("/mo"));
    }

    #[test]
    fn render_uses_full_area_width() {
        let registry = builtins();
        let mut menu = CommandMenu::new();
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| {
                let area = frame.area();
                menu.render(frame, area, "/", registry);
            })
            .expect("render command menu");

        let rect = menu.overlay_rect().expect("menu overlay rect");
        assert_eq!(rect.x, 0);
        assert_eq!(rect.width, 60);
    }

    #[test]
    fn render_scrolls_selected_command_into_view() {
        let registry = scroll_registry();
        let mut menu = CommandMenu::new();
        for _ in 0..10 {
            menu.select_next("/cmd", registry);
        }

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                menu.render(frame, area, "/cmd", registry);
            })
            .expect("render command menu");

        assert_eq!(menu.selected(), 10);
        assert!(menu.list_offset() > 0);

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("/cmd10"));
        assert!(!rendered.contains("/cmd00"));
    }

    fn scroll_registry() -> SlashCommandRegistry {
        SlashCommandRegistry::new(SCROLL_COMMANDS)
    }

    fn noop_command(
        _args: &str,
        _snapshot: &SessionSnapshot<'_>,
    ) -> Result<Vec<AppCommand>, SlashError> {
        Ok(Vec::new())
    }

    static SCROLL_COMMANDS: &[SlashCommand] = &[
        test_command("cmd00"),
        test_command("cmd01"),
        test_command("cmd02"),
        test_command("cmd03"),
        test_command("cmd04"),
        test_command("cmd05"),
        test_command("cmd06"),
        test_command("cmd07"),
        test_command("cmd08"),
        test_command("cmd09"),
        test_command("cmd10"),
    ];

    const fn test_command(name: &'static str) -> SlashCommand {
        SlashCommand {
            spec: SlashCommandSpec {
                name,
                summary: "Test command",
                usage: "/test",
            },
            build: noop_command,
        }
    }

    fn buffer_text(buffer: &Buffer) -> String {
        let mut text = String::new();
        for y in buffer.area.top()..buffer.area.bottom() {
            for x in buffer.area.left()..buffer.area.right() {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }
}
