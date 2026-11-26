# Deadlock's patches to gpui-base

Upstream: https://github.com/longbridge/gpui-component, `crates/base`, rev
`1c7e7f7c6c8d6585da909804496614da01bb8663` (the rev `gpui-kit` is pinned to in the
workspace `Cargo.toml`). The workspace `[patch]` section swaps this copy in.

To move to a new upstream rev: copy the new `crates/base/src` over `src/`, reapply the
patches below (grep for `Deadlock patch`), update the rev here and in the workspace.

## 1. Accessibility text window for multi-line editors

`src/input/base/a11y_text.rs`, `src/input/base/state.rs` (the `aria_value` call).

With an accessibility client active, the editor copied the whole document into a
`String`, split it into graphemes and built one AccessKit node per line on every
frame: 146 ms per frame at 50,000 lines. A multi-line field now exposes at most
200 lines centred on the caret (`MULTI_LINE_WINDOW`), as its value and as its text
runs; the selection is mapped into that window. Single-line fields are unchanged.

Measured with `spike_editor` (50,000 lines, scripted scroll), Xvfb + lavapipe, AT-SPI
enabled: draw CPU p50 256 ms before, 4.5 ms after.

## 2. Block widgets between editor rows

New: `src/input/editor/display_map/block_map.rs` (block list, anchors, per-frame
geometry), `src/input/editor/blocks.rs` (public API on `EditorState`). Changed:
`display_map/display_map.rs` (owns the `BlockMap`, adjusts it in
`adjust_folds_for_edit`, `block_layout`), `display_map/mod.rs` and `input/mod.rs`
(module and re-exports), `base/layout.rs` (`LastLayout::blocks`, block-aware
`local_row_y` / `display_row_y`, `block_y`), `base/element.rs` (scroll height, visible
range, scroll-to-caret, prepaint and paint of the blocks, selection outlines split at
blocks, the extra frame below), `base/state.rs` (`scroll_to` and
`character_index_for_point` use the block-aware row tops), `text_wrapper.rs` (test
layouts). About 740 added lines, 160 of them tests; grep for `Deadlock patch`.

API (`EditorState`): `insert_block(row, height, render, cx) -> BlockId`,
`set_block_height`, `set_block_render`, `remove_block`, `clear_blocks`, `block_row`,
`block_height`, `scroll_to_block`. `render` builds the block's element on each frame
it is visible; it is laid out across the text area (not the gutter), `height` tall.

How it works:

- Row geometry was `row × line_height` everywhere. Blocks sit under a display row; for
  one frame, `BlockLayout` holds the visible blocks sorted by row with the running
  total of their heights, so row → y (`row_top`) and y → row (`row_at`) are a binary
  search. `local_row_y` / `display_row_y`, which painting, carets, selections, line
  numbers, folds, gutter marks, indent guides and hit-testing already go through, add
  the blocks between the first laid-out row and the row. Scroll height, the visible
  row range and the reveal of the caret use `row_top`. Without blocks every one of
  these returns early.
- A block is anchored to the end of its line as a byte offset that follows edits:
  typing at the anchor leaves it under its line, edits before it shift it, an edit
  that deletes the start of its line removes it. Blocks under folded lines are hidden.
- Blocks are prepainted after the editor's hitbox in a box that blocks the mouse
  except the wheel, so clicks never place the caret through a block. Their element is
  inside the editor's key dispatch path, so the editor's own key bindings match while
  something in a block has focus: the box swallows every `input::` action so they do
  not edit the text.
- A notification raised while drawing does not reach observers in this GPUI, so when
  blocks change rows the editor requests one more frame; overlays positioned with
  `range_to_bounds` (Deadlock's gutter run icons) follow without a stale frame.
- Selection and frame decorations are outlined per run of touching rows, so a range
  around a block does not paint over it.

Measured with `spike_editor` (50,000 lines, scripted scroll, Xvfb + lavapipe), draw CPU
p50: 3.5 ms before the patch, 3.8–4.4 ms after with no blocks, 3.8–4.0 ms with 50
blocks of 120 px the scroll passes through (frame interval is capped at 48 ms by
lavapipe in all runs).

Known gaps: page up / down moves by the viewport's height in rows, so it skips further
when blocks are on screen; the scrollbar's markers (diagnostics, search) ignore block
heights; with soft wrap a block sits under the line's last wrapped row (untested);
block contents were not checked with an accessibility client; IME candidate placement
near blocks is untested. The block map's tests run from the
Deadlock workspace (`cargo test -p deadlock --test editor_blocks`), because this
crate's own test suite needs GPUI's git test-support build.

## 3. Roomier context menus (`vendor/gpui-component`, not gpui-base)

`vendor/gpui-component` is `crates/component` from the same rev, wired in by the
workspace `[patch]`; only `src/menu/popup_menu.rs` differs (grep `Deadlock patch`, plus
the container `p_1p5`/`min_w(px(180.))`, separator `my_1`, and `gap_2` after icons): 6 px
container padding, 30 px items with 12 px side padding, 180 px minimum width, 16 px
item icons. Its `Cargo.toml` has the workspace-inherited fields written out.

## 4. Gutter widgets and Ctrl/Cmd links without a mouse move

`src/input/editor/blocks.rs` (`EditorState::set_gutter_widgets`),
`display_map/block_map.rs` (`GutterWidgets`), `display_map/display_map.rs` (the
callback is kept on the `DisplayMap`), `base/element.rs` (`prepaint_gutter_widgets`,
painted after the fold icons; the modifiers listener in `paint_mouse_listeners`).
Grep `Deadlock patch`.

- `set_gutter_widgets(Some(render))`: on every frame the editor calls `render` with the
  visible buffer lines and lays out each returned `(line, element)` at that line's
  first display row, from this frame's layout (block-aware `local_row_y`), in a box
  the gutter's width and one line high, after the text's hitbox so it takes clicks and
  hovers. Deadlock's run ▶ icons used to be absolutely positioned overlays read from
  last frame's `range_to_bounds`, so they trailed a scroll by a frame and only caught up
  when the tab re-rendered; now they move with the text.
- Pressing or releasing Cmd/Ctrl over the text runs the mouse-move hover path with the
  pointer's position, so the definition underline shows and goes without moving the
  mouse. Modifier events reach the listener only when the editor is on the focus path.
  Not observable under Xvfb (XTEST modifier presses produce no `ModifiersChanged` there);
  the mouse-move path was checked.
