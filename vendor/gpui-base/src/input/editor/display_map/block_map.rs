//! Deadlock patch: block widgets between rows.
//!
//! A block is an element of a given height drawn after a buffer line. It pushes
//! every later row down by its height, so row geometry stops being `row ×
//! line_height`: [`BlockLayout`] holds, for one frame, each visible block's
//! display row and the running total of their heights, and answers row → y and
//! y → row in `O(log blocks)`.
//!
//! A block is anchored to the end of its line as a byte offset that follows
//! edits: typing at the anchor leaves the block where it is, edits before it
//! shift it, and a deletion that removes the start of the anchor's line removes
//! the block.

use std::{ops::Range, rc::Rc};

use gpui::{AnyElement, App, Pixels, Window};

/// The text queries blocks need. Kept apart from the rope so this file builds
/// and tests on its own.
pub(crate) trait Lines {
    /// Number of lines, at least 1.
    fn line_count(&self) -> usize;
    /// The line holding byte `offset`.
    fn line_of(&self, offset: usize) -> usize;
    /// Byte offset where `line` starts.
    fn line_start(&self, line: usize) -> usize;
    /// Byte offset where `line` ends, before its newline.
    fn line_end(&self, line: usize) -> usize;
}

/// Builds a block's element. Called on every frame the block is visible.
pub type RenderBlock = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;

/// Identifies a block for [`crate::input::EditorState::set_block_height`] and friends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(u64);

impl BlockId {
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

struct Block {
    id: BlockId,
    /// Byte offset of the end of the line the block sits under.
    anchor: usize,
    height: Pixels,
    render: RenderBlock,
}

#[derive(Default)]
pub(crate) struct BlockMap {
    /// In insertion order; [`BlockLayout`] sorts by row.
    blocks: Vec<Block>,
    next_id: u64,
}

impl BlockMap {
    pub(crate) fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub(crate) fn insert(
        &mut self,
        text: &impl Lines,
        row: usize,
        height: Pixels,
        render: RenderBlock,
    ) -> BlockId {
        let id = BlockId(self.next_id);
        self.next_id += 1;
        let row = row.min(text.line_count().saturating_sub(1));
        self.blocks.push(Block {
            id,
            anchor: text.line_end(row),
            height,
            render,
        });
        id
    }

    fn get_mut(&mut self, id: BlockId) -> Option<&mut Block> {
        self.blocks.iter_mut().find(|b| b.id == id)
    }

    pub(crate) fn set_height(&mut self, id: BlockId, height: Pixels) -> bool {
        self.get_mut(id)
            .filter(|b| b.height != height)
            .map(|b| b.height = height)
            .is_some()
    }

    pub(crate) fn set_render(&mut self, id: BlockId, render: RenderBlock) -> bool {
        self.get_mut(id).map(|b| b.render = render).is_some()
    }

    pub(crate) fn remove(&mut self, id: BlockId) -> bool {
        let len = self.blocks.len();
        self.blocks.retain(|b| b.id != id);
        self.blocks.len() != len
    }

    pub(crate) fn clear(&mut self) {
        self.blocks.clear();
    }

    /// The buffer line the block sits under, or `None` once it was removed.
    pub(crate) fn row(&self, id: BlockId, text: &impl Lines) -> Option<usize> {
        let block = self.blocks.iter().find(|b| b.id == id)?;
        Some(text.line_of(block.anchor))
    }

    pub(crate) fn height(&self, id: BlockId) -> Option<Pixels> {
        self.blocks.iter().find(|b| b.id == id).map(|b| b.height)
    }

    /// Follow an edit replacing `range` of `old_text` with `new_len` bytes.
    pub(crate) fn adjust_for_edit(
        &mut self,
        old_text: &impl Lines,
        range: &Range<usize>,
        new_len: usize,
    ) {
        if self.blocks.is_empty() {
            return;
        }
        self.blocks.retain_mut(|block| {
            match adjust_anchor(old_text, block.anchor, range, new_len) {
                Some(anchor) => {
                    block.anchor = anchor;
                    true
                }
                None => false,
            }
        });
    }

    /// This frame's block geometry. `display_row_of_line` maps a buffer line to its
    /// display rows, `None` when folded away (the block is then hidden).
    pub(crate) fn layout(
        &self,
        text: &impl Lines,
        display_rows_of_line: impl Fn(usize) -> Option<Range<usize>>,
    ) -> BlockLayout {
        if self.blocks.is_empty() {
            return BlockLayout::default();
        }
        let mut placed: Vec<(usize, Pixels, BlockId, RenderBlock)> = self
            .blocks
            .iter()
            .filter_map(|b| {
                let line = text.line_of(b.anchor);
                let rows = display_rows_of_line(line)?;
                Some((rows.end.checked_sub(1)?, b.height, b.id, b.render.clone()))
            })
            .collect();
        // Stable: blocks under the same row stack in insertion order.
        placed.sort_by_key(|(row, ..)| *row);
        BlockLayout::new(placed)
    }
}

/// Where `anchor` goes when `range` of `old_text` becomes `new_len` bytes; `None`
/// removes the block.
fn adjust_anchor(
    old_text: &impl Lines,
    anchor: usize,
    range: &Range<usize>,
    new_len: usize,
) -> Option<usize> {
    if anchor <= range.start {
        return Some(anchor);
    }
    // The deletion swallows the start of the anchor's line: the line is gone.
    let line_start = old_text.line_start(old_text.line_of(anchor));
    if range.start < line_start && anchor <= range.end {
        return None;
    }
    if anchor >= range.end {
        Some(anchor - (range.end - range.start) + new_len)
    } else {
        // Inside a replacement that keeps the line's start: end of the new text.
        Some(range.start + new_len)
    }
}

/// One visible block placed for a frame.
#[derive(Clone)]
pub(crate) struct PlacedBlock {
    pub(crate) id: BlockId,
    /// The display row the block sits under.
    pub(crate) row: usize,
    pub(crate) height: Pixels,
    pub(crate) render: RenderBlock,
}

/// Block geometry for one frame, sorted by row.
#[derive(Clone, Default)]
pub(crate) struct BlockLayout {
    pub(crate) blocks: Vec<PlacedBlock>,
    /// `ends[i]` is the total height of blocks `0..=i`.
    ends: Vec<f64>,
}

impl BlockLayout {
    fn new(placed: Vec<(usize, Pixels, BlockId, RenderBlock)>) -> Self {
        let mut total = 0.0;
        let mut ends = Vec::with_capacity(placed.len());
        let blocks = placed
            .into_iter()
            .map(|(row, height, id, render)| {
                total += f64::from(f32::from(height));
                ends.push(total);
                PlacedBlock {
                    id,
                    row,
                    height,
                    render,
                }
            })
            .collect();
        Self { blocks, ends }
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Total height of the blocks above display row `row` (those under rows `< row`).
    #[inline]
    pub(crate) fn height_above(&self, row: usize) -> f64 {
        if self.blocks.is_empty() {
            return 0.0;
        }
        match self.blocks.partition_point(|b| b.row < row) {
            0 => 0.0,
            k => self.ends[k - 1],
        }
    }

    /// Whether both place the same blocks at the same rows with the same heights.
    pub(crate) fn same_geometry(&self, other: &Self) -> bool {
        self.ends == other.ends
            && self.blocks.len() == other.blocks.len()
            && self
                .blocks
                .iter()
                .zip(&other.blocks)
                .all(|(a, b)| a.row == b.row)
    }

    /// Total height of all blocks.
    pub(crate) fn total_height(&self) -> f64 {
        self.ends.last().copied().unwrap_or(0.0)
    }

    /// Top of display row `row` in content coordinates.
    #[inline]
    pub(crate) fn row_top(&self, row: usize, line_height: f64) -> f64 {
        row as f64 * line_height + self.height_above(row)
    }

    /// The display row under content y `y`: the last row whose top is at or above it.
    /// A y inside a block gives the row the block sits under.
    pub(crate) fn row_at(&self, y: f64, line_height: f64, row_count: usize) -> usize {
        if row_count == 0 || line_height <= 0.0 {
            return 0;
        }
        let last = row_count - 1;
        if self.blocks.is_empty() {
            return ((y / line_height).floor().max(0.0) as usize).min(last);
        }
        // Block tops increase with the index, so the blocks starting at or above `y`
        // are a prefix; they shift `y` up by their total height.
        let (mut lo, mut hi) = (0, self.blocks.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.block_top(mid, line_height) <= y {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let above = match lo {
            0 => 0.0,
            k => self.ends[k - 1],
        };
        // A y inside block `lo - 1` lands on that block's row.
        if lo > 0 && y < self.block_top(lo - 1, line_height) + self.block_height(lo - 1) {
            return self.blocks[lo - 1].row.min(last);
        }
        (((y - above) / line_height).floor().max(0.0) as usize).min(last)
    }

    fn block_height(&self, ix: usize) -> f64 {
        f64::from(f32::from(self.blocks[ix].height))
    }

    /// Top of block `ix` in content coordinates.
    pub(crate) fn block_top(&self, ix: usize, line_height: f64) -> f64 {
        let above = if ix == 0 { 0.0 } else { self.ends[ix - 1] };
        (self.blocks[ix].row + 1) as f64 * line_height + above
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{IntoElement as _, px};

    /// A `String` with `Lines`, standing in for the rope.
    struct Text(String);

    impl Text {
        fn starts(&self) -> Vec<usize> {
            std::iter::once(0)
                .chain(self.0.match_indices('\n').map(|(i, _)| i + 1))
                .collect()
        }
    }

    impl Lines for Text {
        fn line_count(&self) -> usize {
            self.starts().len()
        }
        fn line_of(&self, offset: usize) -> usize {
            self.starts().partition_point(|&s| s <= offset) - 1
        }
        fn line_start(&self, line: usize) -> usize {
            self.starts()[line]
        }
        fn line_end(&self, line: usize) -> usize {
            self.starts().get(line + 1).map_or(self.0.len(), |s| s - 1)
        }
    }

    fn render() -> RenderBlock {
        Rc::new(|_, _| gpui::Empty.into_any_element())
    }

    fn layout(blocks: &[(usize, f32)]) -> BlockLayout {
        BlockLayout::new(
            blocks
                .iter()
                .enumerate()
                .map(|(i, &(row, h))| (row, px(h), BlockId(i as u64), render()))
                .collect(),
        )
    }

    #[test]
    fn prefix_sums_and_row_tops() {
        // Rows are 20 px; a 100 px block under row 2 and a 50 px block under row 5.
        let l = layout(&[(2, 100.), (5, 50.)]);
        assert_eq!(l.height_above(0), 0.0);
        assert_eq!(l.height_above(2), 0.0);
        assert_eq!(l.height_above(3), 100.0);
        assert_eq!(l.height_above(6), 150.0);
        assert_eq!(l.row_top(2, 20.), 40.0);
        assert_eq!(l.row_top(3, 20.), 160.0);
        assert_eq!(l.row_top(6, 20.), 270.0);
        assert_eq!(l.total_height(), 150.0);
        assert_eq!(l.block_top(0, 20.), 60.0);
        assert_eq!(l.block_top(1, 20.), 220.0);
    }

    #[test]
    fn y_to_row_around_blocks() {
        let l = layout(&[(2, 100.), (5, 50.)]);
        assert_eq!(l.row_at(0., 20., 100), 0);
        assert_eq!(l.row_at(59., 20., 100), 2);
        // Inside the first block: the row it sits under.
        assert_eq!(l.row_at(60., 20., 100), 2);
        assert_eq!(l.row_at(159., 20., 100), 2);
        assert_eq!(l.row_at(160., 20., 100), 3);
        assert_eq!(l.row_at(219., 20., 100), 5);
        assert_eq!(l.row_at(220., 20., 100), 5);
        assert_eq!(l.row_at(270., 20., 100), 6);
        assert_eq!(l.row_at(1e9, 20., 100), 99);
        assert_eq!(l.row_at(-5., 20., 100), 0);
        // Round trip for every row.
        for row in 0..100 {
            assert_eq!(l.row_at(l.row_top(row, 20.), 20., 100), row);
        }
    }

    #[test]
    fn stacked_blocks_under_one_row() {
        let l = layout(&[(1, 30.), (1, 40.)]);
        assert_eq!(l.row_top(2, 10.), 90.0);
        assert_eq!(l.block_top(0, 10.), 20.0);
        assert_eq!(l.block_top(1, 10.), 50.0);
        assert_eq!(l.row_at(55., 10., 10), 1);
        assert_eq!(l.row_at(90., 10., 10), 2);
    }

    #[test]
    fn no_blocks_is_uniform() {
        let l = BlockLayout::default();
        assert_eq!(l.row_top(7, 20.), 140.0);
        assert_eq!(l.row_at(141., 20., 10), 7);
        assert_eq!(l.total_height(), 0.0);
    }

    fn map_with_block(text: &Text, row: usize) -> (BlockMap, BlockId) {
        let mut map = BlockMap::default();
        let id = map.insert(text, row, px(10.), render());
        (map, id)
    }

    /// Apply `range → new` to `text` and the map, return the block's row.
    fn edit(map: &mut BlockMap, text: &mut Text, range: Range<usize>, new: &str) {
        map.adjust_for_edit(text, &range, new.len());
        text.0.replace_range(range, new);
    }

    #[test]
    fn block_follows_edits() {
        let mut text = Text(String::from("a\nselect 1;\nb"));
        let (mut map, id) = map_with_block(&text, 1);
        // Typing above shifts it down.
        edit(&mut map, &mut text, 0..0, "x\n");
        assert_eq!(map.row(id, &text), Some(2));
        // Typing inside the line keeps it.
        edit(&mut map, &mut text, 4..4, "zz");
        assert_eq!(map.row(id, &text), Some(2));
        // Enter at the end of its line: the block stays under that line.
        let end = text.line_end(2);
        edit(&mut map, &mut text, end..end, "\nselect 2;");
        assert_eq!(map.row(id, &text), Some(2));
        // Deleting a line above moves it up.
        edit(&mut map, &mut text, 0..2, "");
        assert_eq!(map.row(id, &text), Some(1));
    }

    #[test]
    fn deleting_the_anchor_line_removes_the_block() {
        let mut text = Text(String::from("a\nselect 1;\nb"));
        let (mut map, id) = map_with_block(&text, 1);
        // Delete "\nselect 1;" (end of line 0 through end of line 1).
        edit(&mut map, &mut text, 1..11, "");
        assert_eq!(map.row(id, &text), None);
        assert!(map.is_empty());
    }

    #[test]
    fn emptying_the_line_keeps_the_block() {
        let mut text = Text(String::from("a\nselect 1;\nb"));
        let (mut map, id) = map_with_block(&text, 1);
        edit(&mut map, &mut text, 2..11, "");
        assert_eq!(map.row(id, &text), Some(1));
        // Joining the next line onto it keeps it under the joined line.
        edit(&mut map, &mut text, 2..3, "");
        assert_eq!(map.row(id, &text), Some(1));
    }

    #[test]
    fn layout_skips_folded_lines() {
        let text = Text(String::from("a\nb\nc"));
        let mut map = BlockMap::default();
        map.insert(&text, 0, px(10.), render());
        map.insert(&text, 2, px(20.), render());
        let l = map.layout(&text, |line| (line != 0).then(|| line..line + 1));
        assert_eq!(l.blocks.len(), 1);
        assert_eq!(l.blocks[0].row, 2);
    }
}
