use std::ops::Range;

use ratatui::{
    buffer::Buffer,
    layout::{Rect, Size},
    style::Style,
    text::{Line, Text},
    widgets::{Paragraph, StatefulWidget, Widget, WidgetRef, Wrap},
};
use unicode_width::UnicodeWidthStr;

use coox_tui::{
    components::{
        image::{ImageRenderer, ImageView},
        scroll_view::{
            ScrollViewState, VirtualContent, VirtualScrollView, VirtualViewport, VisibleItem,
            item_prefix_heights,
        },
    },
    selection::{
        CopySeparator, Selection, SelectionCell, SelectionMap, SelectionRow, SelectionRowContent,
    },
};
use copro_api::message::ImageContent;

use crate::tui::components::blocks::{
    BLOCK_PADDING, BlockLine, BlockSegment, block_container_style, image_placeholder_text,
    image_source, render_block_segments,
};
use crate::tui::state::{AppState, BlockState};

const IMAGE_PREVIEW_HEIGHT: u16 = 14;

pub struct ConversationView<'a> {
    layout: &'a ConversationLayout,
    image_renderer: &'a ImageRenderer,
    scroll_from_bottom: u32,
    selection: Option<Selection>,
}

impl<'a> ConversationView<'a> {
    pub fn new(layout: &'a ConversationLayout, image_renderer: &'a ImageRenderer) -> Self {
        Self {
            layout,
            image_renderer,
            scroll_from_bottom: 0,
            selection: None,
        }
    }

    pub fn scroll_from_bottom(mut self, scroll_from_bottom: u32) -> Self {
        self.scroll_from_bottom = scroll_from_bottom;
        self
    }

    pub fn selection(mut self, selection: Option<Selection>) -> Self {
        self.selection = selection;
        self
    }
}

impl WidgetRef for ConversationView<'_> {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        self.layout.render(
            area,
            self.scroll_from_bottom,
            buf,
            self.image_renderer,
            self.selection,
        );
    }
}

#[derive(Clone, Debug)]
pub struct ConversationLayout {
    revision: u64,
    width: u16,
    blocks: Vec<PreparedBlock>,
    prefix_heights: Vec<u32>,
}

impl ConversationLayout {
    pub fn prepare(state: &AppState, width: u16) -> Self {
        let blocks = prepare_blocks(state.blocks().iter(), width);
        let prefix_heights = item_prefix_heights(&blocks, |block| block.height);

        Self {
            revision: state.revision(),
            width,
            blocks,
            prefix_heights,
        }
    }

    pub fn is_current(&self, state: &AppState, width: u16) -> bool {
        self.revision == state.revision() && self.width == width
    }

    pub fn render(
        &self,
        area: Rect,
        scroll_from_bottom: u32,
        buf: &mut Buffer,
        image_renderer: &ImageRenderer,
        selection: Option<Selection>,
    ) {
        render_prepared_blocks_range(
            &self.blocks,
            &self.prefix_heights,
            area,
            scroll_from_bottom,
            buf,
            image_renderer,
        );

        if let Some(selection) = selection {
            self.selection_map(area, scroll_from_bottom)
                .apply_selection_highlight(buf, selection);
        }
    }

    pub fn selection_map(&self, area: Rect, scroll_from_bottom: u32) -> SelectionMap {
        build_selection_map(
            &self.blocks,
            &self.prefix_heights,
            self.viewport(area, scroll_from_bottom),
        )
    }

    pub fn copy_selection(&self, area: Rect, selection: Selection) -> String {
        build_selection_copy_map(&self.blocks, &self.prefix_heights, area, selection)
            .copy_selection(selection)
    }

    pub fn viewport(&self, area: Rect, scroll_from_bottom: u32) -> VirtualViewport {
        VirtualViewport::new(area, self.total_height(), scroll_from_bottom)
    }

    pub fn total_height(&self) -> u32 {
        self.prefix_heights.last().copied().unwrap_or_default()
    }
}

fn render_prepared_blocks_range(
    blocks: &[PreparedBlock],
    prefix_heights: &[u32],
    area: Rect,
    scroll_from_bottom: u32,
    buf: &mut Buffer,
    image_renderer: &ImageRenderer,
) {
    let content = VirtualContent::from_prefix_heights(blocks, prefix_heights);
    let mut scroll_state = ScrollViewState::from_bottom(scroll_from_bottom);
    VirtualScrollView::new(
        content,
        |block: &PreparedBlock, view: VisibleItem, buf: &mut Buffer| {
            render_block_view(
                &block.segments,
                block.style,
                block.height,
                view.visible_top,
                view.target,
                buf,
                image_renderer,
            );
        },
    )
    .render(area, buf, &mut scroll_state);
}

#[derive(Clone, Debug)]
struct PreparedBlock {
    segments: Vec<BlockSegment>,
    style: Style,
    height: u32,
}

#[derive(Clone, Debug)]
struct TextLineGroup {
    lines: Vec<Line<'static>>,
    trim: bool,
}

impl Default for TextLineGroup {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            trim: true,
        }
    }
}

impl TextLineGroup {
    fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    fn push(&mut self, line: &BlockLine) -> Option<Self> {
        if self.lines.is_empty() {
            self.trim = line.trim();
            self.lines.push(line.line().clone());
            return None;
        }

        if self.trim == line.trim() {
            self.lines.push(line.line().clone());
            return None;
        }

        let previous = std::mem::replace(
            self,
            Self {
                lines: vec![line.line().clone()],
                trim: line.trim(),
            },
        );
        Some(previous)
    }

    fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

fn prepare_blocks<'a>(
    blocks: impl Iterator<Item = &'a BlockState>,
    width: u16,
) -> Vec<PreparedBlock> {
    blocks
        .map(|block| {
            let segments = render_block_segments(block);
            let style = block_container_style(block);
            let height = block_height(width, &segments);
            PreparedBlock {
                segments,
                style,
                height,
            }
        })
        .collect()
}

fn build_selection_map(
    blocks: &[PreparedBlock],
    prefix_heights: &[u32],
    viewport: VirtualViewport,
) -> SelectionMap {
    let area = viewport.area();
    let mut map = SelectionMap::new(area, viewport.source_start(), viewport.total_height());
    let source_start = viewport.source_start();
    let source_end = source_start.saturating_add(u32::from(area.height));

    if area.is_empty() {
        return map;
    }

    for index in block_range_for_source(prefix_heights, source_start, source_end) {
        let Some(block) = blocks.get(index) else {
            continue;
        };
        let Some(block_top) = prefix_heights.get(index).copied() else {
            continue;
        };
        collect_block_selection_rows(block, block_top, area, source_start, source_end, &mut map);
    }

    map
}

fn build_selection_copy_map(
    blocks: &[PreparedBlock],
    prefix_heights: &[u32],
    area: Rect,
    selection: Selection,
) -> SelectionMap {
    let (start, end) = selection.normalized();
    let total_height = prefix_heights.last().copied().unwrap_or_default();
    let mut map = SelectionMap::new(area, u32::MAX, total_height);

    if area.is_empty() {
        return map;
    }

    let source_start = start.y;
    let source_end = end.y.saturating_add(1);
    for index in block_range_for_source(prefix_heights, source_start, source_end) {
        let Some(block) = blocks.get(index) else {
            continue;
        };
        let Some(block_top) = prefix_heights.get(index).copied() else {
            continue;
        };
        collect_block_selection_rows(block, block_top, area, source_start, source_end, &mut map);
    }

    map
}

fn block_range_for_source(
    prefix_heights: &[u32],
    source_start: u32,
    source_end: u32,
) -> Range<usize> {
    if source_start >= source_end || prefix_heights.len() < 2 {
        return 0..0;
    }

    let item_count = prefix_heights.len() - 1;
    let start = prefix_heights
        .partition_point(|height| *height <= source_start)
        .saturating_sub(1)
        .min(item_count);
    let end = prefix_heights
        .partition_point(|height| *height < source_end)
        .min(item_count);

    start..end.max(start)
}

fn collect_block_selection_rows(
    block: &PreparedBlock,
    block_top: u32,
    area: Rect,
    source_start: u32,
    source_end: u32,
    map: &mut SelectionMap,
) {
    if area.is_empty() {
        return;
    }
    let block_bottom = block_top.saturating_add(block.height);
    if visible_range(block_top, block_bottom, source_start, source_end).is_none() {
        return;
    }

    let inner_x = area.x.saturating_add(BLOCK_PADDING.left);
    let inner_width = area
        .width
        .saturating_sub(BLOCK_PADDING.left + BLOCK_PADDING.right);
    let clip = RenderClip {
        target: area,
        target_x: inner_x,
        width: inner_width,
        source_start,
        source_end,
    };
    let mut virtual_y = u32::from(BLOCK_PADDING.top);
    let mut line_group = TextLineGroup::default();

    for segment in &block.segments {
        match segment {
            BlockSegment::Line(line) => {
                if let Some(group) = line_group.push(line) {
                    virtual_y = collect_line_group_selection_rows(
                        group,
                        block.style,
                        block_top,
                        virtual_y,
                        clip,
                        map,
                    );
                }
            }
            BlockSegment::Image(image) => {
                virtual_y = collect_line_group_selection_rows(
                    line_group.take(),
                    block.style,
                    block_top,
                    virtual_y,
                    clip,
                    map,
                );
                collect_image_placeholder_selection_rows(
                    &image_placeholder_text(image),
                    block_top,
                    virtual_y,
                    clip,
                    map,
                );
                virtual_y = virtual_y.saturating_add(u32::from(IMAGE_PREVIEW_HEIGHT));
            }
        }
    }

    collect_line_group_selection_rows(line_group, block.style, block_top, virtual_y, clip, map);
}

fn collect_line_group_selection_rows(
    group: TextLineGroup,
    style: Style,
    block_top: u32,
    virtual_y: u32,
    clip: RenderClip,
    map: &mut SelectionMap,
) -> u32 {
    if group.is_empty() || clip.width == 0 {
        return virtual_y;
    }

    let mut row = virtual_y;
    for line in group.lines {
        let line_height = line_height(clip.width, &line, style, group.trim);
        let line_bottom = row.saturating_add(line_height);
        let global_top = block_top.saturating_add(row);
        let global_bottom = block_top.saturating_add(line_bottom);

        if visible_range(
            global_top,
            global_bottom,
            clip.source_start,
            clip.source_end,
        )
        .is_none()
        {
            row = line_bottom;
            continue;
        }

        let rendered_rows = rendered_line_selection_rows(&line, clip.width, style, group.trim);

        for source_row in row..line_bottom {
            let row_index = source_row.saturating_sub(row) as usize;
            let Some(rendered_row) = rendered_rows.get(row_index) else {
                continue;
            };
            let y = block_top.saturating_add(source_row);
            if y < clip.source_start || y >= clip.source_end {
                continue;
            }
            map.push_line(SelectionRow::new(
                clip.target_x,
                y,
                map_screen_y(map, y),
                clip.width,
                SelectionRowContent::new(
                    rendered_row.text_width,
                    rendered_row.text.clone(),
                    rendered_row.copy_separator.clone(),
                    rendered_row.cells.clone(),
                ),
            ));
        }

        row = line_bottom;
    }

    row
}

fn collect_image_placeholder_selection_rows(
    placeholder: &str,
    block_top: u32,
    virtual_y: u32,
    clip: RenderClip,
    map: &mut SelectionMap,
) {
    if clip.width == 0 {
        return;
    }

    let copy_group = map.lines().len();
    for offset in 0..IMAGE_PREVIEW_HEIGHT {
        let y = block_top
            .saturating_add(virtual_y)
            .saturating_add(u32::from(offset));
        if y < clip.source_start || y >= clip.source_end {
            continue;
        }
        map.push_line(
            SelectionRow::new(
                clip.target_x,
                y,
                map_screen_y(map, y),
                clip.width,
                SelectionRowContent::new(
                    clip.width,
                    placeholder.to_string(),
                    CopySeparator::HardLine,
                    Vec::new(),
                ),
            )
            .with_copy_group(copy_group)
            .with_copy_text(placeholder.to_string()),
        );
    }
}

fn map_screen_y(map: &SelectionMap, y: u32) -> Option<u16> {
    let area = map.area();
    let viewport_start = map.viewport_start();
    let visible_end = viewport_start.saturating_add(u32::from(area.height));
    (y >= viewport_start && y < visible_end)
        .then(|| area.y.saturating_add((y - viewport_start) as u16))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RenderedSelectionRow {
    text_width: u16,
    text: String,
    copy_separator: CopySeparator,
    cells: Vec<SelectionCell>,
}

fn rendered_line_selection_rows(
    line: &Line<'static>,
    width: u16,
    style: Style,
    trim: bool,
) -> Vec<RenderedSelectionRow> {
    let height = line_height(width, line, style, trim);
    let virtual_area = Rect::new(0, 0, width, u16_saturated(height));
    let mut virtual_buf = Buffer::empty(virtual_area);
    text_paragraph(vec![line.clone()], style, trim).render(virtual_area, &mut virtual_buf);

    let mut rows = (0..virtual_area.height)
        .map(|y| {
            let (text, text_width, cells) = rendered_row_selection_cells(&virtual_buf, y, width);
            RenderedSelectionRow {
                text_width,
                text,
                copy_separator: CopySeparator::HardLine,
                cells,
            }
        })
        .collect::<Vec<_>>();

    let separators = line_copy_separators(line, &rows);
    for (row, separator) in rows.iter_mut().zip(separators) {
        row.copy_separator = separator;
    }

    rows
}

fn rendered_row_selection_cells(
    buffer: &Buffer,
    y: u16,
    width: u16,
) -> (String, u16, Vec<SelectionCell>) {
    let last_content_column = (0..width).rev().find(|x| {
        let symbol = buffer[(*x, y)].symbol();
        !symbol.is_empty() && symbol != " "
    });
    let Some(last_content_column) = last_content_column else {
        return (String::new(), 0, Vec::new());
    };

    let mut text_width = last_content_column.saturating_add(1);
    let mut cells = Vec::new();
    let mut covered_until = 0_u16;

    for column in 0..=last_content_column {
        if column < covered_until {
            continue;
        }

        let symbol = buffer[(column, y)].symbol();
        let symbol_width = symbol_width(symbol);
        if symbol_width == 0 {
            continue;
        }

        text_width = text_width.max(column.saturating_add(symbol_width));
        covered_until = column.saturating_add(symbol_width);
        cells.push(SelectionCell::new(column, symbol_width, symbol.to_string()));
    }

    let text = cells.iter().map(SelectionCell::text).collect();
    (text, text_width, cells)
}

fn line_copy_separators(line: &Line<'static>, rows: &[RenderedSelectionRow]) -> Vec<CopySeparator> {
    let text = line_plain_text(line);
    let ranges = rendered_row_ranges(&text, rows);

    rows.iter()
        .enumerate()
        .map(|(index, _)| {
            if index + 1 == rows.len() {
                return CopySeparator::HardLine;
            }

            let Some((_, end)) = ranges.get(index).and_then(|range| *range) else {
                return CopySeparator::None;
            };
            let Some((start, _)) = ranges.get(index + 1).and_then(|range| *range) else {
                return CopySeparator::None;
            };
            let Some(separator) = text.get(end..start) else {
                return CopySeparator::None;
            };

            if separator.is_empty() {
                CopySeparator::None
            } else {
                CopySeparator::SoftWrap(separator.to_string())
            }
        })
        .collect()
}

fn rendered_row_ranges(text: &str, rows: &[RenderedSelectionRow]) -> Vec<Option<(usize, usize)>> {
    let mut cursor = 0;
    rows.iter()
        .map(|row| {
            if row.text.is_empty() {
                return Some((cursor, cursor));
            }

            let search = text.get(cursor..)?;
            let offset = search.find(&row.text)?;
            let start = cursor.saturating_add(offset);
            let end = start.saturating_add(row.text.len());
            cursor = end;
            Some((start, end))
        })
        .collect()
}

fn render_block_view(
    segments: &[BlockSegment],
    style: Style,
    height: u32,
    source_y: u32,
    target: Rect,
    buf: &mut Buffer,
    image_renderer: &ImageRenderer,
) {
    if target.is_empty() {
        return;
    }

    buf.set_style(target, style);

    let inner_x = target.x.saturating_add(BLOCK_PADDING.left);
    let inner_width = target
        .width
        .saturating_sub(BLOCK_PADDING.left + BLOCK_PADDING.right);
    let source_end = source_y.saturating_add(u32::from(target.height));
    let inner_bottom = height.saturating_sub(u32::from(BLOCK_PADDING.bottom));
    let clip = RenderClip {
        target,
        target_x: inner_x,
        width: inner_width,
        source_start: source_y,
        source_end,
    };
    let mut virtual_y = u32::from(BLOCK_PADDING.top);
    let mut line_group = TextLineGroup::default();

    for segment in segments {
        match segment {
            BlockSegment::Line(line) => {
                if let Some(group) = line_group.push(line) {
                    virtual_y = render_line_group_view(group, style, virtual_y, clip, buf);
                }
            }
            BlockSegment::Image(image) => {
                virtual_y = render_line_group_view(line_group.take(), style, virtual_y, clip, buf);
                virtual_y = render_image_segment_view(
                    image,
                    image_renderer,
                    virtual_y,
                    inner_bottom,
                    clip,
                    buf,
                );
            }
        }
    }

    render_line_group_view(line_group, style, virtual_y, clip, buf);
}

#[derive(Clone, Copy)]
struct RenderClip {
    target: Rect,
    target_x: u16,
    width: u16,
    source_start: u32,
    source_end: u32,
}

fn render_line_group_view(
    group: TextLineGroup,
    style: Style,
    virtual_y: u32,
    clip: RenderClip,
    buf: &mut Buffer,
) -> u32 {
    if group.is_empty() || clip.width == 0 {
        return virtual_y;
    }

    let height = text_height(clip.width, &group);
    let virtual_bottom = virtual_y.saturating_add(height);
    if let Some((visible_top, visible_bottom)) = visible_range(
        virtual_y,
        virtual_bottom,
        clip.source_start,
        clip.source_end,
    ) {
        let (slice_top, visible_lines) = visible_text_lines(
            &group,
            clip.width,
            style,
            visible_top,
            visible_bottom,
            virtual_y,
        );
        let slice_height = text_height(clip.width, &visible_lines);
        let virtual_area = Rect::new(0, 0, clip.width, u16_saturated(slice_height));
        let mut virtual_buf = Buffer::empty(virtual_area);
        text_paragraph(visible_lines.lines, style, visible_lines.trim)
            .render(virtual_area, &mut virtual_buf);

        for source_row in visible_top..visible_bottom {
            let local_y = source_row.saturating_sub(slice_top);
            if local_y >= u32::from(virtual_area.height) {
                break;
            }
            let target_y = clip.target.y + (source_row - clip.source_start) as u16;
            for x in 0..clip.width {
                buf[(clip.target_x + x, target_y)] = virtual_buf[(x, local_y as u16)].clone();
            }
        }
    }

    virtual_bottom
}

fn visible_text_lines(
    group: &TextLineGroup,
    width: u16,
    style: Style,
    visible_top: u32,
    visible_bottom: u32,
    group_top: u32,
) -> (u32, TextLineGroup) {
    let mut row = group_top;
    let mut slice_top = None;
    let mut visible = TextLineGroup {
        lines: Vec::new(),
        trim: group.trim,
    };

    for line in &group.lines {
        let line_height = line_height(width, line, style, group.trim);
        let line_bottom = row.saturating_add(line_height);
        if line_bottom > visible_top && row < visible_bottom {
            slice_top.get_or_insert(row);
            visible.lines.push(line.clone());
        }
        if row >= visible_bottom {
            break;
        }
        row = line_bottom;
    }

    (slice_top.unwrap_or(visible_top), visible)
}

fn render_image_segment_view(
    image: &ImageContent,
    image_renderer: &ImageRenderer,
    virtual_y: u32,
    inner_bottom: u32,
    clip: RenderClip,
    buf: &mut Buffer,
) -> u32 {
    let source = image_source(image);
    let height = u32::from(IMAGE_PREVIEW_HEIGHT).min(inner_bottom.saturating_sub(virtual_y));
    let virtual_bottom = virtual_y.saturating_add(height);

    if clip.width > 0
        && let Some((visible_top, visible_bottom)) = visible_range(
            virtual_y,
            virtual_bottom,
            clip.source_start,
            clip.source_end,
        )
    {
        let visible_height = (visible_bottom - visible_top) as u16;
        let target_y = clip.target.y + (visible_top - clip.source_start) as u16;
        let area = Rect::new(clip.target_x, target_y, clip.width, visible_height);
        let y_offset = -((visible_top - virtual_y) as i16);

        ImageView::new(image_renderer, &source)
            .size(Size::new(clip.width.max(1), IMAGE_PREVIEW_HEIGHT))
            .y_offset(y_offset)
            .render_ref(area, buf);
    }

    virtual_y.saturating_add(u32::from(IMAGE_PREVIEW_HEIGHT))
}

fn visible_range(top: u32, bottom: u32, source_start: u32, source_end: u32) -> Option<(u32, u32)> {
    let visible_top = top.max(source_start);
    let visible_bottom = bottom.min(source_end);
    (visible_top < visible_bottom).then_some((visible_top, visible_bottom))
}

fn block_height(width: u16, segments: &[BlockSegment]) -> u32 {
    let inner_width = width
        .saturating_sub(BLOCK_PADDING.left + BLOCK_PADDING.right)
        .max(1);
    let content_height = content_height(inner_width, segments);
    content_height
        .saturating_add(u32::from(BLOCK_PADDING.top + BLOCK_PADDING.bottom))
        .max(u32::from(BLOCK_PADDING.top + BLOCK_PADDING.bottom))
}

fn content_height(width: u16, segments: &[BlockSegment]) -> u32 {
    let mut height = 0_u32;
    let mut line_group = TextLineGroup::default();

    for segment in segments {
        match segment {
            BlockSegment::Line(line) => {
                if let Some(group) = line_group.push(line) {
                    height = height.saturating_add(text_height(width, &group));
                }
            }
            BlockSegment::Image(_) => {
                height = height.saturating_add(text_height(width, &line_group));
                line_group = TextLineGroup::default();
                height = height.saturating_add(u32::from(IMAGE_PREVIEW_HEIGHT));
            }
        }
    }

    height.saturating_add(text_height(width, &line_group))
}

fn text_height(width: u16, group: &TextLineGroup) -> u32 {
    if group.is_empty() {
        return 0;
    }

    group
        .lines
        .iter()
        .map(|line| line_height(width, line, Style::default(), group.trim))
        .sum()
}

fn line_height(width: u16, line: &Line<'static>, style: Style, trim: bool) -> u32 {
    text_paragraph(vec![line.clone()], style, trim)
        .line_count(width)
        .try_into()
        .unwrap_or(u32::MAX)
}

fn u16_saturated(value: u32) -> u16 {
    value.min(u32::from(u16::MAX)) as u16
}

fn text_paragraph(lines: Vec<Line<'static>>, style: Style, trim: bool) -> Paragraph<'static> {
    let text = Text::from(lines).style(style);
    Paragraph::new(text).style(style).wrap(Wrap { trim })
}

fn symbol_width(symbol: &str) -> u16 {
    UnicodeWidthStr::width(symbol)
        .try_into()
        .unwrap_or(u16::MAX)
}

fn line_plain_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::image::{DynamicImage, ImageFormat, RgbaImage};
    use coox_tui::selection::TextPosition;
    use copro_api::{
        message::{InputContent, InputMessage, ToolCallId, ToolResult, ToolResultStatus},
        stream::OutputContentDelta,
    };
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, style::Modifier};
    use ratatui_image::picker::{Picker, ProtocolType};
    use std::{io::Cursor, time::Duration};

    #[test]
    fn conversation_layout_cache_key_tracks_revision_and_width() {
        let mut state = AppState::default();
        state.apply_delta(OutputContentDelta::Text("alpha".to_string()));
        let layout = ConversationLayout::prepare(&state, 40);

        assert!(layout.is_current(&state, 40));
        assert!(!layout.is_current(&state, 41));

        state.apply_delta(OutputContentDelta::Text(" beta".to_string()));

        assert!(!layout.is_current(&state, 40));
        assert!(ConversationLayout::prepare(&state, 40).is_current(&state, 40));
    }

    #[test]
    fn renders_blocks_in_feed_order() {
        let mut state = AppState::default();
        state.push_input(InputMessage::User(vec![
            copro_api::message::InputContent::Text("fix cursor".to_string()),
        ]));
        state.apply_delta(OutputContentDelta::Text("- done".to_string()));
        state.apply_delta(OutputContentDelta::ToolCall {
            id: Some("call_1".to_string()),
            name: Some("rg".to_string()),
            arguments: "{}".to_string(),
        });

        let backend = TestBackend::new(24, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        render_conversation(&mut terminal, &state, &ImageRenderer::halfblocks(), 0, None);

        let lines = buffer_lines(terminal.backend().buffer());
        assert_eq!(lines[0], "");
        assert_eq!(lines[1], " fix cursor");
        assert_eq!(lines[2], "");
        assert_eq!(lines[3], "");
        assert_eq!(lines[4], " • done");
        assert_eq!(lines[5], "");
        assert_eq!(lines[6], "");
        assert_eq!(lines[7], " rg");
        assert_eq!(lines[8], " {}");
        assert_eq!(lines[9], "");
    }

    #[test]
    fn read_tool_result_preserves_leading_line_number_gutter() {
        let mut state = AppState::default();
        state.apply_delta(OutputContentDelta::ToolCall {
            id: Some("call_1".to_string()),
            name: Some("read".to_string()),
            arguments: "{\"path\":\"Cargo.toml\"}".to_string(),
        });
        state.apply_tool_result(ToolResult {
            call_id: ToolCallId::new("call_1"),
            name: "read".to_string(),
            status: ToolResultStatus::Success,
            content: vec![InputContent::Text(
                "Cargo.toml\n 1: [workspace]\n 2: members = [\n10:     \"crates/coox-tui\","
                    .to_string(),
            )],
        });

        let backend = TestBackend::new(48, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        render_conversation(&mut terminal, &state, &ImageRenderer::halfblocks(), 0, None);

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines.iter().any(|line| line == "  1: [workspace]"));
        assert!(lines.iter().any(|line| line == "  2: members = ["));
        assert!(
            lines
                .iter()
                .any(|line| line == " 10:     \"crates/coox-tui\",")
        );
    }

    #[test]
    fn wraps_long_user_text_to_available_width() {
        let mut state = AppState::default();
        state.push_input(InputMessage::User(vec![
            copro_api::message::InputContent::Text("abcdefg".to_string()),
        ]));

        let backend = TestBackend::new(8, 5);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        render_conversation(&mut terminal, &state, &ImageRenderer::halfblocks(), 0, None);

        let lines = buffer_lines(terminal.backend().buffer());
        assert_eq!(lines[0], "");
        assert!(lines.iter().any(|line| line.contains("abcdef")));
        assert!(lines.iter().any(|line| line.contains("g")));
        assert_eq!(lines[3], "");
    }

    #[test]
    fn renders_error_block_in_feed_order() {
        let mut state = AppState::default();
        state.push_input(InputMessage::User(vec![
            copro_api::message::InputContent::Text("hello".to_string()),
        ]));
        state.push_error("request failed");

        let backend = TestBackend::new(24, 8);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        render_conversation(&mut terminal, &state, &ImageRenderer::halfblocks(), 0, None);

        let lines = buffer_lines(terminal.backend().buffer());
        let hello_row = lines
            .iter()
            .position(|line| line.contains("hello"))
            .expect("user block rendered");
        let error_row = lines
            .iter()
            .position(|line| line.contains("request failed"))
            .expect("error block rendered");
        assert!(hello_row < error_row);
    }

    #[test]
    fn renders_latest_blocks_when_feed_is_taller_than_viewport() {
        let mut state = AppState::default();
        for text in ["one", "two", "three", "four"] {
            state.push_input(InputMessage::User(vec![
                copro_api::message::InputContent::Text(text.to_string()),
            ]));
        }

        let backend = TestBackend::new(24, 6);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        render_conversation(&mut terminal, &state, &ImageRenderer::halfblocks(), 0, None);

        let lines = buffer_lines(terminal.backend().buffer());
        assert_eq!(lines[1], " three");
        assert_eq!(lines[4], " four");
        assert!(!lines.iter().any(|line| line.contains("one")));
        assert!(!lines.iter().any(|line| line.contains("two")));
    }

    #[test]
    fn scroll_from_bottom_renders_retained_older_content() {
        let mut state = AppState::default();
        for text in ["one", "two", "three", "four"] {
            state.push_input(InputMessage::User(vec![
                copro_api::message::InputContent::Text(text.to_string()),
            ]));
        }

        let backend = TestBackend::new(24, 6);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        render_conversation(&mut terminal, &state, &ImageRenderer::halfblocks(), 6, None);

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines.iter().any(|line| line.contains("one")));
        assert!(lines.iter().any(|line| line.contains("two")));
        assert!(!lines.iter().any(|line| line.contains("four")));
    }

    #[test]
    fn selection_map_rejoins_visible_soft_wraps_with_copy_separator() {
        let mut state = AppState::default();
        state.push_input(InputMessage::User(vec![InputContent::Text(
            "hello world".to_string(),
        )]));

        let map = selection_map(&state, Rect::new(0, 0, 8, 5), 0);

        assert_eq!(
            map.lines()
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["hello", "world"]
        );
        assert_eq!(
            map.lines()[0].copy_separator,
            CopySeparator::SoftWrap(" ".to_string())
        );
        assert_eq!(map.copy_visible_text(), "hello world");
    }

    #[test]
    fn selection_map_rejoins_visible_split_words_without_newline() {
        let mut state = AppState::default();
        state.push_input(InputMessage::User(vec![InputContent::Text(
            "abcdefg".to_string(),
        )]));

        let map = selection_map(&state, Rect::new(0, 0, 8, 5), 0);

        assert_eq!(
            map.lines()
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["abcdef", "g"]
        );
        assert_eq!(map.lines()[0].copy_separator, CopySeparator::None);
        assert_eq!(map.copy_visible_text(), "abcdefg");
    }

    #[test]
    fn copy_selection_uses_visible_text_positions() {
        let mut state = AppState::default();
        state.push_input(InputMessage::User(vec![InputContent::Text(
            "hello world".to_string(),
        )]));
        let map = selection_map(&state, Rect::new(0, 0, 8, 5), 0);
        let first = &map.lines()[0];
        let second = &map.lines()[1];

        let selection = Selection::new(
            TextPosition::new(first.x + 2, first.y),
            TextPosition::new(second.x + 2, second.y),
        );

        assert_eq!(map.copy_selection(selection), "llo wo");
        assert_eq!(
            map.position_at(0, first.screen_y.expect("visible row")),
            Some(TextPosition::new(first.x, first.y))
        );
        assert_eq!(
            map.position_at(99, second.screen_y.expect("visible row")),
            Some(TextPosition::new(second.end_x(), second.y))
        );
    }

    #[test]
    fn lazy_copy_selection_can_include_rows_outside_current_viewport() {
        let mut state = AppState::default();
        state.apply_delta(OutputContentDelta::Text(
            (0..8)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        let area = Rect::new(0, 0, 20, 4);
        let layout = ConversationLayout::prepare(&state, area.width);
        let map = layout.selection_map(area, 0);
        let last = map
            .lines()
            .iter()
            .find(|line| line.text == "line 7")
            .expect("visible last line exists in copy map");

        assert!(map.lines().iter().all(|line| line.text != "line 0"));
        assert!(last.screen_y.is_some());
        let selection = Selection::new(
            TextPosition::new(last.x, last.y.saturating_sub(7)),
            TextPosition::new(last.end_x(), last.y),
        );

        let copied = layout.copy_selection(area, selection);
        assert!(copied.contains("line 0"));
        assert!(copied.contains("line 7"));
    }

    #[test]
    fn selection_map_contains_only_viewport_rows_for_large_content() {
        let mut state = AppState::default();
        state.apply_delta(OutputContentDelta::Text(
            (0..1_000)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        let area = Rect::new(0, 0, 24, 6);
        let map = selection_map(&state, area, 0);

        assert!(map.lines().len() <= usize::from(area.height));
        assert!(map.lines().iter().all(|line| line.screen_y.is_some()));
        assert!(map.lines().iter().all(|line| line.text != "line 0"));
    }

    #[test]
    fn selection_map_includes_visible_image_placeholder_text() {
        let mut state = AppState::default();
        state.push_input(InputMessage::User(vec![InputContent::Image(
            copro_api::message::ImageContent::Url {
                url: "https://example.test/image.png".to_string(),
            },
        )]));
        let map = selection_map(&state, Rect::new(0, 0, 30, 18), 0);

        assert!(map.copy_visible_text().contains("[image: url]"));
    }

    #[test]
    fn selection_map_includes_placeholder_for_data_image() {
        let mut state = AppState::default();
        let image = png_bytes();
        let expected = format!("[image: image/png, {} bytes]", image.len());
        state.push_input(InputMessage::User(vec![InputContent::Image(
            copro_api::message::ImageContent::Data {
                mime_type: "image/png".to_string(),
                data: image.into(),
            },
        )]));
        let area = Rect::new(0, 0, 40, 18);

        let map = selection_map(&state, area, 0);

        assert!(map.copy_visible_text().contains(&expected));
    }

    #[test]
    fn copy_selection_from_data_image_middle_row_copies_placeholder_once() {
        let mut state = AppState::default();
        let image = png_bytes();
        let expected = format!("[image: image/png, {} bytes]", image.len());
        state.push_input(InputMessage::User(vec![InputContent::Image(
            copro_api::message::ImageContent::Data {
                mime_type: "image/png".to_string(),
                data: image.into(),
            },
        )]));
        let area = Rect::new(0, 0, 40, 18);

        let map = selection_map(&state, area, 0);
        let image_rows = map
            .lines()
            .iter()
            .filter(|line| line.copy_group().is_some())
            .collect::<Vec<_>>();
        let middle = image_rows[usize::from(IMAGE_PREVIEW_HEIGHT / 2)];

        let selection = Selection::new(
            TextPosition::new(middle.x, middle.y),
            TextPosition::new(middle.x + 1, middle.y),
        );

        assert_eq!(map.copy_selection(selection), expected);
        assert_eq!(
            map.copy_visible_text().matches(expected.as_str()).count(),
            1
        );
    }

    #[test]
    fn selection_highlight_marks_visible_text_cells() {
        let mut state = AppState::default();
        state.push_input(InputMessage::User(vec![InputContent::Text(
            "select me".to_string(),
        )]));
        let area = Rect::new(0, 0, 14, 5);
        let map = selection_map(&state, area, 0);
        let line = &map.lines()[0];
        let selection = Selection::new(
            TextPosition::new(line.x + 1, line.y),
            TextPosition::new(line.x + 4, line.y),
        );

        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        render_conversation(
            &mut terminal,
            &state,
            &ImageRenderer::halfblocks(),
            0,
            Some(selection),
        );

        let buffer = terminal.backend().buffer();
        let screen_y = line.screen_y.expect("visible row");
        assert!(
            buffer[(line.x + 1, screen_y)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            buffer[(line.x + 3, screen_y)]
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

    #[test]
    fn renders_tool_result_image_as_preview_not_placeholder() {
        let mut state = AppState::default();
        state.apply_tool_result(ToolResult {
            call_id: ToolCallId::new("call_1"),
            name: "read".to_string(),
            status: ToolResultStatus::Success,
            content: vec![InputContent::Image(
                copro_api::message::ImageContent::Data {
                    mime_type: "image/png".to_string(),
                    data: tall_png_bytes().into(),
                },
            )],
        });

        let renderer = ImageRenderer::halfblocks();
        let backend = TestBackend::new(24, 16);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        render_conversation(&mut terminal, &state, &renderer, 0, None);
        wait_for_image_jobs(&renderer);
        render_conversation(&mut terminal, &state, &renderer, 0, None);

        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        assert!(!lines.iter().any(|line| line.contains("[image:")));
        assert!(
            buffer
                .content()
                .iter()
                .any(|cell| matches!(cell.symbol(), "▀" | "▄"))
        );
    }

    #[test]
    fn clipped_kitty_image_restores_cursor_with_visible_height() {
        let mut state = AppState::default();
        state.apply_tool_result(ToolResult {
            call_id: ToolCallId::new("call_1"),
            name: "read".to_string(),
            status: ToolResultStatus::Success,
            content: vec![InputContent::Image(
                copro_api::message::ImageContent::Data {
                    mime_type: "image/png".to_string(),
                    data: png_bytes().into(),
                },
            )],
        });
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        let renderer = ImageRenderer::new(picker);

        let backend = TestBackend::new(24, 8);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        render_conversation(&mut terminal, &state, &renderer, 9, None);
        wait_for_image_jobs(&renderer);
        render_conversation(&mut terminal, &state, &renderer, 9, None);

        let symbols = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(symbols.contains('\u{10EEEE}'));
        assert!(!symbols.contains("\x1b[13B"));
    }

    #[test]
    fn top_clipped_kitty_image_remains_visible() {
        let mut state = AppState::default();
        state.apply_tool_result(ToolResult {
            call_id: ToolCallId::new("call_1"),
            name: "read".to_string(),
            status: ToolResultStatus::Success,
            content: vec![InputContent::Image(
                copro_api::message::ImageContent::Data {
                    mime_type: "image/png".to_string(),
                    data: tall_png_bytes().into(),
                },
            )],
        });
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        let renderer = ImageRenderer::new(picker);

        let backend = TestBackend::new(24, 8);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        render_conversation(&mut terminal, &state, &renderer, 0, None);
        wait_for_image_jobs(&renderer);
        render_conversation(&mut terminal, &state, &renderer, 0, None);

        let symbols = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(symbols.contains('\u{10EEEE}'));
    }

    fn render_conversation(
        terminal: &mut Terminal<TestBackend>,
        state: &AppState,
        renderer: &ImageRenderer,
        scroll_from_bottom: u32,
        selection: Option<Selection>,
    ) {
        terminal
            .draw(|frame| {
                let layout = ConversationLayout::prepare(state, frame.area().width);
                ConversationView::new(&layout, renderer)
                    .scroll_from_bottom(scroll_from_bottom)
                    .selection(selection)
                    .render_ref(frame.area(), frame.buffer_mut());
            })
            .expect("render conversation");
    }

    fn selection_map(state: &AppState, area: Rect, scroll_from_bottom: u32) -> SelectionMap {
        ConversationLayout::prepare(state, area.width).selection_map(area, scroll_from_bottom)
    }

    fn wait_for_image_jobs(renderer: &ImageRenderer) {
        for _ in 0..100 {
            renderer.drain_prepared();
            if !renderer.has_in_flight() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        panic!("image prepare job did not finish");
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

    fn png_bytes() -> Vec<u8> {
        png_bytes_with_size(32, 32)
    }

    fn tall_png_bytes() -> Vec<u8> {
        png_bytes_with_size(16, 256)
    }

    fn png_bytes_with_size(width: u32, height: u32) -> Vec<u8> {
        let mut data = Vec::new();
        let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            let color = if y < height / 2 {
                [255, 0, 0, 255]
            } else {
                [0, 0, 255, 255]
            };
            for _ in 0..width {
                pixels.extend_from_slice(&color);
            }
        }
        let image = DynamicImage::ImageRgba8(
            RgbaImage::from_vec(width, height, pixels).expect("valid rgba image"),
        );

        image
            .write_to(&mut Cursor::new(&mut data), ImageFormat::Png)
            .expect("valid png");

        data
    }
}
