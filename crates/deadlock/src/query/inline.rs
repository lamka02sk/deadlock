//! Inline results: a statement's result drawn inside the editor, under the statement's
//! last line, as one of the editor's block widgets (`vendor/gpui-base`, `insert_block`).
//! The block is a card: a header (collapse, summary, Open in panel, Export, close) over
//! the paged result grid, or a one-line form for errors, row counts and collapsed grids.

use std::time::{Duration, Instant};

use gpui_kit::component::input::EditorState;
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{Sizable as _, Size};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, CursorStyle, DragMoveEvent, Empty, Entity, EntityId,
    EventEmitter, FontWeight, InteractiveElement, IntoElement, ParentElement, Pixels, Render,
    ScrollEvent, StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity, Window,
    canvas, div, px,
};

use crate::assets::Icon;
use crate::results::{Grid, duration, grid_actions, grid_text, shown};
use crate::theme::{MONO_FONT, Palette, RADIUS, UI_FONT, ui_size};
use crate::ui::Clickable as _;
use crate::ui::{self, Kind};

/// Card margins inside the block: `.res { margin: 4px 16px 8px }`, measured from the
/// start of the text (the design's 76 px is the 60 px gutter plus 16).
const MARGIN_TOP: f32 = 4.;
const MARGIN_BOTTOM: f32 = 8.;
const MARGIN_X: f32 = 16.;
/// `.rh { min-height: 32px }`.
const HEADER: f32 = 32.;
/// `.gr { height: 28px }`.
const ROW: f32 = 28.;
/// Rows a grid shows before it scrolls inside the block.
const VISIBLE_ROWS: usize = 10;

/// What the block shows.
pub enum Body {
    /// The statement is running; the header shows the time so far and Stop.
    Running(Instant),
    /// A result set in a grid that pages as it scrolls.
    Rows(Entity<TableState<Grid>>),
    /// A statement without rows: `3 rows affected in 8 ms`.
    Done(String),
    /// The server's error code (when it has one) and message.
    Error {
        code: Option<String>,
        message: String,
    },
}

/// What the block asks its tab to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockEvent {
    Close,
    OpenInPanel,
    Stop,
    /// Export the grid in this format: all rows (`true`) or the loaded ones.
    Export(deadlock_core::export::Format, bool),
}

pub struct ResultBlock {
    pub body: Body,
    /// The statement's text when it ran, to tell when it has been edited since.
    pub statement: String,
    pub collapsed: bool,
    /// The statement was edited after it ran.
    pub stale: bool,
    /// The grid's height once the user dragged the bottom edge.
    grid_height: Option<Pixels>,
    /// SQL INSERT is offered in the export menu (not on Redis).
    pub sql: bool,
    export_open: bool,
    export_all: bool,
    /// The editor the block sits in: the wheel goes there once the grid is at its end.
    editor: WeakEntity<EditorState>,
    _ticker: Option<Task<()>>,
    _grid: Option<Subscription>,
}

impl EventEmitter<BlockEvent> for ResultBlock {}

/// The drag of a block's bottom edge.
struct Resize(EntityId);

struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

impl ResultBlock {
    /// A block for a statement that has just started.
    pub fn running(
        statement: String,
        editor: WeakEntity<EditorState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            body: Body::Done(String::new()),
            statement,
            collapsed: false,
            stale: false,
            grid_height: None,
            sql: true,
            export_open: false,
            export_all: true,
            editor,
            _ticker: None,
            _grid: None,
        };
        this.set_body(Body::Running(Instant::now()), cx);
        this
    }

    pub fn set_body(&mut self, body: Body, cx: &mut Context<Self>) {
        self._grid = match &body {
            // Rows arriving change the block's height.
            Body::Rows(t) => Some(cx.observe(t, |_, _, cx| cx.notify())),
            _ => None,
        };
        // The elapsed time ticks while the statement runs.
        self._ticker = matches!(body, Body::Running(_)).then(|| {
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(250))
                        .await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            })
        });
        self.body = body;
        cx.notify();
    }

    pub fn is_running(&self) -> bool {
        matches!(self.body, Body::Running(_))
    }

    pub fn grid(&self) -> Option<&Entity<TableState<Grid>>> {
        match &self.body {
            Body::Rows(t) => Some(t),
            _ => None,
        }
    }

    /// The block's height in the editor: the card and its margins.
    pub fn height(&self, cx: &App) -> Pixels {
        let chrome = px(MARGIN_TOP + MARGIN_BOTTOM + 2. + HEADER);
        match (&self.body, self.collapsed) {
            // The grid's top border sits above its rows.
            (Body::Rows(_), false) => chrome + px(1.) + self.grid_height(cx),
            _ => chrome,
        }
    }

    /// Header row plus up to [`VISIBLE_ROWS`] rows, or what the user dragged it to.
    fn grid_height(&self, cx: &App) -> Pixels {
        if let Some(h) = self.grid_height {
            return h;
        }
        let rows = self
            .grid()
            .map_or(0, |t| t.read(cx).delegate().row_count())
            .clamp(1, VISIBLE_ROWS);
        #[allow(clippy::cast_precision_loss)] // at most VISIBLE_ROWS
        px(ROW * (rows + 1) as f32)
    }

    fn header_text(&self, cx: &App) -> String {
        match &self.body {
            Body::Running(started) => format!("Running… {}", duration(started.elapsed())),
            Body::Rows(t) => {
                let g = t.read(cx).delegate();
                match g.sel_status() {
                    Some(s) => format!("{s} · {}", g.inline_summary()),
                    None => g.inline_summary(),
                }
            }
            Body::Done(text) => text.clone(),
            Body::Error { .. } => String::new(),
        }
    }

    /// `count(*) = 1284` for a collapsed result of one row and one column.
    fn single_value(&self, cx: &App) -> Option<String> {
        let g = self.grid()?.read(cx).delegate();
        match (g.columns(), g.rows()) {
            ([c], [row]) => Some(format!("{} = {}", c.name, shown(row.first()?))),
            _ => None,
        }
    }
}

impl ResultBlock {
    fn export_menu(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let busy = self.is_running();
        let trigger = header_button("export", Some("Export"), None, !busy, "", cx);
        let Some(grid) = self.grid().filter(|_| !busy) else {
            return crate::shell::Trigger(trigger, false, "Export this result to a file")
                .into_any_element();
        };
        let g = grid.read(cx).delegate();
        let offer = crate::export::Offer {
            loaded: g.row_count(),
            complete: g.is_complete(),
            rerun: g.source.is_some(),
            sql: self.sql,
        };
        let p = *cx.global::<Palette>();
        let (all, open) = (self.export_all, self.export_open);
        let this = cx.entity().downgrade();
        gpui_kit::component::popover::Popover::new("block-export")
            .p(px(4.))
            .anchor(gpui_kit::Anchor::TopRight)
            .open(open)
            .on_open_change(cx.listener(|this, open: &bool, _, cx| {
                this.export_open = *open;
                cx.notify();
            }))
            .trigger(crate::shell::Trigger(
                trigger,
                open,
                "Export this result to a file",
            ))
            .content(move |_, _, _| {
                let (a, b) = (this.clone(), this.clone());
                crate::export::menu(
                    offer,
                    all,
                    move |v, cx| {
                        let _ = a.update(cx, |t, cx| {
                            t.export_all = v;
                            cx.notify();
                        });
                    },
                    move |f, all, cx| {
                        let _ = b.update(cx, |t, cx| {
                            t.export_open = false;
                            cx.emit(BlockEvent::Export(f, all));
                            cx.notify();
                        });
                    },
                    &p,
                )
            })
            .into_any_element()
    }
}

/// `.b` in the result header: 24 px high, 6 px side padding.
fn header_button(
    id: &'static str,
    label: Option<&'static str>,
    i: Option<Icon>,
    enabled: bool,
    tooltip: &'static str,
    cx: &App,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let b = ui::button(id, Kind::Ghost, enabled, tooltip, cx)
        .h(px(24.))
        .px(px(6.));
    let b = match i {
        Some(i) => b.w(px(24.)).px_0().justify_center().child(ui::button_icon(
            id,
            i,
            Kind::Ghost,
            enabled,
            cx,
        )),
        None => b,
    };
    b.when_some(label, |d, l| d.child(l))
}

impl Render for ResultBlock {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *cx.global::<Palette>();
        let error = matches!(self.body, Body::Error { .. });
        let rows = matches!(self.body, Body::Rows(_));
        let expanded = rows && !self.collapsed;
        let export = expanded.then(|| self.export_menu(cx));
        let emit = |e: BlockEvent| cx.listener(move |_, _, _, cx| cx.emit(e));

        let mut header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .h(px(HEADER))
            .pl(px(4.))
            .pr(px(6.))
            .bg(if error { p.warn_bg } else { p.panel })
            .text_color(p.tx2)
            .whitespace_nowrap()
            .overflow_hidden();
        if rows {
            let (i, tip) = if self.collapsed {
                (Icon::ChevronRight, "Expand result")
            } else {
                (Icon::ChevronDown, "Collapse result")
            };
            header = header.child(
                header_button("toggle", None, Some(i), true, tip, cx).clickable(cx.listener(
                    |this, _, _, cx| {
                        this.collapsed = !this.collapsed;
                        cx.notify();
                    },
                )),
            );
        } else if !error {
            // Lines the summary up with a grid's, after the chevron's place.
            header = header.child(div().w(px(4.)));
        }
        header = match &self.body {
            Body::Error { code, message } => header
                .child(
                    div()
                        .pl(px(8.))
                        .flex_none()
                        .text_color(p.danger)
                        .font_weight(FontWeight::MEDIUM)
                        .child(code.clone().unwrap_or_else(|| "Error".into())),
                )
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_color(p.tx)
                        .child(message.clone()),
                ),
            _ => header.child(
                div()
                    .flex_none()
                    .when(expanded, |d| {
                        d.text_color(p.tx).font_weight(FontWeight::MEDIUM)
                    })
                    .child(self.header_text(cx)),
            ),
        };
        if self.collapsed
            && let Some(v) = self.single_value(cx)
        {
            header = header.child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_color(p.tx3)
                    .font_family(MONO_FONT)
                    .text_size(crate::theme::data_size())
                    .child(v),
            );
        }
        if self.stale {
            header = header.child(
                div()
                    .flex_none()
                    .text_color(p.tx3)
                    .italic()
                    .child("outdated"),
            );
        }
        header = header.child(div().flex_1());
        if self.is_running() {
            header = header.child(
                header_button(
                    "stop",
                    Some("Stop"),
                    None,
                    true,
                    "Cancel the running statement",
                    cx,
                )
                .clickable(emit(BlockEvent::Stop)),
            );
        }
        if expanded {
            header = header
                .child(
                    header_button(
                        "open-in-panel",
                        Some("Open in panel"),
                        None,
                        true,
                        "Show this result in the bottom panel",
                        cx,
                    )
                    .clickable(emit(BlockEvent::OpenInPanel)),
                )
                .children(export);
        }
        if !self.is_running() {
            header = header.child(
                header_button("close", None, Some(Icon::Close), true, "Close result", cx)
                    .clickable(emit(BlockEvent::Close)),
            );
        }
        // An edited statement's result is dimmed, not dropped.
        let header = header.when(self.stale, |d| d.opacity(0.6));

        let me = cx.entity_id();
        let grid = self.grid().filter(|_| expanded).map(|t| {
            grid_actions(grid_text(div()).key_context("Results"), t)
                .id("grid")
                .flex_1()
                .min_h_0()
                .border_t_1()
                .border_color(p.line)
                .text_color(p.tx)
                .relative()
                .child(wheel_to_editor(t.clone(), self.editor.clone()))
                .child(
                    DataTable::new(t)
                        .bordered(false)
                        .with_size(Size::Size(px(ROW)))
                        .header_row_height(px(ROW)),
                )
        });
        // On the card's bottom border and the margin under it, clear of the grid's
        // horizontal scrollbar.
        let handle = expanded.then(|| {
            div()
                .id("resize")
                .absolute()
                .left(px(MARGIN_X))
                .right(px(MARGIN_X))
                .bottom(px(1.))
                .h(px(MARGIN_BOTTOM))
                .cursor(CursorStyle::ResizeRow)
                .on_drag(Resize(me), |_, _, _, cx| cx.new(|_| DragGhost))
        });

        div()
            .id("result-block")
            .relative()
            .size_full()
            .pt(px(MARGIN_TOP))
            .pb(px(MARGIN_BOTTOM))
            .px(px(MARGIN_X))
            .font_family(UI_FONT)
            .text_size(ui_size())
            .on_drag_move(cx.listener(move |this, e: &DragMoveEvent<Resize>, _, cx| {
                if e.drag(cx).0 != me {
                    return;
                }
                // From the card's top: margin, border, header, the grid's border.
                let top = e.bounds.top() + px(MARGIN_TOP + 1. + HEADER + 1.);
                let h = (e.event.position.y - top).clamp(px(ROW * 2.), px(4000.));
                this.grid_height = Some(h);
                cx.notify();
            }))
            .child(
                div()
                    .relative()
                    .size_full()
                    .flex()
                    .flex_col()
                    .border_1()
                    .border_color(if error { p.warn_line } else { p.line })
                    .rounded(RADIUS)
                    .bg(p.bg)
                    .overflow_hidden()
                    .child(header)
                    .children(grid),
            )
            .children(handle)
    }
}

/// Painted before the grid, so it sees the wheel first: once the grid cannot scroll
/// further that way, the editor scrolls instead. The grid keeps the wheel from the
/// areas around it otherwise.
fn wheel_to_editor(
    table: Entity<TableState<Grid>>,
    editor: WeakEntity<EditorState>,
) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            window.on_mouse_event(move |e: &ScrollEvent, phase, window, cx| {
                if !phase.capture() || !bounds.contains(&e.position) {
                    return;
                }
                let delta = e.delta.pixel_delta(window.line_height());
                if delta.y.abs() < delta.x.abs() || delta.y == px(0.) {
                    return;
                }
                let (offset, max) = {
                    let handle = &table.read(cx).vertical_scroll_handle;
                    let state = handle.0.borrow();
                    (
                        state.base_handle.offset().y,
                        state.base_handle.max_offset().y,
                    )
                };
                // Offsets run from 0 (top) to -max (bottom); a positive delta scrolls up.
                let at_end = if delta.y > px(0.) {
                    offset >= px(-0.5)
                } else {
                    offset <= -max + px(0.5)
                };
                if !at_end {
                    return;
                }
                let dy = f64::from(f32::from(delta.y));
                if editor.update(cx, |e, cx| e.scroll_by_y_f64(dy, cx)).is_ok() {
                    cx.stop_propagation();
                }
            });
        },
    )
    .absolute()
    .size_full()
}

/// Splits a driver error into the server's code and its message: `ERROR 1142
/// (42000): denied` gives `Error 1142` and `denied`, `ORA-00942: …` keeps `ORA-00942`.
pub fn error_parts(text: &str) -> (Option<String>, String) {
    // mysql_async wraps it: Server error: `ERROR 1146 (42S02): Table … doesn't exist'
    let server = text.find("ERROR ").map(|i| {
        let t = &text[i..];
        match text[..i].ends_with('`') {
            true => t.strip_suffix('\'').unwrap_or(t),
            false => t,
        }
    });
    if let Some(rest) = server.and_then(|t| t.strip_prefix("ERROR "))
        && let Some((code, msg)) = rest.split_once(": ")
    {
        let number = code.split_whitespace().next().unwrap_or(code);
        if !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()) {
            return (Some(format!("Error {number}")), msg.to_owned());
        }
    }
    if let Some(i) = text.find("ORA-")
        && let Some((code, msg)) = text[i..].split_once(": ")
    {
        return (Some(code.to_owned()), msg.to_owned());
    }
    (None, text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_split_from_messages() {
        assert_eq!(
            error_parts("ERROR 1142 (42000): UPDATE command denied to user"),
            (
                Some("Error 1142".into()),
                "UPDATE command denied to user".into()
            )
        );
        assert_eq!(
            error_parts("ORA-00942: table or view does not exist"),
            (
                Some("ORA-00942".into()),
                "table or view does not exist".into()
            )
        );
        assert_eq!(
            error_parts("Server error: `ERROR 1054 (42S22): Unknown column 'x' in 'f''"),
            (
                Some("Error 1054".into()),
                "Unknown column 'x' in 'f'".into()
            )
        );
        assert_eq!(
            error_parts("connection lost"),
            (None, "connection lost".into())
        );
    }
}
