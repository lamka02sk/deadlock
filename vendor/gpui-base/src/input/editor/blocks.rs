//! Deadlock patch: block widgets between rows (see `display_map/block_map.rs`).

use gpui::{AnyElement, App, Context, Pixels, Window};
use std::rc::Rc;

use super::EditorState;
use crate::input::{BlockId, GutterWidgets};

impl EditorState {
    /// Draw elements in the gutter, each on the first display row of its buffer
    /// line, laid out in the same frame as the text (so they never trail a scroll).
    /// `render` gets the visible buffer lines and returns `(line, element)`; each
    /// element gets the gutter's width and one line's height, and sits above the
    /// text's hitbox, so it takes clicks and hovers. `None` removes them.
    pub fn set_gutter_widgets(&mut self, render: Option<GutterWidgets>, cx: &mut Context<Self>) {
        self.display_map.gutter_widgets = render;
        cx.notify();
    }

    /// Draw an element `height` tall under buffer line `row`, across the text
    /// area (not the gutter). Later rows move down by `height`.
    ///
    /// `render` builds the element on every frame the block is visible. The
    /// block follows its line as text above it changes, and is removed when an
    /// edit deletes that line ([`Self::block_row`] then returns `None`).
    pub fn insert_block(
        &mut self,
        row: usize,
        height: Pixels,
        render: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
        cx: &mut Context<Self>,
    ) -> BlockId {
        let id = self
            .display_map
            .blocks
            .insert(&self.text, row, height, Rc::new(render));
        cx.notify();
        id
    }

    /// Change a block's height.
    pub fn set_block_height(&mut self, id: BlockId, height: Pixels, cx: &mut Context<Self>) {
        if self.display_map.blocks.set_height(id, height) {
            cx.notify();
        }
    }

    /// Replace the function that builds a block's element.
    pub fn set_block_render(
        &mut self,
        id: BlockId,
        render: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.display_map.blocks.set_render(id, Rc::new(render)) {
            cx.notify();
        }
    }

    /// Remove a block. Returns whether it existed.
    pub fn remove_block(&mut self, id: BlockId, cx: &mut Context<Self>) -> bool {
        let removed = self.display_map.blocks.remove(id);
        if removed {
            cx.notify();
        }
        removed
    }

    /// Remove every block.
    pub fn clear_blocks(&mut self, cx: &mut Context<Self>) {
        if !self.display_map.blocks.is_empty() {
            self.display_map.blocks.clear();
            cx.notify();
        }
    }

    /// The buffer line a block sits under, `None` once it is gone.
    pub fn block_row(&self, id: BlockId) -> Option<usize> {
        self.display_map.blocks.row(id, &self.text)
    }

    /// Scroll the least needed to show a block and the line above it. A block
    /// taller than the editor shows its top. Does nothing before the first layout.
    pub fn scroll_to_block(&mut self, id: BlockId, cx: &mut Context<Self>) {
        let Some(line_height) = self.last_layout.as_ref().map(|l| l.line_height) else {
            return;
        };
        let line_height = f64::from(f32::from(line_height));
        let blocks = self.display_map.block_layout(&self.text);
        let Some(ix) = blocks.blocks.iter().position(|b| b.id == id) else {
            return;
        };
        let top = blocks.row_top(blocks.blocks[ix].row, line_height);
        let bottom =
            blocks.block_top(ix, line_height) + f64::from(f32::from(blocks.blocks[ix].height));
        let view = f64::from(f32::from(self.input_bounds.size.height));
        let shown = -self.scroll_y_f64();
        let to = if top < shown {
            top
        } else if bottom > shown + view {
            (bottom - view).min(top)
        } else {
            return;
        };
        self.set_scroll_y_f64(-to, cx);
    }

    /// A block's height, `None` once it is gone.
    pub fn block_height(&self, id: BlockId) -> Option<Pixels> {
        self.display_map.blocks.height(id)
    }
}
