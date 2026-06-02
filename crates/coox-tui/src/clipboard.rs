use std::fmt;

pub struct ClipboardHandler {
    clipboard: Option<arboard::Clipboard>,
    #[cfg(any(test, feature = "test-support"))]
    written_text: Vec<String>,
}

impl ClipboardHandler {
    pub const fn new() -> Self {
        Self {
            clipboard: None,
            #[cfg(any(test, feature = "test-support"))]
            written_text: Vec::new(),
        }
    }

    pub fn write_text(&mut self, text: &str) -> Result<(), String> {
        #[cfg(any(test, feature = "test-support"))]
        {
            self.written_text.push(text.to_owned());
            Ok(())
        }

        #[cfg(not(any(test, feature = "test-support")))]
        {
            let clipboard = self.clipboard()?;
            clipboard
                .set_text(text.to_owned())
                .map_err(|error| format!("failed to write clipboard text: {error}"))
        }
    }

    #[cfg(not(any(test, feature = "test-support")))]
    fn clipboard(&mut self) -> Result<&mut arboard::Clipboard, String> {
        if self.clipboard.is_none() {
            self.clipboard = Some(
                arboard::Clipboard::new()
                    .map_err(|error| format!("failed to access clipboard: {error}"))?,
            );
        }

        Ok(self
            .clipboard
            .as_mut()
            .expect("clipboard initialized before returning"))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn last_written_text(&self) -> Option<&str> {
        self.written_text.last().map(String::as_str)
    }
}

impl Default for ClipboardHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ClipboardHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClipboardHandler")
            .field("clipboard_initialized", &self.clipboard.is_some())
            .field("written_text_count", &self.written_text_count())
            .finish()
    }
}

impl ClipboardHandler {
    fn written_text_count(&self) -> usize {
        #[cfg(any(test, feature = "test-support"))]
        {
            self.written_text.len()
        }
        #[cfg(not(any(test, feature = "test-support")))]
        {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ClipboardHandler;

    #[test]
    fn clipboard_handler_starts_without_opening_platform_clipboard() {
        let handler = ClipboardHandler::new();

        assert!(handler.clipboard.is_none());
    }

    #[test]
    fn default_handler_is_lazy() {
        let handler = ClipboardHandler::default();

        assert!(handler.clipboard.is_none());
    }
}
