use std::{
    io,
    time::{Duration, Instant},
};

use crate::agent::config::{RuntimeConfig, build_model};
use crate::agent::events::{apply_agent_event, apply_runtime_error};
use crate::agent::runtime::{AgentRuntime, RuntimeEvent, RuntimeTurnSnapshot, SubmitError};
use crate::command::{
    AppCommand, InputIntent, RuntimeCommand, SessionSnapshot, SlashCommandRegistry, SlashError,
    TurnSnapshot, UiCommand, builtins, parse_input,
};
use crate::tui::components::{
    command_menu::CommandMenu,
    conversation::{ConversationLayout, ConversationView},
    notifications::{NotificationCenter, NotificationKind},
    status::{StatusBar, StatusBarState},
};
use crate::tui::state::AppState;
use crate::tui::terminal::Tui;
use coox_tui::{
    clipboard::ClipboardHandler,
    components::{
        image::ImageRenderer,
        input::{InputBox, InputEditor},
        scroll_view::ScrollViewState,
    },
    selection::{SelectionManager, SelectionMap},
};
use copro_agent::AgentHistory;
use copro_api::message::{InputContent, InputMessage};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    widgets::FrameExt,
};
use tokio::sync::mpsc;

const INPUT_TOP_GAP: u16 = 1;
const PAGE_SCROLL_ROWS: u32 = 6;
const MOUSE_SCROLL_ROWS: u32 = 3;
const SELECTION_AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(30);
const IDLE_INPUT_POLL_TIMEOUT: Duration = Duration::from_millis(50);
const IMAGE_INPUT_POLL_TIMEOUT: Duration = Duration::from_millis(16);

#[derive(Debug)]
pub struct App {
    runtime: AgentRuntime,
    runtime_config: RuntimeConfig,
    slash_commands: SlashCommandRegistry,
    runtime_events_tx: mpsc::UnboundedSender<RuntimeEvent>,
    runtime_events_rx: mpsc::UnboundedReceiver<RuntimeEvent>,
    image_renderer: ImageRenderer,
    state: AppState,
    conversation_scroll_from_bottom: u32,
    input: InputEditor,
    command_menu: CommandMenu,
    status: StatusBarState,
    notifications: NotificationCenter,
    clipboard: ClipboardHandler,
    render_dirty: bool,
    conversation_dirty: bool,
    conversation_layout: Option<ConversationLayout>,
    conversation_cache: Option<ConversationCache>,
    selection_manager: SelectionManager<AppSelectionSurface>,
    conversation_selection_autoscroll: Option<SelectionAutoscroll>,
    frozen_selection_viewport_start: Option<u32>,
    quit: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AppSelectionSurface {
    Conversation,
}

#[derive(Debug)]
struct ConversationCache {
    area: Rect,
    buffer: Buffer,
    copy_map: SelectionMap,
}

#[derive(Clone, Copy, Debug)]
struct SelectionAutoscroll {
    scroll_delta: i32,
    column: u16,
    next_tick: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DirtyState {
    frame: bool,
    conversation: bool,
}

impl DirtyState {
    const fn none() -> Self {
        Self {
            frame: false,
            conversation: false,
        }
    }

    const fn frame() -> Self {
        Self {
            frame: true,
            conversation: false,
        }
    }

    const fn conversation() -> Self {
        Self {
            frame: true,
            conversation: true,
        }
    }
}

impl App {
    pub fn new(runtime: AgentRuntime, workspace: String, model: String) -> Self {
        Self::new_with_image_renderer(runtime, workspace, model, ImageRenderer::default())
    }

    pub fn new_with_image_renderer(
        runtime: AgentRuntime,
        workspace: String,
        model: String,
        image_renderer: ImageRenderer,
    ) -> Self {
        let mut runtime_config = RuntimeConfig::from_env();
        runtime_config.model_id = model;

        Self::new_with_runtime_config(runtime, workspace, runtime_config, image_renderer)
    }

    pub fn new_with_runtime_config(
        runtime: AgentRuntime,
        workspace: String,
        runtime_config: RuntimeConfig,
        image_renderer: ImageRenderer,
    ) -> Self {
        let (runtime_events_tx, runtime_events_rx) = mpsc::unbounded_channel();
        let model = runtime_config.model_id.clone();

        Self {
            runtime,
            runtime_config,
            slash_commands: builtins(),
            runtime_events_tx,
            runtime_events_rx,
            image_renderer,
            state: AppState::default(),
            conversation_scroll_from_bottom: 0,
            input: InputEditor::default(),
            command_menu: CommandMenu::new(),
            status: StatusBarState {
                workspace,
                model,
                context: "0%".to_string(),
            },
            notifications: NotificationCenter::new(),
            clipboard: ClipboardHandler::new(),
            render_dirty: true,
            conversation_dirty: true,
            conversation_layout: None,
            conversation_cache: None,
            selection_manager: SelectionManager::default(),
            conversation_selection_autoscroll: None,
            frozen_selection_viewport_start: None,
            quit: false,
        }
    }

    fn mark_dirty(&mut self, dirty: DirtyState) {
        self.render_dirty |= dirty.frame;
        self.conversation_dirty |= dirty.conversation;
    }
}

pub async fn run(tui: &mut Tui, app: &mut App) -> io::Result<()> {
    while !app.quit {
        let autoscroll_dirty = tick_selection_autoscroll(app);
        app.mark_dirty(autoscroll_dirty);
        draw_if_dirty(&mut tui.terminal, app)?;

        let now = Instant::now();
        let mut poll_timeout = if app.image_renderer.has_in_flight() {
            IMAGE_INPUT_POLL_TIMEOUT
        } else {
            IDLE_INPUT_POLL_TIMEOUT
        };
        if let Some(autoscroll) = app.conversation_selection_autoscroll {
            poll_timeout = poll_timeout.min(autoscroll.next_tick.saturating_duration_since(now));
        }
        if let Some(deadline) = app.notifications.next_deadline(now) {
            poll_timeout = poll_timeout.min(deadline);
        }

        if event::poll(poll_timeout)? {
            let area = tui.terminal.size()?;
            let input_box = input_box();
            let input_layout = input_box.layout(&app.input, area.width);
            handle_event(app, event::read()?, input_layout.content_width());
        }

        tokio::task::yield_now().await;
    }

    Ok(())
}

fn draw_if_dirty<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<bool, B::Error> {
    let notification_dirty = tick_notifications(app);
    app.mark_dirty(notification_dirty);

    let dirty = drain_runtime_events(app);
    app.mark_dirty(dirty);
    if app.image_renderer.drain_prepared() {
        app.mark_dirty(DirtyState::conversation());
    }
    if !app.render_dirty {
        return Ok(false);
    }

    terminal.draw(|frame| render_app(frame, app))?;
    app.render_dirty = false;
    Ok(true)
}

fn drain_runtime_events(app: &mut App) -> DirtyState {
    let mut changed = false;
    while let Ok(event) = app.runtime_events_rx.try_recv() {
        changed = true;
        match event {
            RuntimeEvent::Agent(event) => {
                let aborted = matches!(&event, copro_agent::AgentEvent::TurnAborted);
                apply_agent_event(event, &mut app.state);
                if aborted {
                    push_notification(app, NotificationKind::Warning, "aborted");
                }
            }
            RuntimeEvent::TurnFinished { history } => {
                app.runtime.finish_success(history);
            }
            RuntimeEvent::TurnFailed { history, message } => {
                app.runtime.finish_failure(history);
                push_notification(app, NotificationKind::Error, message.clone());
                apply_runtime_error(message, &mut app.state);
            }
            RuntimeEvent::ControlFailed { message } => {
                app.state.push_command_error(message.clone());
                push_notification(app, NotificationKind::Error, message);
            }
        }
    }

    if changed {
        preserve_frozen_selection_viewport(app);
        DirtyState::conversation()
    } else {
        DirtyState::none()
    }
}

fn render_app(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let input_box = input_box();
    let input_layout = input_box.layout(&app.input, area.width);
    let input_height = input_layout.render_height();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(INPUT_TOP_GAP),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(area);

    render_conversation_area(frame, app, chunks[0]);
    frame.render_stateful_widget_ref(input_box, chunks[2], &mut app.input);
    if let Some(position) = input_layout.cursor_position(chunks[2]) {
        frame.set_cursor_position(position);
    }
    frame.render_widget_ref(StatusBar::new(&app.status), chunks[3]);
    render_command_menu(frame, app, chunks[0]);
    app.notifications.render(frame, chunks[0]);
}

fn input_box() -> InputBox {
    InputBox::new().style(crate::tui::components::blocks::user_style())
}

fn render_conversation_area(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let cache_is_current = app
        .conversation_cache
        .as_ref()
        .is_some_and(|cache| cache.area == area);

    if app.conversation_dirty || !cache_is_current {
        ensure_conversation_layout(app, area.width);
        let selection = app
            .selection_manager
            .selection_for(&AppSelectionSurface::Conversation);
        let layout = app
            .conversation_layout
            .as_ref()
            .expect("conversation layout is prepared");
        let selection_map = layout.selection_map(area, app.conversation_scroll_from_bottom);
        frame.render_widget_ref(
            ConversationView::new(layout, &app.image_renderer)
                .scroll_from_bottom(app.conversation_scroll_from_bottom)
                .selection(selection),
            area,
        );
        app.selection_manager
            .register(AppSelectionSurface::Conversation, selection_map.clone());
        app.conversation_cache = Some(ConversationCache {
            area,
            buffer: copy_buffer_area(frame.buffer_mut(), area),
            copy_map: selection_map,
        });
        app.conversation_dirty = false;
    } else if let Some(cache) = &app.conversation_cache {
        copy_cached_area(&cache.buffer, frame.buffer_mut(), cache.area);
        app.selection_manager
            .register(AppSelectionSurface::Conversation, cache.copy_map.clone());
    }
}

fn render_command_menu(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let input = app.input.text().to_string();
    let registry = app.slash_commands;
    app.command_menu.render(frame, area, &input, registry);
}

fn ensure_conversation_layout(app: &mut App, width: u16) {
    if app
        .conversation_layout
        .as_ref()
        .is_some_and(|layout| layout.is_current(&app.state, width))
    {
        return;
    }

    app.conversation_layout = Some(ConversationLayout::prepare(&app.state, width));
}

fn copy_buffer_area(source: &Buffer, area: Rect) -> Buffer {
    let mut target = Buffer::empty(area);
    copy_cached_area(source, &mut target, area);
    target
}

fn copy_cached_area(source: &Buffer, target: &mut Buffer, area: Rect) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            target[(x, y)] = source[(x, y)].clone();
        }
    }
}

fn handle_event(app: &mut App, event: Event, input_width: usize) -> DirtyState {
    match event {
        Event::Key(key) => handle_key(app, key, input_width),
        Event::Mouse(mouse) => handle_mouse(app, mouse),
        Event::Resize(_, _) => {
            let dirty = DirtyState::conversation();
            app.mark_dirty(dirty);
            dirty
        }
        _ => DirtyState::none(),
    }
}

fn handle_mouse(app: &mut App, mouse: MouseEvent) -> DirtyState {
    if app.command_menu.is_open(app.input.text())
        && app.command_menu.contains(mouse.column, mouse.row)
    {
        return DirtyState::none();
    }

    let dirty = match mouse.kind {
        MouseEventKind::ScrollUp if mouse_in_conversation_area(app, mouse) => {
            let had_selection = has_conversation_selection(app);
            clear_conversation_selection(app);
            if scroll_conversation(app, MOUSE_SCROLL_ROWS as i32) || had_selection {
                DirtyState::conversation()
            } else {
                DirtyState::none()
            }
        }
        MouseEventKind::ScrollDown if mouse_in_conversation_area(app, mouse) => {
            let had_selection = has_conversation_selection(app);
            clear_conversation_selection(app);
            if scroll_conversation(app, -(MOUSE_SCROLL_ROWS as i32)) || had_selection {
                DirtyState::conversation()
            } else {
                DirtyState::none()
            }
        }
        MouseEventKind::Down(MouseButton::Left) if mouse_in_conversation_area(app, mouse) => {
            app.conversation_selection_autoscroll = None;
            if app
                .selection_manager
                .start_at(mouse.column, mouse.row)
                .is_some()
            {
                app.frozen_selection_viewport_start = app
                    .selection_manager
                    .map_for(&AppSelectionSurface::Conversation)
                    .map(SelectionMap::viewport_start);
                DirtyState::conversation()
            } else {
                clear_conversation_selection(app);
                DirtyState::conversation()
            }
        }
        MouseEventKind::Drag(MouseButton::Left) if app.selection_manager.is_dragging() => {
            update_conversation_selection_drag(app, mouse);
            DirtyState::conversation()
        }
        MouseEventKind::Up(MouseButton::Left) if app.selection_manager.is_dragging() => {
            finish_conversation_selection(app);
            DirtyState::conversation()
        }
        _ => DirtyState::none(),
    };

    app.mark_dirty(dirty);
    dirty
}

fn has_conversation_selection(app: &App) -> bool {
    app.selection_manager
        .selection_for(&AppSelectionSurface::Conversation)
        .is_some()
        || app.selection_manager.is_dragging()
        || app.conversation_selection_autoscroll.is_some()
}

fn mouse_in_conversation_area(app: &App, mouse: MouseEvent) -> bool {
    app.selection_manager.contains_point(
        &AppSelectionSurface::Conversation,
        mouse.column,
        mouse.row,
    )
}

fn scroll_conversation(app: &mut App, delta: i32) -> bool {
    let max_scroll_from_bottom =
        conversation_selection_map(app).map(SelectionMap::max_viewport_start);
    let mut scroll_state = ScrollViewState::from_bottom(app.conversation_scroll_from_bottom);
    scroll_state.scroll_by(delta, max_scroll_from_bottom.unwrap_or(u32::MAX));

    set_conversation_scroll_from_bottom(app, scroll_state.scroll_from_bottom())
}

fn set_conversation_scroll_from_bottom(app: &mut App, scroll_from_bottom: u32) -> bool {
    let max_scroll_from_bottom =
        conversation_selection_map(app).map(SelectionMap::max_viewport_start);
    let mut scroll_state = ScrollViewState::from_bottom(app.conversation_scroll_from_bottom);
    scroll_state.set_scroll_from_bottom(
        scroll_from_bottom,
        max_scroll_from_bottom.unwrap_or(u32::MAX),
    );
    let scroll_from_bottom = scroll_state.scroll_from_bottom();
    let changed = app.conversation_scroll_from_bottom != scroll_from_bottom;
    app.conversation_scroll_from_bottom = scroll_from_bottom;

    if scroll_from_bottom == 0 {
        app.frozen_selection_viewport_start = None;
    } else if let Some(max_scroll_from_bottom) = max_scroll_from_bottom {
        app.frozen_selection_viewport_start =
            Some(max_scroll_from_bottom.saturating_sub(scroll_from_bottom));
    }

    changed
}

fn update_conversation_selection_drag(app: &mut App, mouse: MouseEvent) {
    let Some(area) = app.selection_manager.active_area() else {
        return;
    };
    if area.is_empty() {
        return;
    }

    if mouse.row >= area.y && mouse.row < area.y.saturating_add(area.height) {
        app.conversation_selection_autoscroll = None;
        app.selection_manager
            .update_focus_nearest(mouse.column, mouse.row);
        return;
    }

    let scroll_delta = if mouse.row < area.y { 1 } else { -1 };
    let max_x = area.x.saturating_add(area.width.saturating_sub(1));
    app.conversation_selection_autoscroll = Some(SelectionAutoscroll {
        scroll_delta,
        column: mouse.column.clamp(area.x, max_x),
        next_tick: Instant::now(),
    });
}

fn finish_conversation_selection(app: &mut App) {
    app.conversation_selection_autoscroll = None;

    let text = app
        .selection_manager
        .finish_copy()
        .map(|(_, text)| text)
        .unwrap_or_default();

    if text.is_empty() {
        push_notification(app, NotificationKind::Info, "no selection");
    } else if app.clipboard.write_text(&text).is_ok() {
        push_notification(app, NotificationKind::Success, "copied");
    } else {
        push_notification(app, NotificationKind::Error, "copy failed");
    }

    clear_conversation_selection(app);
}

fn clear_conversation_selection(app: &mut App) {
    app.selection_manager.clear();
    app.conversation_selection_autoscroll = None;
    app.frozen_selection_viewport_start = None;
}

fn conversation_selection_map(app: &App) -> Option<&SelectionMap> {
    app.selection_manager
        .map_for(&AppSelectionSurface::Conversation)
}

fn tick_selection_autoscroll(app: &mut App) -> DirtyState {
    let Some(autoscroll) = app.conversation_selection_autoscroll else {
        return DirtyState::none();
    };
    if !app.selection_manager.is_dragging() {
        app.conversation_selection_autoscroll = None;
        return DirtyState::none();
    }

    let now = Instant::now();
    if now < autoscroll.next_tick {
        return DirtyState::none();
    }

    scroll_conversation(app, autoscroll.scroll_delta);
    refresh_conversation_selection_map(app);

    let Some(area) = conversation_selection_map(app).map(SelectionMap::area) else {
        return DirtyState::conversation();
    };
    let edge_row = if autoscroll.scroll_delta > 0 {
        area.y
    } else {
        area.y.saturating_add(area.height.saturating_sub(1))
    };
    app.selection_manager
        .update_focus_nearest(autoscroll.column, edge_row);

    app.conversation_selection_autoscroll = Some(SelectionAutoscroll {
        next_tick: now + SELECTION_AUTOSCROLL_INTERVAL,
        ..autoscroll
    });
    DirtyState::conversation()
}

fn preserve_frozen_selection_viewport(app: &mut App) {
    let Some(frozen_start) = app.frozen_selection_viewport_start else {
        return;
    };
    let Some(area) = conversation_selection_map(app).map(SelectionMap::area) else {
        return;
    };
    if area.is_empty() {
        return;
    }

    ensure_conversation_layout(app, area.width);
    let bottom_map = app
        .conversation_layout
        .as_ref()
        .expect("conversation layout is prepared")
        .selection_map(area, 0);
    let clamped_start = frozen_start.min(bottom_map.max_viewport_start());
    app.conversation_scroll_from_bottom = bottom_map
        .max_viewport_start()
        .saturating_sub(clamped_start);
}

fn refresh_conversation_selection_map(app: &mut App) {
    let Some(area) = conversation_selection_map(app).map(SelectionMap::area) else {
        return;
    };
    ensure_conversation_layout(app, area.width);
    let selection_map = app
        .conversation_layout
        .as_ref()
        .expect("conversation layout is prepared")
        .selection_map(area, app.conversation_scroll_from_bottom);
    app.frozen_selection_viewport_start = Some(selection_map.viewport_start());
    app.selection_manager
        .register(AppSelectionSurface::Conversation, selection_map);
}

fn handle_key(app: &mut App, key: KeyEvent, input_width: usize) -> DirtyState {
    let dirty = if key.kind == KeyEventKind::Release {
        DirtyState::none()
    } else if let Some(dirty) = handle_command_menu_key(app, key) {
        dirty
    } else {
        match key.code {
            KeyCode::Char('c' | 'C') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.quit = true;
                DirtyState::none()
            }
            KeyCode::Char('o' | 'O') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.state.toggle_all_folds();
                DirtyState::conversation()
            }
            KeyCode::PageUp => {
                if scroll_conversation(app, PAGE_SCROLL_ROWS as i32) {
                    DirtyState::conversation()
                } else {
                    DirtyState::none()
                }
            }
            KeyCode::PageDown => {
                if scroll_conversation(app, -(PAGE_SCROLL_ROWS as i32)) {
                    DirtyState::conversation()
                } else {
                    DirtyState::none()
                }
            }
            KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if set_conversation_scroll_from_bottom(app, u32::MAX) {
                    DirtyState::conversation()
                } else {
                    DirtyState::none()
                }
            }
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if set_conversation_scroll_from_bottom(app, 0) {
                    DirtyState::conversation()
                } else {
                    DirtyState::none()
                }
            }
            KeyCode::Char('j' | 'J') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.input.insert_newline();
                DirtyState::frame()
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                app.input.insert_newline();
                DirtyState::frame()
            }
            KeyCode::Enter if key.modifiers.is_empty() => {
                submit_input(app);
                DirtyState::conversation()
            }
            KeyCode::Backspace => {
                app.input.backspace();
                app.command_menu.input_changed();
                DirtyState::frame()
            }
            KeyCode::Left => {
                app.input.move_left();
                DirtyState::frame()
            }
            KeyCode::Right => {
                app.input.move_right();
                DirtyState::frame()
            }
            KeyCode::Up => {
                app.input.move_up(input_width);
                DirtyState::frame()
            }
            KeyCode::Down => {
                app.input.move_down(input_width);
                DirtyState::frame()
            }
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                app.input.insert_char(ch);
                app.command_menu.input_changed();
                DirtyState::frame()
            }
            _ => DirtyState::none(),
        }
    };

    app.mark_dirty(dirty);
    dirty
}

fn handle_command_menu_key(app: &mut App, key: KeyEvent) -> Option<DirtyState> {
    if !app.command_menu.is_open(app.input.text()) || !key.modifiers.is_empty() {
        return None;
    }

    match key.code {
        KeyCode::Up => {
            let input = app.input.text().to_string();
            app.command_menu.select_prev(&input, app.slash_commands);
            Some(DirtyState::frame())
        }
        KeyCode::Down => {
            let input = app.input.text().to_string();
            app.command_menu.select_next(&input, app.slash_commands);
            Some(DirtyState::frame())
        }
        KeyCode::Esc => {
            app.command_menu.dismiss();
            Some(DirtyState::frame())
        }
        KeyCode::Tab => {
            accept_selected_menu_command(app, false);
            Some(DirtyState::frame())
        }
        KeyCode::Enter => {
            if accept_selected_menu_command(app, true) {
                Some(DirtyState::conversation())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn accept_selected_menu_command(app: &mut App, submit: bool) -> bool {
    let input = app.input.text().to_string();
    let Some(command) = app
        .command_menu
        .selected_command(&input, app.slash_commands)
    else {
        return false;
    };

    let replacement = if submit {
        format!("/{}", command.spec.name)
    } else {
        format!("/{} ", command.spec.name)
    };
    let _ = app.input.take_submission();
    restore_input(app, &replacement);
    app.command_menu.input_changed();

    if submit {
        submit_input(app);
    }

    true
}

fn submit_input(app: &mut App) {
    let Some(text) = app.input.take_submission() else {
        return;
    };

    match parse_input(&text) {
        Some(InputIntent::UserText(user_text)) => submit_user_text(app, user_text, text),
        Some(InputIntent::Slash(invocation)) => {
            dispatch_slash(app, invocation.name, invocation.args, text)
        }
        None => {}
    }
}

fn submit_user_text(app: &mut App, user_text: String, original_text: String) {
    let message = InputMessage::User(vec![InputContent::Text(user_text)]);

    match app
        .runtime
        .submit(message.clone(), app.runtime_events_tx.clone())
    {
        Ok(()) => {
            app.state.push_input(message);
            app.conversation_scroll_from_bottom = 0;
        }
        Err(SubmitError::Busy) => {
            restore_input(app, &original_text);
            push_notification(app, NotificationKind::Warning, "busy");
        }
    }
}

fn dispatch_slash(app: &mut App, name: String, args: String, original_text: String) {
    let Some(command) = app.slash_commands.find(&name) else {
        restore_input(app, &original_text);
        app.state
            .push_command_error(format!("unknown command: /{name}"));
        return;
    };

    let snapshot = session_snapshot(app);
    let commands = match (command.build)(&args, &snapshot) {
        Ok(commands) => commands,
        Err(error) => {
            restore_input(app, &original_text);
            render_slash_error(app, error);
            return;
        }
    };

    execute_app_commands(app, commands);
    app.conversation_scroll_from_bottom = 0;
}

fn session_snapshot(app: &App) -> SessionSnapshot<'_> {
    SessionSnapshot {
        model_id: &app.runtime_config.model_id,
        turn_state: turn_snapshot(app.runtime.turn_snapshot()),
    }
}

fn turn_snapshot(snapshot: RuntimeTurnSnapshot) -> TurnSnapshot {
    match snapshot {
        RuntimeTurnSnapshot::Idle => TurnSnapshot::Idle,
        RuntimeTurnSnapshot::Running => TurnSnapshot::Running,
        RuntimeTurnSnapshot::Paused => TurnSnapshot::Paused,
        RuntimeTurnSnapshot::Preempting => TurnSnapshot::Running,
        RuntimeTurnSnapshot::PendingAck => TurnSnapshot::PendingAck,
        RuntimeTurnSnapshot::Failed => TurnSnapshot::Failed,
    }
}

fn execute_app_commands(app: &mut App, commands: Vec<AppCommand>) {
    for command in commands {
        if let Err(message) = execute_app_command(app, command) {
            app.state.push_command_error(message.clone());
            push_notification(app, NotificationKind::Error, message);
            return;
        }
    }
}

fn execute_app_command(app: &mut App, command: AppCommand) -> Result<(), String> {
    match command {
        AppCommand::Ui(command) => execute_ui_command(app, command),
        AppCommand::Runtime(command) => execute_runtime_command(app, command),
    }
}

fn execute_ui_command(app: &mut App, command: UiCommand) -> Result<(), String> {
    match command {
        UiCommand::ShowHelp => app
            .state
            .push_command_output(format_help(app.slash_commands)),
        UiCommand::ClearConversation => app.state.clear_conversation(),
        UiCommand::PushCommandOutput(text) => app.state.push_command_output(text),
        UiCommand::Scroll { rows } => {
            scroll_conversation(app, rows);
        }
        UiCommand::ScrollToBottom => {
            set_conversation_scroll_from_bottom(app, 0);
        }
        UiCommand::Quit => app.quit = true,
    }
    Ok(())
}

fn execute_runtime_command(app: &mut App, command: RuntimeCommand) -> Result<(), String> {
    match command {
        RuntimeCommand::ClearSessionHistory => app
            .runtime
            .reset_history(AgentHistory::default())
            .map_err(|error| error.to_string()),
        RuntimeCommand::SwitchModel(model_id) => switch_model(app, model_id),
        RuntimeCommand::StopTurn => {
            ensure_running(app.runtime.turn_snapshot())?;
            spawn_runtime_control(app, |runtime| async move { runtime.abort_active().await });
            Ok(())
        }
        RuntimeCommand::PauseTurn => {
            ensure_running(app.runtime.turn_snapshot())?;
            spawn_runtime_control(app, |runtime| async move { runtime.pause_active().await });
            Ok(())
        }
        RuntimeCommand::ResumeTurn => {
            ensure_running(app.runtime.turn_snapshot())?;
            spawn_runtime_control(app, |runtime| async move { runtime.resume_active().await });
            Ok(())
        }
    }
}

fn switch_model(app: &mut App, model_id: String) -> Result<(), String> {
    let mut config = app.runtime_config.clone();
    config.model_id.clone_from(&model_id);
    let model = build_model(&config).map_err(|error| error.to_string())?;

    app.runtime_config = config;
    app.runtime.set_model(model);
    app.status.model = model_id;
    Ok(())
}

fn ensure_running(snapshot: RuntimeTurnSnapshot) -> Result<(), String> {
    match snapshot {
        RuntimeTurnSnapshot::Running
        | RuntimeTurnSnapshot::Paused
        | RuntimeTurnSnapshot::Preempting => Ok(()),
        RuntimeTurnSnapshot::Idle
        | RuntimeTurnSnapshot::PendingAck
        | RuntimeTurnSnapshot::Failed => Err("no active turn".to_string()),
    }
}

fn spawn_runtime_control<F, Fut>(app: &App, control: F)
where
    F: FnOnce(AgentRuntime) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), crate::agent::runtime::RuntimeControlError>>
        + Send
        + 'static,
{
    let runtime = app.runtime.clone();
    let events = app.runtime_events_tx.clone();
    tokio::spawn(async move {
        if let Err(error) = control(runtime).await {
            let _ = events.send(RuntimeEvent::ControlFailed {
                message: error.to_string(),
            });
        }
    });
}

fn format_help(registry: SlashCommandRegistry) -> String {
    let mut lines = vec!["local commands:".to_string()];
    for command in registry.iter() {
        lines.push(format!(
            "  {:<28} {}",
            command.spec.usage, command.spec.summary
        ));
    }
    lines.join("\n")
}

fn render_slash_error(app: &mut App, error: SlashError) {
    app.state.push_command_error(error.render());
}

fn restore_input(app: &mut App, text: &str) {
    for ch in text.chars() {
        app.input.insert_char(ch);
    }
}

fn tick_notifications(app: &mut App) -> DirtyState {
    if app.notifications.tick(Instant::now()) {
        DirtyState::frame()
    } else {
        DirtyState::none()
    }
}

fn push_notification(app: &mut App, kind: NotificationKind, message: impl Into<String>) {
    app.notifications.push(kind, message);
    app.mark_dirty(DirtyState::frame());
}

#[cfg(test)]
mod tests {
    use super::*;
    use copro_agent::AgentHistory;
    use copro_agent::{AgentTurnConfig, ToolExecutionPolicy, ToolRouter, async_trait};
    use copro_api::error::{Error, Result};
    use copro_api::message::{Message, ToolCall, ToolResult};
    use copro_api::request::GenerateRequest;
    use copro_api::response::FinishReason;
    use copro_api::stream::{Model, ModelStream, OutputContentDelta, OutputStreamEvent};
    use copro_api::tool::ToolDefinition;
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};
    use std::sync::Arc;

    struct FinishedModel;

    impl Model for FinishedModel {
        fn stream(&self, _request: GenerateRequest) -> ModelStream {
            Box::pin(futures_util::stream::iter(vec![Ok(
                OutputStreamEvent::Finished {
                    reason: FinishReason::Stop,
                    usage: None,
                },
            )]))
        }
    }

    struct PendingModel;

    impl Model for PendingModel {
        fn stream(&self, _request: GenerateRequest) -> ModelStream {
            Box::pin(futures_util::stream::pending())
        }
    }

    #[derive(Default)]
    struct NoopTools;

    #[async_trait]
    impl ToolRouter for NoopTools {
        async fn definitions(&self) -> Result<Vec<ToolDefinition>> {
            Ok(Vec::new())
        }

        async fn execute(
            &self,
            call: ToolCall,
            _cancel: copro_agent::CancellationToken,
        ) -> Result<ToolResult> {
            Err(Error::client(format!("unknown tool: {}", call.name)))
        }

        async fn execution_policy(&self, _call: &ToolCall) -> Result<ToolExecutionPolicy> {
            Ok(ToolExecutionPolicy::Serial)
        }
    }

    fn runtime(model: impl Model + 'static) -> AgentRuntime {
        AgentRuntime::new(
            AgentTurnConfig::default(),
            Arc::new(model),
            Arc::new(NoopTools),
        )
    }

    fn app_with(model: impl Model + 'static) -> App {
        App::new(runtime(model), "copro".to_string(), "gpt-test".to_string())
    }

    fn app_with_seed(model: impl Model + 'static, seed: AgentHistory) -> App {
        let runtime = AgentRuntime::new_with_history(
            AgentTurnConfig::default(),
            Arc::new(model),
            Arc::new(NoopTools),
            seed.clone(),
        );
        let mut runtime_config = RuntimeConfig::from_env();
        runtime_config.model_id = "gpt-test".to_string();
        App::new_with_runtime_config(
            runtime,
            "copro".to_string(),
            runtime_config,
            ImageRenderer::default(),
        )
    }

    fn insert_text(input: &mut InputEditor, text: &str) {
        for ch in text.chars() {
            input.insert_char(ch);
        }
    }

    fn buffer_text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .flat_map(|y| {
                (0..buffer.area.width)
                    .map(move |x| buffer[(x, y)].symbol())
                    .chain(std::iter::once("\n"))
            })
            .collect::<String>()
    }

    fn render_text(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| render_app(frame, app))
            .expect("render app");

        buffer_text(terminal.backend().buffer())
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    async fn wait_app_until_not_busy(app: &mut App) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while app.runtime.is_busy() {
                drain_runtime_events(app);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("app runtime stayed busy");
    }

    #[test]
    fn idle_frame_is_not_redrawn_after_initial_render() {
        let mut app = app_with(FinishedModel);
        let backend = TestBackend::new(30, 8);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        assert!(draw_if_dirty(&mut terminal, &mut app).expect("initial render"));
        assert!(!draw_if_dirty(&mut terminal, &mut app).expect("idle render"));
    }

    #[test]
    fn completed_image_prepare_marks_next_frame_dirty_once() {
        let mut app = app_with(FinishedModel);
        app.state.apply_delta(OutputContentDelta::Image(
            copro_api::message::ImageContent::Data {
                mime_type: "image/png".to_string(),
                data: vec![1, 2, 3].into(),
            },
        ));
        let backend = TestBackend::new(30, 18);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        assert!(draw_if_dirty(&mut terminal, &mut app).expect("initial image render"));
        assert!(app.image_renderer.has_in_flight());

        for _ in 0..100 {
            if draw_if_dirty(&mut terminal, &mut app).expect("image completion render") {
                assert!(!draw_if_dirty(&mut terminal, &mut app).expect("post image idle"));
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        panic!("image prepare job did not finish");
    }

    #[test]
    fn typing_marks_frame_dirty_without_rerendering_conversation() {
        let mut app = app_with(FinishedModel);

        let dirty = handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            20,
        );

        assert_eq!(dirty, DirtyState::frame());
    }

    #[test]
    fn scroll_marks_conversation_dirty() {
        let mut app = app_with(FinishedModel);

        let dirty = handle_key(
            &mut app,
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            20,
        );

        assert_eq!(dirty, DirtyState::conversation());
    }

    #[test]
    fn mouse_wheel_scrolls_conversation_area() {
        let mut app = app_with(FinishedModel);
        app.state.apply_delta(OutputContentDelta::Text(
            (0..20)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        render_text(&mut app, 40, 8);
        let area = conversation_selection_map(&app)
            .expect("conversation selection map")
            .area();

        let dirty = handle_mouse(&mut app, mouse(MouseEventKind::ScrollUp, area.x, area.y));

        assert_eq!(dirty, DirtyState::conversation());
        assert_eq!(app.conversation_scroll_from_bottom, MOUSE_SCROLL_ROWS);
    }

    #[test]
    fn mouse_wheel_at_top_clamps_without_overscroll() {
        let mut app = app_with(FinishedModel);
        app.state.apply_delta(OutputContentDelta::Text(
            (0..20)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        render_text(&mut app, 40, 8);
        let area = conversation_selection_map(&app)
            .expect("conversation selection map")
            .area();
        let max_scroll = conversation_selection_map(&app)
            .expect("conversation selection map")
            .max_viewport_start();

        for _ in 0..20 {
            handle_mouse(&mut app, mouse(MouseEventKind::ScrollUp, area.x, area.y));
        }

        assert_eq!(app.conversation_scroll_from_bottom, max_scroll);

        let dirty = handle_mouse(&mut app, mouse(MouseEventKind::ScrollUp, area.x, area.y));

        assert_eq!(dirty, DirtyState::none());
        assert_eq!(app.conversation_scroll_from_bottom, max_scroll);

        let dirty = handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown, area.x, area.y));

        assert_eq!(dirty, DirtyState::conversation());
        assert!(app.conversation_scroll_from_bottom < max_scroll);
    }

    #[test]
    fn mouse_wheel_at_bottom_does_not_dirty_conversation() {
        let mut app = app_with(FinishedModel);
        app.state.apply_delta(OutputContentDelta::Text(
            (0..20)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        render_text(&mut app, 40, 8);
        let area = conversation_selection_map(&app)
            .expect("conversation selection map")
            .area();

        let dirty = handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown, area.x, area.y));

        assert_eq!(dirty, DirtyState::none());
        assert_eq!(app.conversation_scroll_from_bottom, 0);
    }

    #[test]
    fn mouse_selection_release_copies_and_clears_selection() {
        let mut app = app_with(FinishedModel);
        app.state
            .apply_delta(OutputContentDelta::Text("alpha beta".to_string()));
        render_text(&mut app, 40, 8);
        let line = app
            .selection_manager
            .map_for(&AppSelectionSurface::Conversation)
            .expect("conversation selection map")
            .lines()
            .iter()
            .find(|line| line.text.contains("alpha"))
            .expect("alpha line is visible")
            .clone();
        let row = line.screen_y.expect("line has screen row");

        handle_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), line.x, row),
        );
        handle_mouse(
            &mut app,
            mouse(
                MouseEventKind::Drag(MouseButton::Left),
                line.x.saturating_add(5),
                row,
            ),
        );
        handle_mouse(
            &mut app,
            mouse(
                MouseEventKind::Up(MouseButton::Left),
                line.x.saturating_add(5),
                row,
            ),
        );

        assert_eq!(app.clipboard.last_written_text(), Some("alpha"));
        assert_eq!(app.notifications.current_message(), Some("copied"));
        assert!(
            app.selection_manager
                .selection_for(&AppSelectionSurface::Conversation)
                .is_none()
        );
        assert!(!app.selection_manager.is_dragging());
    }

    #[test]
    fn drag_below_conversation_autoscrolls_selection_toward_bottom() {
        let mut app = app_with(FinishedModel);
        app.state.apply_delta(OutputContentDelta::Text(
            (0..30)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        app.conversation_scroll_from_bottom = 10;
        render_text(&mut app, 40, 8);
        let map = conversation_selection_map(&app).expect("conversation selection map");
        let area = map.area();
        let line = app
            .selection_manager
            .map_for(&AppSelectionSurface::Conversation)
            .expect("conversation selection map")
            .lines()
            .iter()
            .find(|line| line.screen_y.is_some())
            .expect("visible line")
            .clone();
        let row = line.screen_y.expect("visible row");

        handle_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), line.x, row),
        );
        handle_mouse(
            &mut app,
            mouse(
                MouseEventKind::Drag(MouseButton::Left),
                line.x,
                area.bottom(),
            ),
        );
        let before = app.conversation_scroll_from_bottom;
        let dirty = tick_selection_autoscroll(&mut app);

        assert_eq!(dirty, DirtyState::conversation());
        assert_eq!(
            app.conversation_scroll_from_bottom,
            before.saturating_sub(1)
        );
        assert!(
            app.selection_manager
                .selection_for(&AppSelectionSurface::Conversation)
                .is_some()
        );
    }

    #[test]
    fn resize_marks_conversation_dirty() {
        let mut app = app_with(FinishedModel);
        app.render_dirty = false;
        app.conversation_dirty = false;

        let dirty = handle_event(&mut app, Event::Resize(80, 24), 20);

        assert_eq!(dirty, DirtyState::conversation());
        assert!(app.render_dirty);
        assert!(app.conversation_dirty);
    }

    #[test]
    fn new_app_starts_empty_with_runtime_context_and_no_notifications() {
        let app = app_with(FinishedModel);

        assert!(app.state.blocks().is_empty());
        assert_eq!(app.status.workspace, "copro");
        assert_eq!(app.status.model, "gpt-test");
        assert_eq!(app.status.context, "0%");
        assert_eq!(app.notifications.current_message(), None);
    }

    #[tokio::test]
    async fn enter_submits_to_runtime_and_appends_protocol_native_user_message() {
        let mut app = app_with(FinishedModel);
        insert_text(&mut app.input, "你好 cursor");

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            20,
        );

        assert!(matches!(
            app.state.blocks()[0].kind(),
            crate::tui::state::BlockKind::User { content }
                if content == &vec![InputContent::Text("你好 cursor".to_string())]
        ));
        assert_eq!(app.notifications.current_message(), None);
    }

    #[tokio::test]
    async fn double_slash_submits_escaped_user_text() {
        let mut app = app_with(FinishedModel);
        insert_text(&mut app.input, "//hello");

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            20,
        );

        assert!(matches!(
            app.state.blocks()[0].kind(),
            crate::tui::state::BlockKind::User { content }
                if content == &vec![InputContent::Text("/hello".to_string())]
        ));
    }

    #[test]
    fn help_command_renders_local_command_output() {
        let mut app = app_with(FinishedModel);
        insert_text(&mut app.input, "/help");

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            20,
        );

        assert!(matches!(
            app.state.blocks()[0].kind(),
            crate::tui::state::BlockKind::Command { text, is_error: false }
                if text.contains("/help") && text.contains("/clear")
        ));
    }

    #[test]
    fn unknown_slash_command_restores_input_and_renders_error() {
        let mut app = app_with(FinishedModel);
        insert_text(&mut app.input, "/wat");

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            20,
        );

        assert_eq!(app.input.text(), "/wat");
        assert!(matches!(
            app.state.blocks()[0].kind(),
            crate::tui::state::BlockKind::Command { text, is_error: true }
                if text == "unknown command: /wat"
        ));
    }

    #[test]
    fn clear_command_clears_session_history_and_visible_conversation() {
        let seed = AgentHistory::from_messages(vec![Message::developer(vec![InputContent::Text(
            "seed".to_string(),
        )])]);
        let mut app = app_with_seed(FinishedModel, seed);
        app.runtime
            .reset_history(AgentHistory::from_messages(vec![Message::user(vec![
                InputContent::Text("other".to_string()),
            ])]))
            .expect("reset to other history");
        app.state
            .apply_delta(OutputContentDelta::Text("visible".to_string()));
        insert_text(&mut app.input, "/clear");

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            20,
        );

        assert!(app.state.blocks().is_empty());
        assert_eq!(app.runtime.history(), Some(AgentHistory::default()));
    }

    #[test]
    fn model_command_without_args_renders_current_model() {
        let mut app = app_with(FinishedModel);
        insert_text(&mut app.input, "/model");

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            20,
        );

        assert!(matches!(
            app.state.blocks()[0].kind(),
            crate::tui::state::BlockKind::Command { text, is_error: false }
                if text == "model: gpt-test"
        ));
    }

    #[tokio::test]
    async fn busy_clear_is_rejected_without_clearing_visible_blocks() {
        let mut app = app_with(PendingModel);
        insert_text(&mut app.input, "first");
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            20,
        );
        insert_text(&mut app.input, "/clear");

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            20,
        );

        assert!(app.runtime.is_busy());
        assert!(matches!(
            app.state.blocks()[0].kind(),
            crate::tui::state::BlockKind::User { .. }
        ));
        assert!(matches!(
            app.state.blocks()[1].kind(),
            crate::tui::state::BlockKind::Command { text, is_error: true }
                if text == "runtime is busy"
        ));
    }

    #[test]
    fn stop_command_idle_renders_local_error() {
        let mut app = app_with(FinishedModel);
        insert_text(&mut app.input, "/stop");

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            20,
        );

        assert!(matches!(
            app.state.blocks()[0].kind(),
            crate::tui::state::BlockKind::Command { text, is_error: true }
                if text == "no active turn"
        ));
    }

    #[tokio::test]
    async fn stop_command_aborts_pending_turn_and_preserves_user_message() {
        let mut app = app_with(PendingModel);
        insert_text(&mut app.input, "question");
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            20,
        );
        insert_text(&mut app.input, "/stop");

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            20,
        );
        wait_app_until_not_busy(&mut app).await;

        let messages = app
            .runtime
            .history()
            .expect("runtime history")
            .messages()
            .to_vec();
        assert_eq!(
            messages.first(),
            Some(&Message::user(vec![InputContent::Text(
                "question".to_string()
            )]))
        );
    }

    #[test]
    fn command_menu_tab_accepts_without_submission() {
        let mut app = app_with(FinishedModel);
        insert_text(&mut app.input, "/mo");

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            20,
        );

        assert_eq!(app.input.text(), "/model ");
        assert!(app.state.blocks().is_empty());
    }

    #[test]
    fn command_menu_enter_accepts_and_submits() {
        let mut app = app_with(FinishedModel);
        app.state
            .apply_delta(OutputContentDelta::Text("visible".to_string()));
        insert_text(&mut app.input, "/cl");

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            20,
        );

        assert!(app.state.blocks().is_empty());
        assert_eq!(app.input.text(), "");
    }

    #[test]
    fn command_menu_down_changes_selected_command() {
        let mut app = app_with(FinishedModel);
        app.state
            .apply_delta(OutputContentDelta::Text("visible".to_string()));
        insert_text(&mut app.input, "/");
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            20,
        );

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            20,
        );

        assert!(app.state.blocks().is_empty());
    }

    #[test]
    fn command_menu_mouse_shields_conversation_scroll() {
        let mut app = app_with(FinishedModel);
        app.state.apply_delta(OutputContentDelta::Text(
            (0..20)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        insert_text(&mut app.input, "/");
        render_text(&mut app, 40, 10);
        let rect = app.command_menu.overlay_rect().expect("menu overlay rect");
        let before = app.conversation_scroll_from_bottom;

        let dirty = handle_mouse(&mut app, mouse(MouseEventKind::ScrollUp, rect.x, rect.y));

        assert_eq!(dirty, DirtyState::none());
        assert_eq!(app.conversation_scroll_from_bottom, before);
    }

    #[tokio::test]
    async fn short_busy_turn_stays_in_live_viewport() {
        let mut app = app_with(PendingModel);
        insert_text(&mut app.input, "question");
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            20,
        );

        assert_eq!(app.conversation_scroll_from_bottom, 0);
    }

    #[tokio::test]
    async fn busy_overflow_renders_latest_content_inside_fullscreen() {
        let mut app = app_with(PendingModel);
        insert_text(&mut app.input, "question");
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            40,
        );
        app.state.apply_delta(OutputContentDelta::Text(
            (0..12)
                .map(|index| format!("stream line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));

        let screen = render_text(&mut app, 40, 8);

        assert!(app.runtime.is_busy());
        assert_eq!(app.conversation_scroll_from_bottom, 0);
        assert!(screen.contains("stream line"));
    }

    #[test]
    fn finished_overflow_uses_app_managed_scroll_not_native_scrollback() {
        let mut app = app_with(FinishedModel);
        app.state.apply_delta(OutputContentDelta::Text(
            (0..20)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        let backend = TestBackend::new(30, 8);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| render_app(frame, &mut app))
            .expect("render app");

        let scrollback = buffer_text(terminal.backend().scrollback());
        let screen = buffer_text(terminal.backend().buffer());
        assert!(scrollback.trim().is_empty());
        assert!(screen.contains("line 19"));
    }

    #[test]
    fn page_up_renders_older_content_inside_fullscreen() {
        let mut app = app_with(FinishedModel);
        app.state.apply_delta(OutputContentDelta::Text(
            (0..20)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        let initial_screen = render_text(&mut app, 40, 8);

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            40,
        );
        let scrolled_screen = render_text(&mut app, 40, 8);

        assert!(initial_screen.contains("line 19"));
        assert_ne!(initial_screen, scrolled_screen);
        assert!(app.conversation_scroll_from_bottom > 0);
    }

    #[test]
    fn page_keys_adjust_fullscreen_scroll() {
        let mut app = app_with(FinishedModel);

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            20,
        );
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            20,
        );

        assert!(app.conversation_scroll_from_bottom > 0);

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL),
            20,
        );

        assert_eq!(app.conversation_scroll_from_bottom, 0);
    }

    #[test]
    fn ctrl_home_scrolls_beyond_u16_sized_history() {
        let mut app = app_with(FinishedModel);
        app.state.apply_delta(OutputContentDelta::Text(
            (0..70_000)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL),
            40,
        );
        let screen = render_text(&mut app, 40, 8);

        assert!(app.conversation_scroll_from_bottom > u16::MAX.into());
        assert!(screen.contains("line 0"));
        assert!(!screen.contains("line 69999"));
    }

    #[tokio::test]
    async fn submitting_new_message_returns_fullscreen_conversation_to_bottom() {
        let mut app = app_with(FinishedModel);
        app.state.apply_delta(OutputContentDelta::Text(
            (0..20)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            40,
        );
        insert_text(&mut app.input, "next question");

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            40,
        );

        assert_eq!(app.conversation_scroll_from_bottom, 0);
    }

    #[tokio::test]
    async fn busy_submit_restores_input_and_does_not_append_user_block() {
        let mut app = app_with(PendingModel);
        app.runtime
            .submit(
                InputMessage::User(vec![InputContent::Text("first".to_string())]),
                app.runtime_events_tx.clone(),
            )
            .unwrap();
        insert_text(&mut app.input, "second\nline");

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            20,
        );

        assert_eq!(app.input.text(), "second\nline");
        assert!(app.state.blocks().is_empty());
        assert_eq!(app.notifications.current_message(), Some("busy"));
    }

    #[tokio::test]
    async fn completed_turn_pending_ack_keeps_ui_submit_busy() {
        let mut app = app_with(FinishedModel);
        insert_text(&mut app.input, "first");

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            20,
        );

        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), app.runtime_events_rx.recv())
                .await
                .expect("runtime event timed out")
                .expect("runtime event channel closed");
            if matches!(event, RuntimeEvent::TurnFinished { .. }) {
                break;
            }
        }

        insert_text(&mut app.input, "second");
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            20,
        );

        assert_eq!(app.input.text(), "second");
        assert_eq!(app.state.blocks().len(), 1);
        assert_eq!(app.notifications.current_message(), Some("busy"));
    }

    #[test]
    fn turn_failed_event_appends_error_block() {
        let mut app = app_with(FinishedModel);

        app.runtime_events_tx
            .send(RuntimeEvent::TurnFailed {
                history: AgentHistory::default(),
                message: "client error: missing api key".to_string(),
            })
            .expect("runtime event sent");

        drain_runtime_events(&mut app);

        assert!(matches!(
            app.state.blocks()[0].kind(),
            crate::tui::state::BlockKind::Error { text }
                if text == "client error: missing api key"
        ));
        assert_eq!(
            app.notifications.current_message(),
            Some("client error: missing api key")
        );
        assert!(!app.runtime.is_busy());
    }

    #[test]
    fn ctrl_j_inserts_newline_without_submission() {
        let mut app = app_with(FinishedModel);
        app.input.insert_char('a');

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            20,
        );
        app.input.insert_char('b');

        assert_eq!(app.input.text(), "a\nb");
        assert!(app.state.blocks().is_empty());
    }

    #[test]
    fn escape_prefixed_enter_inserts_newline_without_submission() {
        let mut app = app_with(FinishedModel);
        app.input.insert_char('a');

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
            20,
        );
        app.input.insert_char('b');

        assert_eq!(app.input.text(), "a\nb");
        assert!(app.state.blocks().is_empty());
    }

    #[test]
    fn horizontal_arrow_keys_move_input_cursor() {
        let mut app = app_with(FinishedModel);
        insert_text(&mut app.input, "你a");

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            20,
        );
        assert_eq!(app.input.cursor(), 3);

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            20,
        );
        assert_eq!(app.input.cursor(), 0);

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            20,
        );
        assert_eq!(app.input.cursor(), 3);

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            20,
        );
        assert_eq!(app.input.cursor(), 4);
    }

    #[test]
    fn vertical_arrow_keys_move_input_cursor_between_input_rows() {
        let mut app = app_with(FinishedModel);
        insert_text(&mut app.input, "a\nb");

        handle_key(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), 20);
        assert_eq!(app.input.cursor(), 1);

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            20,
        );
        assert_eq!(app.input.cursor(), 3);
    }
}
