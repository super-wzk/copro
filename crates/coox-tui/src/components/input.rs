use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::Style,
    text::Line,
    widgets::{Block, Padding, Paragraph, StatefulWidgetRef, Widget},
};

use crate::text::display_width;

const MAX_INPUT_ROWS: usize = 14;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InputEditor {
    text: String,
    cursor: usize,
    cursor_visual_hint: Option<CursorVisualHint>,
}

impl InputEditor {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn cursor_visual_position(&self, width: usize) -> (usize, usize) {
        let (positions, _) = self.visual_positions(width);
        let position = self.position_for_cursor(&positions, width);

        (position.row, position.col)
    }

    pub fn insert_char(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.cursor_visual_hint = None;
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }

        if let Some((previous_cursor, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.text.drain(previous_cursor..self.cursor);
            self.cursor = previous_cursor;
            self.cursor_visual_hint = None;
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }

        if let Some((previous_cursor, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.cursor = previous_cursor;
            self.cursor_visual_hint = None;
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }

        self.cursor = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(offset, _)| self.cursor + offset)
            .unwrap_or(self.text.len());
        self.cursor_visual_hint = None;
    }

    pub fn move_up(&mut self, width: usize) {
        let (positions, _) = self.visual_positions(width);
        let position = self.position_for_cursor(&positions, width);

        if position.row == 0 {
            return;
        }

        if let Some(target) =
            Self::nearest_position_in_row(&positions, position.row - 1, position.col)
        {
            self.cursor = target.byte;
            self.cursor_visual_hint = Some(CursorVisualHint::new(width, target.row));
        }
    }

    pub fn move_down(&mut self, width: usize) {
        let (positions, row_count) = self.visual_positions(width);
        let position = self.position_for_cursor(&positions, width);
        let target_row = position.row + 1;

        if target_row >= row_count {
            return;
        }

        if let Some(target) = Self::nearest_position_in_row(&positions, target_row, position.col) {
            self.cursor = target.byte;
            self.cursor_visual_hint = Some(CursorVisualHint::new(width, target.row));
        }
    }

    pub fn take_submission(&mut self) -> Option<String> {
        if self.text.trim().is_empty() {
            return None;
        }

        self.cursor = 0;
        self.cursor_visual_hint = None;
        Some(std::mem::take(&mut self.text))
    }

    pub fn visual_rows(&self, width: usize) -> usize {
        let (_, row_count) = self.visual_positions(width);

        row_count.clamp(1, MAX_INPUT_ROWS)
    }

    pub fn visual_lines(&self, width: usize) -> Vec<String> {
        let (positions, row_count) = self.visual_positions(width);

        (0..row_count)
            .map(|row| {
                let mut row_positions = positions
                    .iter()
                    .filter(|position| position.row == row)
                    .map(|position| position.byte);

                let Some(start) = row_positions.next() else {
                    return String::new();
                };

                let end = row_positions.fold(start, usize::max);

                self.text[start..end].to_string()
            })
            .collect()
    }

    fn visual_positions(&self, width: usize) -> (Vec<VisualPosition>, usize) {
        let width = width.max(1);
        let mut positions = vec![VisualPosition {
            byte: 0,
            row: 0,
            col: 0,
        }];
        let mut row = 0;
        let mut row_start = 0;
        let mut col = 0;

        for (index, ch) in self.text.char_indices() {
            let end = index + ch.len_utf8();

            if ch == '\n' {
                row += 1;
                row_start = end;
                col = 0;
                positions.push(VisualPosition {
                    byte: end,
                    row,
                    col,
                });
                continue;
            }

            let next_col = display_width(&self.text[row_start..end]);
            if col > 0 && next_col > width {
                row += 1;
                row_start = index;
                col = 0;
                positions.push(VisualPosition {
                    byte: index,
                    row,
                    col,
                });
            }

            col = display_width(&self.text[row_start..end]);
            positions.push(VisualPosition {
                byte: end,
                row,
                col,
            });
        }

        (positions, row + 1)
    }

    fn position_for_cursor(&self, positions: &[VisualPosition], width: usize) -> VisualPosition {
        if let Some(position) = self.cursor_visual_hint.and_then(|hint| {
            (hint.width == width.max(1)).then(|| {
                positions
                    .iter()
                    .find(|position| position.byte == self.cursor && position.row == hint.row)
                    .copied()
            })?
        }) {
            return position;
        }

        positions
            .iter()
            .find(|position| position.byte == self.cursor)
            .copied()
            .or_else(|| {
                positions
                    .iter()
                    .rev()
                    .find(|position| position.byte < self.cursor)
                    .copied()
            })
            .unwrap_or_default()
    }

    fn nearest_position_in_row(
        positions: &[VisualPosition],
        row: usize,
        col: usize,
    ) -> Option<VisualPosition> {
        positions
            .iter()
            .filter(|position| position.row == row)
            .min_by_key(|position| {
                (
                    position.col.abs_diff(col),
                    usize::from(position.col > col),
                    position.byte,
                )
            })
            .copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CursorVisualHint {
    width: usize,
    row: usize,
}

impl CursorVisualHint {
    fn new(width: usize, row: usize) -> Self {
        Self {
            width: width.max(1),
            row,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct VisualPosition {
    byte: usize,
    row: usize,
    col: usize,
}

pub const INPUT_BOX_PADDING: Padding = Padding::new(1, 1, 1, 1);

#[derive(Debug, Clone, Copy)]
pub struct InputBox {
    style: Style,
    padding: Padding,
}

impl InputBox {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    pub fn padding(mut self, padding: Padding) -> Self {
        self.padding = padding;
        self
    }

    pub fn layout(&self, input: &InputEditor, area_width: u16) -> InputBoxLayout {
        InputBoxLayout::new(self.padding, input, area_width)
    }
}

impl Default for InputBox {
    fn default() -> Self {
        Self {
            style: Style::default(),
            padding: INPUT_BOX_PADDING,
        }
    }
}

impl StatefulWidgetRef for InputBox {
    type State = InputEditor;

    fn render_ref(&self, area: Rect, buf: &mut Buffer, input: &mut Self::State) {
        if area.is_empty() {
            return;
        }

        let layout = self.layout(input, area.width);
        Paragraph::new(
            input
                .visual_lines(layout.content_width())
                .into_iter()
                .map(Line::from)
                .collect::<Vec<_>>(),
        )
        .style(self.style)
        .block(Block::new().style(self.style).padding(self.padding))
        .render(area, buf);
    }
}

#[derive(Debug, Clone, Copy)]
pub struct InputBoxLayout {
    padding: Padding,
    area_width: u16,
    content_width: usize,
    render_height: u16,
    cursor_row: usize,
    cursor_col: usize,
}

impl InputBoxLayout {
    fn new(padding: Padding, input: &InputEditor, area_width: u16) -> Self {
        let content_width = Self::measure_content_width(padding, area_width);
        let render_height = Self::measure_render_height(padding, input, content_width);
        let (cursor_row, cursor_col) = input.cursor_visual_position(content_width);

        Self {
            padding,
            area_width,
            content_width,
            render_height,
            cursor_row,
            cursor_col,
        }
    }

    pub const fn area_width(self) -> u16 {
        self.area_width
    }

    pub const fn content_width(self) -> usize {
        self.content_width
    }

    pub const fn render_height(self) -> u16 {
        self.render_height
    }

    pub fn cursor_position(self, area: Rect) -> Option<Position> {
        debug_assert_eq!(
            area.width, self.area_width,
            "InputBoxLayout must be used with the same width it was measured for"
        );
        if area.is_empty() {
            return None;
        }

        let max_x = area.x.saturating_add(area.width.saturating_sub(1));
        let max_y = area.y.saturating_add(area.height.saturating_sub(1));
        let cursor_x = area
            .x
            .saturating_add(self.padding.left)
            .saturating_add(self.cursor_col as u16)
            .min(max_x);
        let cursor_y = area
            .y
            .saturating_add(self.padding.top)
            .saturating_add(self.cursor_row as u16)
            .min(max_y);

        Some(Position::new(cursor_x, cursor_y))
    }

    fn measure_content_width(padding: Padding, area_width: u16) -> usize {
        usize::from(area_width.saturating_sub(padding.left.saturating_add(padding.right)))
    }

    fn measure_render_height(padding: Padding, input: &InputEditor, content_width: usize) -> u16 {
        let height = input
            .visual_rows(content_width)
            .saturating_add(usize::from(padding.top.saturating_add(padding.bottom)));
        height.min(usize::from(u16::MAX)) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        Terminal, backend::TestBackend, buffer::Buffer, layout::Position, widgets::FrameExt,
    };

    #[test]
    fn input_box_layout_reports_shared_measurements() {
        let mut input = InputEditor::default();
        for ch in "abcde".chars() {
            input.insert_char(ch);
        }
        let input_box = InputBox::new();
        let layout = input_box.layout(&input, 6);

        assert_eq!(layout.area_width(), 6);
        assert_eq!(layout.content_width(), 4);
        assert_eq!(layout.render_height(), 4);
        assert_eq!(
            layout.cursor_position(Rect::new(10, 5, 6, 4)),
            Some(Position::new(12, 7))
        );
    }

    #[test]
    fn places_cursor_by_display_width_after_wide_text() {
        let mut input = InputEditor::default();
        input.insert_char('你');
        input.insert_char('a');
        let backend = TestBackend::new(12, 3);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| {
                let area = frame.area();
                let input_box = InputBox::new();
                let layout = input_box.layout(&input, area.width);
                frame.render_stateful_widget_ref(input_box, area, &mut input);
                frame.set_cursor_position(layout.cursor_position(area).expect("cursor"));
            })
            .expect("render input");

        terminal
            .backend_mut()
            .assert_cursor_position(Position::new(4, 1));
    }

    #[test]
    fn wraps_input_lines_with_cursor_on_continuation_row() {
        let mut input = InputEditor::default();
        for ch in "abcde".chars() {
            input.insert_char(ch);
        }
        let backend = TestBackend::new(6, 4);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| {
                let area = frame.area();
                let input_box = InputBox::new();
                let layout = input_box.layout(&input, area.width);
                frame.render_stateful_widget_ref(input_box, area, &mut input);
                frame.set_cursor_position(layout.cursor_position(area).expect("cursor"));
            })
            .expect("render input");

        let lines = buffer_lines(terminal.backend().buffer());
        assert_eq!(lines[0], "");
        assert_eq!(lines[1], " abcd");
        assert_eq!(lines[2], " e");
        terminal
            .backend_mut()
            .assert_cursor_position(Position::new(2, 2));
    }

    #[test]
    fn renders_hard_newline_without_continuation_gutter() {
        let mut input = InputEditor::default();
        for ch in "ab\nc".chars() {
            input.insert_char(ch);
        }
        let backend = TestBackend::new(8, 4);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| {
                let area = frame.area();
                let input_box = InputBox::new();
                let layout = input_box.layout(&input, area.width);
                frame.render_stateful_widget_ref(input_box, area, &mut input);
                frame.set_cursor_position(layout.cursor_position(area).expect("cursor"));
            })
            .expect("render input");

        let lines = buffer_lines(terminal.backend().buffer());
        assert_eq!(lines[0], "");
        assert_eq!(lines[1], " ab");
        assert_eq!(lines[2], " c");
        terminal
            .backend_mut()
            .assert_cursor_position(Position::new(2, 2));
    }

    #[test]
    fn clamps_cursor_inside_input_area() {
        let mut input = InputEditor::default();
        for ch in "abcdef".chars() {
            input.insert_char(ch);
        }
        let backend = TestBackend::new(4, 2);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| {
                let area = frame.area();
                let input_box = InputBox::new();
                let layout = input_box.layout(&input, area.width);
                frame.render_stateful_widget_ref(input_box, area, &mut input);
                frame.set_cursor_position(layout.cursor_position(area).expect("cursor"));
            })
            .expect("render input");

        terminal
            .backend_mut()
            .assert_cursor_position(Position::new(3, 1));
    }

    #[test]
    fn insert_and_submit_text() {
        let mut input = InputEditor::default();
        input.insert_char('h');
        input.insert_char('i');

        assert_eq!(input.take_submission(), Some("hi".to_string()));
        assert_eq!(input.text(), "");
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn empty_submission_returns_none() {
        let mut input = InputEditor::default();

        assert_eq!(input.take_submission(), None);
    }

    #[test]
    fn inserted_newline_is_part_of_submission() {
        let mut input = InputEditor::default();
        input.insert_char('a');
        input.insert_newline();
        input.insert_char('b');

        assert_eq!(input.take_submission(), Some("a\nb".to_string()));
    }

    #[test]
    fn backspace_removes_previous_scalar() {
        let mut input = InputEditor::default();
        input.insert_char('你');
        input.insert_char('a');
        input.backspace();

        assert_eq!(input.text(), "你");
    }

    #[test]
    fn visual_rows_wrap_by_display_width_and_cap_at_fourteen() {
        let mut input = InputEditor::default();
        for ch in "你".repeat(20).chars() {
            input.insert_char(ch);
        }

        assert_eq!(display_width(input.text()), 40);
        assert_eq!(input.visual_rows(4), 10);
        assert_eq!(input.visual_rows(1), MAX_INPUT_ROWS);
    }

    #[test]
    fn visual_lines_share_wrapping_with_cursor_positions() {
        let mut input = InputEditor::default();
        for ch in "abcde".chars() {
            input.insert_char(ch);
        }

        assert_eq!(input.visual_lines(4), vec!["abcd", "e"]);
        assert_eq!(input.cursor_visual_position(4), (1, 1));
    }

    #[test]
    fn visual_lines_split_hard_newlines_without_newline_bytes() {
        let mut input = InputEditor::default();
        for ch in "ab\ncd".chars() {
            input.insert_char(ch);
        }

        assert_eq!(input.visual_lines(10), vec!["ab", "cd"]);
        assert_eq!(input.cursor_visual_position(10), (1, 2));
    }

    #[test]
    fn unicode_text_wraps_without_counting_scalar_values_as_columns() {
        let mut input = InputEditor::default();
        for ch in "你a你a".chars() {
            input.insert_char(ch);
        }

        assert_eq!(input.visual_rows(3), 2);
        assert_eq!(input.visual_lines(3), vec!["你a", "你a"]);
    }

    #[test]
    fn horizontal_cursor_movement_uses_scalar_boundaries() {
        let mut input = InputEditor::default();
        input.insert_char('你');
        input.insert_char('a');

        assert_eq!(input.cursor(), 4);
        input.move_left();
        assert_eq!(input.cursor(), 3);
        assert_eq!(input.cursor_visual_position(10), (0, 2));

        input.move_left();
        assert_eq!(input.cursor(), 0);
        assert_eq!(input.cursor_visual_position(10), (0, 0));

        input.move_left();
        assert_eq!(input.cursor(), 0);

        input.move_right();
        assert_eq!(input.cursor(), 3);
        assert_eq!(input.cursor_visual_position(10), (0, 2));

        input.move_right();
        assert_eq!(input.cursor(), 4);
        assert_eq!(input.cursor_visual_position(10), (0, 3));

        input.move_right();
        assert_eq!(input.cursor(), 4);
    }

    #[test]
    fn vertical_cursor_movement_uses_wrapped_rows() {
        let mut input = InputEditor::default();
        for ch in "abcd".chars() {
            input.insert_char(ch);
        }

        assert_eq!(input.cursor_visual_position(2), (1, 2));
        input.move_up(2);
        assert_eq!(input.cursor_visual_position(2), (0, 2));
        assert_eq!(input.cursor(), 2);

        input.move_down(2);
        assert_eq!(input.cursor_visual_position(2), (1, 2));
        assert_eq!(input.cursor(), 4);
    }

    #[test]
    fn moving_to_wrapped_row_start_reports_row_start_position() {
        let mut input = InputEditor {
            text: "abcd".to_string(),
            cursor: 0,
            ..InputEditor::default()
        };

        assert_eq!(input.cursor_visual_position(2), (0, 0));
        assert_eq!(input.cursor(), 0);

        input.move_down(2);
        assert_eq!(input.cursor_visual_position(2), (1, 0));
        assert_eq!(input.cursor(), 2);
    }

    fn buffer_lines(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }
}
