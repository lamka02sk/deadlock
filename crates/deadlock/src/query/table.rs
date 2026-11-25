//! A table tab: the Data view of one table or view, and the switch to its Structure
//! view. Rows load in pages through the result grid; edits stay pending in the grid
//! until Submit runs them in one transaction on the tab's session.

use std::sync::Arc;

use deadlock_core::connection::Environment;
use deadlock_core::driver::{Value, page_size};
use deadlock_core::edit::{
    self, ColumnInfo, Edit, INLINE_LIMIT, RowId, TableInfo, TableRef, edit_text, parse_input,
};
use gpui_kit::component::date_picker::{DatePickerEvent, DatePickerState};
use gpui_kit::component::input::{self, Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::table::{DataTable, TableEvent, TableSelection, TableState};
use gpui_kit::component::time_field::TimePrecision;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, Focusable as _, FontWeight, HighlightStyle,
    InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, StyledText, Subscription, Task, Window, actions, div, hsla, px,
};

use super::{QueryTab, Session, TabEvent, TxMode};
use crate::app::Db;
use crate::assets::Icon;
use crate::results::{Action, EditOverlay, Grid, GridEvent, date_kind, grid_actions, grid_text};
use crate::theme::{MONO_FONT, Palette, RADIUS, small_size};
use crate::ui::Clickable as _;
use crate::ui::{Kind as ButtonKind, button, icon_button};

actions!(
    table,
    [
        EditCell, SetNull, SetDefault, AddRow, DeleteRows, UndoChange, NextCell, PrevCell,
        PickDate, PasteCells
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
    /// The Edit JSON dialog: the text must parse, and is stored minified.
    json: bool,
    /// A date column's picker, beside the inline field.
    picker: Option<Entity<DatePickerState>>,
    _subs: Vec<Subscription>,
}

impl Editor {
    /// Focus is in the date picker or its popup: leaving the field for it keeps the
    /// editor open.
    fn in_picker(&self, window: &Window, cx: &App) -> bool {
        self.picker
            .as_ref()
            .is_some_and(|p| p.read(cx).focus_handle(cx).contains_focused(window, cx))
    }
}

/// The editor's text as the picker's value: `2026-11-22` or `2026-11-22 16:05:00`.
fn picker_value(text: &str) -> Option<jiff::civil::DateTime> {
    let t = text.trim();
    t.replacen(' ', "T", 1)
        .parse::<jiff::civil::DateTime>()
        .ok()
        .or_else(|| {
            t.parse::<jiff::civil::Date>()
                .ok()
                .map(|d| d.to_datetime(jiff::civil::Time::midnight()))
        })
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
    /// A manual "Count all rows" is running.
    counting: bool,
    /// The next load is a page change: a counted total stays.
    keep_total: bool,
    /// The page on screen, from 0, and its size in rows.
    page: usize,
    page_size: usize,
    /// A row past this page exists.
    has_next: bool,
    size_menu_open: bool,
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
        counting: false,
        keep_total: false,
        page: 0,
        page_size: page_size(),
        has_next: false,
        size_menu_open: false,
        busy: false,
        waiting: false,
        generation: 0,
        redis: None,
        _subs: subs,
    }
}

/// The number in a `count(*)` answer.
fn count_value(rows: &[deadlock_core::driver::Row]) -> Option<u64> {
    match rows.first().and_then(|r| r.first()) {
        Some(Value::Int(n)) => u64::try_from(*n).ok(),
        Some(Value::UInt(n)) => Some(*n),
        Some(v) => v.to_string().parse().ok(),
        None => None,
    }
}

/// `1234567` → `1,234,567`.
pub(super) fn grouped(n: u64) -> String {
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
        let first = (t.page * t.page_size) as u64;
        let last = first + shown as u64;
        let total = match t.total {
            Total::Exact(n) => format!(" of {}", grouped(n)),
            Total::About(n) => format!(" of about {}", grouped(n)),
            Total::Unknown if !t.has_next => format!(" of {}", grouped(last)),
            Total::Unknown => " of more".into(),
        };
        let rows = format!("Rows {} to {}{total}", grouped(first + 1), grouped(last));
        Some(match grid.sel_status() {
            Some(s) => format!("{s} · {rows}"),
            None => rows,
        })
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
        // A draft loads nothing; Create needs the session.
        if t.structure.is_draft() {
            if matches!(self.session, Session::None) && !t.waiting {
                t.waiting = true;
                cx.emit(TabEvent::NeedSession);
            }
            return;
        }
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
        if error.is_none() {
            self.read_draft_mode(cx);
            self.ensure_collations(cx);
        }
        let Some(t) = &mut self.table else { return };
        if !std::mem::take(&mut t.waiting) {
            return;
        }
        if t.structure.is_draft() {
            if let Some(e) = error {
                t.structure.fail(e);
            }
            cx.notify();
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
        t.busy = true;
        if !keep {
            t.notice = None;
            t.page = 0;
        }
        let (size, offset) = (t.page_size, t.page * t.page_size);
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
            // One row past the page tells whether a next page exists.
            let sql = edit::page_sql(&sql, info.oracle, size + 1, offset);
            let fail = |e: deadlock_core::driver::DriverError| (e.to_string(), true);
            let mut rows = driver
                .execute(&sql)
                .await
                .map_err(fail)?
                .collect()
                .await
                .map_err(fail)?;
            let more = rows.len() > size;
            rows.truncate(size);
            Ok((info, rows, more))
        });
        cx.spawn(async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err(("the table task stopped".into(), false)));
            let window = this.read_with(cx, |t, _| t.window).ok();
            let Some(window) = window else { return };
            let _ = cx.update_window(window, |_, window, cx| {
                this.update(cx, |this, cx| {
                    let result = result.map(|(info, rows, more)| {
                        if let Some(t) = this.table.as_mut().filter(|t| t.generation == generation)
                        {
                            t.has_next = more;
                        }
                        (info, rows, None)
                    });
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
        let sort = sort_of(&info, &t.order.read(cx).value()).unwrap_or_default();
        let row_offset = t.page * t.page_size;
        match t.grid.clone().filter(|_| keep) {
            Some(g) => g.update(cx, |g, cx| {
                let grid = g.delegate_mut();
                grid.set_rows(rows, stream);
                grid.row_offset = row_offset;
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
                grid.row_offset = row_offset;
                if key_tab {
                    grid.widths = super::key::widths(&info);
                }
                let reason = if read_only {
                    Some("Read-only connection: rows cannot be changed.".to_owned())
                } else {
                    info.read_only_reason().map(str::to_owned)
                };
                let mut overlay = EditOverlay::new(info.clone(), reason);
                overlay.sort = sort.clone();
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
                table.update(cx, |_, cx| Grid::track(cx));
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
        t.counting = false;
        if std::mem::take(&mut t.keep_total) && matches!(t.total, Total::Exact(_)) {
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
        // Statistics can be missing (views, unanalyzed tables) or stale: past the
        // limit the total stays unknown and the Count button counts it all.
        let Ok(sql) = edit::count_sql(&t.table, info, &filter, Some(COUNT_LIMIT + 1)) else {
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
            let n = count_value(&rows).filter(|&n| n <= COUNT_LIMIT);
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

    /// The "Count all rows" button: `count(*)` with the where on screen, for tables
    /// too big to count on every load. A newer load drops the answer.
    fn count_all(&mut self, cx: &mut Context<Self>) {
        let (Session::Ready(driver), Some(t)) = (&self.session, &mut self.table) else {
            return;
        };
        let Some(info) = &t.info else { return };
        if t.counting || t.busy {
            return;
        }
        let filter = t.filter.read(cx).value().to_string();
        let sql = match edit::count_sql(&t.table, info, &filter, None) {
            Ok(sql) => sql,
            Err(e) => {
                t.notice = Some((e, true));
                return cx.notify();
            }
        };
        t.counting = true;
        t.notice = None;
        let driver = Arc::clone(driver);
        let generation = t.generation;
        let rx = cx.global::<Db>().0.spawn(async move {
            let rows = driver.execute(&sql).await?.collect().await?;
            Ok::<_, deadlock_core::driver::DriverError>(rows)
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |this, cx| {
                let Some(t) = this.table.as_mut().filter(|t| t.generation == generation) else {
                    return;
                };
                t.counting = false;
                match result {
                    Ok(Ok(rows)) => match count_value(&rows) {
                        Some(n) => t.total = Total::Exact(n),
                        None => t.notice = Some(("the count returned no number".into(), true)),
                    },
                    Ok(Err(e)) => t.notice = Some((e.to_string(), true)),
                    Err(_) => t.notice = Some(("the table task stopped".into(), true)),
                }
                cx.emit(TabEvent::Changed);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Prev / Next: another page with the same where and order by, after the user
    /// agrees to drop the pending changes (they point into this page's rows).
    fn go_page(&mut self, page: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_discard(window, cx, move |this, _, cx| {
            let Some(t) = &mut this.table else { return };
            t.page = page;
            t.keep_total = true;
            t.notice = None;
            if let Some(g) = &t.grid {
                g.update(cx, |g, cx| g.scroll_to_row(0, cx));
            }
            this.load_table(true, cx);
        });
    }

    /// The page size picker: back to the first page.
    fn set_page_size(&mut self, size: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(t) = &mut self.table {
            t.size_menu_open = false;
        }
        self.ask_discard(window, cx, move |this, _, cx| {
            if let Some(t) = &mut this.table {
                t.page_size = size;
            }
            this.load_table(false, cx);
        });
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
            GridEvent::Sort(c, extend) => self.sort_by(c, extend, window, cx),
            GridEvent::Selected => {
                cx.emit(TabEvent::Changed);
                cx.notify();
            }
            GridEvent::Act(a) => {
                let grid = table.read(cx).delegate();
                let (sel, rows) = (grid.sel, grid.sel_rows());
                // The cell actions need a selection; the editor's own NULL does not.
                let head = sel.map(|s| s.head);
                let corner = sel.map(|s| (*s.rows().start(), *s.cols().start()));
                match (a, head, corner) {
                    (Action::Edit, Some((r, c)), _) => self.start_edit(r, c, window, cx),
                    (Action::PickDate, Some((r, c)), _) => self.pick_date(r, c, window, cx),
                    (Action::EditJson, Some((r, c)), _) => self.edit_json(r, c, window, cx),
                    (Action::Paste, _, Some((r, c))) => self.paste(r, c, cx),
                    (Action::Edit | Action::PickDate | Action::EditJson | Action::Paste, ..) => {}
                    (Action::Duplicate, ..) => self.duplicate_rows(window, cx),
                    (Action::CopyInsert, ..) => self.copy_insert(cx),
                    (Action::SetNull, ..) => {
                        self.set_selected(Edit::Value(Value::Null), window, cx);
                    }
                    (Action::SetDefault, ..) => self.set_selected(Edit::Default, window, cx),
                    (Action::Delete, ..) => self.delete_rows(cx),
                    (Action::Revert, ..) => {
                        self.cancel_edit(cx);
                        self.with_pending(cx, |p| {
                            for id in rows {
                                p.revert(id);
                            }
                        });
                    }
                }
            }
        }
    }

    /// Opens the editor on a date cell with its calendar open; Alt+Down in the
    /// editor toggles the calendar.
    fn pick_date(&mut self, display: usize, c: usize, window: &mut Window, cx: &mut Context<Self>) {
        let open = self
            .cell
            .as_ref()
            .filter(|ed| ed.col == c)
            .and_then(|ed| ed.picker.clone())
            .filter(|_| {
                let g = self.grid().map(|g| g.read(cx).delegate());
                let ed = self.cell.as_ref();
                g.zip(ed)
                    .is_some_and(|(g, ed)| g.display_row(ed.row) == Some(display))
            });
        if let Some(picker) = open {
            crate::results::toggle_picker(&picker, window, cx);
            return;
        }
        self.start_edit(display, c, window, cx);
        if self.cell.as_ref().is_some_and(|ed| ed.picker.is_some())
            && let Some(g) = self.grid().cloned()
        {
            g.update(cx, |g, cx| {
                if let Some(e) = &mut g.delegate_mut().edit {
                    e.open_picker = true;
                }
                cx.notify();
            });
        }
    }

    /// A header click: this column alone cycles ascending, descending, off; with Shift
    /// it is added to the keys (or cycled among them). The order by field is rewritten
    /// to match, replacing whatever text it held.
    fn sort_by(&mut self, col: usize, extend: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(t) = &self.table else { return };
        let Some(info) = &t.info else { return };
        let current = sort_of(info, &t.order.read(cx).value()).unwrap_or_default();
        let next = sort_text(info, &click_sort(&current, col, extend));
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
        self.open_editor(display, c, false, window, cx);
    }

    /// The Edit JSON dialog: the cell's object or array, pretty-printed.
    fn edit_json(&mut self, display: usize, c: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.open_editor(display, c, true, window, cx);
    }

    fn open_editor(
        &mut self,
        display: usize,
        c: usize,
        json: bool,
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
        let initial = if json {
            edit::json_reflow(&initial, Some(2))
        } else {
            initial
        };
        let dialog = json
            || col.is_binary()
            || initial.chars().count() > INLINE_LIMIT
            || initial.contains('\n')
            || matches!(
                col.data_type.as_str(),
                "text" | "mediumtext" | "longtext" | "json"
            );
        let (field, mut subs) = if dialog {
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
            (Field::Dialog(state), vec![sub])
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
                    InputEvent::Blur
                        if this
                            .cell
                            .as_ref()
                            .is_some_and(|e| e.error.is_none() && !e.in_picker(window, cx)) =>
                    {
                        this.commit_edit(window, cx);
                    }
                    _ => {}
                },
            );
            (Field::Inline(state), vec![sub])
        };
        let picker = match (&field, date_kind(&col)) {
            (Field::Inline(input), Some(with_time)) => {
                let start = picker_value(&initial);
                let picker = cx.new(|cx| {
                    let mut s = DatePickerState::new(window, cx);
                    if with_time {
                        s = s.time_precision(TimePrecision::Second);
                    }
                    if let Some(v) = start {
                        s.set_date_time(v, window, cx);
                    }
                    s
                });
                let format = if with_time {
                    "%Y-%m-%d %H:%M:%S"
                } else {
                    "%Y-%m-%d"
                };
                let input = input.clone();
                subs.push(cx.subscribe_in(
                    &picker,
                    window,
                    move |_, _, e: &DatePickerEvent, window, cx| {
                        let DatePickerEvent::Change(v) = e;
                        let Some(text) = v.format(format) else { return };
                        input.update(cx, |s, cx| {
                            s.set_value(text, window, cx);
                            // A date alone is picked in one click: back to the field.
                            if !with_time {
                                s.focus(window, cx);
                            }
                        });
                    },
                ));
                // The calendar hands focus back to the (hidden) picker when it closes:
                // pass it on to the field, so Enter commits and typing goes on.
                subs.push(cx.on_focus(
                    &picker.read(cx).focus_handle(cx),
                    window,
                    |this, window, cx| {
                        if let Some(Field::Inline(s)) = this.cell.as_ref().map(|e| &e.field) {
                            s.clone().update(cx, |s, cx| s.focus(window, cx));
                        }
                    },
                ));
                // Leaving the picker for anything but the field keeps a valid value,
                // as leaving the field does.
                subs.push(cx.on_focus_out(
                    &picker.read(cx).focus_handle(cx),
                    window,
                    |this, _, window, cx| {
                        let Some(ed) = &this.cell else { return };
                        let in_field = matches!(&ed.field, Field::Inline(s)
                            if s.read(cx).focus_handle(cx).is_focused(window));
                        if !in_field && ed.error.is_none() && !ed.in_picker(window, cx) {
                            this.commit_edit(window, cx);
                        }
                    },
                ));
                Some(picker)
            }
            _ => None,
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
                e.editing = inline.map(|s| (row, c, s, picker.clone()));
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
            json,
            picker,
            _subs: subs,
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
        let text = if ed.json {
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(_) => edit::json_reflow(&text, None),
                Err(e) => {
                    if let Some(ed) = &mut self.cell {
                        ed.error = Some(format!("Not valid JSON: {e}"));
                    }
                    cx.notify();
                    return false;
                }
            }
        } else {
            text
        };
        let original = self
            .grid()
            .and_then(|g| g.read(cx).delegate().original(row, c).cloned());
        match self.parse_cell(&col, &text, original.as_ref()) {
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

    /// Typed text as the column's value: a Redis key tab has its own rules.
    fn parse_cell(
        &self,
        col: &ColumnInfo,
        text: &str,
        original: Option<&Value>,
    ) -> Result<Value, String> {
        if self.is_key() {
            deadlock_core::redis::parse_cell(col, text, original)
        } else {
            parse_input(col, text)
        }
    }

    /// Pastes tab-separated clipboard text from cell (`display`, `c`) down and right,
    /// as pending changes. An empty field is NULL where the column takes it. Stops at
    /// the first value that does not fit, before changing anything.
    fn paste(&mut self, display: usize, c: usize, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) else {
            return;
        };
        let Some(g) = self.grid().cloned() else {
            return;
        };
        // One value pasted over a range of cells fills them all.
        let fill = g
            .read(cx)
            .delegate()
            .sel
            .filter(|s| s.count() > 1 && (*s.rows().start(), *s.cols().start()) == (display, c))
            .map(|s| (s.rows().count(), s.cols().count()));
        let mut sets = Vec::new();
        for (i, line) in paste_grid(&text, fill).into_iter().enumerate() {
            let Some(row) = g.read(cx).delegate().row_id(display + i) else {
                break;
            };
            for (j, field) in line.into_iter().enumerate() {
                let Some(col) = self.column(c + j) else { break };
                if let Some(why) = self.locked(row, &col, cx) {
                    self.table_note(why, true, cx);
                    return;
                }
                let original = g.read(cx).delegate().original(row, c + j).cloned();
                let value = if field.is_empty() && col.nullable {
                    Ok(Value::Null)
                } else {
                    self.parse_cell(&col, field, original.as_ref())
                };
                match value {
                    Ok(v) => sets.push((row, c + j, v, original)),
                    Err(e) => {
                        self.table_note(format!("{}: {e}", col.name), true, cx);
                        return;
                    }
                }
            }
        }
        self.cancel_edit(cx);
        self.with_pending(cx, |p| {
            for (row, c, v, original) in sets {
                p.set(row, c, Edit::Value(v), original.as_ref());
            }
        });
    }

    /// Copies of the selection's rows as new pending rows; generated and
    /// auto-increment columns keep their defaults.
    fn duplicate_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(why) = self.read_only_reason() {
            self.table_note(why, true, cx);
            return;
        }
        let (Some(g), Some(info)) = (
            self.grid().cloned(),
            self.table.as_ref().and_then(|t| t.info.clone()),
        ) else {
            return;
        };
        let grid = g.read(cx).delegate();
        let copies: Vec<Vec<Edit>> = grid
            .sel_rows()
            .into_iter()
            .map(|id| {
                info.columns
                    .iter()
                    .enumerate()
                    .map(|(c, col)| match grid.current(id, c) {
                        Some(v) if col.locked().is_none() && !col.auto_increment => {
                            Edit::Value(v.clone())
                        }
                        _ => Edit::Default,
                    })
                    .collect()
            })
            .collect();
        self.cancel_edit(cx);
        let mut added = None;
        self.with_pending(cx, |p| {
            for values in copies {
                added = Some(p.add_values(values));
            }
        });
        if let Some(d) = added.and_then(|a| g.read(cx).delegate().display_row(a)) {
            g.update(cx, |t, cx| {
                t.scroll_to_row(d, cx);
                t.set_selected_row(d, cx);
            });
            window.focus(&g.read(cx).focus_handle(cx), cx);
        }
    }

    /// The selection's rows as INSERT statements, with their pending values.
    fn copy_insert(&mut self, cx: &mut Context<Self>) {
        let (Some(g), Some(t)) = (self.grid(), self.table.as_ref()) else {
            return;
        };
        let Some(info) = &t.info else { return };
        let grid = g.read(cx).delegate();
        let sql: Result<Vec<String>, String> = grid
            .sel_rows()
            .into_iter()
            .map(|id| {
                let values: Vec<Edit> = (0..info.columns.len())
                    .map(|c| {
                        grid.current(id, c)
                            .cloned()
                            .map_or(Edit::Default, Edit::Value)
                    })
                    .collect();
                edit::insert_sql(&t.table, info, &values).map(|s| format!("{s};"))
            })
            .collect();
        match sql {
            Ok(s) => cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(s.join("\n"))),
            Err(e) => self.table_note(e, true, cx),
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

    /// Set NULL / Set DEFAULT on the cell being edited, else every selected cell.
    /// Changes nothing when one of them cannot take it.
    fn set_selected(&mut self, value: Edit, window: &mut Window, cx: &mut Context<Self>) {
        let targets: Vec<(RowId, usize)> = match &self.cell {
            Some(ed) => vec![(ed.row, ed.col)],
            None => self.grid().map_or_else(Vec::new, |g| {
                let g = g.read(cx).delegate();
                g.sel.map_or_else(Vec::new, |s| {
                    s.rows()
                        .filter_map(|r| g.row_id(r))
                        .flat_map(|id| s.cols().map(move |c| (id, c)))
                        .collect()
                })
            }),
        };
        let mut sets = Vec::with_capacity(targets.len());
        for (row, c) in targets {
            let Some(col) = self.column(c) else { return };
            if let Some(why) = self.locked(row, &col, cx) {
                self.table_note(why, true, cx);
                return;
            }
            if value == Edit::Value(Value::Null) && !col.nullable {
                self.table_note(format!("{} does not accept NULL.", col.name), true, cx);
                return;
            }
            let original = self
                .grid()
                .and_then(|g| g.read(cx).delegate().original(row, c).cloned());
            sets.push((row, c, original));
        }
        if sets.is_empty() {
            return;
        }
        self.cancel_edit(cx);
        self.with_pending(cx, |p| {
            for (row, c, original) in sets {
                p.set(row, c, value.clone(), original.as_ref());
            }
        });
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
        // The rows picked by number, else the selection's rows.
        let rows = g.read(cx).delegate().sel_rows();
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

/// Clipboard text as rows of tab-separated fields, without its last line break.
/// A single value fills `fill` (rows, columns) when given.
fn paste_grid(text: &str, fill: Option<(usize, usize)>) -> Vec<Vec<&str>> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    let rows: Vec<Vec<&str>> = text
        .split('\n')
        .map(|l| l.trim_end_matches('\r').split('\t').collect())
        .collect();
    match (fill, rows.as_slice()) {
        (Some((r, c)), [one]) if one.len() == 1 => vec![vec![one[0]; c]; r],
        _ => rows,
    }
}

/// The data columns and directions the order by text sorts on, in order, when it is
/// just a list of `col` or `` `col` ``, each optionally followed by `asc` or `desc`.
/// Empty text is an empty list.
fn sort_of(info: &TableInfo, order: &str) -> Option<Vec<(usize, bool)>> {
    let quote = if info.oracle { '"' } else { '`' };
    let mut rest = order.trim();
    let mut keys: Vec<(usize, bool)> = Vec::new();
    while !rest.is_empty() {
        let name = match rest.strip_prefix(quote) {
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
                rest = &quoted[end?..];
                name
            }
            None => {
                let n = rest
                    .find(|c: char| c.is_whitespace() || c == ',')
                    .unwrap_or(rest.len());
                let name = rest[..n].to_owned();
                rest = &rest[n..];
                name
            }
        };
        rest = rest.trim_start();
        let word = rest
            .find(|c: char| !c.is_alphabetic())
            .map_or(rest, |n| &rest[..n]);
        let desc = match word.to_ascii_lowercase().as_str() {
            "" | "asc" => false,
            "desc" => true,
            _ => return None,
        };
        rest = rest[word.len()..].trim_start();
        if !rest.is_empty() {
            rest = rest.strip_prefix(',')?.trim_start();
            if rest.is_empty() {
                return None;
            }
        }
        let col = info
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&name))?;
        if name.is_empty() || keys.iter().any(|(k, _)| *k == col) {
            return None;
        }
        keys.push((col, desc));
    }
    Some(keys)
}

/// The sort keys after a click on a header: the column goes ascending, then
/// descending, then off. Plain, it is the only key; extended (Shift), the other keys
/// stay, and a new one goes last.
fn click_sort(keys: &[(usize, bool)], col: usize, extend: bool) -> Vec<(usize, bool)> {
    let at = keys.iter().position(|(c, _)| *c == col);
    let next = match at.map(|i| keys[i].1) {
        None => Some(false),
        Some(false) => Some(true),
        Some(true) => None,
    };
    let mut out = if extend { keys.to_vec() } else { Vec::new() };
    match (at.filter(|_| extend), next) {
        (Some(i), Some(d)) => out[i].1 = d,
        (Some(i), None) => {
            out.remove(i);
        }
        (None, Some(d)) => out.push((col, d)),
        (None, None) => {}
    }
    out
}

/// The order by text for the sort keys; the table's key follows in `select_sql`.
fn sort_text(info: &TableInfo, keys: &[(usize, bool)]) -> String {
    keys.iter()
        .map(|&(c, desc)| {
            let name = info.ident(&info.columns[c].name);
            if desc { format!("{name} desc") } else { name }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `sql` as a preview shows it: each line longer than `width` breaks at spaces outside
/// quotes, continued two spaces deeper than it started, so a long statement reads
/// without scrolling sideways. Shorter lines stay as they are. Display only.
pub(crate) fn wrap_sql(sql: &str, width: usize) -> String {
    let mut out = String::with_capacity(sql.len());
    // A quote may span lines, so its state carries over.
    let (mut quote, mut escaped) = (None, false);
    for (n, line) in sql.split('\n').enumerate() {
        if n > 0 {
            out.push('\n');
        }
        let mut words = Vec::new();
        let mut start = 0;
        for (i, c) in line.char_indices() {
            match quote {
                _ if escaped => escaped = false,
                Some(q) if c == q => quote = None,
                Some(q) if c == '\\' && q != '`' => escaped = true,
                Some(_) => {}
                None if matches!(c, '\'' | '"' | '`') => quote = Some(c),
                None if c.is_whitespace() => {
                    words.push(&line[start..i]);
                    start = i + c.len_utf8();
                }
                None => {}
            }
        }
        words.push(&line[start..]);
        if line.len() <= width {
            out.push_str(line);
            continue;
        }
        let indent = &line[..line.len() - line.trim_start().len()];
        let mut current = indent.to_owned();
        for word in words.into_iter().filter(|w| !w.is_empty()) {
            if !current.trim().is_empty() && current.len() + 1 + word.len() > width {
                out.push_str(&current);
                out.push('\n');
                current = format!("{indent}  ");
            } else if !current.trim().is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        out.push_str(&current);
    }
    out
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
/// A New table draft has no Data yet.
pub(super) fn segments(view: View, draft: bool, cx: &mut Context<QueryTab>) -> gpui_kit::Div {
    let p = *cx.global::<Palette>();
    let segment = |id: &'static str, label: &'static str, target: View, tip: &'static str| {
        let on = view == target;
        let off = draft && target == View::Data;
        let tip = if off { "Create the table first" } else { tip };
        button(id, ButtonKind::Outline, !off, tip, cx)
            .when(on, |b| b.bg(p.tx).border_color(p.tx).text_color(p.bg))
            .child(label)
            .when(!off, |b| {
                b.clickable(
                    cx.listener(move |this, _, window, cx| this.set_view(target, window, cx)),
                )
            })
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

/// `Page 3 of 12`, or `Page 3` while the total is unknown.
fn page_label(page: usize, size: usize, total: Total) -> String {
    let pages = match total {
        Total::Exact(n) => Some(n),
        Total::About(n) => Some(n),
        Total::Unknown => None,
    }
    .map(|n| n.div_ceil(size.max(1) as u64).max(1));
    match pages {
        Some(n) => format!("Page {} of {}", page + 1, grouped(n)),
        None => format!("Page {}", page + 1),
    }
}

/// Under the grid: Previous, the page, Next, and the page size.
fn pager(t: &TableData, cx: &mut Context<QueryTab>) -> gpui_kit::Div {
    use gpui_kit::component::popover::Popover;
    let p = *cx.global::<Palette>();
    let (page, size, busy) = (t.page, t.page_size, t.busy);
    let (prev, next) = (!busy && page > 0, !busy && t.has_next);
    let prev_button = button("page-prev", ButtonKind::Outline, prev, "Previous page", cx)
        .child("Previous")
        .when(prev, |b| {
            b.clickable(cx.listener(move |this, _, window, cx| this.go_page(page - 1, window, cx)))
        });
    let next_button = button("page-next", ButtonKind::Outline, next, "Next page", cx)
        .child("Next")
        .when(next, |b| {
            b.clickable(cx.listener(move |this, _, window, cx| this.go_page(page + 1, window, cx)))
        });
    let counting = t.counting;
    let count_button = (t.total == Total::Unknown && t.info.is_some()).then(|| {
        let on = !busy && !counting;
        icon_button(
            "page-count",
            Icon::Table,
            None,
            ButtonKind::Ghost,
            on,
            if counting {
                "Counting…"
            } else {
                "Count all rows"
            },
            cx,
        )
        .when(on, |b| {
            b.clickable(cx.listener(|this, _, _, cx| this.count_all(cx)))
        })
        .when(counting, |b| b.child("Counting…"))
    });
    let trigger = button("page-size", ButtonKind::Ghost, !busy, "", cx)
        .child(format!("{} rows per page", grouped(size as u64)))
        .child(crate::ui::button_icon(
            "page-size",
            Icon::ChevronDown,
            ButtonKind::Ghost,
            !busy,
            cx,
        ));
    let this = cx.entity().downgrade();
    let sizes = Popover::new("page-size-menu")
        .p(px(4.))
        .open(t.size_menu_open)
        .on_open_change(cx.listener(|this, open: &bool, _, cx| {
            if let Some(t) = &mut this.table {
                t.size_menu_open = *open;
            }
            cx.notify();
        }))
        .trigger(crate::shell::Trigger(
            trigger,
            t.size_menu_open,
            "Rows per page",
        ))
        .content(move |_, _, _| {
            let rows = deadlock_core::settings::PAGE_SIZES.map(|n| {
                let n = n as usize;
                let this = this.clone();
                div()
                    .id(("page-size", n))
                    .flex()
                    .items_center()
                    .h(crate::theme::row_height())
                    .px(px(8.))
                    .rounded(RADIUS)
                    .when(n == size, |d| d.bg(p.sel))
                    .hover(|s| s.bg(p.hover))
                    .child(grouped(n as u64))
                    .clickable(move |_, window, cx| {
                        let _ = this.update(cx, |this, cx| this.set_page_size(n, window, cx));
                    })
            });
            div().flex().flex_col().gap(px(1.)).children(rows)
        });
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.))
        .px(px(16.))
        .py(px(6.))
        .border_t_1()
        .border_color(p.line)
        .text_size(small_size())
        .child(prev_button)
        .child(
            div()
                .text_color(p.tx2)
                .child(page_label(page, size, t.total)),
        )
        .children(count_button)
        .child(next_button)
        .child(div().flex_1())
        .child(sizes)
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
        let draft = this.table.as_ref().is_some_and(|t| t.structure.is_draft());
        let segments = segments(View::Structure, draft, cx);
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
        .child(segments(View::Data, false, cx))
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
                b.clickable(cx.listener(|this, _, window, cx| this.add_row(window, cx)))
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
                b.clickable(cx.listener(|this, _, _, cx| this.delete_rows(cx)))
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
            grid_actions(div().key_context("Results"), g)
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
    let pager = (!key && t.grid.is_some()).then(|| pager(t, cx));
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
                            b.clickable(
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
                            b.clickable(cx.listener(|this, _, window, cx| {
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
                                .clickable(cx.listener(
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
                                .clickable(cx.listener(
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
                                .clickable(cx.listener(
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
        .on_action(cx.listener(|this, _: &PickDate, window, cx| {
            let at = this.cell.as_ref().map(|ed| (ed.row, ed.col));
            let display = at
                .and_then(|(row, c)| Some((this.grid()?.read(cx).delegate().display_row(row)?, c)));
            if let Some((r, c)) = display {
                this.pick_date(r, c, window, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &PasteCells, _, cx| {
            let corner = this
                .grid()
                .and_then(|g| g.read(cx).delegate().sel)
                .map(|s| (*s.rows().start(), *s.cols().start()));
            if let Some((r, c)) = corner {
                this.paste(r, c, cx);
            }
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
        .children(pager)
        .children(panel)
        .children(dialog)
        .children(super::key::dialog(this, cx))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_lines_wrap_keeping_their_indent_and_short_lines_as_typed() {
        let sql = "select  1;\n  update t set note = 'a b c d e', qty = 1 where id = 2;";
        assert_eq!(
            wrap_sql(sql, 24),
            "select  1;\n  update t set note =\n    'a b c d e', qty = 1\n    where id = 2;"
        );
        // A quote opened on an earlier line keeps its spaces.
        assert_eq!(wrap_sql("'x\ny z' or a", 4), "'x\ny z'\n  or\n  a");
    }

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
        assert_eq!(sort_of(&info, "id"), Some(vec![(0, false)]));
        assert_eq!(sort_of(&info, "ID desc"), Some(vec![(0, true)]));
        assert_eq!(sort_of(&info, "`Created At` DESC"), Some(vec![(1, true)]));
        assert_eq!(
            sort_of(&info, "id, `Created At` desc"),
            Some(vec![(0, false), (1, true)])
        );
        assert_eq!(sort_of(&info, "id, created"), None);
        assert_eq!(sort_of(&info, "id,"), None);
        assert_eq!(sort_of(&info, "id, ID"), None);
        assert_eq!(sort_of(&info, "id desc nulls"), None);
        assert_eq!(sort_of(&info, ""), Some(vec![]));
        // Clicks: alone it cycles; with Shift it adds, cycles in place, then drops.
        let (a, d) = (false, true);
        assert_eq!(click_sort(&[], 0, false), vec![(0, a)]);
        assert_eq!(click_sort(&[(0, a)], 0, false), vec![(0, d)]);
        assert_eq!(click_sort(&[(0, d)], 0, false), vec![]);
        assert_eq!(click_sort(&[(0, a), (1, d)], 1, false), vec![]);
        assert_eq!(click_sort(&[(0, a), (1, d)], 2, false), vec![(2, a)]);
        assert_eq!(click_sort(&[(0, a)], 1, true), vec![(0, a), (1, a)]);
        assert_eq!(click_sort(&[(0, a), (1, a)], 0, true), vec![(0, d), (1, a)]);
        assert_eq!(click_sort(&[(0, d), (1, a)], 0, true), vec![(1, a)]);
        assert_eq!(
            sort_text(&info, &[(1, true), (0, false)]),
            "`Created At` desc, `id`"
        );
        assert_eq!(grouped(1_234_567), "1,234,567");
        assert_eq!(grouped(214), "214");
    }

    #[test]
    fn pasted_text_and_fills() {
        assert_eq!(paste_grid("1\ta\n2\tb\n", None), [["1", "a"], ["2", "b"]]);
        assert_eq!(paste_grid("1\r\n2\r\n", None), [["1"], ["2"]]);
        // One value over a 2 × 3 selection fills it; more than one pastes as is.
        assert_eq!(paste_grid("x", Some((2, 3))), [["x"; 3], ["x"; 3]]);
        assert_eq!(paste_grid("x\ty", Some((2, 3))), [["x", "y"]]);
        assert_eq!(paste_grid("", None), [[""]]);
    }

    #[test]
    fn count_answers() {
        let row = |v| vec![vec![v]];
        assert_eq!(count_value(&row(Value::Int(7))), Some(7));
        assert_eq!(count_value(&row(Value::Int(-1))), None);
        assert_eq!(count_value(&row(Value::UInt(9))), Some(9));
        assert_eq!(count_value(&[]), None);
    }

    #[test]
    fn pages_and_picker_values() {
        assert_eq!(page_label(0, 500, Total::Unknown), "Page 1");
        assert_eq!(page_label(2, 500, Total::Exact(1001)), "Page 3 of 3");
        assert_eq!(page_label(0, 500, Total::Exact(0)), "Page 1 of 1");
        let dt = picker_value("2026-11-22 16:05:07").unwrap();
        assert_eq!(
            dt.strftime("%Y-%m-%d %H:%M:%S").to_string(),
            "2026-11-22 16:05:07"
        );
        let d = picker_value("2026-11-22").unwrap();
        assert_eq!(
            d.strftime("%Y-%m-%d %H:%M:%S").to_string(),
            "2026-11-22 00:00:00"
        );
        assert!(picker_value("").is_none() && picker_value("soon").is_none());
        // What the picker writes reads back as the column's value.
        let col = |t: &str| ColumnInfo {
            data_type: t.into(),
            ..ColumnInfo::default()
        };
        assert!(parse_input(&col("datetime"), "2026-11-22 16:05:07").is_ok());
        assert!(parse_input(&col("date"), "2026-11-22").is_ok());
        assert_eq!(date_kind(&col("timestamp")), Some(true));
        assert_eq!(date_kind(&col("time")), None);
    }
}
