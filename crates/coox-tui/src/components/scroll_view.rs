use std::{borrow::Cow, ops::Range};

use ratatui::{
    buffer::Buffer,
    layout::{Rect, Size},
    widgets::StatefulWidget,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScrollViewState {
    scroll_from_bottom: u32,
}

pub type ScrollState = ScrollViewState;

impl ScrollViewState {
    pub const fn new() -> Self {
        Self {
            scroll_from_bottom: 0,
        }
    }

    pub const fn from_bottom(scroll_from_bottom: u32) -> Self {
        Self { scroll_from_bottom }
    }

    pub const fn scroll_from_bottom(self) -> u32 {
        self.scroll_from_bottom
    }

    pub fn clamp(&mut self, max_scroll_from_bottom: u32) -> bool {
        self.set_scroll_from_bottom(self.scroll_from_bottom, max_scroll_from_bottom)
    }

    pub fn set_scroll_from_bottom(
        &mut self,
        scroll_from_bottom: u32,
        max_scroll_from_bottom: u32,
    ) -> bool {
        let scroll_from_bottom = scroll_from_bottom.min(max_scroll_from_bottom);
        let changed = self.scroll_from_bottom != scroll_from_bottom;
        self.scroll_from_bottom = scroll_from_bottom;
        changed
    }

    pub fn scroll_by(&mut self, delta: i32, max_scroll_from_bottom: u32) -> bool {
        let scroll_from_bottom = if delta > 0 {
            self.scroll_from_bottom.saturating_add(delta.unsigned_abs())
        } else if delta < 0 {
            self.scroll_from_bottom.saturating_sub(delta.unsigned_abs())
        } else {
            self.scroll_from_bottom
        };

        self.set_scroll_from_bottom(scroll_from_bottom, max_scroll_from_bottom)
    }

    pub fn scroll_up(&mut self, max_scroll_from_bottom: u32) -> bool {
        self.scroll_by(1, max_scroll_from_bottom)
    }

    pub fn scroll_down(&mut self, max_scroll_from_bottom: u32) -> bool {
        self.scroll_by(-1, max_scroll_from_bottom)
    }

    pub fn scroll_page_up(&mut self, viewport_height: u16, max_scroll_from_bottom: u32) -> bool {
        self.scroll_by(i32::from(viewport_height), max_scroll_from_bottom)
    }

    pub fn scroll_page_down(&mut self, viewport_height: u16, max_scroll_from_bottom: u32) -> bool {
        self.scroll_by(-i32::from(viewport_height), max_scroll_from_bottom)
    }

    pub fn scroll_to_top(&mut self, max_scroll_from_bottom: u32) -> bool {
        self.set_scroll_from_bottom(max_scroll_from_bottom, max_scroll_from_bottom)
    }

    pub fn scroll_to_bottom(&mut self, max_scroll_from_bottom: u32) -> bool {
        self.set_scroll_from_bottom(0, max_scroll_from_bottom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualContent<'a, T> {
    items: &'a [T],
    prefix_heights: Cow<'a, [u32]>,
}

impl<'a, T> VirtualContent<'a, T> {
    pub fn new(items: &'a [T], height_of: impl FnMut(&T) -> u32) -> Self {
        Self {
            items,
            prefix_heights: Cow::Owned(item_prefix_heights(items, height_of)),
        }
    }

    pub fn from_prefix_heights(items: &'a [T], prefix_heights: &'a [u32]) -> Self {
        debug_assert_eq!(
            prefix_heights.len(),
            items.len() + 1,
            "prefix heights must contain one leading zero plus one entry per item"
        );

        Self {
            items,
            prefix_heights: Cow::Borrowed(prefix_heights),
        }
    }

    pub const fn items(&self) -> &'a [T] {
        self.items
    }

    pub fn prefix_heights(&self) -> &[u32] {
        &self.prefix_heights
    }

    pub fn total_height(&self) -> u32 {
        self.prefix_heights.last().copied().unwrap_or_default()
    }

    pub fn size(&self, width: u16) -> Size {
        Size::new(width, u16_saturated(self.total_height()))
    }

    pub fn viewport(&self, area: Rect, state: ScrollViewState) -> VirtualViewport {
        VirtualViewport::new(area, self.total_height(), state.scroll_from_bottom())
    }

    pub fn visible_range(&self, viewport: VirtualViewport) -> Range<usize> {
        viewport.visible_item_range(self.prefix_heights())
    }

    pub fn for_each_visible(
        &self,
        viewport: VirtualViewport,
        mut render: impl FnMut(&T, VisibleItem),
    ) {
        viewport.for_each_visible_index(self.prefix_heights(), |index, visible| {
            if let Some(item) = self.items.get(index) {
                render(item, visible);
            }
        });
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtualViewport {
    area: Rect,
    content_size: Size,
    total_height: u32,
    scroll_from_bottom: u32,
    source_start: u32,
    source_end: u32,
}

impl VirtualViewport {
    pub fn new(area: Rect, total_height: u32, scroll_from_bottom: u32) -> Self {
        Self::with_content_size(
            area,
            Size::new(area.width, u16_saturated(total_height)),
            total_height,
            scroll_from_bottom,
        )
    }

    pub fn with_content_size(
        area: Rect,
        content_size: Size,
        total_height: u32,
        scroll_from_bottom: u32,
    ) -> Self {
        let max_scroll_from_bottom = max_scroll_from_bottom(total_height, area.height);
        let scroll_from_bottom = scroll_from_bottom.min(max_scroll_from_bottom);
        let source_start = max_scroll_from_bottom.saturating_sub(scroll_from_bottom);
        let source_end = source_start.saturating_add(u32::from(area.height));

        Self {
            area,
            content_size,
            total_height,
            scroll_from_bottom,
            source_start,
            source_end,
        }
    }

    pub const fn area(self) -> Rect {
        self.area
    }

    pub const fn content_size(self) -> Size {
        self.content_size
    }

    pub const fn total_height(self) -> u32 {
        self.total_height
    }

    pub const fn scroll_from_bottom(self) -> u32 {
        self.scroll_from_bottom
    }

    pub const fn source_start(self) -> u32 {
        self.source_start
    }

    pub const fn source_end(self) -> u32 {
        self.source_end
    }

    pub fn max_scroll_from_bottom(self) -> u32 {
        max_scroll_from_bottom(self.total_height, self.area.height)
    }

    pub fn screen_y(self, source_y: u32) -> Option<u16> {
        (source_y >= self.source_start && source_y < self.source_end).then(|| {
            self.area
                .y
                .saturating_add((source_y - self.source_start) as u16)
        })
    }

    pub fn visible_range(self, top: u32, bottom: u32) -> Option<(u32, u32)> {
        let visible_top = top.max(self.source_start);
        let visible_bottom = bottom.min(self.source_end);
        (visible_top < visible_bottom).then_some((visible_top, visible_bottom))
    }

    pub fn visible_item_range(self, prefix_heights: &[u32]) -> Range<usize> {
        if self.area.is_empty() || prefix_heights.len() < 2 {
            return 0..0;
        }

        let item_count = prefix_heights.len() - 1;
        let start = prefix_heights
            .partition_point(|height| *height <= self.source_start)
            .saturating_sub(1)
            .min(item_count);
        let end = prefix_heights
            .partition_point(|height| *height < self.source_end)
            .min(item_count);

        start..end.max(start)
    }

    pub fn for_each_visible_index(
        self,
        prefix_heights: &[u32],
        mut render: impl FnMut(usize, VisibleItem),
    ) {
        for index in self.visible_item_range(prefix_heights) {
            let Some(visible) = self.visible_item(prefix_heights, index) else {
                continue;
            };
            render(index, visible);
        }
    }

    fn visible_item(self, prefix_heights: &[u32], index: usize) -> Option<VisibleItem> {
        let item_top = *prefix_heights.get(index)?;
        let item_bottom = *prefix_heights.get(index + 1)?;
        let (visible_source_top, visible_source_bottom) =
            self.visible_range(item_top, item_bottom)?;
        let visible_top = visible_source_top.saturating_sub(item_top);
        let visible_bottom = visible_source_bottom.saturating_sub(item_top);
        let target_y = self
            .area
            .y
            .saturating_add((visible_source_top - self.source_start) as u16);
        let target = Rect::new(
            self.area.x,
            target_y,
            self.area.width,
            (visible_source_bottom - visible_source_top) as u16,
        );

        Some(VisibleItem {
            index,
            item_top,
            item_bottom,
            visible_top,
            visible_bottom,
            target,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisibleItem {
    pub index: usize,
    pub item_top: u32,
    pub item_bottom: u32,
    pub visible_top: u32,
    pub visible_bottom: u32,
    pub target: Rect,
}

pub struct VirtualScrollView<'a, T, R> {
    content: VirtualContent<'a, T>,
    render_item: R,
}

impl<'a, T, R> VirtualScrollView<'a, T, R> {
    pub fn new(content: VirtualContent<'a, T>, render_item: R) -> Self {
        Self {
            content,
            render_item,
        }
    }

    pub const fn content(&self) -> &VirtualContent<'a, T> {
        &self.content
    }
}

impl<'a, T, R> StatefulWidget for VirtualScrollView<'a, T, R>
where
    R: FnMut(&T, VisibleItem, &mut Buffer),
{
    type State = ScrollViewState;

    fn render(mut self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let max_scroll_from_bottom =
            max_scroll_from_bottom(self.content.total_height(), area.height);
        state.clamp(max_scroll_from_bottom);

        let viewport = self.content.viewport(area, *state);
        self.content.for_each_visible(viewport, |item, visible| {
            (self.render_item)(item, visible, buf)
        });
    }
}

pub fn item_prefix_heights<T>(items: &[T], mut height_of: impl FnMut(&T) -> u32) -> Vec<u32> {
    let mut prefix_heights = Vec::with_capacity(items.len() + 1);
    prefix_heights.push(0);

    for item in items {
        let next = prefix_heights
            .last()
            .copied()
            .unwrap_or(0_u32)
            .saturating_add(height_of(item));
        prefix_heights.push(next);
    }

    prefix_heights
}

pub fn max_scroll_from_bottom(total_height: u32, viewport_height: u16) -> u32 {
    total_height.saturating_sub(u32::from(viewport_height))
}

fn u16_saturated(value: u32) -> u16 {
    value.min(u32::from(u16::MAX)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_viewport_clamps_scroll_from_bottom() {
        let viewport = VirtualViewport::new(Rect::new(0, 2, 10, 4), 20, u32::MAX);

        assert_eq!(viewport.scroll_from_bottom(), 16);
        assert_eq!(viewport.source_start(), 0);
        assert_eq!(viewport.source_end(), 4);
        assert_eq!(viewport.screen_y(0), Some(2));
        assert_eq!(viewport.screen_y(3), Some(5));
        assert_eq!(viewport.screen_y(4), None);
    }

    #[test]
    fn bottom_viewport_starts_at_latest_rows() {
        let viewport = VirtualViewport::new(Rect::new(0, 0, 10, 4), 20, 0);

        assert_eq!(viewport.source_start(), 16);
        assert_eq!(viewport.source_end(), 20);
    }

    #[test]
    fn scroll_view_state_clamps_at_edges() {
        let mut state = ScrollViewState::new();

        assert!(!state.scroll_by(-3, 10));
        assert_eq!(state.scroll_from_bottom(), 0);
        assert!(state.scroll_by(12, 10));
        assert_eq!(state.scroll_from_bottom(), 10);
        assert!(!state.scroll_by(1, 10));
        assert_eq!(state.scroll_from_bottom(), 10);
        assert!(state.scroll_by(-4, 10));
        assert_eq!(state.scroll_from_bottom(), 6);
    }

    #[test]
    fn scroll_view_state_has_directional_helpers() {
        let mut state = ScrollViewState::new();

        assert!(state.scroll_up(10));
        assert_eq!(state.scroll_from_bottom(), 1);
        assert!(state.scroll_page_up(4, 10));
        assert_eq!(state.scroll_from_bottom(), 5);
        assert!(state.scroll_down(10));
        assert_eq!(state.scroll_from_bottom(), 4);
        assert!(state.scroll_to_top(10));
        assert_eq!(state.scroll_from_bottom(), 10);
        assert!(state.scroll_to_bottom(10));
        assert_eq!(state.scroll_from_bottom(), 0);
    }

    #[test]
    fn virtual_content_can_measure_or_borrow_prefix_heights() {
        let items = [4_u32, 5, 6, 3];
        let measured = VirtualContent::new(&items, |height| *height);
        let prefix_heights = [0, 4, 9, 15, 18];
        let borrowed = VirtualContent::from_prefix_heights(&items, &prefix_heights);

        assert_eq!(measured.prefix_heights(), prefix_heights);
        assert_eq!(borrowed.prefix_heights(), prefix_heights);
        assert_eq!(measured.total_height(), 18);
        assert_eq!(measured.size(8), Size::new(8, 18));
    }

    #[test]
    fn visible_item_range_uses_prefix_heights() {
        let viewport = VirtualViewport::new(Rect::new(5, 10, 20, 6), 18, 4);
        let prefix_heights = [0, 4, 9, 15, 18];

        assert_eq!(viewport.visible_item_range(&prefix_heights), 1..3);
    }

    #[test]
    fn visible_item_iteration_skips_offscreen_items() {
        let items = [4_u32, 5, 6, 3];
        let content = VirtualContent::new(&items, |height| *height);
        let viewport = VirtualViewport::new(Rect::new(5, 10, 20, 6), 18, 4);
        let mut visible_items = Vec::new();

        content.for_each_visible(viewport, |_, visible| {
            visible_items.push(visible);
        });

        assert_eq!(
            visible_items,
            vec![
                VisibleItem {
                    index: 1,
                    item_top: 4,
                    item_bottom: 9,
                    visible_top: 4,
                    visible_bottom: 5,
                    target: Rect::new(5, 10, 20, 1),
                },
                VisibleItem {
                    index: 2,
                    item_top: 9,
                    item_bottom: 15,
                    visible_top: 0,
                    visible_bottom: 5,
                    target: Rect::new(5, 11, 20, 5),
                },
            ]
        );
    }

    #[test]
    fn virtual_scroll_view_renders_only_visible_items() {
        let items = [4_u32, 5, 6, 3];
        let content = VirtualContent::new(&items, |height| *height);
        let mut state = ScrollViewState::from_bottom(4);
        let area = Rect::new(0, 0, 8, 6);
        let mut buffer = Buffer::empty(area);

        VirtualScrollView::new(
            content,
            |_: &u32, visible: VisibleItem, buf: &mut Buffer| {
                let symbol = match visible.index {
                    1 => "1",
                    2 => "2",
                    _ => "?",
                };
                for y in visible.target.y..visible.target.bottom() {
                    buf[(0, y)].set_symbol(symbol);
                }
            },
        )
        .render(area, &mut buffer, &mut state);

        assert_eq!(state.scroll_from_bottom(), 4);
        assert_eq!(buffer[(0, 0)].symbol(), "1");
        for y in 1..6 {
            assert_eq!(buffer[(0, y)].symbol(), "2");
        }
    }

    #[test]
    fn virtual_scroll_view_uses_supplied_prefix_heights() {
        let items = ["a", "b", "c", "d"];
        let prefix_heights = [0, 4, 9, 15, 18];
        let content = VirtualContent::from_prefix_heights(&items, &prefix_heights);
        let mut state = ScrollViewState::from_bottom(4);
        let area = Rect::new(0, 0, 8, 6);
        let mut buffer = Buffer::empty(area);

        VirtualScrollView::new(
            content,
            |item: &&str, visible: VisibleItem, buf: &mut Buffer| {
                for y in visible.target.y..visible.target.bottom() {
                    buf[(0, y)].set_symbol(item);
                }
            },
        )
        .render(area, &mut buffer, &mut state);

        assert_eq!(buffer[(0, 0)].symbol(), "b");
        for y in 1..6 {
            assert_eq!(buffer[(0, y)].symbol(), "c");
        }
    }

    #[test]
    fn virtual_scroll_view_clamps_state_during_render() {
        let items = [2_u32, 2];
        let content = VirtualContent::new(&items, |height| *height);
        let mut state = ScrollViewState::from_bottom(u32::MAX);
        let area = Rect::new(0, 0, 8, 3);
        let mut buffer = Buffer::empty(area);

        VirtualScrollView::new(content, |_: &u32, _: VisibleItem, _: &mut Buffer| {}).render(
            area,
            &mut buffer,
            &mut state,
        );

        assert_eq!(state.scroll_from_bottom(), 1);
    }
}
