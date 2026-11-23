//! A table tab: the Data view of one table or view, and the switch to its Structure
//! view. Rows load in pages through the result grid; edits stay pending in the grid
//! until Submit runs them in one transaction on the tab's session.

use std::sync::Arc;

use deadlock_core::connection::Environment;
use deadlock_core::driver::{Value, page_size};
use deadlock_core::edit::{
    self, ColumnInfo, Edit, INLINE_LIMIT, RowId, TableInfo, TableRef, edit_text, parse_input,
};
use gpui_kit::component::input::{self, Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::table::{DataTable, TableEvent, TableSelection, TableState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, Focusable as _, FontWeight, HighlightStyle,
    InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, StyledText, Subscription, Task, Window, actions, div, hsla, px,
};

use super::{QueryTab, Session, TabEvent, TxMode};
use crate::app::Db;
use crate::assets::Icon;
use crate::results::{Action, EditOverlay, Grid, GridEvent, grid_text};
use crate::theme::{MONO_FONT, Palette, RADIUS, small_size};
use crate::ui::{Kind as ButtonKind, button, icon_button};

actions!(
    table,
    [
        EditCell, SetNull, SetDefault, AddRow, DeleteRows, UndoChange, NextCell, PrevCell
    ]
);

/// Tables with more rows than this, by the server's statistics, are not counted
/// exactly unless a filter is set… and then not at all: a count would scan them.
const COUNT_LIMIT: u64 = 1_000_000;

/// The row total in the status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Total {
    Unknown,
    Exact(u64),
    /// From the server's statistics.
    About(u64),
}

/// Which view of the table the tab shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Data,
    Structure,
}

/// The cell editor: inline for short values, a dialog for binary and long ones.
pub(super) enum Field {
    Inline(Entity<InputState>),
    Dialog(Entity<TextareaState>),
}

pub(super) struct Editor {
    row: RowId,
    col: usize,
    field: Field,
    /// The text it started with: committing it unchanged changes nothing.
    initial: String,
    error: Option<String>,
    _sub: Subscription,
}

pub struct TableData {
    pub table: TableRef,
    pub view: View,
    pub(super) structure: super::structure::Structure,
    pub(crate) info: Option<TableInfo>,
    pub(super) grid: Option<Entity<TableState<Grid>>>,
    /// The `where` field; a Redis key tab's `match` field.
    pub(super) filter: Entity<InputState>,
    order: Entity<InputState>,
    /// The server's answer to the where or order by text, shown under the fields.
    pub(super) filter_error: Option<String>,
    /// A load or submit message; `true` for an error.
    pub(super) notice: Option<(String, bool)>,
    total: Total,
    pub(super) busy: bool,
    /// Waiting for the tab's session to open.
    pub waiting: bool,
    /// Bumped on every load, so an older answer is dropped.
    pub(super) generation: u64,
    /// A Redis key tab: the key, its facts and value.
    pub(super) redis: Option<super::key::KeyState>,
    _subs: Vec<Subscription>,
}

/// The `where` and `order by` fields, applied on Enter.
pub(super) fn data(table: TableRef, window: &mut Window, cx: &mut Context<QueryTab>) -> TableData {
    let filter = cx.new(|cx| InputState::new(window, cx));
    let order = cx.new(|cx| InputState::new(window, cx));
    let subs = [&filter, &order]
        .into_iter()
        .map(|f| {
            cx.subscribe_in(f, window, |this, _, e: &InputEvent, window, cx| {
                if matches!(e, InputEvent::PressEnter { .. }) {
                    this.apply_fields(window, cx);
                }
            })
        })
        .collect();
    TableData {
        table,
        view: View::Data,
        structure: super::structure::Structure::default(),
        info: None,
        grid: None,
        filter,
        order,
        filter_error: None,
        notice: None,
        total: Total::Unknown,
        busy: false,
        waiting: false,
        generation: 0,
        redis: None,
        _subs: subs,
    }
}

/// `1234567` → `1,234,567`.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The panel's sentence for how a submit commits.
fn mode_sentence(mode: TxMode) -> &'static str {
    match mode {
        TxMode::Auto => "They run in one transaction and commit together.",
        TxMode::Manual => "They run in the open transaction and stay uncommitted until you commit.",
    }
}

impl QueryTab {
    fn grid(&self) -> Option<&Entity<TableState<Grid>>> {
        self.table.as_ref()?.grid.as_ref()
    }

    /// The Data view's grid, for an export.
    pub(super) fn data_grid(&self) -> Option<Entity<TableState<Grid>>> {
        self.table
            .as_ref()
            .filter(|t| t.view == View::Data)?
            .grid
            .clone()
    }

    /// The statement that reads the whole table with its where and order by fields, and
    /// how to write it in `format`.
    pub(super) fn table_export(
        &self,
        format: deadlock_core::export::Format,
        cx: &App,
    ) -> Result<(String, deadlock_core::export::Spec), String> {
        use deadlock_core::export::{Dialect, Spec};
        let t = self.table.as_ref().ok_or("not a table tab")?;
        let info = t.info.as_ref().ok_or("the table has not loaded yet")?;
        let (filter, order) = (t.filter.read(cx).value(), t.order.read(cx).value());
        let sql = edit::select_sql(&t.table, info, &filter, &order)?;
        let dialect = if info.oracle {
            Dialect::Oracle
        } else {
            Dialect::Mysql {
                backslash_escapes: info.backslash_escapes,
            }
        };
        Ok((
            sql,
            Spec {
                format,
                dialect,
                table: t.table.table.clone(),
                keep: Some(Spec::table_columns(info, format)),
            },
        ))
    }

    /// Statements a submit would run.
    pub fn pending_changes(&self, cx: &App) -> usize {
        if self.is_key() && self.grid().is_none() {
            return self.key_text_pending(cx);
        }
        self.grid().map_or(0, |g| {
            g.read(cx)
                .delegate()
                .edit
                .as_ref()
                .map_or(0, |e| e.pending.count())
        })
    }

    /// `Rows 1 to 13 of 214`, for the status bar. Rows added and not yet submitted do
    /// not count: they are not on the server.
    pub fn rows_status(&self, cx: &App) -> Option<String> {
        if self.is_key() {
            return self.key_status(cx);
        }
        let t = self.table.as_ref().filter(|t| t.view == View::Data)?;
        let g = t.grid.as_ref()?.read(cx);
        let grid = g.delegate();
        let shown = grid.row_count();
        if shown == 0 {
            return Some("No rows".into());
        }
        let total = match t.total {
            Total::Exact(n) => format!(" of {}", grouped(n)),
            Total::About(n) => format!(" of about {}", grouped(n)),
            Total::Unknown if grid.is_complete() => format!(" of {}", grouped(shown as u64)),
            Total::Unknown => " of more".into(),
        };
        Some(format!("Rows 1 to {}{total}", grouped(shown as u64)))
    }

    /// Focuses the grid, or the where field before rows arrive.
    pub fn focus_table(&self, window: &mut Window, cx: &mut App) {
        let Some(t) = self.table.as_ref().filter(|t| t.view == View::Data) else {
            return;
        };
        match &t.grid {
            Some(g) => window.focus(&g.read(cx).focus_handle(cx), cx),
            None => t.filter.update(cx, |f, cx| f.focus(window, cx)),
        }
    }

    /// Loads the view in front once: asks for a session first when there is none.
    pub fn open_table(&mut self, cx: &mut Context<Self>) {
        let Some(t) = &mut self.table else { return };
        let loaded = match t.view {
            View::Data => t.grid.is_some() || t.busy,
            View::Structure => t.structure.is_loaded(),
        };
        if loaded || t.waiting {
            return;
        }
        match self.session {
            Session::Ready(_) => self.load_view(cx),
            Session::Connecting => t.waiting = true,
            Session::None => {
                t.waiting = true;
                cx.emit(TabEvent::NeedSession);
            }
        }
    }

    /// The session opened or failed while the table waited for it.
    pub(super) fn table_session(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        let Some(t) = &mut self.table else { return };
        if !std::mem::take(&mut t.waiting) {
            return;
        }
        match error {
            None => self.load_view(cx),
            Some(e) if t.view == View::Structure => t.structure.fail(e),
            Some(e) => t.notice = Some((e, true)),
        }
        cx.notify();
    }

    fn load_view(&mut self, cx: &mut Context<Self>) {
        match self.table.as_ref().map(|t| t.view) {
            Some(View::Data) => self.load_table(false, cx),
            Some(View::Structure) => self.load_structure(cx),
            None => {}
        }
    }

    /// The Data / Structure switch. Each view loads once; switching back keeps it.
    pub fn set_view(&mut self, view: View, window: &mut Window, cx: &mut Context<Self>) {
        let Some(t) = &mut self.table else { return };
        if t.view == view {
            return;
        }
        t.view = view;
        self.open_table(cx);
        self.focus_table(window, cx);
        cx.emit(TabEvent::Changed);
        cx.notify();
    }

    /// After an ALTER: the column list is stale, so the Data view reloads it, and its
    /// rows, if it was loaded.
    pub(super) fn columns_changed(&mut self, cx: &mut Context<Self>) {
        self.cancel_edit(cx);
        let Some(t) = &mut self.table else { return };
        t.info = None;
        if t.grid.take().is_some() {
            t.notice = None;
            t.filter_error = None;
            self.load_table(false, cx);
        }
    }

    pub(super) fn read_only_reason(&self) -> Option<String> {
        if self.connection.as_ref().is_some_and(|c| c.read_only) {
            return Some("Read-only connection: rows cannot be changed.".into());
        }
        let info = self.table.as_ref()?.info.as_ref()?;
        info.read_only_reason().map(str::to_owned)
    }

    /// Runs `then` now, or after the user agrees to drop the pending data changes.
    pub(super) fn after_discard(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        self.ask_discard(window, cx, then);
    }

    /// Enter in the where or order by field.
    fn apply_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_discard(window, cx, |this, _, cx| this.load_table(false, cx));
    }

    /// Runs `then` now, or after the user agrees to drop the pending changes.
    pub(super) fn ask_discard(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let n = self.pending_changes(cx);
        if n == 0 {
            then(self, window, cx);
            return;
        }
        let answer = crate::dialog::ask(
            window,
            cx,
            crate::dialog::Ask::new(
                format!(
                    "Discard {n} pending {}?",
                    if n == 1 { "change" } else { "changes" }
                ),
                &["Discard", "Cancel"],
            )
            .text("They have not been submitted.")
            .danger(true),
        );
        let handle = self.window;
        cx.spawn(async move |this, cx| {
            if answer.await == Some(0) {
                let _ = cx.update_window(handle, |_, window, cx| {
                    this.update(cx, |this, cx| {
                        this.discard_now(cx);
                        then(this, window, cx);
                    })
                });
            }
        })
        .detach();
    }

    /// Drops every pending change without asking.
    pub fn discard_now(&mut self, cx: &mut Context<Self>) {
        self.cancel_edit(cx);
        if self.is_key() && self.key_text_pending(cx) > 0 {
            // The editor comes back with the value as loaded.
            self.reload_key(cx);
        }
        if let Some(g) = self.grid().cloned() {
            g.update(cx, |g, cx| {
                if let Some(e) = &mut g.delegate_mut().edit {
                    e.pending = edit::Pending::default();
                    e.marked.clear();
                }
                g.refresh(cx);
            });
        }
        self.changed(cx);
    }

    pub(super) fn changed(&mut self, cx: &mut Context<Self>) {
        self.dirty = self.pending_changes(cx) > 0;
        cx.emit(TabEvent::Changed);
        cx.notify();
    }

    /// Fetches the table's columns (once), then its first page with the where and
    /// order by text. `keep` reloads into the grid in place, after a submit or the end
    /// of a transaction. A failed where keeps the rows on screen.
    pub(crate) fn load_table(&mut self, keep: bool, cx: &mut Context<Self>) {
        if self.is_key() {
            return self.load_key(keep, cx);
        }
        let Session::Ready(driver) = &self.session else {
            return;
        };
        let Some(t) = &mut self.table else { return };
        let driver = Arc::clone(driver);
        let (filter, order) = (
            t.filter.read(cx).value().to_string(),
            t.order.read(cx).value().to_string(),
        );
        let info = t.info.clone();
        let table = t.table.clone();
        let rows_on_screen = if keep {
            t.grid
                .as_ref()
                .map_or(0, |g| g.read(cx).delegate().row_count())
        } else {
            0
        };
        t.busy = true;
        if !keep {
            t.notice = None;
        }
        t.generation += 1;
        let generation = t.generation;
        let rx = cx.global::<Db>().0.spawn(async move {
            let info = match info {
                Some(i) => i,
                None => driver
                    .table_info(&table)
                    .await
                    .map_err(|e| (e.to_string(), false))?,
            };
            let sql = edit::select_sql(&table, &info, &filter, &order).map_err(|e| (e, true))?;
            let mut stream = edit::browse(Arc::clone(&driver), sql, info.oracle)
                .await
                .map_err(|e| (e.to_string(), true))?;
            let mut rows = Vec::new();
            // A reload fetches as many rows as were on screen.
            loop {
                match stream.next_page().await {
                    Some(Ok(page)) => {
                        let full = page.len() == page_size();
                        rows.extend(page);
                        if !full || rows.len() >= rows_on_screen {
                            let more = full.then_some(stream);
                            return Ok((info, rows, more));
                        }
                    }
                    Some(Err(e)) => return Err((e.to_string(), true)),
                    None => return Ok((info, rows, None)),
                }
            }
        });
        cx.spawn(async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err(("the table task stopped".into(), false)));
            let window = this.read_with(cx, |t, _| t.window).ok();
            let Some(window) = window else { return };
            let _ = cx.update_window(window, |_, window, cx| {
                this.update(cx, |this, cx| {
                    this.table_loaded(generation, keep, result, window, cx)
                })
            });
        })
        .detach();
        cx.notify();
    }

    #[allow(clippy::type_complexity)]
    pub(super) fn table_loaded(
        &mut self,
        generation: u64,
        keep: bool,
        result: Result<
            (
                TableInfo,
                Vec<deadlock_core::driver::Row>,
                Option<deadlock_core::driver::ResultStream>,
            ),
            (String, bool),
        >,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let read_only = self.connection.as_ref().is_some_and(|c| c.read_only);
        let key_tab = self.is_key();
        let rt = Arc::clone(&cx.global::<Db>().0);
        let Some(t) = &mut self.table else { return };
        if t.generation != generation {
            return;
        }
        t.busy = false;
        let (info, rows, stream) = match result {
            Ok(r) => r,
            Err((e, from_fields)) => {
                if from_fields {
                    t.filter_error = Some(e);
                } else {
                    t.notice = Some((e, true));
                }
                cx.notify();
                return;
            }
        };
        t.filter_error = None;
        t.info = Some(info.clone());
        let sort = sort_of(&info, &t.order.read(cx).value());
        match t.grid.clone().filter(|_| keep) {
            Some(g) => g.update(cx, |g, cx| {
                let grid = g.delegate_mut();
                grid.set_rows(rows, stream);
                if let Some(e) = &mut grid.edit {
                    e.pending = edit::Pending::default();
                    e.marked.clear();
                }
                g.refresh(cx);
            }),
            None => {
                let columns = info
                    .columns
                    .iter()
                    .map(|c| deadlock_core::driver::Column {
                        name: c.name.clone(),
                        // Oracle's own type; MySQL's base type.
                        type_name: if info.oracle {
                            &c.column_type
                        } else {
                            &c.data_type
                        }
                        .to_ascii_uppercase(),
                    })
                    .collect();
                let mut grid = Grid::new(columns, rows, stream, rt, std::time::Duration::ZERO);
                if key_tab {
                    grid.widths = super::key::widths(&info);
                }
                let reason = if read_only {
                    Some("Read-only connection: rows cannot be changed.".to_owned())
                } else {
                    info.read_only_reason().map(str::to_owned)
                };
                let mut overlay = EditOverlay::new(info.clone(), reason);
                overlay.sort = sort;
                grid.edit = Some(overlay);
                let table = cx.new(|cx| {
                    TableState::new(grid, window, cx)
                        .cell_selectable(true)
                        .row_selectable(true)
                        .row_header(false)
                        .col_movable(false)
                        .col_selectable(false)
                        .sortable(false)
                });
                let subs = vec![
                    cx.subscribe_in(&table, window, Self::on_grid_event),
                    cx.subscribe_in(&table, window, Self::on_table_event),
                ];
                if let Some(t) = self.table.as_mut() {
                    t.grid = Some(table.clone());
                    t._subs.truncate(2);
                    t._subs.extend(subs);
                }
                self.cell = None;
                window.focus(&table.read(cx).focus_handle(cx), cx);
            }
        }
        if let Some(g) = self.grid().cloned() {
            g.update(cx, |g, _| {
                if let Some(e) = &mut g.delegate_mut().edit {
                    e.sort = sort;
                }
            });
        }
        self.count_rows(&info, cx);
        self.changed(cx);
    }

    /// The total for the status bar: an exact count when it is cheap, else the
    /// server's estimate, else none.
    fn count_rows(&mut self, info: &TableInfo, cx: &mut Context<Self>) {
        let (Session::Ready(driver), Some(t)) = (&self.session, &mut self.table) else {
            return;
        };
        // A key's facts header already holds its size.
        if t.redis.is_some() {
            return;
        }
        let filter = t.filter.read(cx).value().to_string();
        let big = info.estimated_rows.is_some_and(|n| n > COUNT_LIMIT);
        t.total = Total::Unknown;
        if big {
            if filter.trim().is_empty()
                && let Some(n) = info.estimated_rows
            {
                t.total = Total::About(n);
            }
            return;
        }
        let Ok(sql) = edit::count_sql(&t.table, info, &filter) else {
            return;
        };
        let driver = Arc::clone(driver);
        let generation = t.generation;
        let rx = cx.global::<Db>().0.spawn(async move {
            let rows = driver.execute(&sql).await?.collect().await?;
            Ok::<_, deadlock_core::driver::DriverError>(rows)
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(rows)) = rx.await else { return };
            let n = match rows.first().and_then(|r| r.first()) {
                Some(Value::Int(n)) => u64::try_from(*n).ok(),
                Some(Value::UInt(n)) => Some(*n),
                Some(v) => v.to_string().parse().ok(),
                None => None,
            };
            let _ = this.update(cx, |this, cx| {
                if let (Some(t), Some(n)) = (&mut this.table, n)
                    && t.generation == generation
                {
                    t.total = Total::Exact(n);
                    cx.emit(TabEvent::Changed);
                }
            });
        })
        .detach();
    }

    /// After Commit or Roll back: the submit's note is stale and a rollback may have
    /// undone rows.
    pub(super) fn table_tx_ended(&mut self, cx: &mut Context<Self>) {
        if let Some(t) = &mut self.table {
            t.notice = None;
        }
        self.reload_table(cx);
    }

    /// Reloads the rows on screen into the grid: after a submit, Commit or Roll back.
    pub(super) fn reload_table(&mut self, cx: &mut Context<Self>) {
        if self.grid().is_some() {
            self.load_table(true, cx);
        }
    }

    fn on_table_event(
        &mut self,
        table: &Entity<TableState<Grid>>,
        e: &TableEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match e {
            TableEvent::DoubleClickedCell(r, c) => {
                let (r, c) = (*r, *c);
                if let Some(c) = table.read(cx).delegate().data_col(c) {
                    self.start_edit(r, c, window, cx);
                }
            }
            TableEvent::RightClickedCell(r, c) => {
                let cell = Some((*r, *c));
                table.update(cx, |t, _| t.delegate_mut().menu_cell = cell);
            }
            TableEvent::SelectCell(_, c) if *c != 1 => {
                // A plain click elsewhere drops the rows picked by number.
                table.update(cx, |t, cx| {
                    if let Some(e) = &mut t.delegate_mut().edit {
                        e.marked.clear();
                    }
                    cx.notify();
                });
            }
            _ => {}
        }
    }

    fn on_grid_event(
        &mut self,
        table: &Entity<TableState<Grid>>,
        e: &GridEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match *e {
            GridEvent::Sort(c) => self.sort_by(c, window, cx),
            GridEvent::Act(a, row, col) => {
                let gcol = col.map(|c| table.read(cx).delegate().grid_col(c));
                table.update(cx, |t, cx| match gcol {
                    Some(gc) => t.set_selected_cell(row, gc, cx),
                    None => t.set_selected_row(row, cx),
                });
                match (a, col) {
                    (Action::Edit, Some(c)) => self.start_edit(row, c, window, cx),
                    (Action::SetNull, _) => self.set_selected(Edit::Value(Value::Null), window, cx),
                    (Action::SetDefault, _) => self.set_selected(Edit::Default, window, cx),
                    (Action::Delete, _) => self.delete_rows(cx),
                    (Action::Revert, _) => {
                        if let Some(id) = table.read(cx).delegate().row_id(row) {
                            self.with_pending(cx, |p| p.revert(id));
                        }
                    }
                    (Action::Edit, None) => {}
                }
            }
        }
    }

    /// A header click: ascending, then descending, then unsorted, in the order by field.
    fn sort_by(&mut self, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(t) = &self.table else { return };
        let Some(info) = &t.info else { return };
        let Some(name) = info.columns.get(col).map(|c| info.ident(&c.name)) else {
            return;
        };
        let next = match sort_of(info, &t.order.read(cx).value()) {
            Some((c, false)) if c == col => format!("{name} desc"),
            Some((c, true)) if c == col => String::new(),
            _ => name,
        };
        let order = t.order.clone();
        self.ask_discard(window, cx, move |this, window, cx| {
            order.update(cx, |o, cx| o.set_value(next, window, cx));
            this.load_table(false, cx);
        });
    }

    /// The selected cell as (display row, data column), or the selected row.
    pub(super) fn selected(&self, cx: &App) -> Option<(usize, Option<usize>)> {
        let g = self.grid()?.read(cx);
        match g.selection() {
            TableSelection::Cell(r, c) => Some((r, g.delegate().data_col(c))),
            TableSelection::Row(r) => Some((r, None)),
            _ => None,
        }
    }

    pub(super) fn with_pending(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut edit::Pending),
    ) {
        let Some(g) = self.grid().cloned() else {
            return;
        };
        g.update(cx, |g, cx| {
            if let Some(e) = &mut g.delegate_mut().edit {
                f(&mut e.pending);
            }
            g.refresh(cx);
        });
        self.changed(cx);
    }

    /// Why `col` of `row` cannot be edited, if it cannot.
    fn locked(&self, row: RowId, col: &ColumnInfo, cx: &App) -> Option<String> {
        if let Some(r) = self.read_only_reason() {
            return Some(r);
        }
        let g = self.grid()?.read(cx).delegate();
        if g.edit.as_ref()?.pending.is_deleted(row) {
            return Some("The row is marked deleted: undo that first.".into());
        }
        if let Some(why) = self.key_locked() {
            return Some(why);
        }
        col.locked().map(|r| format!("{}: {r}.", col.name))
    }

    fn column(&self, c: usize) -> Option<ColumnInfo> {
        self.table.as_ref()?.info.as_ref()?.columns.get(c).cloned()
    }

    pub(super) fn table_note(&mut self, text: String, error: bool, cx: &mut Context<Self>) {
        // The read-only banner already says it.
        if self.read_only_reason().as_ref() == Some(&text) {
            return;
        }
        if let Some(t) = &mut self.table {
            t.notice = Some((text, error));
        }
        cx.notify();
    }

    /// Opens the editor on a cell: inline, or in a dialog for binary and long values.
    fn start_edit(
        &mut self,
        display: usize,
        c: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.commit_edit(window, cx) {
            return;
        }
        let Some(g) = self.grid().cloned() else {
            return;
        };
        let Some(col) = self.column(c) else { return };
        let Some(row) = g.read(cx).delegate().row_id(display) else {
            return;
        };
        if let Some(why) = self.locked(row, &col, cx) {
            self.table_note(why, true, cx);
            return;
        }
        let grid = g.read(cx).delegate();
        let current = grid
            .edit
            .as_ref()
            .and_then(|e| e.pending.cell(row, c).cloned())
            .or_else(|| grid.original(row, c).cloned().map(Edit::Value));
        let (initial, placeholder) = match &current {
            None | Some(Edit::Default) => (String::new(), "default"),
            Some(Edit::Value(Value::Null)) => (String::new(), "NULL"),
            Some(Edit::Value(v)) => (edit_text(&col, v), ""),
        };
        let dialog = col.is_binary()
            || initial.chars().count() > INLINE_LIMIT
            || initial.contains('\n')
            || matches!(
                col.data_type.as_str(),
                "text" | "mediumtext" | "longtext" | "json"
            );
        let (field, sub) = if dialog {
            let state = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(initial.clone())
            });
            state.update(cx, |s, cx| s.focus(window, cx));
            let sub = cx.subscribe_in(&state, window, |this, _, e: &InputEvent, window, cx| {
                if matches!(
                    e,
                    InputEvent::PressEnter {
                        secondary: true,
                        ..
                    }
                ) {
                    this.commit_edit(window, cx);
                }
            });
            (Field::Dialog(state), sub)
        } else {
            let state = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(initial.clone())
            });
            state.update(cx, |s, cx| s.focus(window, cx));
            let sub = cx.subscribe_in(
                &state,
                window,
                |this, _, e: &InputEvent, window, cx| match e {
                    InputEvent::PressEnter { .. } => {
                        this.commit_edit(window, cx);
                    }
                    // Clicking away keeps a valid value, and keeps the editor open on a bad one.
                    InputEvent::Blur if this.cell.as_ref().is_some_and(|e| e.error.is_none()) => {
                        this.commit_edit(window, cx);
                    }
                    _ => {}
                },
            );
            (Field::Inline(state), sub)
        };
        let inline = match &field {
            Field::Inline(s) => Some(s.clone()),
            Field::Dialog(_) => None,
        };
        g.update(cx, |g, cx| {
            // The selection is drawn over the cell; the editor shows instead.
            if inline.is_some() {
                g.clear_selection(cx);
            }
            if let Some(e) = &mut g.delegate_mut().edit {
                e.editing = inline.map(|s| (row, c, s));
            }
            cx.notify();
        });
        if let Some(t) = &mut self.table {
            t.notice = None;
        }
        self.cell = Some(Editor {
            row,
            col: c,
            field,
            initial,
            error: None,
            _sub: sub,
        });
        cx.notify();
    }

    fn close_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_edit(cx);
        if let Some(g) = self.grid() {
            window.focus(&g.read(cx).focus_handle(cx), cx);
        }
    }

    /// Esc: closes the editor and keeps the cell as it was; the cell is selected again.
    pub(super) fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        let Some(ed) = self.cell.take() else { return };
        if let Some(g) = self.grid().cloned() {
            g.update(cx, |g, cx| {
                let grid = g.delegate_mut();
                if let Some(e) = &mut grid.edit {
                    e.editing = None;
                }
                let at = grid.display_row(ed.row).map(|r| (r, grid.grid_col(ed.col)));
                if let Some((r, c)) = at {
                    g.set_selected_cell(r, c, cx);
                }
                cx.notify();
            });
        }
        cx.notify();
    }

    /// Checks the editor's text against the column type and makes it a pending
    /// change. Returns `false`, keeping the editor open, when the text does not fit.
    pub(super) fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(ed) = &self.cell else { return true };
        let text = match &ed.field {
            Field::Inline(s) => s.read(cx).value().to_string(),
            Field::Dialog(s) => s.read(cx).value().to_string(),
        };
        let (row, c) = (ed.row, ed.col);
        if text == ed.initial {
            self.close_editor(window, cx);
            return true;
        }
        let Some(col) = self.column(c) else {
            return true;
        };
        let original = self
            .grid()
            .and_then(|g| g.read(cx).delegate().original(row, c).cloned());
        let parsed = if self.is_key() {
            deadlock_core::redis::parse_cell(&col, &text, original.as_ref())
        } else {
            parse_input(&col, &text)
        };
        match parsed {
            Ok(v) => {
                self.with_pending(cx, |p| p.set(row, c, Edit::Value(v), original.as_ref()));
                self.close_editor(window, cx);
                true
            }
            Err(e) => {
                if let Some(ed) = &mut self.cell {
                    ed.error = Some(format!("{}: {e}", col.name));
                }
                cx.notify();
                false
            }
        }
    }

    /// Tab and Shift+Tab: commit, then edit the next or previous editable cell.
    fn move_edit(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ed) = &self.cell else { return };
        let (row, c) = (ed.row, ed.col);
        if !self.commit_edit(window, cx) {
            return;
        }
        let Some(g) = self.grid().cloned() else {
            return;
        };
        let Some(display) = g.read(cx).delegate().display_row(row) else {
            return;
        };
        let n = self
            .table
            .as_ref()
            .and_then(|t| t.info.as_ref())
            .map_or(0, |i| i.columns.len());
        let mut next = c;
        for _ in 0..n {
            next = if forward {
                (next + 1) % n
            } else {
                (next + n - 1) % n
            };
            if self.column(next).is_some_and(|col| col.locked().is_none()) {
                let gc = g.read(cx).delegate().grid_col(next);
                g.update(cx, |t, cx| t.set_selected_cell(display, gc, cx));
                self.start_edit(display, next, window, cx);
                return;
            }
        }
    }

    /// Set NULL / Set DEFAULT on the selected cell, or the cell being edited.
    fn set_selected(&mut self, value: Edit, window: &mut Window, cx: &mut Context<Self>) {
        let target = match &self.cell {
            Some(ed) => Some((ed.row, ed.col)),
            None => self
                .selected(cx)
                .and_then(|(r, c)| Some((self.grid()?.read(cx).delegate().row_id(r)?, c?))),
        };
        let Some((row, c)) = target else { return };
        let Some(col) = self.column(c) else { return };
        if let Some(why) = self.locked(row, &col, cx) {
            self.table_note(why, true, cx);
            return;
        }
        if value == Edit::Value(Value::Null) && !col.nullable {
            self.table_note(format!("{} does not accept NULL.", col.name), true, cx);
            return;
        }
        self.cancel_edit(cx);
        let original = self
            .grid()
            .and_then(|g| g.read(cx).delegate().original(row, c).cloned());
        self.with_pending(cx, |p| p.set(row, c, value, original.as_ref()));
        if let Some(g) = self.grid().cloned() {
            window.focus(&g.read(cx).focus_handle(cx), cx);
        }
    }

    pub(super) fn add_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(why) = self.read_only_reason() {
            self.table_note(why, true, cx);
            return;
        }
        let Some(n) = self
            .table
            .as_ref()
            .and_then(|t| t.info.as_ref())
            .map(|i| i.columns.len())
        else {
            return;
        };
        let mut added = None;
        self.with_pending(cx, |p| added = Some(p.add(n)));
        let Some(g) = self.grid().cloned() else {
            return;
        };
        if let Some(display) = added.and_then(|a| g.read(cx).delegate().display_row(a)) {
            let first = (0..n).find(|&c| {
                self.column(c)
                    .is_some_and(|c| c.locked().is_none() && !c.auto_increment)
            });
            g.update(cx, |t, cx| {
                t.scroll_to_row(display, cx);
                if let Some(c) = first {
                    let gc = t.delegate().grid_col(c);
                    t.set_selected_cell(display, gc, cx);
                }
            });
            window.focus(&g.read(cx).focus_handle(cx), cx);
        }
    }

    /// Marks the rows picked by number, or the selected row, deleted.
    pub(super) fn delete_rows(&mut self, cx: &mut Context<Self>) {
        if let Some(why) = self.read_only_reason() {
            self.table_note(why, true, cx);
            return;
        }
        let Some(g) = self.grid().cloned() else {
            return;
        };
        let marked: Vec<RowId> = g
            .read(cx)
            .delegate()
            .edit
            .as_ref()
            .map(|e| e.marked.iter().copied().collect())
            .unwrap_or_default();
        let rows = if marked.is_empty() {
            self.selected(cx)
                .and_then(|(r, _)| g.read(cx).delegate().row_id(r))
                .into_iter()
                .collect()
        } else {
            marked
        };
        if rows.is_empty() {
            self.table_note("Select a row to delete first.".into(), true, cx);
            return;
        }
        self.cancel_edit(cx);
        g.update(cx, |g, _| {
            if let Some(e) = &mut g.delegate_mut().edit {
                e.marked.clear();
            }
        });
        self.with_pending(cx, |p| p.delete(&rows));
    }

    fn undo_change(&mut self, cx: &mut Context<Self>) {
        self.cancel_edit(cx);
        self.with_pending(cx, |p| {
            p.undo();
        });
    }

    /// Discard changes: asks, then drops them.
    pub(super) fn discard_button(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_discard(window, cx, |_, _, _| {});
    }

    /// Runs the pending changes in one transaction on the tab's session; production
    /// connections confirm first. Resolves once done: `Err` when it failed or was
    /// cancelled, and the changes stay pending.
    pub fn submit_table(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), String>> {
        if self.is_key() {
            return self.submit_key(window, cx);
        }
        if !self.commit_edit(window, cx) {
            return Task::ready(Err("a cell holds a value its column cannot take".into()));
        }
        let (Session::Ready(driver), Some(t), Some(g)) = (&self.session, &self.table, self.grid())
        else {
            return Task::ready(Err("the table is not loaded".into()));
        };
        let driver = Arc::clone(driver);
        let grid = g.read(cx).delegate();
        let Some(e) = &grid.edit else {
            return Task::ready(Ok(()));
        };
        let changes = match e.pending.changes(&t.table, &e.info, grid.rows()) {
            Ok(c) if c.is_empty() => return Task::ready(Ok(())),
            Ok(c) => c,
            Err(err) => {
                self.table_note(err.clone(), true, cx);
                return Task::ready(Err(err));
            }
        };
        let n = changes.len();
        let auto = self.tx_mode == TxMode::Auto;
        let production = self
            .connection
            .as_ref()
            .filter(|c| c.environment == Environment::Production)
            .map(|c| c.name.clone());
        let title = t.table.table.clone();
        let rt = Arc::clone(&cx.global::<Db>().0);
        if let Some(t) = &mut self.table {
            t.busy = true;
        }
        cx.notify();
        let window = self.window;
        cx.spawn(async move |this, cx| {
            if let Some(name) = production {
                let answer = cx.update_window(window, |_, window, cx| {
                    crate::dialog::ask(
                        window,
                        cx,
                        crate::dialog::Ask::new(
                            format!(
                                "Submit {n} {} to {title} on production connection {name}?",
                                if n == 1 { "change" } else { "changes" }
                            ),
                            &["Submit", "Cancel"],
                        )
                        .text("The changes run on a production connection.")
                        .production(true),
                    )
                });
                let go = match answer {
                    Ok(a) => a.await == Some(0),
                    Err(_) => false,
                };
                if !go {
                    let _ = this.update(cx, |this, cx| {
                        if let Some(t) = &mut this.table {
                            t.busy = false;
                        }
                        cx.notify();
                    });
                    return Err("cancelled".to_owned());
                }
            }
            let result = rt
                .spawn(async move { driver.apply(&changes, auto).await })
                .await
                .unwrap_or_else(|_| Err(deadlock_core::driver::DriverError::Gone))
                .map_err(|e| e.to_string());
            this.update(cx, |this, cx| {
                if let Some(t) = &mut this.table {
                    t.busy = false;
                }
                match &result {
                    Ok(()) => {
                        let what = if auto {
                            "submitted and committed"
                        } else {
                            "submitted, not committed yet"
                        };
                        let noun = if n == 1 { "change" } else { "changes" };
                        this.table_note(format!("{n} {noun} {what}."), false, cx);
                        this.reload_table(cx);
                    }
                    Err(e) => this.table_note(e.clone(), true, cx),
                }
                this.check_tx(false, result.is_err(), cx).detach();
                cx.notify();
            })
            .map_err(|_| "the tab was closed".to_string())?;
            result
        })
    }
}

/// The data column and direction the order by text sorts on, when it is just
/// `col`, `` `col` `` or either followed by `asc` or `desc`.
fn sort_of(info: &TableInfo, order: &str) -> Option<(usize, bool)> {
    let order = order.trim();
    let quote = if info.oracle { '"' } else { '`' };
    let (name, rest) = match order.strip_prefix(quote) {
        Some(quoted) => {
            // The closing quote is the first one not doubled.
            let mut name = String::new();
            let mut chars = quoted.char_indices().peekable();
            let mut end = None;
            while let Some((i, c)) = chars.next() {
                if c == quote {
                    if chars.peek().is_some_and(|(_, n)| *n == quote) {
                        chars.next();
                    } else {
                        end = Some(i + 1);
                        break;
                    }
                }
                name.push(c);
            }
            (name, &quoted[end?..])
        }
        None => {
            let (n, r) = order.split_once(char::is_whitespace).unwrap_or((order, ""));
            (n.to_owned(), r)
        }
    };
    let mut words = rest.split_whitespace();
    let desc = match words.next().map(str::to_ascii_lowercase).as_deref() {
        None | Some("asc") => false,
        Some("desc") => true,
        Some(_) => return None,
    };
    if words.next().is_some() || name.is_empty() {
        return None;
    }
    let col = info
        .columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case(&name))?;
    Some((col, desc))
}

/// SQL with the design's colors: keywords in the link color, strings green. For the
/// pending changes, the Structure view's definition and its ALTER previews.
pub(crate) fn highlighted(sql: &str, p: &Palette) -> StyledText {
    const KEYWORDS: [&str; 72] = [
        "update",
        "set",
        "where",
        "and",
        "delete",
        "from",
        "insert",
        "into",
        "values",
        "default",
        "alter",
        "table",
        "add",
        "column",
        "drop",
        "index",
        "key",
        "primary",
        "foreign",
        "unique",
        "fulltext",
        "spatial",
        "first",
        "after",
        "not",
        "null",
        "comment",
        "create",
        "view",
        "references",
        "on",
        "cascade",
        "restrict",
        "no",
        "action",
        "constraint",
        "generated",
        "always",
        "as",
        "stored",
        "virtual",
        "unsigned",
        "desc",
        "select",
        "engine",
        "charset",
        "collate",
        "auto_increment",
        "check",
        "exists",
        "global",
        "persist",
        "persist_only",
        "reset",
        "join",
        "left",
        "right",
        "inner",
        "outer",
        "in",
        "is",
        "or",
        "like",
        "between",
        "order",
        "group",
        "by",
        "having",
        "limit",
        "distinct",
        "union",
        "truncate",
    ];
    let keyword = HighlightStyle {
        color: Some(p.link),
        font_weight: Some(FontWeight::MEDIUM),
        ..HighlightStyle::default()
    };
    let string = HighlightStyle {
        color: Some(p.ok),
        ..HighlightStyle::default()
    };
    let mut marks = Vec::new();
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            q @ (b'\'' | b'`') => {
                let start = i;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' && q == b'\'' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == q {
                        if bytes.get(i + 1) == Some(&q) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i = (i + 1).min(bytes.len());
                if q == b'\'' {
                    marks.push((start..i, string));
                }
            }
            b if b.is_ascii_alphabetic() => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                if KEYWORDS
                    .iter()
                    .any(|k| k.eq_ignore_ascii_case(&sql[start..i]))
                {
                    marks.push((start..i, keyword));
                }
            }
            _ => i += 1,
        }
    }
    StyledText::new(SharedString::from(sql.to_owned())).with_highlights(marks)
}

/// The Data / Structure switch: the view in front is filled with `--tx`.
pub(super) fn segments(view: View, cx: &mut Context<QueryTab>) -> gpui_kit::Div {
    let p = *cx.global::<Palette>();
    let segment = |id: &'static str, label: &'static str, target: View, tip: &'static str| {
        let on = view == target;
        button(id, ButtonKind::Outline, true, tip, cx)
            .when(on, |b| b.bg(p.tx).border_color(p.tx).text_color(p.bg))
            .child(label)
            .on_click(cx.listener(move |this, _, window, cx| this.set_view(target, window, cx)))
    };
    div()
        .flex()
        .gap(px(4.))
        .child(segment("data-view", "Data", View::Data, "Table data"))
        .child(segment(
            "structure-view",
            "Structure",
            View::Structure,
            "Columns, indexes, keys and definition",
        ))
}

/// `.fi`: a 28 px field with a mono label.
fn field(
    label: &'static str,
    input: &Entity<InputState>,
    error: bool,
    p: &Palette,
) -> gpui_kit::Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .h(px(28.))
        .px(px(8.))
        .border_1()
        .border_color(if error { p.danger } else { p.line2 })
        .rounded(RADIUS)
        .font_family(MONO_FONT)
        .text_size(crate::theme::data_size())
        .child(div().flex_none().text_color(p.tx3).child(label))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(Input::new(input).appearance(false).h(px(26.)).px_0()),
        )
}

pub(super) fn render(this: &mut QueryTab, cx: &mut Context<QueryTab>) -> AnyElement {
    if this
        .table
        .as_ref()
        .is_some_and(|t| t.view == View::Structure)
    {
        let segments = segments(View::Structure, cx);
        return super::structure::render(this, segments, cx);
    }
    let p = *cx.global::<Palette>();
    let read_only = this.read_only_reason();
    let pending = this.pending_changes(cx);
    let mode = this.tx_mode;
    let Some(t) = &this.table else {
        return div().into_any_element();
    };
    let can_edit = read_only.is_none() && t.grid.is_some();
    let key = t.redis.is_some();
    let toolbar = div()
        .flex()
        .flex_wrap()
        .flex_none()
        .items_center()
        .gap_x(px(12.))
        .gap_y(px(8.))
        .px(px(16.))
        .py(px(10.))
        .border_b_1()
        .border_color(p.line)
        .child(segments(View::Data, cx))
        .child(
            div()
                .flex_grow(2.)
                .flex_shrink(1.)
                .flex_basis(px(300.))
                .min_w(px(160.))
                .child(field("where", &t.filter, t.filter_error.is_some(), &p)),
        )
        .child(
            div()
                .flex_grow(1.)
                .flex_shrink(1.)
                .flex_basis(px(200.))
                .min_w(px(140.))
                .child(field("order by", &t.order, false, &p)),
        )
        .child(
            icon_button(
                "add-row",
                Icon::Plus,
                Some("Add row"),
                ButtonKind::Outline,
                can_edit,
                "Add row (Alt+Insert)",
                cx,
            )
            .when(can_edit, |b| {
                b.on_click(cx.listener(|this, _, window, cx| this.add_row(window, cx)))
            }),
        )
        .child(
            icon_button(
                "delete-row",
                Icon::Minus,
                Some("Delete row"),
                ButtonKind::Outline,
                can_edit,
                "Delete the selected rows (Delete)",
                cx,
            )
            .when(can_edit, |b| {
                b.on_click(cx.listener(|this, _, _, cx| this.delete_rows(cx)))
            }),
        );
    let toolbar = if key {
        super::key::header(this, read_only.is_some(), cx)
    } else {
        toolbar
    };
    let Some(t) = &this.table else {
        return div().into_any_element();
    };
    let line = |text: String, color| {
        div()
            .flex_none()
            .px(px(16.))
            .py(px(6.))
            .border_b_1()
            .border_color(p.line)
            .text_size(small_size())
            .text_color(color)
            .child(text)
    };
    let editor_error = this.cell.as_ref().and_then(|e| e.error.clone());
    let notes = [
        t.filter_error.clone().map(|e| line(e, p.danger)),
        read_only.clone().map(|r| line(r, p.tx2).bg(p.panel)),
        t.info
            .as_ref()
            .and_then(TableInfo::key_note)
            .filter(|_| read_only.is_none() && !key)
            .map(|n| line(n.to_owned(), p.tx2).bg(p.panel)),
        editor_error
            .filter(|_| matches!(this.cell.as_ref().map(|e| &e.field), Some(Field::Inline(_))))
            .map(|e| line(e, p.danger)),
        t.notice
            .clone()
            .map(|(n, err)| line(n, if err { p.danger } else { p.tx2 })),
    ];
    let key_body = super::key::body(this, cx);
    let body = match &t.grid {
        _ if key_body.is_some() => key_body.unwrap_or_else(|| div().into_any_element()),
        Some(g) => grid_text(
            div()
                .size_full()
                .text_color(p.tx)
                .child(DataTable::new(g).bordered(false)),
        )
        .into_any_element(),
        None => div()
            .size_full()
            .p(px(16.))
            .text_color(p.tx3)
            .child(if t.busy || t.waiting {
                "Loading rows…"
            } else {
                "No rows loaded."
            })
            .into_any_element(),
    };
    let key_panel = (key && pending > 0).then(|| super::key::panel_text(this, cx));
    let panel = (pending > 0).then(|| {
        let (summary, sql) = t.grid.as_ref().map_or_else(Default::default, |g| {
            let grid = g.read(cx).delegate();
            grid.edit.as_ref().map_or_else(Default::default, |e| {
                let sql = match e.pending.changes(&t.table, &e.info, grid.rows()) {
                    Ok(c) => c
                        .iter()
                        .map(|c| format!("{};", c.sql))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    Err(err) => format!("-- {err}"),
                };
                (e.pending.summary(), sql)
            })
        });
        let noun = if pending == 1 { "change" } else { "changes" };
        div()
            .id("pending")
            .flex()
            .flex_wrap()
            .flex_none()
            .items_start()
            .gap_x(px(24.))
            .gap_y(px(12.))
            .px(px(20.))
            .py(px(16.))
            .bg(p.warn_bg)
            .border_t_1()
            .border_color(p.warn_line)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .min_w_0()
                    .flex_grow(1.)
                    .flex_shrink(1.)
                    .flex_basis(px(200.))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("{pending} pending {noun}")),
                    )
                    .child(
                        div()
                            .text_color(p.tx2)
                            .line_height(gpui_kit::relative(1.5))
                            .child(match &key_panel {
                                Some((sentence, _)) => sentence.clone(),
                                None => format!("{summary} {}", mode_sentence(mode)),
                            }),
                    ),
            )
            .child(
                div()
                    .id("pending-sql")
                    .flex_grow(3.)
                    .flex_shrink(1.)
                    .flex_basis(px(420.))
                    .min_w_0()
                    .max_h(px(20. * 8. + 18.))
                    .overflow_scroll()
                    .px(px(12.))
                    .py(px(8.))
                    .bg(p.bg)
                    .border_1()
                    .border_color(p.warn_line)
                    .rounded(RADIUS)
                    .font_family(MONO_FONT)
                    .text_size(crate::theme::data_size())
                    .line_height(px(20.))
                    .whitespace_nowrap()
                    .child(match &key_panel {
                        Some((_, commands)) => super::key::panel_commands(commands, &p),
                        None => highlighted(&sql, &p).into_any_element(),
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.))
                    .child(
                        button(
                            "discard",
                            ButtonKind::Outline,
                            !t.busy,
                            "Discard every pending change",
                            cx,
                        )
                        .child("Discard changes")
                        .when(!t.busy, |b| {
                            b.on_click(
                                cx.listener(|this, _, window, cx| this.discard_button(window, cx)),
                            )
                        }),
                    )
                    .child(
                        button(
                            "submit",
                            ButtonKind::Primary,
                            !t.busy,
                            if key {
                                "Run the commands together in MULTI / EXEC"
                            } else {
                                "Run the changes in one transaction"
                            },
                            cx,
                        )
                        .child(format!("Submit {pending} {noun}"))
                        .when(!t.busy, |b| {
                            b.on_click(cx.listener(|this, _, window, cx| {
                                this.submit_table(window, cx).detach()
                            }))
                        }),
                    ),
            )
    });
    let dialog = this.cell.as_ref().and_then(|e| match &e.field {
        Field::Dialog(state) => Some((state.clone(), e.col, e.error.clone())),
        Field::Inline(_) => None,
    });
    let dialog = dialog.map(|(state, c, error)| {
        let name = this
            .column(c)
            .map_or_else(String::new, |c| format!("{} · {}", c.name, c.column_type));
        let binary = this.column(c).is_some_and(|c| c.is_binary());
        div()
            .id("cell-dialog")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(hsla(0., 0., 0., 0.3))
            .child(
                div()
                    .key_context("CellEditor")
                    .w(px(560.))
                    .p(px(16.))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .bg(p.bg)
                    .border_1()
                    .border_color(p.line)
                    .rounded(RADIUS)
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(name))
                    .when(binary, |d| {
                        d.child(
                            div()
                                .text_size(small_size())
                                .text_color(p.tx3)
                                .child("Binary value as hex, like 0x89504E47."),
                        )
                    })
                    .child(
                        div()
                            .h(px(220.))
                            .border_1()
                            .border_color(p.line2)
                            .rounded(RADIUS)
                            .font_family(MONO_FONT)
                            .text_size(crate::theme::data_size())
                            .child(Textarea::new(&state).appearance(false).h_full()),
                    )
                    .children(
                        error.map(|e| div().text_size(small_size()).text_color(p.danger).child(e)),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.))
                            .child(
                                button(
                                    "cell-null",
                                    ButtonKind::Outline,
                                    true,
                                    format!("Set NULL ({})", crate::keymap::label("set-null", cx)),
                                    cx,
                                )
                                .child("Set NULL")
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.set_selected(Edit::Value(Value::Null), window, cx);
                                    },
                                )),
                            )
                            .child(div().flex_1())
                            .child(
                                button(
                                    "cell-cancel",
                                    ButtonKind::Outline,
                                    true,
                                    "Cancel (Esc)",
                                    cx,
                                )
                                .child("Cancel")
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.cancel_edit(cx);
                                        this.focus_table(window, cx);
                                    },
                                )),
                            )
                            .child(
                                button(
                                    "cell-save",
                                    ButtonKind::Primary,
                                    true,
                                    "Keep the value (Ctrl+Enter)",
                                    cx,
                                )
                                .child("Keep value")
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.commit_edit(window, cx);
                                    },
                                )),
                            ),
                    ),
            )
    });
    div()
        .key_context("TableData")
        .relative()
        .size_full()
        .flex()
        .flex_col()
        .on_action(cx.listener(|this, _: &EditCell, window, cx| {
            if let Some((r, Some(c))) = this.selected(cx) {
                this.start_edit(r, c, window, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &SetNull, window, cx| {
            this.set_selected(Edit::Value(Value::Null), window, cx)
        }))
        .on_action(cx.listener(|this, _: &SetDefault, window, cx| {
            this.set_selected(Edit::Default, window, cx)
        }))
        .on_action(cx.listener(|this, _: &AddRow, window, cx| this.add_row(window, cx)))
        .on_action(cx.listener(|this, _: &DeleteRows, _, cx| this.delete_rows(cx)))
        .on_action(cx.listener(|this, _: &UndoChange, _, cx| this.undo_change(cx)))
        .on_action(cx.listener(|this, _: &NextCell, window, cx| this.move_edit(true, window, cx)))
        .on_action(cx.listener(|this, _: &PrevCell, window, cx| this.move_edit(false, window, cx)))
        .on_action(cx.listener(|this, _: &input::Escape, window, cx| {
            if this.cell.is_some() {
                this.cancel_edit(cx);
                this.focus_table(window, cx);
            } else {
                cx.propagate();
            }
        }))
        .child(toolbar)
        .children(notes.into_iter().flatten())
        .child(div().flex_1().min_h_0().child(body))
        .children(panel)
        .children(dialog)
        .children(super::key::dialog(this, cx))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_text_maps_to_a_header_sort() {
        let info = TableInfo {
            columns: vec![
                ColumnInfo {
                    name: "id".into(),
                    ..ColumnInfo::default()
                },
                ColumnInfo {
                    name: "Created At".into(),
                    ..ColumnInfo::default()
                },
            ],
            ..TableInfo::default()
        };
        assert_eq!(sort_of(&info, "id"), Some((0, false)));
        assert_eq!(sort_of(&info, "ID desc"), Some((0, true)));
        assert_eq!(sort_of(&info, "`Created At` DESC"), Some((1, true)));
        assert_eq!(sort_of(&info, "id, created"), None);
        assert_eq!(sort_of(&info, "id desc nulls"), None);
        assert_eq!(sort_of(&info, ""), None);
        assert_eq!(grouped(1_234_567), "1,234,567");
        assert_eq!(grouped(214), "214");
    }
}
