use unicode_width::UnicodeWidthStr;

pub fn truncate_display_width(text: &str, max_width: usize) -> String {
    let mut best_end = 0;

    for (start, ch) in text.char_indices() {
        let end = start + ch.len_utf8();
        if UnicodeWidthStr::width(&text[..end]) <= max_width {
            best_end = end;
        }
    }

    text[..best_end].to_string()
}

pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_uses_display_width_for_wide_and_combining_text() {
        assert_eq!(display_width("你a"), 3);
        assert_eq!(display_width("e\u{301}"), 1);
        assert_eq!(truncate_display_width("你abc", 3), "你a");
    }

    #[test]
    fn truncate_uses_string_width_for_emoji_sequences() {
        let family = "👨\u{200d}👩\u{200d}👧\u{200d}👦";

        assert_eq!(display_width(family), UnicodeWidthStr::width(family));
        assert_eq!(UnicodeWidthStr::width(family), 2);
        assert_eq!(truncate_display_width(family, 2), family);
    }
}
