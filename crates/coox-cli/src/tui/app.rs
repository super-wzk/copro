use std::{
    io,
    time::{Duration, Instant},
};

use crate::agent::events::{apply_agent_event, apply_runtime_error};
use crate::agent::runtime::{AgentRuntime, RuntimeEvent, SubmitError};
use crate::tui::components::{
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
    runtime_events_tx: mpsc::UnboundedSender<RuntimeEvent>,
    runtime_events_rx: mpsc::UnboundedReceiver<RuntimeEvent>,
    image_renderer: ImageRenderer,
    state: AppState,
    conversation_scroll_from_bottom: u32,
    input: InputEditor,
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
        let (runtime_events_tx, runtime_events_rx) = mpsc::unbounded_channel();

        Self {
            runtime,
            runtime_events_tx,
            runtime_events_rx,
            image_renderer,
            state: AppState::default(),
            conversation_scroll_from_bottom: 0,
            input: InputEditor::default(),
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
                DirtyState::frame()
            }
            _ => DirtyState::none(),
        }
    };

    app.mark_dirty(dirty);
    dirty
}

fn submit_input(app: &mut App) {
    let Some(text) = app.input.take_submission() else {
        return;
    };
    let message = InputMessage::User(vec![InputContent::Text(text.clone())]);

    match app
        .runtime
        .submit(message.clone(), app.runtime_events_tx.clone())
    {
        Ok(()) => {
            app.state.push_input(message);
            app.conversation_scroll_from_bottom = 0;
        }
        Err(SubmitError::Busy) => {
            for ch in text.chars() {
                app.input.insert_char(ch);
            }
            push_notification(app, NotificationKind::Warning, "busy");
        }
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
    use copro_api::message::{ToolCall, ToolResult};
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
