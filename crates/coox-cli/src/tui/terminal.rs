use std::io;

use coox_tui::components::image::ImageRenderer;
use crossterm::{
    cursor::{DisableBlinking, SetCursorStyle},
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

pub struct Tui {
    pub terminal: Terminal<CrosstermBackend<io::Stdout>>,
    pub image_renderer: ImageRenderer,
}

impl Tui {
    pub fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = enter_terminal_screen(&mut stdout) {
            let _ = restore_terminal();
            return Err(error);
        }

        let image_renderer = ImageRenderer::from_terminal_query_or_halfblocks();
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = restore_terminal();
                return Err(error);
            }
        };
        if let Err(error) = terminal.clear() {
            let _ = restore_terminal();
            return Err(error);
        }

        Ok(Self {
            terminal,
            image_renderer,
        })
    }
}

fn enter_terminal_screen(writer: &mut impl io::Write) -> io::Result<()> {
    execute!(
        writer,
        EnterAlternateScreen,
        EnableMouseCapture,
        DisableBlinking
    )
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = restore_terminal();
    }
}

fn restore_terminal() -> io::Result<()> {
    let raw_result = disable_raw_mode();
    let mut stdout = io::stdout();
    let terminal_result = restore_terminal_screen(&mut stdout);

    raw_result?;
    terminal_result
}

fn restore_terminal_screen(writer: &mut impl io::Write) -> io::Result<()> {
    execute!(
        writer,
        SetCursorStyle::DefaultUserShape,
        DisableMouseCapture,
        LeaveAlternateScreen
    )
}

#[cfg(test)]
mod tests {
    use super::{enter_terminal_screen, restore_terminal_screen};

    #[test]
    fn terminal_module_does_not_configure_inline_viewport() {
        let source = include_str!("terminal.rs");
        let production_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source before tests");

        assert!(!production_source.contains("TerminalOptions"));
        assert!(!production_source.contains("Viewport::Inline"));
        assert!(!production_source.contains("insert_before"));
    }

    #[test]
    fn terminal_enter_sequence_enables_mouse_capture() {
        let mut output = Vec::new();

        enter_terminal_screen(&mut output).expect("terminal enter sequence writes");
        let output = String::from_utf8(output).expect("terminal commands are utf-8 ansi");

        assert!(output.contains("\u{1b}[?1000h"));
        assert!(output.contains("\u{1b}[?1002h"));
        assert!(output.contains("\u{1b}[?1003h"));
        assert!(output.contains("\u{1b}[?1006h"));
    }

    #[test]
    fn terminal_restore_sequence_disables_mouse_capture() {
        let mut output = Vec::new();

        restore_terminal_screen(&mut output).expect("terminal restore sequence writes");
        let output = String::from_utf8(output).expect("terminal commands are utf-8 ansi");

        assert!(output.contains("\u{1b}[?1000l"));
        assert!(output.contains("\u{1b}[?1002l"));
        assert!(output.contains("\u{1b}[?1003l"));
        assert!(output.contains("\u{1b}[?1006l"));
    }
}
