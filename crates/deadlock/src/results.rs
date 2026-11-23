//! One result set in the bottom panel: a virtual grid over rows that arrive in pages.
//! Only the first page is fetched up front; the grid asks for the next page when the
//! user scrolls near the end of what has arrived.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use deadlock_core::driver::{Column, ResultStream, Row, Runtime, Value};
use deadlock_core::edit::{Edit, INLINE_LIMIT, Pending, RowId, TableInfo};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{self, ColumnSort, TableDelegate, TableState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, ClipboardItem, Context, Entity, FontWeight, InteractiveElement, IntoElement, Modifiers,
    MouseButton, MouseDownEvent, ParentElement, SharedString, StatefulInteractiveElement, Styled,
    Window, div, px,
};
use tokio::sync::oneshot;

use crate::assets::{Icon, icon};
use crate::theme::{MONO_FONT, Palette, UI_FONT, small_size};

/// The result set after this one, sent once this one has been read or dropped.
pub type NextSet = oneshot::Receiver<deadlock_core::driver::Result<ResultStream>>;

/// The grid's stream ended and a later result set may follow: the tab takes it with
/// [`Grid::close`].
pub struct Ended;

impl gpui_kit::EventEmitter<Ended> for TableState<Grid> {}

/// What a table tab's grid asks the tab to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridEvent {
    /// A data column's header was clicked.
    Sort(usize),
    /// A context-menu entry on a display row and data column.
    Act(Action, usize, Option<usize>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Edit,
    SetNull,
    SetDefault,
    Delete,
    Revert,
}

impl gpui_kit::EventEmitter<GridEvent> for TableState<Grid> {}

/// A table tab's editing state, drawn over the fetched rows: marks, changed cells,
/// added rows at the end, the inline editor.
pub struct EditOverlay {
    pub info: TableInfo,
    pub pending: Pending,
    /// Why nothing can be edited, if it cannot.
    pub read_only: Option<String>,
    /// The cell edited in place, and its field.
    pub editing: Option<(RowId, usize, Entity<InputState>)>,
    /// Rows picked with Ctrl or Shift on their number, for Delete row.
    pub marked: BTreeSet<RowId>,
    anchor: Option<usize>,
    /// The data column the order by field sorts on, and whether descending.
    pub sort: Option<(usize, bool)>,
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
            sort: None,
        }
    }
}

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
    /// The cell under the last right click, for the context menu.
    pub menu_cell: Option<(usize, usize)>,
    /// Set on a table tab's grid.
    pub edit: Option<EditOverlay>,
    /// The statement that produced the rows and its bind values, when running it again
    /// only reads: an export of all rows runs it again.
    pub source: Option<Source>,
    /// Data column widths, where the tab knows better than the default.
    pub widths: Vec<f32>,
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
            menu_cell: None,
            edit: None,
            source: None,
            widths: Vec::new(),
            rt,
            took,
        }
    }

    /// Columns before the data: the row number, and on a table tab the mark.
    fn lead(&self) -> usize {
        if self.edit.is_some() { 2 } else { 1 }
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

    /// Replaces the rows in place (a reload after a submit), keeping the scroll position.
    pub fn set_rows(&mut self, rows: Vec<Row>, stream: Option<ResultStream>) {
        self.rows = rows;
        self.stream = stream;
        self.fetching = false;
        self.truncated = false;
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

    /// Every fetched row with a header line.
    fn copy_all(&self) -> String {
        let mut out: Vec<String> = vec![
            self.columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join("\t"),
        ];
        out.extend((0..self.rows.len()).map(|r| self.tsv_row(r)));
        out.join("\n")
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
        let (Some(e), Some(id)) = (self.edit.as_ref(), self.row_id(row)) else {
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
        if col == 0 {
            let mark = e.pending.mark(id);
            let tip = match mark {
                Some('M') => "Modified",
                Some('D') => "Deleted",
                Some('A') => "Added",
                _ => "",
            };
            return cell
                .id(("mark", row))
                .justify_center()
                .text_color(p.warn)
                .font_weight(FontWeight::MEDIUM)
                .when(!tip.is_empty(), |d| {
                    d.tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                })
                .children(mark.map(|m| m.to_string()))
                .into_any_element();
        }
        // The mark stays unstruck; the rest of a deleted row is struck and dim.
        let cell = cell.when(deleted, |d| d.text_color(p.tx3).line_through());
        let Some(c) = self.data_col(col) else {
            return cell
                .justify_end()
                .pr(px(8.))
                .text_color(p.tx3)
                .child((row + 1).to_string())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |t, ev: &MouseDownEvent, _, cx| {
                        let secondary = ev.modifiers.secondary(cx);
                        t.delegate_mut().mark_click(row, ev.modifiers, secondary);
                        cx.notify();
                    }),
                )
                .into_any_element();
        };
        let right = self.numeric[c];
        if let Some((_, _, input)) = e.editing.as_ref().filter(|(r, ec, _)| *r == id && *ec == c) {
            return cell
                .px(px(3.))
                .child(
                    div()
                        .key_context("CellEditor")
                        .w_full()
                        .h(px(26.))
                        .border_1()
                        .border_color(p.blue)
                        .rounded(px(4.))
                        .bg(p.bg)
                        .child(
                            Input::new(input)
                                .appearance(false)
                                .h(px(24.))
                                .px(px(8.))
                                .when(right, |i| i.text_right()),
                        ),
                )
                .into_any_element();
        }
        let pending = e.pending.cell(id, c);
        let value = match pending {
            Some(Edit::Value(v)) => Some(v),
            Some(Edit::Default) => None,
            None => self.original(id, c),
        };
        let changed = pending.is_some() && !added;
        let cell =
            cell.px(px(12.))
                .when(right, |d| d.justify_end())
                .when(changed && !deleted, |d| {
                    d.bg(p.warn_bg)
                        .text_color(p.warn)
                        .font_weight(FontWeight::MEDIUM)
                });
        let dim = |d: gpui_kit::Div, text: SharedString| {
            d.when(!changed, |d| d.text_color(p.tx3))
                .italic()
                .child(text)
        };
        match value {
            None => dim(cell, "default".into()),
            Some(Value::Null) => dim(cell, null_text(cx)),
            Some(v) => cell.child(div().overflow_hidden().text_ellipsis().child(shown(v))),
        }
        .into_any_element()
    }

    fn edit_menu(
        &self,
        row: usize,
        menu: PopupMenu,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let col = self
            .menu_cell
            .filter(|(r, _)| *r == row)
            .and_then(|(_, c)| self.data_col(c));
        let this = cx.entity();
        let act = |a: Action| {
            let this = this.clone();
            move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                this.update(cx, |_, cx| cx.emit(GridEvent::Act(a, row, col)));
            }
        };
        let can = self.edit.as_ref().is_some_and(|e| e.read_only.is_none());
        let copy = |text: String| {
            move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            }
        };
        let menu = if can {
            let menu = if col.is_some() {
                menu.item(PopupMenuItem::new("Edit cell").on_click(act(Action::Edit)))
                    .item(PopupMenuItem::new("Set NULL").on_click(act(Action::SetNull)))
                    .item(PopupMenuItem::new("Set DEFAULT").on_click(act(Action::SetDefault)))
                    .separator()
            } else {
                menu
            };
            menu.item(PopupMenuItem::new("Delete row").on_click(act(Action::Delete)))
                .item(PopupMenuItem::new("Undo changes to this row").on_click(act(Action::Revert)))
                .separator()
        } else {
            menu
        };
        let cell = col.and_then(|c| self.rows.get(row)?.get(c)).map(tsv_cell);
        let menu = match cell {
            Some(text) => menu.item(PopupMenuItem::new("Copy cell").on_click(copy(text))),
            None => menu,
        };
        menu.item(PopupMenuItem::new("Copy row").on_click(copy(self.tsv_row(row))))
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
        self.columns.len() + self.lead()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len() + self.edit.as_ref().map_or(0, |e| e.pending.added_len())
    }

    fn column(&self, col: usize, _: &App) -> table::Column {
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
            return if editing {
                lead("#", 44.).p_0().text_right()
            } else {
                lead("#", 52.).text_right()
            };
        };
        let base = table::Column::new(
            SharedString::from(format!("c{col}")),
            self.columns[c].name.clone(),
        )
        .width(px(self
            .widths
            .get(c)
            .copied()
            .unwrap_or(if self.numeric[c] { 110. } else { 190. })))
        .movable(false);
        let base = if self.numeric[c] {
            base.text_right()
        } else {
            base
        };
        // Sorting reorders the fetched rows only, so it waits for all of them. A table
        // tab sorts on the server through its order by field instead.
        if editing {
            base.p_0()
        } else if self.is_open() || self.truncated {
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
        let sorted = self.data_col(col).and_then(|c| {
            let (s, desc) = self.edit.as_ref()?.sort?;
            (s == c).then_some(desc)
        });
        let sortable = self.edit.is_some() && self.data_col(col).is_some();
        let (name, tip): (SharedString, SharedString) = match self.data_col(col) {
            None => (SharedString::default(), SharedString::default()),
            Some(c) => (
                self.columns[c].name.clone().into(),
                self.columns[c].type_name.clone().into(),
            ),
        };
        div()
            .id(("th", col))
            .w_full()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .font_family(UI_FONT)
            .font_weight(FontWeight::MEDIUM)
            .text_size(small_size())
            .text_color(p.tx2)
            .when(self.data_col(col).is_some_and(|c| self.numeric[c]), |d| {
                d.text_right().justify_end()
            })
            .when(!tip.is_empty(), |d| {
                d.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            })
            .when(sortable, |d| {
                let c = col - self.lead();
                d.flex()
                    .items_center()
                    .gap(px(4.))
                    .px(px(12.))
                    .cursor_pointer()
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(GridEvent::Sort(c))))
            })
            .when(sorted.is_some(), |d| d.text_color(p.tx))
            .child(name)
            .when_some(sorted, |d, desc| {
                d.child(icon(
                    if desc { Icon::SortDesc } else { Icon::SortAsc },
                    p.link,
                ))
            })
    }

    fn render_td(
        &mut self,
        row: usize,
        col: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        if self.edit.is_some() {
            return self.render_edit_td(row, col, cx).into_any_element();
        }
        let p = cx.global::<Palette>();
        let d = div()
            .w_full()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap();
        if col == 0 {
            return d
                .text_right()
                .text_color(p.tx3)
                .child((row + 1).to_string())
                .into_any_element();
        }
        let right = self.numeric[col - 1];
        match self.cell(row, col) {
            None | Some(Value::Null) => d
                .when(right, |d| d.text_right())
                .text_color(p.tx3)
                .italic()
                .child(null_text(cx))
                .into_any_element(),
            Some(v) => d
                .when(right, |d| d.text_right())
                .child(shown(v))
                .into_any_element(),
        }
    }

    fn cell_label(&self, row: usize, col: usize, _: &App) -> Option<SharedString> {
        if col == 0 {
            return Some((row + 1).to_string().into());
        }
        self.cell(row, col).map(|v| v.to_string().into())
    }

    fn context_menu(
        &mut self,
        row: usize,
        menu: PopupMenu,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        if self.edit.is_some() {
            return self.edit_menu(row, menu, cx);
        }
        let copy = |text: String| {
            move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            }
        };
        let cell = self
            .menu_cell
            .filter(|(r, c)| *r == row && *c > 0)
            .and_then(|(r, c)| self.cell(r, c))
            .map(tsv_cell);
        let menu = match cell {
            Some(text) => menu.item(PopupMenuItem::new("Copy cell").on_click(copy(text))),
            None => menu,
        };
        menu.item(PopupMenuItem::new("Copy row").on_click(copy(self.tsv_row(row))))
            .item(
                PopupMenuItem::new("Copy fetched rows with header").on_click(copy(self.copy_all())),
            )
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
            .font_family(UI_FONT)
            .text_color(p.tx3)
            .child("No rows")
    }
}

/// Grid text styles the table does not take from the theme.
pub fn grid_text<E: Styled>(e: E) -> E {
    e.font_family(MONO_FONT)
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
        assert_eq!(g.copy_all(), "id\tnote\n1\ta b\n2\t");
        assert_eq!(g.copy_text(S::None), None);
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
