use copro_agent::{CancellationToken, ToolRouter};
use copro_api::message::{InputContent, ToolCall, ToolResult, ToolResultStatus};
use copro_harness::tools::{
    ErasedTool, LocalToolRouter, ToolSlots, ToolUpdate, ToolUpdatePayload, ToolUpdateSlot,
};
use copro_workspace::tools::{GrepMatchFound, GrepProgress, GrepTool};
use crossterm::event::{self, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::io::AsyncWriteExt;
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, Paragraph, Wrap};
use serde_json::{Value, json};
use std::env;
use std::error::Error as StdError;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use vfs::async_vfs::{AsyncMemoryFS, AsyncVfsPath};

const MAX_UPDATES_PER_FRAME: usize = 24;

#[tokio::main]
async fn main() -> Result<(), Box<dyn StdError>> {
    let mut args = env::args().skip(1);
    let pattern = args.next().unwrap_or_else(|| "TODO".to_string());
    let path = args.next().unwrap_or_else(|| "src".to_string());

    let root: AsyncVfsPath = AsyncMemoryFS::new().into();
    seed_workspace(&root).await?;

    let (tx, rx) = mpsc::channel(64);
    let slots = ToolSlots::new().with(ToolUpdateSlot::new(move |update| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(update).await;
        }
    }));
    let tool: Arc<dyn ErasedTool> = Arc::new(GrepTool::new(root));
    let router = LocalToolRouter::new(vec![tool]).with_slots(slots);
    let cancel = CancellationToken::new();
    let search = tokio::spawn(execute_grep(
        router,
        grep_call(&pattern, &path),
        cancel.clone(),
    ));

    let mut terminal = init_terminal()?;
    let result = run_app(&mut terminal, App::new(pattern, path), rx, search, cancel).await;
    restore_terminal(&mut terminal)?;
    result
}

async fn execute_grep(
    router: LocalToolRouter,
    call: ToolCall,
    cancel: CancellationToken,
) -> copro_api::error::Result<ToolResult> {
    router.execute(call, cancel).await
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut app: App,
    mut updates: mpsc::Receiver<ToolUpdate>,
    mut search: tokio::task::JoinHandle<copro_api::error::Result<ToolResult>>,
    cancel: CancellationToken,
) -> Result<(), Box<dyn StdError>> {
    loop {
        app.drain_updates(&mut updates);

        if search.is_finished() && !app.finished {
            match (&mut search).await {
                Ok(Ok(result)) => app.finish(result),
                Ok(Err(error)) => app.finish_error(error.to_string()),
                Err(error) => app.finish_error(error.to_string()),
            }
        }

        terminal.draw(|frame| draw(frame, &app))?;

        if event::poll(Duration::from_millis(33))?
            && let Event::Key(key) = event::read()?
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        {
            cancel.cancel();
            search.abort();
            break;
        }

        if app.finished
            && app
                .finished_at
                .is_some_and(|finished_at| finished_at.elapsed() >= Duration::from_secs(4))
        {
            break;
        }
    }

    Ok(())
}

fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(7),
        ])
        .split(area);

    let title = format!(
        "grep progress | pattern: {} | path: {} | press q to quit",
        app.pattern, app.path
    );
    let status_style = if app.finished {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    };
    let header = Paragraph::new(Line::from(vec![
        Span::styled(app.status_text(), status_style),
        Span::raw(format!(
            " | elapsed: {:.1}s",
            app.started_at.elapsed().as_secs_f32()
        )),
    ]))
    .block(Block::default().title(title).borders(Borders::ALL));
    frame.render_widget(header, chunks[0]);

    let ratio = if app.finished {
        1.0
    } else {
        (app.searched_files % 100) as f64 / 100.0
    };
    let gauge = Gauge::default()
        .block(Block::default().title("activity").borders(Borders::ALL))
        .gauge_style(Style::default().fg(Color::Magenta))
        .label(format!(
            "searched {} files, {} matched files, {} updates",
            app.searched_files, app.matched_files, app.update_count
        ))
        .ratio(ratio);
    frame.render_widget(gauge, chunks[1]);

    let current = app
        .current_path
        .as_deref()
        .unwrap_or("waiting for grep.progress");
    let current_path = Paragraph::new(current.to_string())
        .block(Block::default().title("current path").borders(Borders::ALL));
    frame.render_widget(current_path, chunks[2]);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(chunks[3]);

    let recent_matches = app
        .matches
        .iter()
        .rev()
        .take(12)
        .map(|mat| {
            let line = mat
                .line_number
                .map(|line| format!("line {line}"))
                .unwrap_or_else(|| format!("byte {}", mat.byte_offset));
            ListItem::new(Line::from(vec![
                Span::styled(mat.path.clone(), Style::default().fg(Color::Yellow)),
                Span::raw(format!(" | {line} | {} line(s)", mat.line_count)),
            ]))
        })
        .collect::<Vec<_>>();
    let matches = if recent_matches.is_empty() {
        List::new(vec![ListItem::new("no grep.match_found updates yet")])
    } else {
        List::new(recent_matches)
    }
    .block(
        Block::default()
            .title("recent matches")
            .borders(Borders::ALL),
    );
    frame.render_widget(matches, body[0]);

    let notes = Paragraph::new(app.notes())
        .block(
            Block::default()
                .title("structured update state")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(notes, body[1]);

    let output = Paragraph::new(app.output_preview())
        .block(
            Block::default()
                .title("final tool output")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(output, chunks[4]);
}

struct App {
    pattern: String,
    path: String,
    started_at: Instant,
    finished_at: Option<Instant>,
    finished: bool,
    success: Option<bool>,
    searched_files: usize,
    matched_files: usize,
    current_path: Option<String>,
    update_count: usize,
    matches: Vec<GrepMatchFound>,
    errors: Vec<String>,
    final_output: Option<String>,
}

impl App {
    fn new(pattern: String, path: String) -> Self {
        Self {
            pattern,
            path,
            started_at: Instant::now(),
            finished_at: None,
            finished: false,
            success: None,
            searched_files: 0,
            matched_files: 0,
            current_path: None,
            update_count: 0,
            matches: Vec::new(),
            errors: Vec::new(),
            final_output: None,
        }
    }

    fn drain_updates(&mut self, updates: &mut mpsc::Receiver<ToolUpdate>) {
        for _ in 0..MAX_UPDATES_PER_FRAME {
            match updates.try_recv() {
                Ok(update) => self.apply_update(update),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
    }

    fn apply_update(&mut self, update: ToolUpdate) {
        self.update_count += 1;
        match update.kind.as_str() {
            <GrepProgress as ToolUpdatePayload>::KIND => {
                match serde_json::from_value::<GrepProgress>(update.payload) {
                    Ok(progress) => {
                        self.searched_files = progress.searched_files;
                        self.matched_files = progress.matched_files;
                        self.current_path = progress.current_path;
                    }
                    Err(error) => self
                        .errors
                        .push(format!("bad grep.progress payload: {error}")),
                }
            }
            <GrepMatchFound as ToolUpdatePayload>::KIND => {
                match serde_json::from_value::<GrepMatchFound>(update.payload) {
                    Ok(match_found) => {
                        self.matches.push(match_found);
                        let overflow = self.matches.len().saturating_sub(200);
                        if overflow > 0 {
                            self.matches.drain(0..overflow);
                        }
                    }
                    Err(error) => self
                        .errors
                        .push(format!("bad grep.match_found payload: {error}")),
                }
            }
            kind => self.errors.push(format!("unknown update kind: {kind}")),
        }
    }

    fn finish(&mut self, result: ToolResult) {
        self.finished = true;
        self.finished_at = Some(Instant::now());
        self.success = Some(result.status == ToolResultStatus::Success);
        self.final_output = Some(input_content_text(&result.content));
    }

    fn finish_error(&mut self, error: String) {
        self.finished = true;
        self.finished_at = Some(Instant::now());
        self.success = Some(false);
        self.final_output = Some(error);
    }

    fn status_text(&self) -> &'static str {
        match (self.finished, self.success) {
            (false, _) => "running",
            (true, Some(true)) => "finished",
            (true, Some(false)) => "failed",
            (true, None) => "finished",
        }
    }

    fn notes(&self) -> String {
        let mut lines = vec![
            format!("grep.progress searched_files: {}", self.searched_files),
            format!("grep.progress matched_files: {}", self.matched_files),
            format!("grep.match_found count: {}", self.matches.len()),
            format!("raw ToolUpdate count: {}", self.update_count),
        ];
        if !self.errors.is_empty() {
            lines.push("".to_string());
            lines.push("decode errors:".to_string());
            lines.extend(self.errors.iter().rev().take(4).cloned());
        }
        lines.join("\n")
    }

    fn output_preview(&self) -> String {
        self.final_output
            .as_ref()
            .map(|output| output.lines().take(5).collect::<Vec<_>>().join("\n"))
            .unwrap_or_else(|| "waiting for final ToolResult".to_string())
    }
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>, Box<dyn StdError>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> Result<(), Box<dyn StdError>> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn grep_call(pattern: &str, path: &str) -> ToolCall {
    let Value::Object(arguments) = json!({
        "pattern": pattern,
        "path": path,
        "output_mode": "content",
        "head_limit": 80
    }) else {
        unreachable!("grep call arguments must be an object")
    };

    ToolCall {
        id: "grep-progress-demo".into(),
        name: "grep".to_string(),
        arguments,
    }
}

async fn seed_workspace(root: &AsyncVfsPath) -> Result<(), Box<dyn StdError>> {
    write_file(root, ".gitignore", b"target/\n*.tmp\n").await?;
    for dir in 0..32 {
        for file in 0..20 {
            let path = format!("src/module_{dir:02}/file_{file:02}.rs");
            let has_match = (dir + file) % 4 == 0;
            let body = if has_match {
                format!(
                    "pub fn item_{dir}_{file}() {{\n    // TODO: wire module {dir} file {file}\n    let value = {dir} + {file};\n    println!(\"{{value}}\");\n}}\n"
                )
            } else {
                format!(
                    "pub fn item_{dir}_{file}() -> usize {{\n    let value = {dir} * {file};\n    value\n}}\n"
                )
            };
            write_file(root, &path, body.as_bytes()).await?;
        }
    }
    Ok(())
}

async fn write_file(
    root: &AsyncVfsPath,
    path: &str,
    bytes: &[u8],
) -> Result<(), Box<dyn StdError>> {
    let path = root.join(path)?;
    path.parent().create_dir_all().await?;
    path.create_file().await?.write_all(bytes).await?;
    Ok(())
}

fn input_content_text(content: &[InputContent]) -> String {
    content
        .iter()
        .map(|content| match content {
            InputContent::Text(text) => text.clone(),
            InputContent::Image(image) => format!("[image: {image:?}]"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}
