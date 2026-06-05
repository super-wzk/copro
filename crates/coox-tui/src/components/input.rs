use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::Style,
    text::Line,
    widgets::{Block, Padding, Paragraph, StatefulWidgetRef, Widget},
};

use crate::selection::{
    CopySeparator, Selection, SelectionCell, SelectionMap, SelectionRow, SelectionRowContent,
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
    selection: Option<Selection>,
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

    pub fn selection(mut self, selection: Option<Selection>) -> Self {
        self.selection = selection;
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
            selection: None,
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

        if let Some(selection) = self.selection {
            layout
                .selection_map(input, area)
                .apply_selection_highlight(buf, selection);
        }
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

    pub fn selection_map(self, input: &InputEditor, area: Rect) -> SelectionMap {
        debug_assert_eq!(
            area.width, self.area_width,
            "InputBoxLayout must be used with the same width it was measured for"
        );

        let content_area = self.content_area(area);
        let rows = input.selection_rows(self.content_width);
        let mut map = SelectionMap::new(content_area, 0, rows.len() as u32);
        if content_area.is_empty() {
            return map;
        }

        for (index, row) in rows.into_iter().enumerate() {
            let y = index as u32;
            let screen_y = (index < usize::from(content_area.height))
                .then(|| content_area.y.saturating_add(index as u16));
            map.push_line(SelectionRow::new(
                content_area.x,
                y,
                screen_y,
                content_area.width,
                SelectionRowContent::new(
                    row.text_width.min(content_area.width),
                    row.text,
                    row.copy_separator,
                    row.cells,
                ),
            ));
        }

        map
    }

    fn content_area(self, area: Rect) -> Rect {
        if area.is_empty() {
            return Rect::default();
        }

        let width = area
            .width
            .saturating_sub(self.padding.left.saturating_add(self.padding.right));
        let height = area
            .height
            .saturating_sub(self.padding.top.saturating_add(self.padding.bottom));

        Rect::new(
            area.x.saturating_add(self.padding.left),
            area.y.saturating_add(self.padding.top),
            width,
            height,
        )
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct InputSelectionRow {
    text_width: u16,
    text: String,
    copy_separator: CopySeparator,
    cells: Vec<SelectionCell>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InputSelectionCell {
    column: u16,
    width: u16,
    text: String,
}

impl InputEditor {
    fn selection_rows(&self, width: usize) -> Vec<InputSelectionRow> {
        let width = width.max(1);
        let mut rows = Vec::new();
        let mut row_start = 0;
        let mut col = 0;
        let mut cells = Vec::new();

        for (index, ch) in self.text.char_indices() {
            let end = index + ch.len_utf8();

            if ch == '\n' {
                rows.push(InputSelectionRow::new(
                    &self.text[row_start..index],
                    CopySeparator::HardLine,
                    cells,
                ));
                row_start = end;
                col = 0;
                cells = Vec::new();
                continue;
            }

            let next_col = display_width(&self.text[row_start..end]);
            if col > 0 && next_col > width {
                rows.push(InputSelectionRow::new(
                    &self.text[row_start..index],
                    CopySeparator::None,
                    cells,
                ));
                row_start = index;
                col = 0;
                cells = Vec::new();
            }

            let next_col = display_width(&self.text[row_start..end]);
            push_selection_cell(&mut cells, ch, col, next_col);
            col = next_col;
        }

        rows.push(InputSelectionRow::new(
            &self.text[row_start..],
            CopySeparator::None,
            cells,
        ));
        rows
    }
}

impl InputSelectionRow {
    fn new(
        text: &str,
        copy_separator: CopySeparator,
        cells: Vec<InputSelectionCell>,
    ) -> InputSelectionRow {
        InputSelectionRow {
            text_width: display_width(text).min(usize::from(u16::MAX)) as u16,
            text: text.to_string(),
            copy_separator,
            cells: cells
                .into_iter()
                .map(|cell| SelectionCell::new(cell.column, cell.width, cell.text))
                .collect(),
        }
    }
}

fn push_selection_cell(cells: &mut Vec<InputSelectionCell>, ch: char, col: usize, next_col: usize) {
    let width = next_col.saturating_sub(col);
    if width == 0 {
        if let Some(previous) = cells.last_mut() {
            previous.text.push(ch);
        }
        return;
    }

    cells.push(InputSelectionCell {
        column: col.min(usize::from(u16::MAX)) as u16,
        width: width.min(usize::from(u16::MAX)) as u16,
        text: ch.to_string(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        Terminal, backend::TestBackend, buffer::Buffer, layout::Position, style::Modifier,
        widgets::FrameExt,
    };

    use crate::selection::{Selection, TextPosition};

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

    #[test]
    fn selection_map_rejoins_soft_wrapped_input_without_newline() {
        let mut input = InputEditor::default();
        for ch in "abcde".chars() {
            input.insert_char(ch);
        }
        let input_box = InputBox::new();
        let layout = input_box.layout(&input, 6);
        let map = layout.selection_map(&input, Rect::new(0, 0, 6, 4));

        assert_eq!(
            map.lines()
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["abcd", "e"]
        );
        assert_eq!(map.lines()[0].copy_separator, CopySeparator::None);
        assert_eq!(map.copy_visible_text(), "abcde");
    }

    #[test]
    fn selection_map_preserves_hard_newlines() {
        let mut input = InputEditor::default();
        for ch in "ab\ncd".chars() {
            input.insert_char(ch);
        }
        let input_box = InputBox::new();
        let layout = input_box.layout(&input, 10);
        let map = layout.selection_map(&input, Rect::new(0, 0, 10, 4));

        assert_eq!(
            map.lines()
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["ab", "cd"]
        );
        assert_eq!(map.lines()[0].copy_separator, CopySeparator::HardLine);
        assert_eq!(map.copy_visible_text(), "ab\ncd");
    }

    #[test]
    fn selection_map_uses_display_width_for_wide_text() {
        let mut input = InputEditor::default();
        for ch in "你a".chars() {
            input.insert_char(ch);
        }
        let input_box = InputBox::new();
        let layout = input_box.layout(&input, 10);
        let map = layout.selection_map(&input, Rect::new(0, 0, 10, 3));
        let line = &map.lines()[0];

        let selection = Selection::new(
            TextPosition::new(line.x, line.y),
            TextPosition::new(line.x + 2, line.y),
        );

        assert_eq!(map.copy_selection(selection), "你");
    }

    #[test]
    fn selection_map_clamps_wide_text_width_to_content_area() {
        let mut input = InputEditor::default();
        input.insert_char('你');
        let input_box = InputBox::new();
        let layout = input_box.layout(&input, 3);
        let map = layout.selection_map(&input, Rect::new(0, 0, 3, 3));
        let line = &map.lines()[0];
        let selection = Selection::new(
            TextPosition::new(line.x, line.y),
            TextPosition::new(line.end_x(), line.y),
        );
        let backend = TestBackend::new(3, 3);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_stateful_widget_ref(
                    input_box.selection(Some(selection)),
                    area,
                    &mut input,
                );
            })
            .expect("render input");

        assert_eq!(line.end_x(), line.x + 1);
        assert_eq!(map.copy_selection(selection), "你");
    }

    #[test]
    fn input_box_highlights_selection() {
        let mut input = InputEditor::default();
        for ch in "select".chars() {
            input.insert_char(ch);
        }
        let input_box = InputBox::new();
        let layout = input_box.layout(&input, 10);
        let map = layout.selection_map(&input, Rect::new(0, 0, 10, 3));
        let line = &map.lines()[0];
        let selection = Selection::new(
            TextPosition::new(line.x + 1, line.y),
            TextPosition::new(line.x + 4, line.y),
        );
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_stateful_widget_ref(
                    input_box.selection(Some(selection)),
                    area,
                    &mut input,
                );
            })
            .expect("render input");

        let buffer = terminal.backend().buffer();
        let screen_y = line.screen_y.expect("visible row");
        assert!(
            buffer[(line.x + 1, screen_y)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            !buffer[(line.x, screen_y)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
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
