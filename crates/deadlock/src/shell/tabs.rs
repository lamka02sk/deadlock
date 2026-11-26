//! Editor tabs: new, open, save, close, consoles, the tab strip, and the tab list in
//! `state.json`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deadlock_core::connection::ConnectionConfig;
use deadlock_core::project::{TabState, console_path, load_text, save_text};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, FontWeight, FutureExt as _, InteractiveElement,
    IntoElement, ParentElement, PathPromptOptions, SharedString, StatefulInteractiveElement,
    Styled, Window, div, px,
};

use super::{Conn, Front, Shell};

/// How many closed tabs Ctrl+Shift+T can bring back.
const CLOSED_LIMIT: usize = 25;

/// How long closing, switching or moving waits for a tab's transaction check.
const TX_CHECK_WAIT: Duration = Duration::from_secs(3);

/// What a tab's transaction check means for a close, a switch or a move.
#[derive(Debug, PartialEq, Eq)]
enum TxAnswer {
    /// An open transaction: Commit / Roll back / Cancel.
    Open,
    /// Nothing to commit: none open, or the connection was lost (the tab says so).
    Nothing,
    /// No answer in time, and the session may hold a transaction.
    Busy,
}

/// `answer` is the check's result, `None` when it did not come in time; `maybe_open`
/// is what the tab last knew ([`QueryTab::may_hold_tx`]).
fn tx_answer(answer: Option<&Result<bool, String>>, maybe_open: bool) -> TxAnswer {
    match answer {
        Some(Ok(true)) => TxAnswer::Open,
        Some(_) => TxAnswer::Nothing,
        None if maybe_open => TxAnswer::Busy,
        None => TxAnswer::Nothing,
    }
}
use crate::assets::{Icon, icon};
use crate::query::{Kind, QueryTab, Scope, TabEvent, TxMode, View};
use crate::theme::{Palette, tab_height};
use crate::ui::Clickable as _;
use deadlock_core::edit::TableRef;
use deadlock_core::objects::{ObjectKind, ObjectRef};
use gpui_kit::{MouseButton, MouseUpEvent};

/// What runs once a tab closed: [`Shell::close_tab_then`].
pub(super) type AfterClose = Box<dyn FnOnce(&mut Shell, &mut Window, &mut Context<Shell>)>;

impl Shell {
    /// A saved connection, as a tab binds to it.
    pub(super) fn binding(&self, id: Option<&str>) -> Option<ConnectionConfig> {
        self.config(id?).cloned()
    }

    /// Reopens the tabs listed in `state.json`. Files that are gone are skipped.
    pub(super) fn restore_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let saved = std::mem::take(&mut self.project.state.tabs);
        let front = self.project.state.active_tab;
        for tab in saved {
            if let TabState::Object {
                connection,
                schema,
                name,
                object,
            } = tab
            {
                if let Some(kind) = ObjectKind::from_label(&object)
                    && self.config(&connection).is_some()
                {
                    let o = ObjectRef::new(schema, name, kind);
                    self.restored_objects.push((connection, o));
                }
                continue;
            }
            let Some((kind, connection, text, schema)) = self.saved_tab(tab) else {
                continue;
            };
            let binding = self.binding(connection.as_deref());
            let i = self.add_tab(kind, binding, text, window, cx);
            self.tabs[i].0.update(cx, |t, _| t.restore_schema(schema));
        }
        self.front = match front {
            Some(i) if i < self.tabs.len() => Front::Query(i),
            _ if self.tabs.is_empty() => Front::None,
            _ => Front::Query(0),
        };
        if let Some(tab) = self.front_tab().cloned() {
            tab.update(cx, |t, cx| t.open_table(cx));
        }
        // Object tabs open once their connection connects; a table tab in front
        // connects first, so two password prompts never race.
        if !self.front_tab().is_some_and(|t| t.read(cx).table.is_some()) {
            self.connect_restored(cx);
        }
        self.follow_front(cx);
    }

    /// A saved editor tab as a tab to add: its kind, connection, text and schema.
    /// `None` for an object tab, and for a file or console that cannot be read.
    fn saved_tab(&self, tab: TabState) -> Option<(Kind, Option<String>, String, Option<String>)> {
        Some(match tab {
            TabState::File {
                path: Some(path),
                connection,
                schema,
                ..
            } => match std::fs::read_to_string(&path) {
                Ok(text) => (Kind::File(Some(path)), connection, text, schema),
                Err(e) => {
                    eprintln!("deadlock: {}: {e}", path.display());
                    return None;
                }
            },
            TabState::File {
                path: None,
                connection,
                text,
                schema,
            } => (
                Kind::File(None),
                connection,
                text.unwrap_or_default(),
                schema,
            ),
            TabState::Table {
                connection,
                database,
                table,
            } => (
                Kind::Table(TableRef { database, table }),
                Some(connection),
                String::new(),
                None,
            ),
            TabState::Console { connection, schema } => {
                match console_path(self.project.root(), &connection)
                    .and_then(|p| load_text(&p).map(|t| (p, t)))
                {
                    Ok((path, text)) => (Kind::Console(path), Some(connection), text, schema),
                    Err(e) => {
                        eprintln!("deadlock: console: {e}");
                        return None;
                    }
                }
            }
            TabState::Object { .. } => return None,
        })
    }

    /// Ctrl+W: closes the tab in front, asking what its own close asks.
    pub(super) fn close_front(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.front {
            Front::Query(i) => self.close_tab(i, window, cx),
            Front::Object(i) => self.close_object(i, window, cx),
            Front::Sessions(i) => self.close_sessions(i, cx),
            Front::Config(i) => self.close_config(i, window, cx),
            Front::Diagram(i) => self.close_diagram(i, cx),
            Front::Import => self.close_import(cx),
            Front::Settings => self.close_settings(cx),
            Front::Form => self.close_form(cx),
            Front::None => {}
        }
    }

    /// Keeps a closed tab for Ctrl+Shift+T, the last [`CLOSED_LIMIT`] of them.
    pub(super) fn remember_closed(&mut self, tab: TabState) {
        self.closed.retain(|t| *t != tab);
        self.closed.push(tab);
        if self.closed.len() > CLOSED_LIMIT {
            self.closed.remove(0);
        }
    }

    /// Ctrl+Shift+T: opens the tab closed last again, or brings it forward when it is
    /// open. One whose connection, file or object is gone is skipped for the one before.
    pub(super) fn reopen_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        while let Some(tab) = self.closed.pop() {
            let connection = match &tab {
                TabState::File { connection, .. } => connection.clone(),
                TabState::Console { connection, .. }
                | TabState::Table { connection, .. }
                | TabState::Object { connection, .. } => Some(connection.clone()),
            };
            if connection
                .as_deref()
                .is_some_and(|c| self.config(c).is_none())
            {
                continue;
            }
            if let TabState::Object {
                connection,
                schema,
                name,
                object,
            } = tab
            {
                let Some(kind) = ObjectKind::from_label(&object) else {
                    continue;
                };
                let o = ObjectRef::new(schema, name, kind);
                self.open_object(&connection, o, None, window, cx);
                return;
            }
            let Some((kind, connection, text, schema)) = self.saved_tab(tab) else {
                continue;
            };
            let open = self.tabs.iter().position(|(t, _)| {
                let t = t.read(cx);
                t.kind == kind
                    && !matches!(kind, Kind::File(None))
                    && t.connection.as_ref().map(|c| &c.id) == connection.as_ref()
            });
            let i = open.unwrap_or_else(|| {
                let binding = self.binding(connection.as_deref());
                let i = self.add_tab(kind, binding, text, window, cx);
                self.tabs[i].0.update(cx, |t, _| t.restore_schema(schema));
                i
            });
            self.show_tab(Front::Query(i), window, cx);
            return;
        }
    }

    pub(super) fn add_tab(
        &mut self,
        kind: Kind,
        binding: Option<ConnectionConfig>,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let untitled = matches!(kind, Kind::File(None));
        let inline = self.results_inline(cx);
        let root = self.project.root().to_owned();
        let widths = match (&kind, &binding) {
            (Kind::Table(r), Some(c)) => self
                .project
                .state
                .widths(&c.id, &r.database, &r.table)
                .cloned(),
            _ => None,
        };
        let tab = cx.new(|cx| {
            let mut t = QueryTab::new(kind, binding, text, window, cx);
            t.inline = inline;
            t.root = root.clone();
            if let (Some(t), Some(w)) = (&mut t.table, widths) {
                t.widths = w;
            }
            t
        });
        if untitled {
            self.untitled += 1;
            let n = self.untitled;
            tab.update(cx, |t, _| t.untitled = n);
        }
        super::complete::install(cx.entity().downgrade(), &tab, cx);
        let sub = cx.subscribe_in(&tab, window, Self::on_tab_event);
        self.tabs.push((tab, sub));
        self.tabs.len() - 1
    }

    fn on_tab_event(
        &mut self,
        tab: &Entity<QueryTab>,
        e: &TabEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let conn = tab.read(cx).connection.as_ref().map(|c| c.id.clone());
        match e {
            TabEvent::NeedSession => self.provide_session(tab, cx),
            TabEvent::OpenTable(table) => {
                if let Some(conn) = conn {
                    self.open_table(&conn, table.clone(), View::Structure, window, cx);
                }
            }
            TabEvent::SchemaChanged(database) => {
                if let Some(conn) = conn
                    && matches!(self.conns.get(&conn), Some(Conn::Connected(_)))
                {
                    self.load(vec![conn, database.clone()], cx);
                }
            }
            TabEvent::NeedTables(schema) => {
                let others = conn
                    .map(|conn| self.tree.cached_tables(&conn, schema))
                    .unwrap_or_default();
                tab.update(cx, |t, cx| t.reference_tables(others, window, cx));
            }
            TabEvent::ResultsInline(inline) => {
                let inline = *inline;
                self.project.state.results_in_panel = Some(!inline);
                for (t, _) in &self.tabs {
                    t.update(cx, |t, cx| t.set_inline(inline, cx));
                }
                self.schedule_save(cx);
            }
            TabEvent::KeysChanged(db, removed, added) => {
                if let Some(conn) = conn {
                    self.keys_changed(&conn, db, removed.clone(), added.clone(), cx);
                }
            }
            TabEvent::TableRenamed(from, to) => {
                if let Some(conn) = conn {
                    if from.table.is_empty() {
                        // Created: the schema reload that follows selects it.
                        self.reveal = Some(vec![
                            conn.clone(),
                            to.database.clone(),
                            "Tables".into(),
                            to.table.clone(),
                        ]);
                    } else {
                        let objects: Vec<_> = self.objects.iter().map(|(o, _)| o.clone()).collect();
                        for o in objects {
                            o.update(cx, |o, cx| {
                                o.table_renamed(&conn, from, &to.table, window, cx);
                            });
                        }
                    }
                    self.table_changed(&conn, from, Some(&to.table), cx);
                }
                self.schedule_save(cx);
                cx.notify();
            }
            TabEvent::TableDropped(table) => {
                if let Some(conn) = conn {
                    self.table_changed(&conn, table, None, cx);
                    self.close_objects_on_table(&conn, table, window, cx);
                }
                // No prompts: the dialog discarded pending edits, and the DDL committed.
                if let Some(i) = self.tabs.iter().position(|(t, _)| t == tab) {
                    self.remove_tab(i, cx);
                    // The table is gone: nothing to reopen.
                    self.closed.pop();
                }
            }
            TabEvent::Discard => {
                if let Some(i) = self.tabs.iter().position(|(t, _)| t == tab) {
                    self.remove_tab(i, cx);
                }
            }
            TabEvent::Recorded => self.history_changed(cx),
            TabEvent::ColumnWidths(widths) => {
                if let (Some(conn), Kind::Table(r)) = (conn, &tab.read(cx).kind) {
                    let widths = widths.clone();
                    self.project
                        .state
                        .set_widths(&conn, &r.database, &r.table, widths);
                    self.schedule_save(cx);
                }
            }
            TabEvent::SchemaSet => {
                self.schedule_save(cx);
                cx.notify();
            }
            TabEvent::Changed => {
                if matches!(tab.read(cx).kind, Kind::File(None)) {
                    self.schedule_save(cx);
                }
                cx.notify();
            }
        }
    }

    /// Hands a tab its connection's settings and password, connecting the connection
    /// (and asking for the password) first when needed.
    pub(super) fn provide_session(&mut self, tab: &Entity<QueryTab>, cx: &mut Context<Self>) {
        let Some(id) = tab.read(cx).connection.as_ref().map(|c| c.id.clone()) else {
            return;
        };
        let (Some(config), Some(password)) = (self.config(&id).cloned(), self.passwords.get(&id))
        else {
            if !matches!(self.conns.get(&id), Some(Conn::Connecting)) && self.prompt.is_none() {
                self.connect(&id, cx);
            }
            return;
        };
        let password = password.clone();
        tab.update(cx, |t, cx| t.connect(config, password, cx));
    }

    /// The connection `id` connected or failed: tabs waiting for it go on or give up.
    pub(super) fn resume_tabs(&mut self, id: &str, error: Option<&str>, cx: &mut Context<Self>) {
        let waiting: Vec<_> = self
            .tabs
            .iter()
            .map(|(t, _)| t.clone())
            .filter(|t| {
                let t = t.read(cx);
                t.needs_session() && t.connection.as_ref().is_some_and(|c| c.id == id)
            })
            .collect();
        for tab in waiting {
            match error {
                None => self.provide_session(&tab, cx),
                Some(e) => tab.update(cx, |t, cx| t.fail_session(e.to_owned(), cx)),
            }
        }
    }

    pub(super) fn front_tab(&self) -> Option<&Entity<QueryTab>> {
        match self.front {
            Front::Query(i) => self.tabs.get(i).map(|(t, _)| t),
            _ => None,
        }
    }

    /// The connection the toolbar, status bar and new tabs use: the front tab's, else
    /// the one selected in the tree, else the project's first.
    pub(super) fn active(&self, cx: &App) -> Option<String> {
        let front = match self.front {
            Front::Query(i) => self
                .tabs
                .get(i)
                .and_then(|(t, _)| t.read(cx).connection.as_ref().map(|c| c.id.clone())),
            Front::Sessions(i) => self
                .sessions
                .get(i)
                .map(|(t, _)| t.read(cx).connection.id.clone()),
            Front::Config(i) => self
                .configs
                .get(i)
                .map(|(t, _)| t.read(cx).connection.id.clone()),
            Front::Diagram(i) => self
                .diagrams
                .get(i)
                .map(|(t, _)| t.read(cx).connection.id.clone()),
            Front::Object(i) => self
                .objects
                .get(i)
                .map(|(t, _)| t.read(cx).connection.id.clone()),
            Front::None | Front::Form | Front::Import | Front::Settings => None,
        };
        front
            .or_else(|| {
                let id = self.selected.as_ref()?.first()?;
                self.config(id).map(|c| c.id.clone())
            })
            .or_else(|| {
                self.project
                    .config
                    .connections
                    .first()
                    .map(|c| c.id.clone())
            })
    }

    /// The front tab changed: the toolbar and status bar follow it.
    pub(super) fn follow_front(&mut self, cx: &mut Context<Self>) {
        self.schedule_save(cx);
        cx.notify();
    }

    pub(super) fn show_tab(&mut self, front: Front, window: &mut Window, cx: &mut Context<Self>) {
        self.front = front;
        self.sidebar_last = false;
        if let Some(tab) = self.front_tab() {
            tab.clone().update(cx, |t, cx| t.open_table(cx));
        }
        self.focus_front(window, cx);
        self.follow_front(cx);
    }

    /// Focuses the editor tab in front (its grid or editor), else the window, so the
    /// window's shortcuts reach it.
    pub(super) fn focus_front(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.front_tab().cloned() {
            Some(tab) if tab.read(cx).table.is_some() => {
                tab.update(cx, |t, cx| t.focus_table(window, cx));
            }
            Some(tab) => {
                let editor = tab.read(cx).editor.clone();
                editor.update(cx, |e, cx| e.focus(window, cx));
            }
            None => window.focus(&self.focus, cx),
        }
    }

    /// Opens a table or view of connection `conn` as a tab, or brings its tab forward;
    /// a reference link asks for its Structure view.
    pub(super) fn open_table(
        &mut self,
        conn: &str,
        table: TableRef,
        view: View,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(binding) = self.binding(Some(conn)) else {
            return;
        };
        let open = self.tabs.iter().position(|(t, _)| {
            let t = t.read(cx);
            t.kind == Kind::Table(table.clone())
                && t.connection.as_ref().is_some_and(|c| c.id == conn)
        });
        let i = open.unwrap_or_else(|| {
            self.add_tab(Kind::Table(table), Some(binding), String::new(), window, cx)
        });
        if view == View::Structure
            && let Some((tab, _)) = self.tabs.get(i)
        {
            tab.update(cx, |t, _| {
                if let Some(t) = &mut t.table {
                    t.view = View::Structure;
                }
            });
        }
        self.show_tab(Front::Query(i), window, cx);
    }

    /// Ctrl+N: an untitled buffer on the active connection.
    pub(super) fn new_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let binding = self.binding(self.active(cx).as_deref());
        let i = self.add_tab(Kind::File(None), binding, String::new(), window, cx);
        self.show_tab(Front::Query(i), window, cx);
    }

    /// Ctrl+Shift+N: the console of what was clicked last (a sidebar row or the front
    /// tab), opened or brought forward, on that row's or tab's schema when it has one.
    pub(super) fn open_console(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.console_target(cx) else {
            return;
        };
        let Some(binding) = self.binding(Some(&target.connection)) else {
            return;
        };
        let open = self.tabs.iter().position(|(t, _)| {
            let t = t.read(cx);
            matches!(t.kind, Kind::Console(_))
                && t.connection.as_ref().is_some_and(|c| c.id == binding.id)
        });
        let i = match open {
            Some(i) => i,
            None => match console_path(self.project.root(), &binding.id)
                .and_then(|p| load_text(&p).map(|t| (p, t)))
            {
                Ok((path, text)) => {
                    self.add_tab(Kind::Console(path), Some(binding), text, window, cx)
                }
                Err(e) => {
                    eprintln!("deadlock: console: {e}");
                    return;
                }
            },
        };
        if let Some(schema) = target.schema {
            let tab = self.tabs[i].0.clone();
            if tab.read(cx).schema() != Some(schema.as_str()) {
                tab.update(cx, |t, cx| t.switch_schema(schema, cx));
            }
        }
        self.show_tab(Front::Query(i), window, cx);
    }

    /// Ctrl+O: the native file picker, starting in the project folder.
    pub(super) fn open_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Open".into()),
        });
        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let paths = match picked.await {
                Ok(Ok(Some(paths))) => paths,
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(e)) => {
                    eprintln!("deadlock: file picker: {e:#}");
                    return;
                }
            };
            let _ = cx.update_window(handle, |_, window, cx| {
                this.update(cx, |this, cx| {
                    for path in paths {
                        this.open_path(path, window, cx);
                    }
                })
            });
        })
        .detach();
    }

    pub(crate) fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        let open = self
            .tabs
            .iter()
            .position(|(t, _)| t.read(cx).kind == Kind::File(Some(path.clone())));
        let i = match open {
            Some(i) => i,
            None => match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let binding = self.binding(self.active(cx).as_deref());
                    self.add_tab(Kind::File(Some(path)), binding, text, window, cx)
                }
                Err(e) => {
                    eprintln!("deadlock: {}: {e}", path.display());
                    return;
                }
            },
        };
        self.show_tab(Front::Query(i), window, cx);
    }

    /// Ctrl+S: writes the front tab; an untitled buffer asks where first.
    pub(super) fn save_front(&mut self, cx: &mut Context<Self>) {
        if let Some(tab) = self.front_tab().cloned() {
            self.save_tab(&tab, None, cx);
        }
    }

    /// Saves `tab`, then closes tab `close` (a save prompt's "Save").
    fn save_tab(&mut self, tab: &Entity<QueryTab>, close: Option<usize>, cx: &mut Context<Self>) {
        let kind = tab.read(cx).kind.clone();
        match kind {
            Kind::Table(_) => {}
            Kind::Console(_) => tab.update(cx, |t, cx| t.save_console(cx)),
            Kind::File(Some(path)) => {
                if write(tab, &path, cx) {
                    self.after_save(close, cx);
                }
            }
            Kind::File(None) => {
                let picked = cx.prompt_for_new_path(self.project.root(), Some("query.sql"));
                let tab = tab.clone();
                cx.spawn(async move |this, cx| {
                    let path = match picked.await {
                        Ok(Ok(Some(path))) => path,
                        Ok(Ok(None)) | Err(_) => return,
                        Ok(Err(e)) => {
                            eprintln!("deadlock: save picker: {e:#}");
                            return;
                        }
                    };
                    let _ = this.update(cx, |this, cx| {
                        if write(&tab, &path, cx) {
                            tab.update(cx, |t, _| t.kind = Kind::File(Some(path)));
                            this.after_save(close, cx);
                        }
                    });
                })
                .detach();
            }
        }
    }

    fn after_save(&mut self, close: Option<usize>, cx: &mut Context<Self>) {
        if let Some(i) = close {
            self.remove_tab(i, cx);
        }
        self.schedule_save(cx);
        cx.notify();
    }

    /// Closes tab `i`, asking first about pending table changes, then about an open
    /// transaction, then about unsaved changes. Consoles save themselves and never ask
    /// about the text.
    pub(super) fn close_tab(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.close_tab_then(i, None, window, cx);
    }

    /// [`Self::close_tab`], then `then` once the tab is gone. A Cancel anywhere drops it.
    pub(super) fn close_tab_then(
        &mut self,
        i: usize,
        then: Option<AfterClose>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(i).map(|(t, _)| t.clone()) else {
            return;
        };
        if tab.read(cx).pending_changes(cx) > 0 && !std::mem::take(&mut self.pending_settled) {
            let question = format!(
                "Submit the pending changes before closing {}?",
                self.tab_title(tab.read(cx))
            );
            self.settle_pending(vec![tab.clone()], question, cx, move |this, window, cx| {
                if let Some(i) = this.tabs.iter().position(|(t, _)| *t == tab) {
                    this.pending_settled = true;
                    this.close_tab_then(i, then, window, cx);
                }
            });
            return;
        }
        self.pending_settled = false;
        if !std::mem::take(&mut self.tx_settled) {
            let question = format!(
                "Commit or roll back before closing {}?",
                self.tab_title(tab.read(cx))
            );
            self.settle(vec![tab.clone()], question, cx, move |this, window, cx| {
                // The tab may have moved while the prompt was open.
                if let Some(i) = this.tabs.iter().position(|(t, _)| *t == tab) {
                    this.tx_settled = true;
                    this.close_tab_then(i, then, window, cx);
                }
            });
            return;
        }
        let unapplied = if tab.read(cx).draft_typed(cx) {
            Some(("Discard the new table?", "It has not been created."))
        } else {
            tab.read(cx).form_unapplied(cx)
        };
        if let Some((question, text)) = unapplied {
            let answer = crate::dialog::ask(
                window,
                cx,
                crate::dialog::Ask::new(question, &["Discard", "Cancel"]).text(text),
            );
            let handle = self.window;
            cx.spawn(async move |this, cx| {
                if answer.await != Some(0) {
                    return;
                }
                let _ = cx.update_window(handle, |_, window, cx| {
                    this.update(cx, |this, cx| {
                        if let Some(i) = this.tabs.iter().position(|(t, _)| *t == tab) {
                            this.remove_tab(i, cx);
                            if let Some(then) = then {
                                then(this, window, cx);
                            }
                        }
                    })
                });
            })
            .detach();
            return;
        }
        let (ask, title) = {
            let t = tab.read(cx);
            let ask = match &t.kind {
                Kind::Console(_) | Kind::Table(_) => false,
                Kind::File(None) => !t.text(cx).trim().is_empty(),
                Kind::File(Some(_)) => t.dirty,
            };
            (ask, self.tab_title(t))
        };
        if !ask {
            tab.update(cx, |t, cx| t.save_console(cx));
            self.remove_tab(i, cx);
            if let Some(then) = then {
                then(self, window, cx);
            }
            return;
        }
        let answer = crate::dialog::ask(
            window,
            cx,
            crate::dialog::Ask::new(
                format!("Save changes to {title}?"),
                &["Save", "Don't Save", "Cancel"],
            ),
        );
        let handle = self.window;
        cx.spawn(async move |this, cx| {
            let Some(answer) = answer.await else { return };
            let _ = cx.update_window(handle, |_, window, cx| {
                this.update(cx, |this, cx| {
                    // The tab may have moved while the prompt was open.
                    let Some(i) = this.tabs.iter().position(|(t, _)| *t == tab) else {
                        return;
                    };
                    match answer {
                        0 => this.save_tab(&tab, Some(i), cx),
                        1 => this.remove_tab(i, cx),
                        _ => {}
                    }
                    // A file with a path saves at once, so the tab is gone by now.
                    if !this.tabs.iter().any(|(t, _)| *t == tab)
                        && let Some(then) = then
                    {
                        then(this, window, cx);
                    }
                })
            });
        })
        .detach();
    }

    pub(super) fn remove_tab(&mut self, i: usize, cx: &mut Context<Self>) {
        if i >= self.tabs.len() {
            return;
        }
        let (tab, _) = self.tabs.remove(i);
        let saved = Self::tab_state(tab.read(cx), cx);
        if let Some(saved) = saved.filter(|s| {
            !matches!(s, TabState::File { path: None, text, .. }
                if text.as_deref().is_none_or(|t| t.trim().is_empty()))
        }) {
            self.remember_closed(saved);
        }
        tab.update(cx, |t, cx| t.close_session(cx));
        self.front = match self.front {
            Front::Query(f) if f == i => match self.tabs.len() {
                0 => Front::None,
                n => Front::Query(i.min(n - 1)),
            },
            Front::Query(f) if f > i => Front::Query(f - 1),
            other => other,
        };
        self.follow_front(cx);
    }

    /// The front file tab, when choosing connection `id` would move it there.
    pub(super) fn moving_front(&self, id: &str, cx: &gpui_kit::App) -> Option<Entity<QueryTab>> {
        self.front_tab()
            .filter(|t| {
                let t = t.read(cx);
                matches!(t.kind, Kind::File(_)) && t.connection.as_ref().is_none_or(|c| c.id != id)
            })
            .cloned()
    }

    /// Asks Commit / Roll back / Cancel when any of `tabs` holds an open transaction,
    /// ends it as answered, then runs `then`. Cancel, or a failed commit or rollback,
    /// stops there: the transaction stays as it is.
    ///
    /// Each check waits at most [`TX_CHECK_WAIT`]: a session busy with a statement, or
    /// a server that does not respond, never answers sooner. Such a tab that may hold
    /// a transaction is asked about (Continue / Cancel); one that had none goes on,
    /// and its Messages say so.
    pub(super) fn settle(
        &mut self,
        tabs: Vec<Entity<QueryTab>>,
        question: String,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Shell, &mut Window, &mut Context<Shell>) + 'static,
    ) {
        let handle = self.window;
        let checks: Vec<_> = tabs
            .iter()
            .map(|t| t.update(cx, |t, cx| t.check_tx(true, false, cx)))
            .collect();
        let titles: Vec<String> = tabs.iter().map(|t| self.tab_title(t.read(cx))).collect();
        cx.spawn(async move |this, cx| {
            let deadline = Instant::now() + TX_CHECK_WAIT;
            let (mut open, mut busy) = (Vec::new(), Vec::new());
            for ((tab, mut check), title) in tabs.into_iter().zip(checks).zip(titles) {
                let left = deadline.saturating_duration_since(Instant::now());
                let answer = (&mut check)
                    .with_timeout(left, cx.background_executor())
                    .await
                    .ok();
                if answer.is_none() {
                    // A late answer still updates the tab.
                    check.detach();
                }
                let maybe = tab.read_with(cx, |t, _| t.may_hold_tx());
                match tx_answer(answer.as_ref(), maybe) {
                    TxAnswer::Open => open.push((tab, title)),
                    TxAnswer::Busy => busy.push(title),
                    TxAnswer::Nothing if answer.is_none() => {
                        tab.update(cx, |t, cx| t.tx_unanswered(TX_CHECK_WAIT, cx));
                    }
                    TxAnswer::Nothing => {}
                }
            }
            if !busy.is_empty() {
                let (who, verb) = match busy.as_slice() {
                    [one] => (one.clone(), "does"),
                    _ => (busy.join(", "), "do"),
                };
                let detail = format!(
                    "{who} {verb} not answer (a statement is still running, or the server \
                     does not respond) and may hold an open transaction. Continuing leaves \
                     it as it is: not committed."
                );
                let Ok(answer) = cx.update_window(handle, |_, window, cx| {
                    crate::dialog::ask(
                        window,
                        cx,
                        crate::dialog::Ask::new(
                            "Continue without committing?",
                            &["Continue", "Cancel"],
                        )
                        .text(detail),
                    )
                }) else {
                    return;
                };
                if answer.await != Some(0) {
                    return;
                }
            }
            if !open.is_empty() {
                let names: Vec<&str> = open.iter().map(|(_, t)| t.as_str()).collect();
                let detail = if names.len() == 1 {
                    format!("{} has uncommitted changes.", names[0])
                } else {
                    format!("{} have uncommitted changes.", names.join(", "))
                };
                let Ok(answer) = cx.update_window(handle, |_, window, cx| {
                    crate::dialog::ask(
                        window,
                        cx,
                        crate::dialog::Ask::new(
                            question.clone(),
                            &["Commit", "Roll Back", "Cancel"],
                        )
                        .text(detail),
                    )
                }) else {
                    return;
                };
                let commit = match answer.await {
                    Some(0) => true,
                    Some(1) => false,
                    _ => return,
                };
                for (tab, _) in open {
                    let end = tab.update(cx, |t, cx| t.end_tx(commit, cx));
                    if end.await.is_err() {
                        return;
                    }
                }
            }
            let _ = cx.update_window(handle, |_, window, cx| {
                this.update(cx, |this, cx| then(this, window, cx))
            });
        })
        .detach();
    }

    /// Asks Submit / Discard / Cancel when any of `tabs` holds pending table changes,
    /// submits or discards them as answered, then runs `then`. Cancel, or a failed
    /// submit, stops there and the changes stay pending.
    pub(super) fn settle_pending(
        &mut self,
        tabs: Vec<Entity<QueryTab>>,
        question: String,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Shell, &mut Window, &mut Context<Shell>) + 'static,
    ) {
        let handle = self.window;
        let open: Vec<(Entity<QueryTab>, String, usize)> = tabs
            .into_iter()
            .filter_map(|t| {
                let n = t.read(cx).pending_changes(cx);
                (n > 0).then(|| {
                    let title = self.tab_title(t.read(cx));
                    (t, title, n)
                })
            })
            .collect();
        cx.spawn(async move |this, cx| {
            if !open.is_empty() {
                let detail = match open.as_slice() {
                    [(_, title, n)] => format!(
                        "{title} has {n} pending {} that {} not been submitted.",
                        if *n == 1 { "change" } else { "changes" },
                        if *n == 1 { "has" } else { "have" }
                    ),
                    _ => {
                        let names: Vec<&str> = open.iter().map(|(_, t, _)| t.as_str()).collect();
                        format!(
                            "{} have changes that have not been submitted.",
                            names.join(", ")
                        )
                    }
                };
                let Ok(answer) = cx.update_window(handle, |_, window, cx| {
                    crate::dialog::ask(
                        window,
                        cx,
                        crate::dialog::Ask::new(question.clone(), &["Submit", "Discard", "Cancel"])
                            .text(detail),
                    )
                }) else {
                    return;
                };
                match answer.await {
                    Some(0) => {
                        for (tab, _, _) in &open {
                            let Ok(task) = cx.update_window(handle, |_, window, cx| {
                                tab.update(cx, |t, cx| t.submit_table(window, cx))
                            }) else {
                                return;
                            };
                            if task.await.is_err() {
                                return;
                            }
                        }
                    }
                    Some(1) => {
                        for (tab, _, _) in &open {
                            tab.update(cx, |t, cx| t.discard_now(cx));
                        }
                    }
                    _ => return,
                }
            }
            let _ = cx.update_window(handle, |_, window, cx| {
                this.update(cx, |this, cx| then(this, window, cx))
            });
        })
        .detach();
    }

    /// Binds the front file tab to the connection chosen in the switcher.
    pub(super) fn rebind_front(&mut self, id: &str, cx: &mut Context<Self>) {
        let binding = self.binding(Some(id));
        if let Some(tab) = self.front_tab().cloned()
            && matches!(tab.read(cx).kind, Kind::File(_))
        {
            tab.update(cx, |t, cx| t.set_connection(binding, cx));
            self.schedule_save(cx);
        }
    }

    pub(super) fn run_front(&mut self, scope: Scope, cx: &mut Context<Self>) {
        if let Some(tab) = self.front_tab().cloned() {
            tab.update(cx, |t, cx| t.run(scope, cx));
        }
    }

    /// Commits (`true`) or rolls back the front tab's transaction.
    pub(super) fn end_front_tx(&mut self, commit: bool, cx: &mut Context<Self>) {
        if let Some(tab) = self.front_tab().cloned()
            && tab.read(cx).tx_open
            && !tab.read(cx).is_running()
        {
            tab.update(cx, |t, cx| t.end_tx(commit, cx)).detach();
        }
    }

    /// Switches the front tab's transaction mode. Going to Auto with an open
    /// transaction asks to commit or roll back first.
    pub(super) fn set_front_tx_mode(&mut self, mode: TxMode, cx: &mut Context<Self>) {
        self.tx_menu_open = false;
        cx.notify();
        let Some(tab) = self.front_tab().cloned() else {
            return;
        };
        if mode == TxMode::Auto && tab.read(cx).tx_mode == TxMode::Manual {
            let question = "Commit or roll back before switching to Auto?".into();
            self.settle(vec![tab.clone()], question, cx, move |_, _, cx| {
                tab.update(cx, |t, cx| t.set_tx_mode(mode, cx));
            });
        } else {
            tab.update(cx, |t, cx| t.set_tx_mode(mode, cx));
        }
    }

    /// Explains the front tab's statement; `analyze` measures it.
    pub(super) fn explain_front(&mut self, analyze: bool, cx: &mut Context<Self>) {
        if self.front_can_explain(cx)
            && let Some(tab) = self.front_tab().cloned()
        {
            tab.update(cx, |t, cx| t.explain(analyze, cx));
        }
    }

    /// Whether the front tab's engine explains statements: from the tab's own session,
    /// or else the connection's. Before either is open it may: like Run, Explain then
    /// connects first, and an engine without plans says so in Messages.
    pub(super) fn front_can_explain(&self, cx: &App) -> bool {
        let Some(t) = self.front_tab().map(|t| t.read(cx)) else {
            return false;
        };
        if t.table.is_some() {
            return false;
        }
        t.can_explain().unwrap_or_else(|| {
            t.connection
                .as_ref()
                .and_then(|c| match self.conns.get(&c.id) {
                    Some(Conn::Connected(d)) => Some(d.capabilities().explain),
                    _ => None,
                })
                .unwrap_or(true)
        })
    }

    pub(super) fn stop_front(&mut self, cx: &mut Context<Self>) {
        if let Some(tab) = self.front_tab().cloned() {
            tab.update(cx, |t, cx| t.stop(cx));
        }
    }

    pub(super) fn format_front(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.front_tab().cloned() {
            tab.update(cx, |t, cx| t.format(window, cx));
        }
    }

    /// `recent.sql`, `Untitled-2`, `local console`, `orders`, `New table`.
    pub(super) fn tab_title(&self, t: &QueryTab) -> String {
        match &t.kind {
            Kind::Table(r) if r.table.is_empty() => "New table".into(),
            Kind::Table(r) => r.table.clone(),
            Kind::File(Some(p)) => deadlock_core::project::folder_name(p),
            Kind::File(None) => format!("Untitled-{}", t.untitled),
            Kind::Console(_) => {
                let name = t.connection.as_ref().map_or("?", |c| c.name.as_str());
                format!("{name} console")
            }
        }
    }

    /// An editor tab as `state.json` keeps it; `None` for a New table draft.
    fn tab_state(t: &QueryTab, cx: &gpui_kit::App) -> Option<TabState> {
        let connection = t.connection.as_ref().map(|c| c.id.clone());
        let schema = t.picked_schema().map(str::to_owned);
        Some(match &t.kind {
            Kind::Table(r) if r.table.is_empty() => return None,
            Kind::File(path) => TabState::File {
                path: path.clone(),
                text: path.is_none().then(|| t.text(cx)),
                connection,
                schema,
            },
            Kind::Console(_) => TabState::Console {
                connection: connection.unwrap_or_default(),
                schema,
            },
            Kind::Table(r) => TabState::Table {
                connection: connection.unwrap_or_default(),
                database: r.database.clone(),
                table: r.table.clone(),
            },
        })
    }

    /// The tab list as `state.json` keeps it. New table drafts are left out.
    pub(super) fn sync_tabs(&mut self, cx: &gpui_kit::App) {
        let draft =
            |t: &Entity<QueryTab>| matches!(&t.read(cx).kind, Kind::Table(r) if r.table.is_empty());
        self.project.state.tabs = self
            .tabs
            .iter()
            .filter_map(|(t, _)| Self::tab_state(t.read(cx), cx))
            .collect();
        let objects = self
            .objects
            .iter()
            .map(|(t, _)| t.read(cx))
            .filter(|t| !t.new)
            .map(|t| (t.connection.id.clone(), t.object.clone()))
            .chain(self.restored_objects.iter().cloned())
            .map(|(connection, o)| TabState::Object {
                connection,
                schema: o.schema,
                name: o.name,
                object: o.kind.label().to_owned(),
            });
        self.project.state.tabs.extend(objects);
        self.project.state.active_tab = match self.front {
            Front::Query(i) => Some(self.tabs.iter().take(i).filter(|(t, _)| !draft(t)).count()),
            _ => None,
        };
    }

    /// Writes every console buffer now. For closing the window.
    pub(super) fn flush_consoles(&self, cx: &gpui_kit::App) {
        for (t, _) in &self.tabs {
            let t = t.read(cx);
            if let (Kind::Console(path), true) = (&t.kind, t.dirty)
                && let Err(e) = save_text(path, &t.text(cx))
            {
                eprintln!("deadlock: console: {e}");
            }
        }
    }

    /// Table tabs whose New table draft or open Structure form holds typing not run yet; asked
    /// about when the window closes.
    pub(super) fn unapplied_tables(&self, cx: &gpui_kit::App) -> Vec<String> {
        self.tabs
            .iter()
            .map(|(t, _)| t.read(cx))
            .filter(|t| t.draft_typed(cx) || t.form_unapplied(cx).is_some())
            .map(|t| self.tab_title(t))
            .collect()
    }

    /// Files with unsaved changes; asked about when the window closes.
    pub(super) fn dirty_files(&self, cx: &gpui_kit::App) -> Vec<Entity<QueryTab>> {
        self.tabs
            .iter()
            .filter(|(t, _)| {
                let t = t.read(cx);
                t.dirty && matches!(t.kind, Kind::File(Some(_)))
            })
            .map(|(t, _)| t.clone())
            .collect()
    }

    /// Saves every file tab with a path. Returns whether all writes worked.
    pub(super) fn save_files(&self, tabs: &[Entity<QueryTab>], cx: &mut Context<Self>) -> bool {
        tabs.iter().all(|tab| match tab.read(cx).kind.clone() {
            Kind::File(Some(path)) => write(tab, &path, cx),
            _ => true,
        })
    }
}

/// Writes the tab's text to `path` and clears its dirty mark.
fn write(tab: &Entity<QueryTab>, path: &Path, cx: &mut gpui_kit::App) -> bool {
    match save_text(path, &tab.read(cx).text(cx)) {
        Ok(()) => {
            tab.update(cx, |t, cx| {
                t.dirty = false;
                cx.emit(TabEvent::Changed);
            });
            true
        }
        Err(e) => {
            eprintln!("deadlock: {e}");
            false
        }
    }
}

/// The tab strip: editor tabs, then the New connection tab.
/// A connection's tint on its tab: a tinted background (stronger in front), a 2 px
/// accent along the top and the front tab's blue underline; a tab behind keeps its
/// bottom border in its own background.
pub(super) fn tinted(
    tab: gpui_kit::Stateful<gpui_kit::Div>,
    color: Option<gpui_kit::Hsla>,
    on: bool,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let Some(c) = color else { return tab };
    let bg = crate::color::over(c, if on { 0.32 } else { 0.14 }, p.bg);
    tab.relative()
        .bg(bg)
        .border_color(if on { p.blue } else { bg })
        .child(div().absolute().top_0().left_0().right_0().h(px(2.)).bg(c))
}

pub(super) fn tab_strip(shell: &Shell, cx: &mut Context<Shell>) -> impl IntoElement {
    let p = *cx.global::<Palette>();
    let tab = |id: SharedString, on: bool| {
        // `.tab`: 36 px, `--tx3` until hovered or in front, 2 px blue underline in front.
        div()
            .id(id.clone())
            .group(id)
            .flex()
            .items_center()
            .gap(px(6.))
            .h(tab_height())
            .pl(px(12.))
            .pr(px(6.))
            .border_b_2()
            .border_color(if on {
                p.blue
            } else {
                gpui_kit::transparent_black()
            })
            .text_color(if on { p.tx } else { p.tx3 })
            .when(on, |d| d.font_weight(FontWeight::MEDIUM))
            .hover(|s| s.text_color(p.tx))
            .whitespace_nowrap()
            .flex_none()
    };
    let editors = shell.tabs.iter().enumerate().map(|(i, (t, _))| {
        let on = shell.front == Front::Query(i);
        let t = t.read(cx);
        let console = matches!(t.kind, Kind::Console(_));
        let table = matches!(t.kind, Kind::Table(_));
        let key = t.is_key();
        let title = shell.tab_title(t);
        let draft = matches!(&t.kind, Kind::Table(r) if r.table.is_empty());
        let tip = match (&t.kind, t.connection.as_ref()) {
            (Kind::Table(r), c) if draft => format!(
                "New table in {}{}",
                r.database,
                c.map_or(String::new(), |c| format!(" · {}", c.name))
            ),
            (Kind::Table(r), c) => format!(
                "{}.{}{}",
                r.database,
                r.table,
                c.map_or(String::new(), |c| format!(" · {}", c.name))
            ),
            (Kind::File(Some(path)), c) => format!(
                "{}{}",
                path.display(),
                c.map_or(String::new(), |c| format!(" · {}", c.name))
            ),
            (_, Some(c)) => format!("Runs on {}", c.name),
            (_, None) => "No connection".into(),
        };
        let group: SharedString = format!("editor-tab-{i}").into();
        let tint = t.connection.as_ref().and_then(|c| shell.color_of(&c.id));
        let element = tinted(tab(group.clone(), on), tint, on, &p)
            .tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
            })
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(move |this, _: &MouseUpEvent, window, cx| {
                    this.close_tab(i, window, cx);
                }),
            )
            .clickable(
                cx.listener(move |this, _, window, cx| this.show_tab(Front::Query(i), window, cx)),
            )
            .when(console, |d| d.child(icon(Icon::Console, p.tx3)))
            .when(table && !key, |d| d.child(icon(Icon::Table, p.tx3)))
            // A Redis key's name, in mono as in the design.
            .when(key, |d| {
                d.font_family(crate::theme::mono_font())
                    .text_size(crate::theme::data_size())
            })
            .child(title)
            .children(
                t.connection
                    .as_ref()
                    .and_then(|c| super::env_marker(c.environment, &p)),
            )
            // An unsaved file, or a draft not created yet.
            .when(t.dirty && !console && !table || draft, |d| {
                d.child(div().size(px(6.)).rounded_full().bg(p.tx3))
            })
            // A table's pending changes and an open transaction share the warn dot.
            .when(t.tx_open || table && t.dirty, |d| {
                d.child(div().size(px(6.)).rounded_full().bg(p.warn))
            })
            .child(
                div()
                    .id(("close-tab", i))
                    .size(px(18.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .opacity(if on { 1. } else { 0. })
                    .group_hover(group, |s| s.opacity(1.))
                    .hover(|s| s.bg(p.hover))
                    .child(icon(Icon::Close, p.tx3).size(px(12.)))
                    .clickable(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.close_tab(i, window, cx);
                    })),
            );
        // A table tab's menu: Import CSV into the table, then the table actions.
        let import = match (&t.kind, &t.connection) {
            (Kind::Table(r), Some(c)) if !key && !draft => {
                Some((c.id.clone(), r.clone(), c.read_only))
            }
            _ => None,
        };
        match import {
            Some((id, r, read_only)) => {
                let shell = cx.entity();
                element
                    .context_menu(move |menu, _, cx| {
                        let (s, id2, r2) = (shell.clone(), id.clone(), r.clone());
                        let menu = menu
                            .item(
                                crate::assets::menu_item(
                                    "Import CSV…",
                                    crate::assets::Icon::Import,
                                )
                                .on_click(move |_, window, cx| {
                                    s.update(cx, |s, cx| {
                                        s.open_import(&id2, Some(r2.clone()), window, cx);
                                    });
                                }),
                            )
                            .separator();
                        super::objects::table_actions(menu, &shell, &id, &r, read_only, cx)
                    })
                    .into_any_element()
            }
            None => element.into_any_element(),
        }
    });
    let screens =
        shell.sessions.iter().enumerate().map(|(i, (t, _))| {
            let on = shell.front == Front::Sessions(i);
            let c = &t.read(cx).connection;
            let redis = c.engine == deadlock_core::connection::Engine::Redis;
            let tip = format!("Sessions and locks · {}", c.name);
            let env = c.environment;
            let group: SharedString = format!("sessions-tab-{i}").into();
            let tint = shell.color_of(&c.id);
            tinted(tab(group.clone(), on), tint, on, &p)
                .tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                })
                .on_mouse_up(
                    MouseButton::Middle,
                    cx.listener(move |this, _: &MouseUpEvent, _, cx| {
                        this.close_sessions(i, cx);
                    }),
                )
                .clickable(cx.listener(move |this, _, window, cx| {
                    this.show_tab(Front::Sessions(i), window, cx)
                }))
                .child(icon(Icon::Sessions, if on { p.tx } else { p.tx3 }))
                .child(if redis {
                    "Clients and slow log"
                } else {
                    "Sessions and locks"
                })
                .children(super::env_marker(env, &p))
                .child(
                    div()
                        .id(("close-sessions", i))
                        .size(px(18.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.))
                        .opacity(if on { 1. } else { 0. })
                        .group_hover(group, |s| s.opacity(1.))
                        .hover(|s| s.bg(p.hover))
                        .child(icon(Icon::Close, p.tx3).size(px(12.)))
                        .clickable(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_sessions(i, cx);
                        })),
                )
        });
    let configs = shell.configs.iter().enumerate().map(|(i, (t, _))| {
        let on = shell.front == Front::Config(i);
        let t = t.read(cx);
        let c = &t.connection;
        let tip = format!("Server configuration · {}", c.name);
        let env = c.environment;
        let pending = t.pending_count() > 0;
        let group: SharedString = format!("config-tab-{i}").into();
        let tint = shell.color_of(&c.id);
        tinted(tab(group.clone(), on), tint, on, &p)
            .tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
            })
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(move |this, _: &MouseUpEvent, window, cx| {
                    this.close_config(i, window, cx);
                }),
            )
            .clickable(
                cx.listener(move |this, _, window, cx| this.show_tab(Front::Config(i), window, cx)),
            )
            .child(icon(Icon::Config, if on { p.tx } else { p.tx3 }))
            .child("Server configuration")
            .children(super::env_marker(env, &p))
            .when(pending, |d| {
                d.child(div().size(px(6.)).rounded_full().bg(p.warn))
            })
            .child(
                div()
                    .id(("close-config", i))
                    .size(px(18.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .opacity(if on { 1. } else { 0. })
                    .group_hover(group, |s| s.opacity(1.))
                    .hover(|s| s.bg(p.hover))
                    .child(icon(Icon::Close, p.tx3).size(px(12.)))
                    .clickable(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.close_config(i, window, cx);
                    })),
            )
    });
    let import = shell.import.as_ref().map(|(t, _)| {
        let on = shell.front == Front::Import;
        tab("import-tab".into(), on)
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(move |this, _: &MouseUpEvent, _, cx| {
                    this.close_import(cx);
                }),
            )
            .clickable(cx.listener(|this, _, window, cx| this.show_tab(Front::Import, window, cx)))
            .child(icon(Icon::Import, if on { p.tx } else { p.tx3 }))
            .child(t.read(cx).title())
            .child(
                div()
                    .id("close-import")
                    .size(px(18.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .opacity(if on { 1. } else { 0. })
                    .group_hover("import-tab", |s| s.opacity(1.))
                    .hover(|s| s.bg(p.hover))
                    .child(icon(Icon::Close, p.tx3).size(px(12.)))
                    .clickable(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.close_import(cx);
                    })),
            )
    });
    let settings = shell.settings_tab.as_ref().map(|_| {
        let on = shell.front == Front::Settings;
        tab("settings-tab".into(), on)
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(move |this, _: &MouseUpEvent, _, cx| {
                    this.close_settings(cx);
                }),
            )
            .clickable(
                cx.listener(|this, _, window, cx| this.show_tab(Front::Settings, window, cx)),
            )
            .child(icon(Icon::Config, if on { p.tx } else { p.tx3 }))
            .child("Settings")
            .child(
                div()
                    .id("close-settings")
                    .size(px(18.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .opacity(if on { 1. } else { 0. })
                    .group_hover("settings-tab", |s| s.opacity(1.))
                    .hover(|s| s.bg(p.hover))
                    .child(icon(Icon::Close, p.tx3).size(px(12.)))
                    .clickable(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.close_settings(cx);
                    })),
            )
    });
    let form = shell.form.as_ref().map(|(f, _)| {
        let editing = f.read(cx).editing().map(|c| c.id.clone());
        let label: SharedString = match editing.as_deref().and_then(|id| shell.config(id)) {
            Some(c) => format!("Edit {}", c.name).into(),
            None => "New connection".into(),
        };
        let on = shell.front == Front::Form;
        tab("form-tab".into(), on)
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.close_form(cx)),
            )
            .clickable(cx.listener(|this, _, window, cx| this.show_tab(Front::Form, window, cx)))
            .when(editing.is_none(), |d| d.child(icon(Icon::Plus, p.tx)))
            .child(label)
            .child(
                div()
                    .id("close-form")
                    .size(px(18.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .opacity(if on { 1. } else { 0. })
                    .group_hover("form-tab", |s| s.opacity(1.))
                    .hover(|s| s.bg(p.hover))
                    .child(icon(Icon::Close, p.tx3).size(px(12.)))
                    .clickable(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.close_form(cx);
                    })),
            )
    });
    div()
        .id("tab-strip")
        .flex()
        .flex_none()
        .h(tab_height() + px(1.))
        .px(px(4.))
        .border_b_1()
        .border_color(p.line)
        .overflow_x_scroll()
        .children(editors)
        .children(screens)
        .children(configs)
        .children(super::diagram::diagram_tabs(shell, cx))
        .children(super::objects::object_tabs(shell, cx))
        .children(import)
        .children(settings)
        .children(form)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unanswered_checks_ask_only_when_a_transaction_may_be_open() {
        assert_eq!(tx_answer(Some(&Ok(true)), false), TxAnswer::Open);
        assert_eq!(tx_answer(Some(&Ok(false)), true), TxAnswer::Nothing);
        // A lost connection has nothing left to commit; the tab says so.
        assert_eq!(
            tx_answer(Some(&Err("gone".into())), true),
            TxAnswer::Nothing
        );
        assert_eq!(tx_answer(None, true), TxAnswer::Busy);
        assert_eq!(tx_answer(None, false), TxAnswer::Nothing);
    }
}
