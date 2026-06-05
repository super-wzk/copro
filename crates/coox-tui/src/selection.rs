use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextPosition {
    pub x: u16,
    pub y: u32,
}

impl TextPosition {
    pub const fn new(x: u16, y: u32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: TextPosition,
    pub focus: TextPosition,
}

impl Selection {
    pub const fn new(anchor: TextPosition, focus: TextPosition) -> Self {
        Self { anchor, focus }
    }

    fn normalized(self) -> (TextPosition, TextPosition) {
        if position_before_or_equal(self.anchor, self.focus) {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopySeparator {
    None,
    SoftWrap(String),
    HardLine,
}

impl CopySeparator {
    pub fn as_str(&self) -> &str {
        match self {
            Self::None => "",
            Self::SoftWrap(separator) => separator,
            Self::HardLine => "\n",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectionCell {
    column: u16,
    width: u16,
    text: String,
}

impl SelectionCell {
    pub fn new(column: u16, width: u16, text: impl Into<String>) -> Self {
        Self {
            column,
            width,
            text: text.into(),
        }
    }

    pub fn column(&self) -> u16 {
        self.column
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    fn overlaps(&self, start: u16, end: u16) -> bool {
        self.column < end && self.column.saturating_add(self.width) > start
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectionRow {
    pub x: u16,
    pub y: u32,
    pub screen_y: Option<u16>,
    pub width: u16,
    pub text_width: u16,
    pub text: String,
    pub copy_separator: CopySeparator,
    copy_group: Option<usize>,
    copy_text: Option<String>,
    cells: Vec<SelectionCell>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectionRowContent {
    pub text_width: u16,
    pub text: String,
    pub copy_separator: CopySeparator,
    pub cells: Vec<SelectionCell>,
}

impl SelectionRowContent {
    pub fn new(
        text_width: u16,
        text: impl Into<String>,
        copy_separator: CopySeparator,
        cells: Vec<SelectionCell>,
    ) -> Self {
        Self {
            text_width,
            text: text.into(),
            copy_separator,
            cells,
        }
    }
}

impl SelectionRow {
    pub fn new(
        x: u16,
        y: u32,
        screen_y: Option<u16>,
        width: u16,
        content: SelectionRowContent,
    ) -> Self {
        Self {
            x,
            y,
            screen_y,
            width,
            text_width: content.text_width,
            text: content.text,
            copy_separator: content.copy_separator,
            copy_group: None,
            copy_text: None,
            cells: content.cells,
        }
    }

    pub fn with_copy_group(mut self, copy_group: usize) -> Self {
        self.copy_group = Some(copy_group);
        self
    }

    pub fn with_copy_text(mut self, copy_text: impl Into<String>) -> Self {
        self.copy_text = Some(copy_text.into());
        self
    }

    pub fn copy_group(&self) -> Option<usize> {
        self.copy_group
    }

    pub fn cells(&self) -> &[SelectionCell] {
        &self.cells
    }

    pub fn end_x(&self) -> u16 {
        self.x.saturating_add(self.text_width)
    }

    fn clamped_position(&self, x: u16) -> TextPosition {
        TextPosition::new(x.clamp(self.x, self.end_x()), self.y)
    }

    fn local_column(&self, x: u16) -> u16 {
        x.saturating_sub(self.x).min(self.text_width)
    }

    fn copy_range(&self, start: u16, end: u16) -> String {
        if start >= end {
            return String::new();
        }

        if let Some(text) = &self.copy_text {
            return text.clone();
        }

        self.cells
            .iter()
            .filter(|cell| cell.overlaps(start, end))
            .map(|cell| cell.text.as_str())
            .collect()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectionMap {
    area: Rect,
    viewport_start: u32,
    total_height: u32,
    lines: Vec<SelectionRow>,
    visible_line_indices: Vec<usize>,
}

impl SelectionMap {
    pub fn new(area: Rect, viewport_start: u32, total_height: u32) -> Self {
        Self {
            area,
            viewport_start,
            total_height,
            lines: Vec::new(),
            visible_line_indices: Vec::new(),
        }
    }

    pub fn area(&self) -> Rect {
        self.area
    }

    pub fn lines(&self) -> &[SelectionRow] {
        &self.lines
    }

    pub fn push_line(&mut self, line: SelectionRow) {
        if line.screen_y.is_some() {
            self.visible_line_indices.push(self.lines.len());
        }
        self.lines.push(line);
    }

    pub fn viewport_start(&self) -> u32 {
        self.viewport_start
    }

    pub fn max_viewport_start(&self) -> u32 {
        self.total_height
            .saturating_sub(u32::from(self.area.height))
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn contains_point(&self, x: u16, y: u16) -> bool {
        x >= self.area.x
            && x < self.area.x.saturating_add(self.area.width)
            && y >= self.area.y
            && y < self.area.y.saturating_add(self.area.height)
    }

    pub fn position_at(&self, x: u16, y: u16) -> Option<TextPosition> {
        self.line_at_screen_y(y)
            .map(|line| line.clamped_position(x))
    }

    pub fn nearest_position(&self, x: u16, y: u16) -> Option<TextPosition> {
        let line = self.line_at_screen_y(y).or_else(|| {
            self.visible_lines()
                .min_by_key(|line| line.screen_y.unwrap_or_default().abs_diff(y))
        })?;

        Some(line.clamped_position(x))
    }

    pub fn selection_for_points(
        &self,
        anchor_x: u16,
        anchor_y: u16,
        focus_x: u16,
        focus_y: u16,
    ) -> Option<Selection> {
        Some(Selection::new(
            self.nearest_position(anchor_x, anchor_y)?,
            self.nearest_position(focus_x, focus_y)?,
        ))
    }

    pub fn copy_visible_text(&self) -> String {
        self.copy_line_indices(self.visible_line_indices.iter().copied())
    }

    pub fn copy_selection(&self, selection: Selection) -> String {
        let (start, end) = selection.normalized();
        let indices =
            self.lines.iter().enumerate().filter_map(|(index, line)| {
                (line.y >= start.y && line.y <= end.y).then_some(index)
            });

        self.copy_line_indices_with_bounds(indices, start, end)
    }

    pub fn apply_selection_highlight(&self, buf: &mut Buffer, selection: Selection) {
        let (start, end) = selection.normalized();
        let style = selection_style();

        for line in self
            .visible_lines()
            .filter(|line| line.y >= start.y && line.y <= end.y)
        {
            let start_column = if line.y == start.y {
                line.local_column(start.x)
            } else {
                0
            };
            let end_column = if line.y == end.y {
                line.local_column(end.x)
            } else {
                line.text_width
            };

            if start_column >= end_column {
                continue;
            }

            let start_x = line.x.saturating_add(start_column);
            let end_x = line.x.saturating_add(end_column);
            let screen_y = line.screen_y.expect("visible lines have a screen row");
            for x in start_x..end_x {
                buf[(x, screen_y)].set_style(style);
            }
        }
    }

    fn line_at_screen_y(&self, y: u16) -> Option<&SelectionRow> {
        self.visible_lines().find(|line| line.screen_y == Some(y))
    }

    fn visible_lines(&self) -> impl Iterator<Item = &SelectionRow> {
        self.visible_line_indices
            .iter()
            .map(|index| &self.lines[*index])
    }

    fn copy_line_indices(&self, indices: impl IntoIterator<Item = usize>) -> String {
        let Some(first) = self.lines.first() else {
            return String::new();
        };
        let Some(last) = self.lines.last() else {
            return String::new();
        };

        self.copy_line_indices_with_bounds(
            indices,
            TextPosition::new(first.x, first.y),
            TextPosition::new(last.end_x(), last.y),
        )
    }

    fn copy_line_indices_with_bounds(
        &self,
        indices: impl IntoIterator<Item = usize>,
        start: TextPosition,
        end: TextPosition,
    ) -> String {
        let indices = indices.into_iter().collect::<Vec<_>>();
        let mut copied_lines = Vec::new();
        let mut last_copy_group = None;
        let mut output = String::new();

        for line_index in indices {
            let line = &self.lines[line_index];
            if line.copy_group.is_some() && line.copy_group == last_copy_group {
                continue;
            }

            let start_column = if line.y == start.y {
                line.local_column(start.x)
            } else {
                0
            };
            let end_column = if line.y == end.y {
                line.local_column(end.x)
            } else {
                line.text_width
            };

            copied_lines.push((
                line.copy_range(start_column, end_column),
                line.copy_separator.clone(),
            ));
            last_copy_group = line.copy_group;
        }

        for (index, (text, separator)) in copied_lines.iter().enumerate() {
            output.push_str(text);
            if index + 1 < copied_lines.len() {
                output.push_str(separator.as_str());
            }
        }

        output
    }
}

pub trait Selectable {
    fn selection_map(&self) -> SelectionMap;
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SelectionSurface<K> {
    key: K,
    map: SelectionMap,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ActiveSelection<K> {
    key: K,
    selection: Selection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectionManager<K> {
    surfaces: Vec<SelectionSurface<K>>,
    active: Option<ActiveSelection<K>>,
    dragging: bool,
}

impl<K> Default for SelectionManager<K> {
    fn default() -> Self {
        Self {
            surfaces: Vec::new(),
            active: None,
            dragging: false,
        }
    }
}

impl<K: Clone + Eq> SelectionManager<K> {
    pub fn register(&mut self, key: K, map: SelectionMap) {
        if let Some(surface) = self.surfaces.iter_mut().find(|surface| surface.key == key) {
            surface.map = map;
        } else {
            self.surfaces.push(SelectionSurface { key, map });
        }
    }

    pub fn map_for(&self, key: &K) -> Option<&SelectionMap> {
        self.surfaces
            .iter()
            .find(|surface| &surface.key == key)
            .map(|surface| &surface.map)
    }

    pub fn active_key(&self) -> Option<&K> {
        self.active.as_ref().map(|active| &active.key)
    }

    pub fn active_area(&self) -> Option<Rect> {
        let key = self.active_key()?;
        Some(self.map_for(key)?.area())
    }

    pub fn is_dragging(&self) -> bool {
        self.dragging
    }

    pub fn selection_for(&self, key: &K) -> Option<Selection> {
        self.active
            .as_ref()
            .and_then(|active| (&active.key == key).then_some(active.selection))
    }

    pub fn contains_point(&self, key: &K, x: u16, y: u16) -> bool {
        self.map_for(key)
            .is_some_and(|map| map.contains_point(x, y))
    }

    pub fn start_at(&mut self, x: u16, y: u16) -> Option<K> {
        let surface = self.surfaces.iter().find(|surface| {
            surface.map.contains_point(x, y) && surface.map.position_at(x, y).is_some()
        })?;
        let point = surface.map.position_at(x, y)?;
        let key = surface.key.clone();
        self.active = Some(ActiveSelection {
            key: key.clone(),
            selection: Selection::new(point, point),
        });
        self.dragging = true;
        Some(key)
    }

    pub fn update_focus_nearest(&mut self, x: u16, y: u16) -> bool {
        let Some(active) = self.active.clone() else {
            return false;
        };
        let Some(focus) = self
            .map_for(&active.key)
            .and_then(|map| map.nearest_position(x, y))
        else {
            return false;
        };
        if let Some(active) = &mut self.active {
            active.selection.focus = focus;
        }
        true
    }

    pub fn finish_copy(&mut self) -> Option<(K, String)> {
        self.dragging = false;
        let active = self.active.clone()?;
        let text = self
            .map_for(&active.key)
            .map(|map| map.copy_selection(active.selection))
            .unwrap_or_default();
        self.active = None;
        Some((active.key, text))
    }

    pub fn clear(&mut self) {
        self.active = None;
        self.dragging = false;
    }
}

fn selection_style() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

fn position_before_or_equal(left: TextPosition, right: TextPosition) -> bool {
    left.y < right.y || (left.y == right.y && left.x <= right.x)
}
