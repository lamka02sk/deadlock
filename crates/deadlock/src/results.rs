//! One result set in the bottom panel: a virtual grid over rows that arrive in pages.
//! Only the first page is fetched up front; the grid asks for the next page when the
//! user scrolls near the end of what has arrived.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::Duration;

use deadlock_core::driver::{Column, ResultStream, Row, Runtime, Value};
use deadlock_core::edit::{ColumnInfo, Edit, INLINE_LIMIT, Pending, RowId, TableInfo};
use gpui_kit::component::date_picker::{DatePicker, DatePickerState};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu};
use gpui_kit::component::table::{self, ColumnSort, TableDelegate, TableEvent, TableState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, ClipboardItem, Context, DispatchPhase, Div, Entity, Focusable as _, FontWeight,
    InteractiveElement, IntoElement, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Point, SharedString, StatefulInteractiveElement, Styled,
    Window, actions, canvas, div, px,
};
use tokio::sync::oneshot;

use crate::assets::{Icon, icon, menu_item, menu_item_danger};
use crate::theme::{Palette, mono_font, small_size, ui_font};
use crate::ui::Clickable as _;

/// The result set after this one, sent once this one has been read or dropped.
pub type NextSet = oneshot::Receiver<deadlock_core::driver::Result<ResultStream>>;

/// The grid's stream ended and a later result set may follow: the tab takes it with
/// [`Grid::close`].
pub struct Ended;

impl gpui_kit::EventEmitter<Ended> for TableState<Grid> {}

actions!(
    grid,
    [
        ExtendUp,
        ExtendDown,
        ExtendLeft,
        ExtendRight,
        SelectAll,
        CopyWithHeaders
    ]
);

/// What a grid asks its tab to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridEvent {
    /// A data column's header was clicked.
    /// With Shift held: add the column to the sort keys.
    Sort(usize, bool),
    /// A context-menu entry, on the selection.
    Act(Action),
    /// The selected cells changed: the status line follows.
    Selected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Edit,
    /// Edit a date cell with its calendar open.
    PickDate,
    SetNull,
    SetDefault,
    Delete,
    Revert,
    /// Clipboard text, tab-separated, from the selection's corner down and right.
    Paste,
    Duplicate,
    EditJson,
    CopyInsert,
}

/// A rectangle of cells: where Shift or a drag started, and the cell it reached, as
/// (display row, data column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sel {
    pub anchor: (usize, usize),
    pub head: (usize, usize),
}

impl Sel {
    pub fn cell(r: usize, c: usize) -> Self {
        Self {
            anchor: (r, c),
            head: (r, c),
        }
    }

    /// Rows `from..=to`, every one of `cols` columns.
    pub fn rows_of(from: usize, to: usize, cols: usize) -> Self {
        Self {
            anchor: (from, 0),
            head: (to, cols.saturating_sub(1)),
        }
    }

    pub fn rows(self) -> RangeInclusive<usize> {
        self.anchor.0.min(self.head.0)..=self.anchor.0.max(self.head.0)
    }

    pub fn cols(self) -> RangeInclusive<usize> {
        self.anchor.1.min(self.head.1)..=self.anchor.1.max(self.head.1)
    }

    pub fn contains(self, r: usize, c: usize) -> bool {
        self.rows().contains(&r) && self.cols().contains(&c)
    }

    pub fn count(self) -> usize {
        self.rows().count() * self.cols().count()
    }

    /// The head moved by `dr` rows and `dc` columns, kept inside `rows` × `cols`.
    pub fn extend(self, dr: isize, dc: isize, rows: usize, cols: usize) -> Self {
        let step =
            |v: usize, d: isize, n: usize| v.saturating_add_signed(d).min(n.saturating_sub(1));
        Self {
            anchor: self.anchor,
            head: (step(self.head.0, dr, rows), step(self.head.1, dc, cols)),
        }
    }
}

/// Whether a column gets the date picker, and with a time of day. The picker writes
/// the text `parse_input` reads for the type.
pub fn date_kind(col: &ColumnInfo) -> Option<bool> {
    match col.data_type.as_str() {
        "date" => Some(false),
        "datetime" | "timestamp" => Some(true),
        _ => None,
    }
}

/// Opens or closes a cell editor's calendar, as Enter on the picker does.
pub fn toggle_picker(picker: &Entity<DatePickerState>, window: &mut Window, cx: &mut App) {
    let focus = picker.read(cx).focus_handle(cx);
    focus.dispatch_action(
        &gpui_kit::base::actions::Confirm { secondary: false },
        window,
        cx,
    );
}

impl gpui_kit::EventEmitter<GridEvent> for TableState<Grid> {}

/// A table tab's editing state, drawn over the fetched rows: marks, changed cells,
/// added rows at the end, the inline editor.
pub struct EditOverlay {
    pub info: TableInfo,
    pub pending: Pending,
    /// Why nothing can be edited, if it cannot.
    pub read_only: Option<String>,
    /// The cell edited in place, its field, and a date column's picker.
    pub editing: Option<Editing>,
    /// Rows picked with Ctrl or Shift on their number, for Delete row.
    pub marked: BTreeSet<RowId>,
    anchor: Option<usize>,
    /// The data columns the order by field sorts on, in priority order, and whether
    /// each is descending.
    pub sort: Vec<(usize, bool)>,
    /// The editor's calendar opens once the editor has been drawn (Pick date…).
    pub open_picker: bool,
    /// A header click's sort is loading: [`Self::sort`] already holds it, so its
    /// arrows show at once, with a spinner until the rows arrive.
    pub sorting: bool,
}

impl EditOverlay {
    pub fn new(info: TableInfo, read_only: Option<String>) -> Self {
        Self {
            info,
            pending: Pending::default(),
            read_only,
            editing: None,
            marked: BTreeSet::new(),
            anchor: None,
            sort: Vec::new(),
            open_picker: false,
            sorting: false,
        }
    }
}

pub type Editing = (
    RowId,
    usize,
    Entity<InputState>,
    Option<Entity<DatePickerState>>,
);

/// Rows of cell values; `None` is DEFAULT (an added row's unset cell).
type Cells<'a> = Vec<Vec<Option<&'a Value>>>;

/// A statement and its bind values (`None` binds NULL).
pub type Source = (String, Vec<(String, Option<String>)>);

pub struct Grid {
    columns: Vec<Column>,
    /// Per column: right-aligned, as numbers are.
    numeric: Vec<bool>,
    rows: Vec<Row>,
    /// Rows still to come. `None` while a page is on its way or once all arrived.
    stream: Option<ResultStream>,
    fetching: bool,
    /// The stream was closed before its end: the grid holds only the rows fetched.
    pub truncated: bool,
    /// Kept for the stream's later result sets, handed over at the stream's end.
    next: Option<NextSet>,
    /// Set on a table tab's grid.
    pub edit: Option<EditOverlay>,
    /// The statement that produced the rows and its bind values, when running it again
    /// only reads: an export of all rows runs it again.
    pub source: Option<Source>,
    /// Data column widths, where the tab knows better than the default.
    pub widths: Vec<f32>,
    /// Rows before the first one shown (a table tab's earlier pages): the row
    /// numbers go on from there. Everything else keeps counting from the page.
    pub row_offset: usize,
    /// The picked cells, on this page only: a reload drops them.
    pub sel: Option<Sel>,
    /// A left drag over the cells is extending the selection.
    dragging: bool,
    /// Where the pointer is during that drag, in window coordinates.
    drag_at: Option<Point<Pixels>>,
    /// Where that drag's press was, so a click near the edge does not scroll.
    press_at: Option<Point<Pixels>>,
    /// Drawn turned (a table tab's Ctrl+T): the table's rows are the fields and its
    /// columns the records, after the field names. Selection and edits stay in
    /// (record, field) terms; [`Self::data_at`] and [`Self::display_at`] convert.
    pub transposed: bool,
    rt: Arc<Runtime>,
    took: Duration,
}

/// Engine type names that hold numbers, as `deadlock-core` reports them.
fn is_numeric(type_name: &str) -> bool {
    let t = type_name.to_ascii_uppercase();
    [
        "INT", "DECIMAL", "NUMBER", "FLOAT", "DOUBLE", "LONG", "SHORT", "TINY", "YEAR",
    ]
    .iter()
    .any(|n| t.contains(n))
}

impl Grid {
    pub fn new(
        columns: Vec<Column>,
        rows: Vec<Row>,
        stream: Option<ResultStream>,
        rt: Arc<Runtime>,
        took: Duration,
    ) -> Self {
        let numeric = columns.iter().map(|c| is_numeric(&c.type_name)).collect();
        Self {
            columns,
            numeric,
            rows,
            stream,
            fetching: false,
            truncated: false,
            next: None,
            edit: None,
            source: None,
            widths: Vec::new(),
            row_offset: 0,
            sel: None,
            dragging: false,
            drag_at: None,
            press_at: None,
            transposed: false,
            rt,
            took,
        }
    }

    /// Keeps [`Self::sel`] in step with the table's own selection (clicks, arrow
    /// keys, Escape). Call once on the grid's table.
    pub fn track(cx: &mut Context<TableState<Self>>) {
        cx.subscribe_self(|t: &mut TableState<Self>, e: &TableEvent, cx| {
            let g = t.delegate_mut();
            let n = g.columns.len();
            let whole_row = |g: &Self, r: usize| {
                // A row picked by number with Shift spans rows already.
                g.sel
                    .filter(|s| s.head.0 == r && s.cols() == (0..=n.saturating_sub(1)))
                    .unwrap_or(Sel::rows_of(r, r, n))
            };
            let next = match *e {
                TableEvent::SelectCell(r, col) => match g.data_at(r, col) {
                    Some(at) if g.sel.is_some_and(|s| s.head == at) => return,
                    Some((r, c)) => Some(Sel::cell(r, c)),
                    None if g.transposed => g.field_sel(r),
                    None => Some(whole_row(g, r)),
                },
                TableEvent::SelectRow(r) if g.transposed => g.field_sel(r),
                TableEvent::SelectRow(r) => Some(whole_row(g, r)),
                TableEvent::SelectColumn(col) if g.transposed => col
                    .checked_sub(1)
                    .filter(|rec| *rec < g.rows_count_now())
                    .map(|rec| Sel::rows_of(rec, rec, n)),
                TableEvent::SelectColumn(col) => {
                    let rows = g.rows_count(cx);
                    match g.data_col(col) {
                        Some(c) if rows > 0 => Some(Sel {
                            anchor: (0, c),
                            head: (rows - 1, c),
                        }),
                        _ => None,
                    }
                }
                TableEvent::ClearSelection => None,
                _ => return,
            };
            if n > 0 {
                g.set_sel(next, cx);
            }
        })
        .detach();
    }

    pub fn set_sel(&mut self, sel: Option<Sel>, cx: &mut Context<TableState<Self>>) {
        if self.sel != sel {
            self.sel = sel;
            cx.emit(GridEvent::Selected);
            cx.notify();
        }
    }

    /// The rows the selection covers; on a table tab the rows picked by number
    /// instead, when there are some.
    pub fn sel_rows(&self) -> Vec<RowId> {
        if let Some(m) = self
            .edit
            .as_ref()
            .map(|e| &e.marked)
            .filter(|m| !m.is_empty())
        {
            return m.iter().copied().collect();
        }
        self.sel
            .map(|s| s.rows().filter_map(|r| self.row_id(r)).collect())
            .unwrap_or_default()
    }

    /// The selected cells as column names and rows of values (`None`: DEFAULT).
    fn sel_table(&self) -> Option<(Vec<&str>, Cells<'_>)> {
        let s = self.sel?;
        let names = s.cols().map(|c| self.columns[c].name.as_str()).collect();
        let rows = s
            .rows()
            .filter_map(|r| self.row_id(r))
            .map(|id| s.cols().map(|c| self.current(id, c)).collect())
            .collect();
        Some((names, rows))
    }

    /// The selection as TSV, with a header line when asked; else the table's own
    /// selection (a whole column). A table tab's plain copy is one `a, b, c` line.
    pub fn copy(&self, selection: table::TableSelection, headers: bool) -> Option<String> {
        match self.sel_table() {
            Some((_, rows)) if !headers => Some(self.plain(&rows)),
            Some((names, rows)) => Some(tsv(Some(&names[..]), &rows)),
            None => self.copy_text(selection),
        }
    }

    /// Copy without headers: TSV, or on a table tab every value on one line.
    fn plain(&self, rows: &[Vec<Option<&Value>>]) -> String {
        if self.edit.is_none() {
            return tsv(None, rows);
        }
        rows.iter()
            .flatten()
            .map(|v| v.map_or_else(String::new, tsv_cell))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// `6 cells selected · sum 21 · avg 3.5`, for two cells and more. The sum and
    /// average take the numeric columns' values.
    pub fn sel_status(&self) -> Option<String> {
        let s = self.sel.filter(|s| s.count() > 1)?;
        let (mut n, mut sum) = (0u32, 0f64);
        for id in s.rows().filter_map(|r| self.row_id(r)) {
            for c in s.cols().filter(|c| self.numeric[*c]) {
                if let Some(x) = self.current(id, c).and_then(number) {
                    n += 1;
                    sum += x;
                }
            }
        }
        let mut out = format!("{} cells selected", s.count());
        if n > 0 {
            out.push_str(&format!(
                " · sum {} · avg {}",
                short_number(sum),
                short_number(sum / f64::from(n))
            ));
        }
        Some(out)
    }

    /// A left press on data cell (`row`, `c`): Shift extends from the anchor, else a
    /// new selection starts there and follows a drag.
    fn press(&mut self, row: usize, c: usize, shift: bool, cx: &mut Context<TableState<Self>>) {
        let sel = match self.sel {
            Some(s) if shift => Sel {
                anchor: s.anchor,
                head: (row, c),
            },
            _ => Sel::cell(row, c),
        };
        self.dragging = !shift;
        self.set_sel(Some(sel), cx);
    }

    /// The pointer moved over data cell (`row`, `c`).
    fn drag(&mut self, row: usize, c: usize, held: bool, cx: &mut Context<TableState<Self>>) {
        if !held {
            self.dragging = false;
        }
        if let (true, Some(s)) = (self.dragging, self.sel) {
            self.set_sel(
                Some(Sel {
                    anchor: s.anchor,
                    head: (row, c),
                }),
                cx,
            );
        }
    }

    /// Data column `c`'s width until the user sets one.
    pub fn default_width(&self, c: usize) -> f32 {
        let default: f32 = if self.numeric[c] { 110. } else { 190. };
        // Room for the name and, on a sorted column, its arrow and key number.
        let sort_slot = self
            .edit
            .as_ref()
            .filter(|e| e.sort.iter().any(|(s, _)| *s == c))
            .map_or(0., |e| if e.sort.len() > 1 { 32. } else { 22. });
        #[allow(clippy::cast_precision_loss)] // a width estimate
        let fit = 24. + 7. * self.columns[c].name.chars().count() as f32 + sort_slot;
        default.max(fit.min(320.))
    }

    /// Columns before the data: the row number, and on a table tab the mark. Turned,
    /// the field names.
    fn lead(&self) -> usize {
        if self.transposed {
            1
        } else if self.edit.is_some() {
            2
        } else {
            1
        }
    }

    /// The (record, field) shown in table cell (`row`, `col`), if it shows a value.
    pub fn data_at(&self, row: usize, col: usize) -> Option<(usize, usize)> {
        if self.transposed {
            let rec = col.checked_sub(1)?;
            (row < self.columns.len() && rec < self.rows_count_now()).then_some((rec, row))
        } else {
            self.data_col(col).map(|c| (row, c))
        }
    }

    /// The table cell that shows (record, field).
    pub fn display_at(&self, rec: usize, field: usize) -> (usize, usize) {
        if self.transposed {
            (field, rec + 1)
        } else {
            (rec, self.grid_col(field))
        }
    }

    /// Every record of field `f`: a turned grid's row.
    fn field_sel(&self, f: usize) -> Option<Sel> {
        let last = self.rows_count_now().checked_sub(1)?;
        Some(Sel {
            anchor: (0, f),
            head: (last, f),
        })
    }

    /// The data column under grid column `col`.
    pub fn data_col(&self, col: usize) -> Option<usize> {
        col.checked_sub(self.lead())
            .filter(|c| *c < self.columns.len())
    }

    /// Grid column of data column `c`.
    pub fn grid_col(&self, c: usize) -> usize {
        c + self.lead()
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// The fetched rows.
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// The row drawn at `display`: fetched rows first, then added ones.
    pub fn row_id(&self, display: usize) -> Option<RowId> {
        if display < self.rows.len() {
            return Some(RowId::Loaded(display));
        }
        let (id, _) = self
            .edit
            .as_ref()?
            .pending
            .added()
            .nth(display - self.rows.len())?;
        Some(RowId::Added(id))
    }

    pub fn display_row(&self, row: RowId) -> Option<usize> {
        match row {
            RowId::Loaded(r) => (r < self.rows.len()).then_some(r),
            RowId::Added(id) => self
                .edit
                .as_ref()?
                .pending
                .added()
                .position(|(i, _)| i == id)
                .map(|p| p + self.rows.len()),
        }
    }

    /// A fetched row's value as the server sent it.
    pub fn original(&self, row: RowId, col: usize) -> Option<&Value> {
        match row {
            RowId::Loaded(r) => self.rows.get(r)?.get(col),
            RowId::Added(_) => None,
        }
    }

    /// A cell as a submit would leave it: the pending value, else the fetched one.
    /// `None` for DEFAULT, or an added row's missing cell.
    pub fn current(&self, row: RowId, c: usize) -> Option<&Value> {
        match self.edit.as_ref().and_then(|e| e.pending.cell(row, c)) {
            Some(Edit::Value(v)) => Some(v),
            Some(Edit::Default) => None,
            None => self.original(row, c),
        }
    }

    /// Replaces the rows in place (a reload after a submit or a page change),
    /// keeping the scroll position. The selection pointed into the old rows.
    pub fn set_rows(&mut self, rows: Vec<Row>, stream: Option<ResultStream>) {
        self.rows = rows;
        self.stream = stream;
        self.fetching = false;
        self.truncated = false;
        self.sel = None;
    }

    /// A click on a row number: plain picks the row, Ctrl toggles it, Shift extends.
    fn mark_click(&mut self, display: usize, m: Modifiers, secondary: bool) {
        let Some(row) = self.row_id(display) else {
            return;
        };
        let ids: Vec<RowId> = match (m.shift, self.edit.as_ref().and_then(|e| e.anchor)) {
            (true, Some(a)) => (a.min(display)..=a.max(display))
                .filter_map(|r| self.row_id(r))
                .collect(),
            _ => Vec::new(),
        };
        let n = self.columns.len();
        if m.shift
            && let Some(a) = self.edit.as_ref().and_then(|e| e.anchor)
        {
            self.sel = Some(Sel::rows_of(a, display, n));
        }
        let Some(e) = self.edit.as_mut() else { return };
        if m.shift && !ids.is_empty() {
            e.marked = ids.into_iter().collect();
        } else if secondary {
            if !e.marked.remove(&row) {
                e.marked.insert(row);
            }
            e.anchor = Some(display);
        } else {
            e.marked = BTreeSet::from([row]);
            e.anchor = Some(display);
        }
    }

    pub fn set_next(&mut self, next: Option<NextSet>) {
        self.next = next;
    }

    /// Whether more rows can still arrive.
    pub fn is_open(&self) -> bool {
        self.stream.is_some() || self.fetching
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Whether the end of the rows has arrived.
    pub fn is_complete(&self) -> bool {
        !self.is_open() && !self.truncated
    }

    /// Stops fetching: the server stops sending once the stream is gone, and the
    /// session is free for the next statement. Returns the later sets' receiver.
    pub fn close(&mut self) -> Option<NextSet> {
        if self.is_open() {
            self.stream = None;
            self.fetching = false;
            self.truncated = true;
        }
        self.next.take()
    }

    /// `214 rows in 38 ms`, with a `+` while more rows may follow.
    pub fn summary(&self) -> String {
        let more = if self.is_open() || self.truncated {
            "+"
        } else {
            ""
        };
        let n = self.rows.len();
        let noun = if n == 1 && more.is_empty() {
            "row"
        } else {
            "rows"
        };
        format!("{n}{more} {noun} in {}", duration(self.took))
    }

    /// As [`Self::summary`], but a result that stopped at its first pages says so, for
    /// an inline result: `first 500 rows in 12 ms`.
    pub fn inline_summary(&self) -> String {
        if self.truncated && !self.is_open() {
            format!("first {} rows in {}", self.rows.len(), duration(self.took))
        } else {
            self.summary()
        }
    }

    fn cell(&self, row: usize, col: usize) -> Option<&Value> {
        self.rows.get(row)?.get(self.data_col(col)?)
    }

    fn tsv_row(&self, row: usize) -> String {
        self.rows.get(row).map_or_else(String::new, |r| {
            r.iter().map(tsv_cell).collect::<Vec<_>>().join("\t")
        })
    }

    /// The selection as tab-separated text: a cell, a row, or a column of the rows
    /// fetched so far.
    pub fn copy_text(&self, selection: table::TableSelection) -> Option<String> {
        use table::TableSelection as S;
        match selection {
            S::None => None,
            S::Row(r) => Some(self.tsv_row(r)),
            S::Cell(r, c) if c < self.lead() => Some(self.tsv_row(r)),
            S::Cell(r, c) => self.cell(r, c).map(tsv_cell),
            S::Column(c) if c < self.lead() => None,
            S::Column(c) => Some(
                (0..self.rows.len())
                    .filter_map(|r| self.cell(r, c).map(tsv_cell))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        }
    }
}

impl Grid {
    /// A cell of a table tab's grid: the mark, the row number, or a value with its
    /// pending state.
    fn render_edit_td(
        &self,
        row: usize,
        col: usize,
        cx: &mut Context<TableState<Self>>,
    ) -> gpui_kit::AnyElement {
        let p = *cx.global::<Palette>();
        let cell = div()
            .size_full()
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap();
        // Turned: the first column names the fields, the others are the records.
        let (rec, data) = if self.transposed {
            if col == 0 {
                return self.render_field_name(row, cx);
            }
            (col - 1, Some(row).filter(|f| *f < self.columns.len()))
        } else {
            (row, self.data_col(col))
        };
        let (Some(e), Some(id)) = (self.edit.as_ref(), self.row_id(rec)) else {
            return cell.into_any_element();
        };
        let deleted = e.pending.is_deleted(id);
        let added = matches!(id, RowId::Added(_));
        let marked = e.marked.contains(&id);
        let row_bg = if marked {
            Some(p.sel)
        } else if added {
            Some(p.warn_bg)
        } else {
            None
        };
        let cell = cell.when_some(row_bg, |d, bg| d.bg(bg));
        if col == 0 && !self.transposed {
            let mark = e.pending.mark(id);
            let tip = match mark {
                Some('M') => "Modified",
                Some('D') => "Deleted",
                Some('A') => "Added",
                _ => "",
            };
            let cell = cell
                .id(("mark", row))
                .justify_center()
                .text_color(p.warn)
                .font_weight(FontWeight::MEDIUM)
                .when(!tip.is_empty(), |d| {
                    d.tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                })
                .children(mark.map(|m| m.to_string()));
            return self.interactive(cell, row, col, cx);
        }
        // The mark stays unstruck; the rest of a deleted row is struck and dim.
        let cell = cell.when(deleted, |d| d.text_color(p.tx3).line_through());
        let Some(c) = data else {
            let cell = cell
                .justify_end()
                .pr(px(8.))
                .text_color(p.tx3)
                .child((self.row_offset + row + 1).to_string())
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |t, ev: &MouseDownEvent, _, cx| {
                        let secondary = ev.modifiers.secondary(cx);
                        t.delegate_mut().mark_click(row, ev.modifiers, secondary);
                        cx.emit(GridEvent::Selected);
                        cx.notify();
                    }),
                );
            return self.interactive(cell, row, col, cx);
        };
        let right = self.numeric[c];
        if let Some((_, _, input, picker)) = e
            .editing
            .as_ref()
            .filter(|(r, ec, ..)| *r == id && *ec == c)
        {
            return self.render_editor(cell, c, input, picker.as_ref(), cx);
        }
        let value = self.current(id, c);
        let changed = e.pending.cell(id, c).is_some() && !added;
        let in_sel = self
            .sel
            .is_some_and(|s| s.count() > 1 && s.contains(rec, c));
        let cell = cell
            .px(px(12.))
            // A line between columns: turned, a record's fields differ in alignment.
            .border_r_1()
            .border_color(p.line)
            .when(right, |d| d.justify_end())
            .when(changed && !deleted, |d| {
                d.bg(p.warn_bg)
                    .text_color(p.warn)
                    .font_weight(FontWeight::MEDIUM)
            })
            .when(in_sel, |d| d.bg(p.sel));
        let dim = |d: gpui_kit::Div, text: SharedString| {
            d.when(!changed, |d| d.text_color(p.tx3))
                .italic()
                .child(text)
        };
        let cell = match value {
            None => dim(cell, "default".into()),
            Some(Value::Null) => dim(cell, null_text(cx)),
            Some(v) => cell.child(div().overflow_hidden().text_ellipsis().child(shown(v))),
        };
        self.interactive(cell, row, col, cx)
    }

    /// A turned grid's field name, as the grid's column header: its type in the
    /// tooltip, a click sorts by it (Shift adds it to the sort), Ctrl (Cmd) picks the
    /// whole field.
    fn render_field_name(
        &self,
        f: usize,
        cx: &mut Context<TableState<Self>>,
    ) -> gpui_kit::AnyElement {
        let p = *cx.global::<Palette>();
        let Some(column) = self.columns.get(f) else {
            return div().into_any_element();
        };
        let sorted = self.edit.as_ref().and_then(|e| {
            let i = e.sort.iter().position(|(s, _)| *s == f)?;
            Some((e.sort[i].1, (e.sort.len() > 1).then_some(i + 1)))
        });
        let tip: SharedString = column.type_name.clone().into();
        let cell = div()
            .id(("field", f))
            .size_full()
            .flex()
            .items_center()
            .gap(px(4.))
            .px(px(12.))
            .overflow_hidden()
            .whitespace_nowrap()
            .font_family(ui_font())
            .font_weight(FontWeight::MEDIUM)
            .text_size(small_size())
            .text_color(if sorted.is_some() { p.tx } else { p.tx2 })
            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            .on_click(cx.listener(move |t, e: &gpui_kit::ClickEvent, _, cx| {
                cx.stop_propagation();
                if e.modifiers().secondary(cx) {
                    let sel = t.delegate().field_sel(f);
                    t.delegate_mut().set_sel(sel, cx);
                } else {
                    cx.emit(GridEvent::Sort(f, e.modifiers().shift));
                }
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(column.name.clone()),
            )
            .when_some(sorted, |d, (desc, n)| {
                d.child(sort_mark(f, desc, n, self.sorting(), &p))
            });
        self.interactive(cell, f, 0, cx)
    }

    /// The inline editor in its cell: the field, and on a date column a calendar
    /// button (and NULL when the column takes it) in fixed slots at the right.
    fn render_editor(
        &self,
        cell: Div,
        c: usize,
        input: &Entity<InputState>,
        picker: Option<&Entity<DatePickerState>>,
        cx: &mut Context<TableState<Self>>,
    ) -> gpui_kit::AnyElement {
        let p = *cx.global::<Palette>();
        let right = self.numeric[c];
        let nullable = self
            .edit
            .as_ref()
            .and_then(|e| e.info.columns.get(c))
            .is_some_and(|col| col.nullable);
        let this = cx.entity();
        let slot = |id: &'static str, tip: SharedString| {
            div()
                .id(id)
                .flex_none()
                .h(px(22.))
                .min_w(px(22.))
                .mr(px(1.))
                .px(px(3.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.))
                .hover(move |s| s.bg(p.hover))
                .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
        };
        let (pick, null) = match picker {
            Some(picker) => {
                let picker = picker.clone();
                let pick = slot(
                    "pick-date",
                    format!("Pick date ({})", crate::keymap::label("pick-date", cx)).into(),
                )
                .child(icon(Icon::Calendar, p.link))
                .clickable(move |_, window, cx| toggle_picker(&picker, window, cx));
                let null = nullable.then(|| {
                    slot(
                        "pick-null",
                        format!("Set NULL ({})", crate::keymap::label("set-null", cx)).into(),
                    )
                    .font_family(ui_font())
                    .text_size(px(10.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(p.tx2)
                    .child("NULL")
                    .clickable(move |_, _, cx| {
                        this.update(cx, |_, cx| cx.emit(GridEvent::Act(Action::SetNull)));
                    })
                });
                (Some(pick), null)
            }
            None => (None, None),
        };
        cell.relative()
            .px(px(3.))
            // The picker itself sits under the field, hidden: it anchors the
            // calendar to the cell and owns its keys; the button above opens it.
            .when_some(picker, |d, picker| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(3.))
                        .right(px(3.))
                        .flex()
                        .items_center()
                        .child(DatePicker::new(picker).appearance(false).h(px(26.))),
                )
            })
            .child(
                div()
                    .key_context("CellEditor")
                    // The hidden picker underneath takes no clicks.
                    .occlude()
                    .w_full()
                    .h(px(26.))
                    .border_1()
                    .border_color(p.blue)
                    .rounded(px(4.))
                    .bg(p.bg)
                    .flex()
                    .items_center()
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(input)
                                .appearance(false)
                                .h(px(24.))
                                .px(px(8.))
                                .when(right, |i| i.text_right()),
                        ),
                    )
                    .children(null)
                    .children(pick),
            )
            .into_any_element()
    }

    /// Mouse handling shared by the body cells: a left press, Shift+click or a drag
    /// picks data cells; a right click opens the menu on the selection.
    fn interactive<E>(
        &self,
        el: E,
        row: usize,
        col: usize,
        cx: &mut Context<TableState<Self>>,
    ) -> gpui_kit::AnyElement
    where
        E: InteractiveElement + ParentElement + Styled + IntoElement + 'static,
    {
        let el = match self.data_at(row, col) {
            Some((r, c)) => el
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |t, e: &MouseDownEvent, _, cx| {
                        t.delegate_mut().press_at = Some(e.position);
                        t.delegate_mut().press(r, c, e.modifiers.shift, cx);
                    }),
                )
                .on_mouse_move(cx.listener(move |t, e: &MouseMoveEvent, _, cx| {
                    let held = e.pressed_button == Some(MouseButton::Left);
                    t.delegate_mut().drag(r, c, held, cx);
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |t, _: &MouseUpEvent, _, cx| {
                        // A drag ends off its first cell, so no click selects:
                        // the table's own cursor goes to where it ended.
                        let g = t.delegate_mut();
                        let dragged =
                            std::mem::take(&mut g.dragging) && g.sel.is_some_and(|s| s.count() > 1);
                        if dragged {
                            t.set_selected_cell(row, col, cx);
                        }
                    }),
                ),
            None => el,
        };
        let view = cx.entity();
        el.context_menu(move |menu, _, cx| {
            view.update(cx, |t, cx| Self::menu_at(t, row, col, menu, cx))
        })
        .into_any_element()
    }

    /// The menu for a right click on grid cell (`row`, `col`): on the selection when
    /// the cell is in it, else on the cell (or the row, from its number) alone.
    fn menu_at(
        t: &mut TableState<Self>,
        row: usize,
        col: usize,
        menu: PopupMenu,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let g = t.delegate();
        let n = g.columns.len();
        if g.transposed {
            // A value: as on the grid. A field's name: the whole field.
            match g.data_at(row, col) {
                Some((r, c)) if !g.sel.is_some_and(|s| s.contains(r, c)) => {
                    t.set_selected_cell(row, col, cx);
                    t.delegate_mut().set_sel(Some(Sel::cell(r, c)), cx);
                }
                None if !g.sel.is_some_and(|s| s.cols().contains(&row)) => {
                    let sel = g.field_sel(row);
                    t.delegate_mut().set_sel(sel, cx);
                }
                _ => {}
            }
            return t.delegate().build_menu(menu, cx);
        }
        let marked = g
            .row_id(row)
            .zip(g.edit.as_ref())
            .is_some_and(|(id, e)| e.marked.contains(&id));
        match g.data_col(col) {
            Some(c) if !g.sel.is_some_and(|s| s.contains(row, c)) => {
                t.set_selected_cell(row, col, cx);
                t.delegate_mut().set_sel(Some(Sel::cell(row, c)), cx);
            }
            None if !marked && !g.sel.is_some_and(|s| s.rows().contains(&row)) => {
                t.set_selected_row(row, cx);
                let g = t.delegate_mut();
                if let Some(e) = &mut g.edit {
                    e.marked.clear();
                }
                g.set_sel(Some(Sel::rows_of(row, row, n)), cx);
            }
            _ => {}
        }
        t.delegate().build_menu(menu, cx)
    }

    fn build_menu(&self, menu: PopupMenu, cx: &mut Context<TableState<Self>>) -> PopupMenu {
        let Some(s) = self.sel else { return menu };
        let this = cx.entity();
        let act = |a: Action| {
            let this = this.clone();
            move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                this.update(cx, |_, cx| cx.emit(GridEvent::Act(a)));
            }
        };
        let select = |sel: Sel| {
            let this = this.clone();
            move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                this.update(cx, |t, cx| t.delegate_mut().set_sel(Some(sel), cx));
            }
        };
        let copy = |text: String| {
            move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            }
        };
        let can = self.edit.as_ref().is_some_and(|e| e.read_only.is_none());
        let one = s.count() == 1;
        let (hr, hc) = s.head;
        let head = self.row_id(hr).and_then(|id| self.current(id, hc));
        let json =
            one && matches!(head, Some(Value::Text(t)) if deadlock_core::edit::is_json_doc(t));
        let date = one
            && self
                .edit
                .as_ref()
                .and_then(|e| e.info.columns.get(hc))
                .is_some_and(|c| date_kind(c).is_some());
        let rows = self.sel_rows().len();
        let plural = |one: &str, many: &str| {
            if rows == 1 {
                one.to_owned()
            } else {
                format!("{many} {rows} rows")
            }
        };
        let menu = if can {
            menu.when(one, |m| {
                m.item(menu_item("Edit cell…", Icon::Rename).on_click(act(Action::Edit)))
            })
            .when(date, |m| {
                m.item(menu_item("Pick date…", Icon::Calendar).on_click(act(Action::PickDate)))
            })
            .when(json, |m| {
                m.item(menu_item("Edit JSON…", Icon::Json).on_click(act(Action::EditJson)))
            })
            .item(menu_item("Paste", Icon::Paste).on_click(act(Action::Paste)))
            .item(menu_item("Set NULL", Icon::SetNull).on_click(act(Action::SetNull)))
            .item(menu_item("Set DEFAULT", Icon::SetDefault).on_click(act(Action::SetDefault)))
            .separator()
            .item(
                menu_item(plural("Duplicate row", "Duplicate"), Icon::Duplicate)
                    .on_click(act(Action::Duplicate)),
            )
            .item(
                menu_item_danger(plural("Delete row", "Delete"), Icon::Trash, cx)
                    .on_click(act(Action::Delete)),
            )
            .item(
                menu_item(
                    plural("Undo changes to this row", "Undo changes to"),
                    Icon::Rollback,
                )
                .on_click(act(Action::Revert)),
            )
            .separator()
        } else {
            menu
        };
        let (names, cells) = self.sel_table().unwrap_or_default();
        let menu = menu
            .item(menu_item("Copy", Icon::Copy).on_click(copy(self.plain(&cells))))
            .item(
                menu_item("Copy with headers", Icon::Copy)
                    .on_click(copy(tsv(Some(&names), &cells))),
            )
            .item(menu_item("Copy as CSV", Icon::Copy).on_click(copy(csv(&names, &cells))))
            .item(menu_item("Copy as JSON", Icon::Json).on_click(copy(json_rows(&names, &cells))))
            .when(self.edit.is_some(), |m| {
                m.item(menu_item("Copy as INSERT", Icon::Copy).on_click(act(Action::CopyInsert)))
            });
        let n = self.columns.len();
        let last = self.rows_count_now().saturating_sub(1);
        let (r0, r1) = (*s.rows().start(), *s.rows().end());
        let (c0, c1) = (*s.cols().start(), *s.cols().end());
        menu.separator()
            .item(
                menu_item("Select row", Icon::SelectAll).on_click(select(Sel::rows_of(r0, r1, n))),
            )
            .item(
                menu_item("Select column", Icon::SelectAll).on_click(select(Sel {
                    anchor: (0, c0),
                    head: (last, c1),
                })),
            )
            .item(
                menu_item("Select all", Icon::SelectAll).on_click(select(Sel::rows_of(0, last, n))),
            )
    }

    /// A header click's sort is loading.
    fn sorting(&self) -> bool {
        self.edit.as_ref().is_some_and(|e| e.sorting)
    }

    /// Rows drawn: fetched plus added.
    pub fn rows_count_now(&self) -> usize {
        self.rows.len() + self.edit.as_ref().map_or(0, |e| e.pending.added_len())
    }
}

/// A sorted column's mark: its arrow, its place among several sort keys, and while
/// the sorted rows load a spinner.
fn sort_mark(c: usize, desc: bool, n: Option<usize>, loading: bool, p: &Palette) -> Div {
    use gpui_kit::{Animation, AnimationExt as _, Transformation, percentage};
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(2.))
        .child(icon(
            if desc { Icon::SortDesc } else { Icon::SortAsc },
            p.link,
        ))
        .children(n.map(|n| {
            div()
                .text_size(px(10.))
                .text_color(p.link)
                .child(n.to_string())
        }))
        .when(loading, |d| {
            d.child(icon(Icon::Spinner, p.link).size(px(12.)).with_animation(
                ("sort-loading", c),
                Animation::new(std::time::Duration::from_millis(800)).repeat(),
                |s, d| s.with_transformation(Transformation::rotate(percentage(d))),
            ))
        })
}

/// Puts the table's cursor on (record, field) and scrolls it into view, turned or not.
pub fn show_cell(
    t: &mut TableState<Grid>,
    rec: usize,
    field: usize,
    cx: &mut Context<TableState<Grid>>,
) {
    let (row, col) = t.delegate().display_at(rec, field);
    t.set_selected_cell(row, col, cx);
    t.scroll_to_row(row, cx);
    t.scroll_to_col(col, cx);
}

/// Extends the selection's head by a step, moving the table's cursor with it.
fn extend(t: &mut TableState<Grid>, dr: isize, dc: isize, cx: &mut Context<TableState<Grid>>) {
    let g = t.delegate_mut();
    let (rows, cols) = (g.rows_count_now(), g.columns.len());
    let Some(s) = g.sel else { return };
    // Turned, Up and Down step through the fields, Left and Right through the records.
    let (dr, dc) = if g.transposed { (dc, dr) } else { (dr, dc) };
    let s = s.extend(dr, dc, rows, cols);
    g.set_sel(Some(s), cx);
    let (row, col) = g.display_at(s.head.0, s.head.1);
    t.set_selected_cell(row, col, cx);
    t.scroll_to_row(row, cx);
    t.scroll_to_col(col, cx);
}

/// The keys every grid takes, on the element around its `DataTable`: Copy, Copy
/// with headers, Select all, Shift+arrows.
pub fn grid_actions(d: Div, table: &Entity<TableState<Grid>>) -> Div {
    let on = |f: fn(&mut TableState<Grid>, &mut Context<TableState<Grid>>)| {
        let table = table.clone();
        move |cx: &mut App| table.update(cx, f)
    };
    let copy = |headers: bool| {
        let table = table.clone();
        move |cx: &mut App| {
            let t = table.read(cx);
            if let Some(text) = t.delegate().copy(t.selection(), headers) {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
        }
    };
    let (up, down, left, right) = (
        on(|t, cx| extend(t, -1, 0, cx)),
        on(|t, cx| extend(t, 1, 0, cx)),
        on(|t, cx| extend(t, 0, -1, cx)),
        on(|t, cx| extend(t, 0, 1, cx)),
    );
    let all = on(|t, cx| {
        let g = t.delegate_mut();
        let (rows, n) = (g.rows_count_now(), g.columns.len());
        if rows > 0 && n > 0 {
            g.set_sel(Some(Sel::rows_of(0, rows - 1, n)), cx);
        }
    });
    let (plain, headers) = (copy(false), copy(true));
    d.child(drag_scroll(table))
        .on_action(move |_: &crate::query::CopySelection, _, cx| plain(cx))
        .on_action(move |_: &CopyWithHeaders, _, cx| headers(cx))
        .on_action(move |_: &SelectAll, _, cx| all(cx))
        .on_action(move |_: &ExtendUp, _, cx| up(cx))
        .on_action(move |_: &ExtendDown, _, cx| down(cx))
        .on_action(move |_: &ExtendLeft, _, cx| left(cx))
        .on_action(move |_: &ExtendRight, _, cx| right(cx))
}

/// While a drag picks cells, a pointer above or below the rows scrolls them and takes
/// the selection to the edge row, frame after frame until it comes back or lets go.
fn drag_scroll(table: &Entity<TableState<Grid>>) -> impl IntoElement {
    let table = table.clone();
    canvas(
        |_, _, _| {},
        move |_, (), window, cx| {
            let t = table.clone();
            window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                t.update(cx, |t, cx| {
                    let g = t.delegate_mut();
                    if !g.dragging {
                        return;
                    }
                    if e.pressed_button == Some(MouseButton::Left) {
                        g.drag_at = Some(e.position);
                    } else {
                        g.dragging = false;
                        g.drag_at = None;
                    }
                    cx.notify();
                });
            });
            let t = table.clone();
            window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                t.update(cx, |t, cx| {
                    let g = t.delegate_mut();
                    g.drag_at = None;
                    // Let go off the rows: the cursor goes where the selection ended.
                    if std::mem::take(&mut g.dragging)
                        && let Some((r, c)) = g.sel.map(|s| s.head)
                    {
                        let gc = g.grid_col(c);
                        t.set_selected_cell(r, gc, cx);
                    }
                });
            });
            if table.update(cx, autoscroll) {
                window.request_animation_frame();
            }
        },
    )
    .absolute()
    .size_0()
}

/// One frame of [`drag_scroll`]: whether the pointer is past the rows or the columns.
fn autoscroll(t: &mut TableState<Grid>, cx: &mut Context<TableState<Grid>>) -> bool {
    let g = t.delegate();
    let Some(at) = g.drag_at.filter(|_| g.dragging) else {
        return false;
    };
    // How far past the near edge `at` is, zero inside.
    let past = |at: Pixels, lo: Pixels, hi: Pixels| {
        if at < lo {
            at - lo
        } else if at > hi {
            at - hi
        } else {
            px(0.)
        }
    };
    // Faster the further out, at most 64 px a frame.
    let step = |past: Pixels| (past / 2.).clamp(px(-64.), px(64.));
    let v = t.vertical_scroll_handle.0.borrow().base_handle.clone();
    let body = v.bounds();
    let past_y = past(at.y, body.top(), body.bottom());
    let h = t.horizontal_scroll_handle.base_handle().clone();
    let cols = h.bounds();
    // Sideways the grid often ends at the screen's edge, where the pointer cannot get
    // past it: scrolling starts this far inside instead.
    // Only once the pointer moved sideways, so a click there stays a click.
    let moved = g.press_at.is_none_or(|p| (at.x - p.x).abs() > px(8.));
    let edge = if moved { px(32.) } else { px(0.) };
    let past_x = past(at.x, cols.left() + edge, cols.right() - edge);
    if past_y == px(0.) && past_x == px(0.) {
        return false;
    }
    let (mut row, mut col) = (None, None);
    if past_y != px(0.) {
        let mut offset = v.offset();
        offset.y = (offset.y - step(past_y)).clamp(-v.max_offset().y, px(0.));
        v.set_offset(offset);
        let rows = t.visible_range().rows().clone();
        row = Some(if past_y < px(0.) {
            rows.start
        } else {
            rows.end.saturating_sub(1)
        });
    }
    if past_x != px(0.) {
        let mut offset = h.offset();
        offset.x = (offset.x - step(past_x)).clamp(-h.max_offset().x, px(0.));
        h.set_offset(offset);
        // Ranges over the scrollable columns, which are the data columns.
        let cols = t.visible_range().cols().clone();
        col = Some(if past_x < px(0.) {
            cols.start
        } else {
            cols.end.saturating_sub(1)
        });
    }
    let g = t.delegate_mut();
    if let Some(s) = g.sel {
        // The edges in view are table rows and columns: turned, fields and records.
        let (rec, field) = if g.transposed { (col, row) } else { (row, col) };
        let rec = rec
            .unwrap_or(s.head.0)
            .min(g.rows_count_now().saturating_sub(1));
        let field = field
            .unwrap_or(s.head.1)
            .min(g.columns.len().saturating_sub(1));
        g.set_sel(
            Some(Sel {
                anchor: s.anchor,
                head: (rec, field),
            }),
            cx,
        );
    }
    cx.notify();
    true
}

/// A number cell's value, for the selection's sum.
#[allow(clippy::cast_precision_loss)] // a status line sum
fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int(n) => Some(*n as f64),
        Value::UInt(n) => Some(*n as f64),
        Value::Float(f) => Some(*f),
        Value::Decimal(d) => d.parse().ok(),
        _ => None,
    }
}

/// `21`, `3.5`, `0.3`: at most six decimals, no trailing zeros.
fn short_number(x: f64) -> String {
    let s = format!("{x:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.to_owned() }
}

/// Rows as TSV, after a header line when given. NULL and DEFAULT stay empty.
fn tsv(names: Option<&[&str]>, rows: &[Vec<Option<&Value>>]) -> String {
    let mut out: Vec<String> = names.map(|n| n.join("\t")).into_iter().collect();
    out.extend(rows.iter().map(|r| {
        r.iter()
            .map(|v| v.map_or_else(String::new, tsv_cell))
            .collect::<Vec<_>>()
            .join("\t")
    }));
    out.join("\n")
}

/// Rows as CSV with a header line; NULL and DEFAULT stay empty.
fn csv(names: &[&str], rows: &[Vec<Option<&Value>>]) -> String {
    let mut out = vec![
        names
            .iter()
            .map(|n| csv_field(n))
            .collect::<Vec<_>>()
            .join(","),
    ];
    out.extend(rows.iter().map(|r| {
        r.iter()
            .map(|v| match v {
                None | Some(Value::Null) => String::new(),
                Some(v) => csv_field(&v.to_string()),
            })
            .collect::<Vec<_>>()
            .join(",")
    }));
    out.join("\n")
}

/// Rows as a JSON array of objects keyed by column name, in column order. Numbers
/// and booleans stay so, NULL and DEFAULT are `null`, the rest are strings.
fn json_rows(names: &[&str], rows: &[Vec<Option<&Value>>]) -> String {
    use serde_json::Value as J;
    let value = |v: Option<&Value>| match v {
        None | Some(Value::Null) => J::Null,
        Some(Value::Bool(b)) => J::Bool(*b),
        Some(Value::Int(n)) => J::from(*n),
        Some(Value::UInt(n)) => J::from(*n),
        Some(Value::Float(f)) => serde_json::Number::from_f64(*f).map_or(J::Null, J::Number),
        Some(v) => J::String(v.to_string()),
    };
    let objects: Vec<String> = rows
        .iter()
        .map(|r| {
            let fields: Vec<String> = names
                .iter()
                .zip(r)
                .map(|(n, v)| format!("{}: {}", J::String((*n).to_owned()), value(*v)))
                .collect();
            format!("  {{{}}}", fields.join(", "))
        })
        .collect();
    if objects.is_empty() {
        "[]".into()
    } else {
        format!("[\n{}\n]", objects.join(",\n"))
    }
}

/// How grids show NULL: the "Show NULL as" setting.
pub fn null_text(cx: &App) -> SharedString {
    crate::settings::get(cx).null_shown().to_owned().into()
}

/// `38 ms`, `1.2 s`.
pub fn duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms == 0 {
        "<1 ms".to_owned()
    } else if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", d.as_secs_f64())
    }
}

/// A value as a cell draws it: one line, long text cut, binary as its size and a
/// hex preview.
pub fn shown(v: &Value) -> String {
    match v {
        Value::Bytes(b) if b.len() > 8 => {
            let head: String = b[..8].iter().map(|x| format!("{x:02X}")).collect();
            // The size first, so a narrow column still shows it.
            format!("{} · 0x{head}…", size(b.len()))
        }
        Value::Text(t) => {
            let cut = match t.char_indices().nth(INLINE_LIMIT) {
                Some((i, _)) => format!("{}…", &t[..i]),
                None => t.clone(),
            };
            cut.replace(['\n', '\r'], " ")
        }
        v => v.to_string(),
    }
}

/// `12 B`, `4.0 KB`, `1.2 MB`.
fn size(n: usize) -> String {
    #[allow(clippy::cast_precision_loss)] // display only
    let f = n as f64;
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", f / 1024.)
    } else {
        format!("{:.1} MB", f / (1024. * 1024.))
    }
}

/// A value as TSV holds it: `null` stays empty, tabs and line breaks become spaces.
/// A CSV field: quoted, with quotes doubled, when it holds a comma, quote or line break.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

fn tsv_cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        v => v.to_string().replace(['\t', '\n', '\r'], " "),
    }
}

fn compare(a: &Value, b: &Value) -> Ordering {
    use Value as V;
    match (a, b) {
        (V::Null, V::Null) => Ordering::Equal,
        (V::Null, _) => Ordering::Less,
        (_, V::Null) => Ordering::Greater,
        (V::Int(x), V::Int(y)) => x.cmp(y),
        (V::UInt(x), V::UInt(y)) => x.cmp(y),
        (V::Float(x), V::Float(y)) => x.total_cmp(y),
        (V::Decimal(x), V::Decimal(y)) => match (x.parse::<f64>(), y.parse::<f64>()) {
            (Ok(x), Ok(y)) => x.total_cmp(&y),
            _ => x.cmp(y),
        },
        (V::Date(x), V::Date(y)) => x.cmp(y),
        (V::DateTime(x), V::DateTime(y)) => x.cmp(y),
        (V::Time(x), V::Time(y)) => x.cmp(y),
        (a, b) => a.to_string().cmp(&b.to_string()),
    }
}

impl TableDelegate for Grid {
    fn columns_count(&self, _: &App) -> usize {
        if self.transposed {
            return self.rows_count_now() + 1;
        }
        self.columns.len() + self.lead()
    }

    fn rows_count(&self, _: &App) -> usize {
        if self.transposed {
            return self.columns.len();
        }
        self.rows.len() + self.edit.as_ref().map_or(0, |e| e.pending.added_len())
    }

    fn column(&self, col: usize, _: &App) -> table::Column {
        if self.transposed {
            return if col == 0 {
                table::Column::new("fields", "")
                    .width(px(220.))
                    .fixed_left()
                    .movable(false)
                    .p_0()
            } else {
                table::Column::new(SharedString::from(format!("r{col}")), "")
                    .width(px(200.))
                    .movable(false)
                    .p_0()
            };
        }
        let lead = |key: &'static str, w: f32| {
            table::Column::new(key, "")
                .width(px(w))
                .fixed_left()
                .resizable(false)
                .movable(false)
        };
        let editing = self.edit.is_some();
        if editing && col == 0 {
            return lead("mark", 28.).p_0().text_center();
        }
        let Some(c) = self.data_col(col) else {
            // Room for the last number shown, 1,000,000 and beyond included.
            let digits = (self.row_offset + self.row_count()).max(1).ilog10() as f32 + 1.;
            let fit = 16. + 8. * digits;
            // No padding: the cell's own element fills it, so a right click anywhere
            // in it reaches the menu.
            return lead("#", fit.max(if editing { 44. } else { 52. }))
                .p_0()
                .text_right();
        };
        let name = &self.columns[c].name;
        let base = table::Column::new(SharedString::from(format!("c{col}")), name.clone())
            .width(px(self
                .widths
                .get(c)
                .copied()
                .unwrap_or_else(|| self.default_width(c))))
            .movable(false)
            .p_0();
        let base = if self.numeric[c] {
            base.text_right()
        } else {
            base
        };
        // Sorting reorders the fetched rows only, so it waits for all of them. A table
        // tab sorts on the server through its order by field instead.
        if editing || self.is_open() || self.truncated {
            base
        } else {
            base.sortable()
        }
    }

    fn perform_sort(
        &mut self,
        col: usize,
        sort: ColumnSort,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) {
        let Some(c) = col.checked_sub(1) else { return };
        match sort {
            ColumnSort::Ascending => self.rows.sort_by(|a, b| compare(&a[c], &b[c])),
            ColumnSort::Descending => self.rows.sort_by(|a, b| compare(&b[c], &a[c])),
            ColumnSort::Default => {}
        }
    }

    fn render_th(
        &mut self,
        col: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let p = *cx.global::<Palette>();
        if self.transposed {
            // The records: a click picks the whole record, as a row number does.
            let rec = col.checked_sub(1);
            let n = self.columns.len();
            let added = rec
                .and_then(|r| self.row_id(r))
                .is_some_and(|id| matches!(id, RowId::Added(_)));
            return div()
                .id(("th", col))
                .size_full()
                .flex()
                .items_center()
                .px(px(12.))
                .whitespace_nowrap()
                .font_family(ui_font())
                .font_weight(FontWeight::MEDIUM)
                .text_size(small_size())
                .text_color(p.tx2)
                .child(match rec {
                    None => SharedString::from("Column"),
                    Some(_) if added => "New row".into(),
                    Some(r) => format!("Row {}", self.row_offset + r + 1).into(),
                })
                .when_some(rec, |d, r| {
                    d.on_click(cx.listener(move |t, _: &gpui_kit::ClickEvent, _, cx| {
                        cx.stop_propagation();
                        t.delegate_mut().set_sel(Some(Sel::rows_of(r, r, n)), cx);
                    }))
                })
                .into_any_element();
        }
        let sorted = self.data_col(col).and_then(|c| {
            let sort = &self.edit.as_ref()?.sort;
            let i = sort.iter().position(|(s, _)| *s == c)?;
            Some((sort[i].1, (sort.len() > 1).then_some(i + 1)))
        });
        let editing = self.edit.is_some();
        let data = self.data_col(col);
        let (name, tip): (SharedString, SharedString) = match data {
            None => (SharedString::default(), SharedString::default()),
            Some(c) => (
                self.columns[c].name.clone().into(),
                self.columns[c].type_name.clone().into(),
            ),
        };
        let numeric = data.is_some_and(|c| self.numeric[c]);
        div()
            .id(("th", col))
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(4.))
            .px(px(if editing { 12. } else { 8. }))
            .whitespace_nowrap()
            .font_family(ui_font())
            .font_weight(FontWeight::MEDIUM)
            .text_size(small_size())
            .text_color(p.tx2)
            .when(!tip.is_empty(), |d| {
                d.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            })
            .when_some(data.filter(|_| editing), |d, c| {
                // Ctrl (Cmd) picks the column; otherwise the click sorts.
                d.clickable(cx.listener(move |t, e: &gpui_kit::ClickEvent, _, cx| {
                    if e.modifiers().secondary(cx) {
                        let last = t.delegate().rows_count_now().saturating_sub(1);
                        t.delegate_mut().set_sel(
                            Some(Sel {
                                anchor: (0, c),
                                head: (last, c),
                            }),
                            cx,
                        );
                    } else {
                        cx.emit(GridEvent::Sort(c, e.modifiers().shift));
                    }
                }))
            })
            .when(sorted.is_some(), |d| d.text_color(p.tx))
            // The name gives way first, cut at its end; the sort mark keeps its room.
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .when(numeric, |d| d.text_right())
                    .child(name),
            )
            .when_some(sorted.zip(data), |d, ((desc, n), c)| {
                d.child(sort_mark(c, desc, n, self.sorting(), &p))
            })
            .into_any_element()
    }

    fn render_td(
        &mut self,
        row: usize,
        col: usize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        if let Some(e) = self.edit.as_mut()
            && std::mem::take(&mut e.open_picker)
            && let Some(picker) = e.editing.as_ref().and_then(|ed| ed.3.clone())
        {
            // Pick date… from the menu: the picker is drawn in this frame, so it can
            // take its key once the frame is done.
            window.defer(cx, move |window, cx| toggle_picker(&picker, window, cx));
        }
        if self.edit.is_some() {
            return self.render_edit_td(row, col, cx);
        }
        let p = *cx.global::<Palette>();
        let d = div()
            .size_full()
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap();
        if col == 0 {
            let d = d
                .justify_end()
                .pr(px(8.))
                .text_color(p.tx3)
                .child((self.row_offset + row + 1).to_string());
            return self.interactive(d, row, col, cx);
        }
        let c = col - 1;
        let in_sel = self
            .sel
            .is_some_and(|s| s.count() > 1 && s.contains(row, c));
        let d = d
            .px(px(8.))
            .when(self.numeric[c], |d| d.justify_end())
            .when(in_sel, |d| d.bg(p.sel));
        let d = match self.cell(row, col) {
            None | Some(Value::Null) => d.text_color(p.tx3).italic().child(null_text(cx)),
            Some(v) => d.child(div().overflow_hidden().text_ellipsis().child(shown(v))),
        };
        self.interactive(d, row, col, cx)
    }

    fn cell_label(&self, row: usize, col: usize, _: &App) -> Option<SharedString> {
        if self.transposed {
            return match self.data_at(row, col) {
                Some((r, c)) => self.rows.get(r)?.get(c).map(|v| v.to_string().into()),
                None => self.columns.get(row).map(|c| c.name.clone().into()),
            };
        }
        if col == 0 {
            return Some((self.row_offset + row + 1).to_string().into());
        }
        self.cell(row, col).map(|v| v.to_string().into())
    }

    // The table's own row menu: cells open theirs first (see `interactive`), so this
    // is only reached off the cells, and acts on the selection the same way.
    fn context_menu(
        &mut self,
        _: usize,
        menu: PopupMenu,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        self.build_menu(menu, cx)
    }

    fn has_more(&self, _: &App) -> bool {
        self.stream.is_some()
    }

    fn load_more_threshold(&self) -> usize {
        // Two pages ahead of the bottom, so a fast scroll rarely waits.
        150
    }

    fn load_more(&mut self, _: &mut Window, cx: &mut Context<TableState<Self>>) {
        let Some(mut stream) = self.stream.take() else {
            return;
        };
        self.fetching = true;
        let rx = self.rt.spawn(async move {
            let page = stream.next_page().await;
            (stream, page)
        });
        cx.spawn(async move |table, cx| {
            let Ok((stream, page)) = rx.await else { return };
            let _ = table.update(cx, |t, cx| {
                let g = t.delegate_mut();
                if !g.fetching {
                    // Closed while the page was on its way.
                    return;
                }
                g.fetching = false;
                match page {
                    Some(Ok(rows)) => {
                        g.rows.extend(rows);
                        g.stream = Some(stream);
                    }
                    Some(Err(e)) => {
                        eprintln!("deadlock: fetching rows: {e}");
                        g.truncated = true;
                    }
                    None => {
                        let more = g.next.is_some();
                        t.refresh(cx);
                        if more {
                            cx.emit(Ended);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let p = cx.global::<Palette>();
        div()
            .size_full()
            .flex()
            .justify_center()
            .pt(px(24.))
            .font_family(ui_font())
            .text_color(p.tx3)
            .child("No rows")
    }
}

/// Grid text styles the table does not take from the theme.
pub fn grid_text<E: Styled>(e: E) -> E {
    e.font_family(mono_font())
        .text_size(crate::theme::data_size())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(rows: Vec<Row>) -> Grid {
        let columns = vec![
            Column {
                name: "id".into(),
                type_name: "LONGLONG".into(),
            },
            Column {
                name: "note".into(),
                type_name: "VAR_STRING".into(),
            },
        ];
        Grid::new(
            columns,
            rows,
            None,
            Arc::new(Runtime::new().unwrap()),
            Duration::from_millis(38),
        )
    }

    #[test]
    fn turned_cells_map_to_records_and_fields_and_back() {
        let mut g = grid(vec![
            vec![Value::Int(1), Value::Null],
            vec![Value::Int(2), Value::Null],
            vec![Value::Int(3), Value::Null],
        ]);
        for turned in [false, true] {
            g.transposed = turned;
            for rec in 0..3 {
                for f in 0..2 {
                    let (r, c) = g.display_at(rec, f);
                    assert_eq!(g.data_at(r, c), Some((rec, f)));
                }
            }
        }
        // Turned: field 1 of record 2 is table row 1, column 3; column 0 names fields.
        assert_eq!(g.display_at(2, 1), (1, 3));
        assert_eq!(g.data_at(1, 0), None);
        assert_eq!(g.data_at(2, 1), None);
        assert_eq!(g.data_at(0, 4), None);
    }

    #[test]
    fn copies_cells_rows_and_columns_as_tsv() {
        use table::TableSelection as S;
        let g = grid(vec![
            vec![Value::Int(1), Value::Text("a\tb".into())],
            vec![Value::Int(2), Value::Null],
        ]);
        assert_eq!(g.copy_text(S::Cell(0, 2)).unwrap(), "a b");
        assert_eq!(g.copy_text(S::Cell(1, 2)).unwrap(), "");
        assert_eq!(g.copy_text(S::Row(0)).unwrap(), "1\ta b");
        assert_eq!(g.copy_text(S::Cell(1, 0)).unwrap(), "2\t");
        assert_eq!(g.copy_text(S::Column(1)).unwrap(), "1\n2");
        assert_eq!(g.copy_text(S::None), None);
        let one: Vec<_> = g.rows[0].iter().map(Some).collect();
        assert_eq!(csv(&["id", "note"], &[one]), "id,note\n1,a\tb");
        assert_eq!(csv_field("say \"hi\", ok"), "\"say \"\"hi\"\", ok\"");
        assert_eq!(
            csv(&["id", "note"], &[vec![None, Some(&Value::Null)]]),
            "id,note\n,"
        );
    }

    #[test]
    fn selections_are_rectangles() {
        let s = Sel {
            anchor: (4, 2),
            head: (1, 0),
        };
        assert_eq!((s.rows(), s.cols(), s.count()), (1..=4, 0..=2, 12));
        assert!(s.contains(1, 0) && s.contains(4, 2) && !s.contains(5, 1));
        // The head moves, the anchor stays; both stop at the grid's edges.
        let s = Sel::cell(0, 0).extend(1, 1, 3, 2);
        assert_eq!((s.anchor, s.head), ((0, 0), (1, 1)));
        assert_eq!(s.extend(5, 5, 3, 2).head, (2, 1));
        assert_eq!(s.extend(-9, -9, 3, 2).head, (0, 0));
        assert_eq!(Sel::rows_of(2, 0, 3).cols(), 0..=2);
        assert_eq!(Sel::rows_of(2, 0, 3).rows(), 0..=2);
    }

    #[test]
    fn selections_copy_and_sum() {
        let mut g = grid(vec![
            vec![Value::Int(1), Value::Text("a\tb".into())],
            vec![Value::Int(2), Value::Null],
            vec![Value::Int(4), Value::Text("x".into())],
        ]);
        assert_eq!(g.sel_status(), None);
        g.sel = Some(Sel {
            anchor: (0, 0),
            head: (1, 1),
        });
        let s = table::TableSelection::None;
        assert_eq!(g.copy(s, false).unwrap(), "1\ta b\n2\t");
        g.edit = Some(EditOverlay::new(TableInfo::default(), None));
        assert_eq!(g.copy(s, false).unwrap(), "1, a b, 2, ");
        assert_eq!(g.copy(s, true).unwrap(), "id\tnote\n1\ta b\n2\t");
        assert_eq!(
            g.sel_status().unwrap(),
            "4 cells selected · sum 3 · avg 1.5"
        );
        g.sel = Some(Sel {
            anchor: (2, 1),
            head: (1, 1),
        });
        assert_eq!(g.sel_status().unwrap(), "2 cells selected");
        let (names, rows) = g.sel_table().unwrap();
        assert_eq!(
            json_rows(&names, &rows),
            "[\n  {\"note\": null},\n  {\"note\": \"x\"}\n]"
        );
        let one: Vec<_> = g.rows[0].iter().map(Some).collect();
        assert_eq!(
            json_rows(&["id", "note"], &[one]),
            "[\n  {\"id\": 1, \"note\": \"a\\tb\"}\n]"
        );
        assert_eq!(json_rows(&["id"], &[]), "[]");
        assert_eq!(short_number(0.1 + 0.2), "0.3");
        assert_eq!(short_number(-0.0), "0");
        assert_eq!(short_number(21.0), "21");
        // A page change drops the selection.
        g.set_rows(Vec::new(), None);
        assert_eq!(g.sel, None);
    }

    #[test]
    fn cells_cut_long_text_and_preview_binary() {
        assert_eq!(shown(&Value::Bytes(vec![0xAB; 4])), "0xABABABAB");
        assert_eq!(
            shown(&Value::Bytes(vec![0x89; 2048])),
            "2.0 KB · 0x8989898989898989…"
        );
        let long = "ab\n".repeat(150);
        let cut = shown(&Value::Text(long));
        assert_eq!(cut.chars().count(), INLINE_LIMIT + 1);
        assert!(cut.ends_with('…') && !cut.contains('\n'));
        assert_eq!(shown(&Value::Text(String::new())), "");
    }

    #[test]
    fn summaries_and_numeric_columns() {
        let g = grid(vec![vec![Value::Int(1), Value::Null]]);
        assert_eq!(g.summary(), "1 row in 38 ms");
        assert_eq!(g.numeric, [true, false]);
        assert!(is_numeric("NEWDECIMAL") && is_numeric("NUMBER") && !is_numeric("VARCHAR2"));
        assert_eq!(duration(Duration::from_millis(1250)), "1.2 s");
        assert_eq!(compare(&Value::Null, &Value::Int(1)), Ordering::Less);
        assert_eq!(
            compare(&Value::Decimal("10.5".into()), &Value::Decimal("9".into())),
            Ordering::Greater
        );
    }
}
