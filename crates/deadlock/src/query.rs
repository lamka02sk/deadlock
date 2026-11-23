//! An editor tab: a `.sql` file, an untitled buffer or a connection's console. Each tab
//! runs statements on its own server session, so a long query never blocks the schema
//! tree, and shows their results in a resizable panel under the editor.

use std::cell::Cell;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use deadlock_core::classify::{Kind as StatementKind, Statement};
use deadlock_core::connection::{ConnectionConfig, Engine, Environment};
use deadlock_core::driver::{Driver, ResultStream, Runtime, connect, page_size};
use deadlock_core::history::{self, Outcome};
use deadlock_core::split;
use gpui_kit::base::input::BlockId;
use gpui_kit::component::input::{
    Editor, EditorState, GutterDecorationCollection, InputEvent, RangeDecoration,
    RangeDecorationCollection, RangeDecorationStyle, RopeExt as _,
};
use gpui_kit::component::resizable::{resizable_panel, v_resizable};
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, Bounds, ClipboardItem, Context, Entity,
    EventEmitter, FontWeight, InteractiveElement, IntoElement, ParentElement, Pixels, Render,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity, Window,
    actions, canvas, div, px,
};

mod binds;
mod exporting;
mod inline;
mod key;
mod plan;
mod structure;
pub(crate) mod table;

use inline::{BlockEvent, Body, ResultBlock};

pub use table::{
    AddRow, DeleteRows, EditCell, NextCell, PrevCell, SetDefault, SetNull, UndoChange, View,
};

use crate::app::{Db, HistoryDb};
use crate::assets::{Icon, icon};
use crate::results::{Grid, NextSet, duration, grid_text};
use crate::theme::{MONO_FONT, Palette, editor_size, small_size, tab_height};

actions!(query, [CopySelection]);

/// How the tab's session commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxMode {
    /// Each statement commits.
    Auto,
    /// The session holds a transaction until Commit or Roll back.
    Manual,
}

impl TxMode {
    /// Manual for production connections; otherwise the "New connections start in"
    /// setting.
    pub fn default_for(config: &ConnectionConfig, cx: &App) -> Self {
        let manual = crate::settings::get(cx).tx_mode == deadlock_core::settings::TxDefault::Manual;
        if config.environment == Environment::Production || manual {
            Self::Manual
        } else {
            Self::Auto
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Manual => "Manual",
        }
    }
}

/// What a tab is backed by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A file on disk, or an untitled buffer (`None`) until it is saved.
    File(Option<PathBuf>),
    /// The connection's console, auto-saved to this file.
    Console(PathBuf),
    /// A table's or view's data.
    Table(deadlock_core::edit::TableRef),
}

/// What to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The selection, or the statement under the caret.
    Caret,
    /// Every statement, stopping at the first error.
    All,
    /// The statement starting at this byte offset (its gutter icon).
    At(usize),
}

pub enum TabEvent {
    /// The tab needs a session for its connection: answer with [`QueryTab::connect`].
    NeedSession,
    /// Text, caret, dirty mark or run state changed: the toolbar and status bar redraw.
    Changed,
    /// A reference link: open that table of the same connection, on its Structure view.
    OpenTable(deadlock_core::edit::TableRef),
    /// An ALTER changed a table in this database: the sidebar reloads it.
    SchemaChanged(String),
    /// The user chose where results go: inline (`true`) or the bottom panel. The
    /// project remembers it and every tab follows.
    ResultsInline(bool),
    /// A statement was added to the project's history.
    Recorded,
    /// A Redis key tab deleted or renamed keys in this database (`db<n>`): the
    /// sidebar drops the old names and adds the new one.
    KeysChanged(
        String,
        Vec<Vec<u8>>,
        Option<(Vec<u8>, deadlock_core::redis::KeyType)>,
    ),
}

enum Session {
    None,
    Connecting,
    Ready(Arc<dyn Driver>),
}

/// One line of the Messages tab.
struct Message {
    at: String,
    statement: String,
    text: String,
    error: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Panel {
    Result(usize),
    Messages,
    Plan,
}

/// A result drawn in the editor under its statement.
struct Inline {
    id: BlockId,
    view: Entity<ResultBlock>,
    _subs: [Subscription; 2],
}

/// Work waiting for the tab's session.
enum Pending {
    Run(Vec<(Range<usize>, String)>),
    /// A statement to explain, and whether to measure it.
    Explain(Range<usize>, String, bool),
}

/// Delay between the last keystroke and the gutter refresh or the console save.
const QUIET: Duration = Duration::from_millis(400);

/// Buffers up to this many bytes re-split on every keystroke.
const SPLIT_NOW_LIMIT: usize = 256 * 1024;

pub struct QueryTab {
    pub kind: Kind,
    /// `Untitled-<n>` for an untitled buffer.
    pub untitled: usize,
    /// The connection statements run on.
    pub connection: Option<ConnectionConfig>,
    pub tx_mode: TxMode,
    /// The session holds an open transaction, as the server last reported it.
    pub tx_open: bool,
    pub editor: Entity<EditorState>,
    pub dirty: bool,
    /// The statements as the gutter shows them: byte range and first row.
    statements: Vec<(Range<usize>, usize)>,
    session: Session,
    /// A run or explain waiting for its session.
    pending: Option<Pending>,
    running: Option<bool>,
    results: Vec<Entity<TableState<Grid>>>,
    messages: Vec<Message>,
    panel: Panel,
    took: Option<Duration>,
    running_mark: RangeDecorationCollection,
    _gutter: GutterDecorationCollection,
    /// The editor area in window coordinates, from the last paint.
    area: Rc<Cell<Bounds<Pixels>>>,
    window: AnyWindowHandle,
    quiet: Option<Task<()>>,
    grid_subs: Vec<Subscription>,
    _editor_subs: [Subscription; 2],
    /// A table tab's data view; `None` for editor tabs.
    pub table: Option<table::TableData>,
    /// A table tab's cell editor, while open.
    cell: Option<table::Editor>,
    /// The last plan, for the Plan tab.
    explained: Option<plan::PlanView>,
    /// Results go under their statements in the editor; otherwise to the bottom panel.
    pub inline: bool,
    /// The inline results, in no particular order.
    blocks: Vec<Inline>,
    /// The block of the statement running now, and whether a grid went into it.
    running_block: Option<(Entity<ResultBlock>, bool)>,
    /// The last statement's block: result sets that arrive later go under it.
    last_block: Option<Entity<ResultBlock>>,
    /// The run already scrolled to an error.
    error_shown: bool,
    /// Inline mode keeps the bottom panel down to its tabs until it has something
    /// to show or the user opens it.
    panel_open: bool,
    /// The bind-variable dialog, while open.
    binds: Option<binds::Prompt>,
    /// The values last given for each bind name, offered again.
    bind_values: std::collections::HashMap<String, String>,
    /// The project folder: where exports are saved by default.
    pub root: PathBuf,
    /// The running or last export.
    export: Option<crate::export::Job>,
    /// The export menu's rows choice: all rows (`true`) or the loaded ones.
    pub export_all: bool,
    /// The typed save path, when no save dialog is available.
    path_field: Option<Entity<crate::export::PathField>>,
}

impl EventEmitter<TabEvent> for QueryTab {}

impl QueryTab {
    pub fn new(
        kind: Kind,
        connection: Option<ConnectionConfig>,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Redis commands are not SQL: no SQL colors on them.
        let redis = connection
            .as_ref()
            .is_some_and(|c| c.engine == Engine::Redis);
        let settings = crate::settings::get(cx).clone();
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language(if redis { "text" } else { "sql" })
                .line_number(true)
                .soft_wrap(settings.wrap)
                .tab_size(tab_size(settings.indent))
                .folding(false)
                .gutter_decorations_width(px(18.))
                .default_value(text)
        });
        let (running_mark, gutter) = editor.update(cx, |e, cx| {
            (
                e.create_range_decorations_collection(Vec::new(), cx),
                // Reserves the strip the run icons are drawn over.
                e.create_gutter_decorations_collection(Vec::new(), cx),
            )
        });
        let sub = cx.subscribe(&editor, |this, _, e: &InputEvent, cx| {
            if matches!(e, InputEvent::Change) {
                this.dirty = true;
                // Large buffers re-split after a quiet moment; the rest keep the
                // gutter in step with every keystroke.
                if this.editor.read(cx).text().len() < SPLIT_NOW_LIMIT {
                    this.split_now(cx);
                }
                this.schedule_quiet(cx);
            }
            cx.emit(TabEvent::Changed);
        });
        // The gutter icons follow the editor's scrolling and layout.
        let observe = cx.observe(&editor, |_, _, cx| cx.notify());
        let mut this = Self {
            kind,
            untitled: 0,
            tx_mode: connection
                .as_ref()
                .map_or(TxMode::Auto, |c| TxMode::default_for(c, cx)),
            tx_open: false,
            connection,
            editor,
            dirty: false,
            statements: Vec::new(),
            session: Session::None,
            pending: None,
            running: None,
            results: Vec::new(),
            messages: Vec::new(),
            panel: Panel::Result(0),
            took: None,
            running_mark,
            _gutter: gutter,
            area: Rc::default(),
            window: window.window_handle(),
            quiet: None,
            grid_subs: Vec::new(),
            _editor_subs: [sub, observe],
            table: None,
            cell: None,
            explained: None,
            inline: true,
            blocks: Vec::new(),
            running_block: None,
            last_block: None,
            error_shown: false,
            panel_open: false,
            binds: None,
            bind_values: std::collections::HashMap::new(),
            root: PathBuf::new(),
            export: None,
            export_all: true,
            path_field: None,
        };
        if let Kind::Table(t) = &this.kind {
            let mut data = table::data(t.clone(), window, cx);
            if redis {
                data.redis = Some(key::KeyState::new(&t.table));
                let hint = "field or member pattern, for example last_*";
                data.filter
                    .update(cx, |f, cx| f.set_placeholder(hint, window, cx));
            }
            this.table = Some(data);
        }
        this.split_now(cx);
        this
    }

    pub fn text(&self, cx: &App) -> String {
        self.editor.read(cx).value().to_string()
    }

    /// A statement or an export runs on the tab's session.
    pub fn is_running(&self) -> bool {
        self.running.is_some() || self.exporting()
    }

    pub fn needs_session(&self) -> bool {
        (self.pending.is_some() || self.table.as_ref().is_some_and(|t| t.waiting))
            && matches!(self.session, Session::None)
    }

    /// 1-based line and column of the caret.
    pub fn position(&self, cx: &App) -> (u32, u32) {
        let p = self.editor.read(cx).cursor_position();
        (p.line + 1, p.character + 1)
    }

    /// Binds the tab to another connection. Its session closes; the next run opens one.
    /// The caller has settled an open transaction first.
    pub fn set_connection(&mut self, connection: Option<ConnectionConfig>, cx: &mut Context<Self>) {
        if self.connection.as_ref().map(|c| &c.id) == connection.as_ref().map(|c| &c.id) {
            return;
        }
        self.tx_mode = connection
            .as_ref()
            .map_or(TxMode::Auto, |c| TxMode::default_for(c, cx));
        self.tx_open = false;
        self.connection = connection;
        self.close_session(cx);
        self.split_now(cx);
        cx.notify();
    }

    /// Drops the session on the core runtime, where drivers close their sockets.
    pub fn close_session(&mut self, cx: &mut Context<Self>) {
        self.results.clear();
        self.grid_subs.clear();
        self.clear_blocks(cx);
        if let Session::Ready(d) = std::mem::replace(&mut self.session, Session::None) {
            drop(cx.global::<Db>().0.spawn(async move { drop(d) }));
        }
    }

    /// Text edited: refresh the gutter and save a console after a quiet moment.
    fn schedule_quiet(&mut self, cx: &mut Context<Self>) {
        self.quiet = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(QUIET).await;
            let _ = this.update(cx, |this, cx| {
                this.split_now(cx);
                this.save_console(cx);
                cx.notify();
            });
        }));
    }

    fn split_now(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.connection.as_ref().map(|c| &c.engine) else {
            self.statements.clear();
            return;
        };
        let state = self.editor.read(cx);
        let text = state.value();
        let rope = state.text();
        self.statements = split::split(&text, *engine)
            .into_iter()
            .map(|r| {
                let row = rope.offset_to_point(r.start).row;
                (r, row)
            })
            .collect();
        self.refresh_blocks(&text, cx);
    }

    /// Writes the console buffer now. Consoles never ask to be saved.
    pub fn save_console(&mut self, cx: &mut Context<Self>) {
        let Kind::Console(path) = &self.kind else {
            return;
        };
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let (path, text) = (path.clone(), self.text(cx));
        drop(cx.global::<Db>().0.spawn_blocking(move || {
            if let Err(e) = deadlock_core::project::save_text(&path, &text) {
                eprintln!("deadlock: console: {e}");
            }
        }));
    }

    /// The statements a run covers, each with its text.
    fn plan(&self, scope: Scope, cx: &App) -> Vec<(Range<usize>, String)> {
        let Some(engine) = self.connection.as_ref().map(|c| &c.engine) else {
            return Vec::new();
        };
        let state = self.editor.read(cx);
        let text = state.value();
        let sel = state.selected_range();
        let ranges = match scope {
            Scope::Caret if !sel.is_empty() => split::split(&text[sel.clone()], *engine)
                .into_iter()
                .map(|r| r.start + sel.start..r.end + sel.start)
                .collect(),
            Scope::Caret => {
                let all = split::split(&text, *engine);
                split::at(&all, state.cursor())
                    .map(|i| vec![all[i].clone()])
                    .unwrap_or_default()
            }
            Scope::All => split::split(&text, *engine),
            Scope::At(start) => split::split(&text, *engine)
                .into_iter()
                .filter(|r| r.start == start)
                .collect(),
        };
        ranges
            .into_iter()
            .map(|r| {
                let t = text[r.clone()].to_owned();
                (r, t)
            })
            .collect()
    }

    pub fn run(&mut self, scope: Scope, cx: &mut Context<Self>) {
        if self.running.is_some() || self.connection.is_none() {
            return;
        }
        let plan = self.plan(scope, cx);
        if plan.is_empty() {
            return;
        }
        self.when_ready(Pending::Run(plan), cx);
    }

    /// Starts `work` now, or once the session is open.
    fn when_ready(&mut self, work: Pending, cx: &mut Context<Self>) {
        match &self.session {
            Session::Ready(_) => self.start_pending(work, cx),
            Session::Connecting => self.pending = Some(work),
            Session::None => {
                self.pending = Some(work);
                cx.emit(TabEvent::NeedSession);
            }
        }
    }

    fn start_pending(&mut self, work: Pending, cx: &mut Context<Self>) {
        match work {
            Pending::Run(plan) => self.start(plan, cx),
            Pending::Explain(range, text, analyze) => self.start_explain(range, text, analyze, cx),
        }
    }

    /// Whether the tab's session, once open, can explain statements.
    pub fn can_explain(&self) -> Option<bool> {
        match &self.session {
            Session::Ready(d) => Some(d.capabilities().explain),
            _ => None,
        }
    }

    /// Explains the selection, or the statement under the caret. `analyze` runs it and
    /// measures each step. A selection of several statements explains the first.
    pub fn explain(&mut self, analyze: bool, cx: &mut Context<Self>) {
        if self.running.is_some() || self.connection.is_none() || self.table.is_some() {
            return;
        }
        let Some((range, text)) = self.plan(Scope::Caret, cx).into_iter().next() else {
            return;
        };
        self.when_ready(Pending::Explain(range, text, analyze), cx);
    }

    /// Explains the statement of the plan in front again, estimated or measured.
    fn reexplain(&mut self, analyze: bool, cx: &mut Context<Self>) {
        let Some(v) = &self.explained else { return };
        if self.running.is_some() {
            return;
        }
        let work = Pending::Explain(v.range.clone(), v.text.clone(), analyze);
        self.when_ready(work, cx);
    }

    /// Asks the server for the plan. Measuring runs the statement, so anything that is
    /// not a read is confirmed first, and refused on a read-only connection.
    fn start_explain(
        &mut self,
        range: Range<usize>,
        text: String,
        analyze: bool,
        cx: &mut Context<Self>,
    ) {
        let Session::Ready(driver) = &self.session else {
            return;
        };
        let driver = Arc::clone(driver);
        let rt = Arc::clone(&cx.global::<Db>().0);
        // Frees the session; the grids keep the rows they have.
        self.close_grids(cx);
        self.running = Some(false);
        self.mark_running(Some(range.clone()), cx);
        cx.emit(TabEvent::Changed);
        cx.notify();
        let label = first_line(&text);
        cx.spawn(async move |this, cx| {
            let checked = if analyze {
                measure_ok(&this, cx, &rt, &driver, &text).await
            } else {
                Ok(true)
            };
            let refusal = match checked {
                Ok(true) => None,
                Ok(false) => Some(CANCELLED.to_owned()),
                Err(e) => Some(e),
            };
            let started = Instant::now();
            let outcome = match refusal {
                Some(r) => Err(r),
                None => {
                    let (d, t) = (Arc::clone(&driver), text.clone());
                    match rt.spawn(async move { d.explain(&t, analyze).await }).await {
                        Ok(r) => r.map_err(|e| e.to_string()),
                        Err(_) => Err("the explain task stopped".into()),
                    }
                }
            };
            let took = started.elapsed();
            let _ = this.update(cx, |t, cx| {
                t.running = None;
                t.took = Some(took);
                t.mark_running(None, cx);
                let failed = outcome.is_err();
                match outcome {
                    Ok(plan) => {
                        let what = if plan.measured {
                            "Measured"
                        } else {
                            "Estimated"
                        };
                        t.note(&label, format!("{what} plan in {}", duration(took)), false);
                        t.explained = Some(plan::PlanView::new(plan, range, text));
                        t.show_panel(Panel::Plan);
                    }
                    Err(e) => {
                        let cancelled = e == CANCELLED;
                        t.note(&label, e, !cancelled);
                        t.show_panel(Panel::Messages);
                    }
                }
                if analyze {
                    t.check_tx(false, failed, cx).detach();
                }
                cx.emit(TabEvent::Changed);
                cx.notify();
            });
        })
        .detach();
    }

    /// Opens the tab's own session, then runs what was waiting for it.
    pub fn connect(&mut self, config: ConnectionConfig, password: String, cx: &mut Context<Self>) {
        if !matches!(self.session, Session::None) {
            return;
        }
        self.session = Session::Connecting;
        // A Redis key tab's session selects the key's database.
        let config = match (&self.kind, config.engine) {
            (Kind::Table(t), Engine::Redis) => ConnectionConfig {
                database: t.database.trim_start_matches("db").to_owned(),
                ..config
            },
            _ => config,
        };
        let auto = self.tx_mode == TxMode::Auto;
        let rx = cx.global::<Db>().0.spawn(async move {
            let driver = connect(&config, &password).await?;
            if driver.capabilities().transactions {
                driver.set_auto_commit(auto).await?;
            }
            Ok::<_, deadlock_core::driver::DriverError>(driver)
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(driver)) => {
                        this.session = Session::Ready(driver);
                        if let Some(work) = this.pending.take() {
                            this.start_pending(work, cx);
                        }
                        this.table_session(None, cx);
                    }
                    Ok(Err(e)) => this.fail_session(e.to_string(), cx),
                    Err(_) => this.fail_session("the connection task stopped".into(), cx),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// The session could not be opened; the waiting run is dropped with a message.
    pub fn fail_session(&mut self, error: String, cx: &mut Context<Self>) {
        self.session = Session::None;
        self.table_session(Some(error.clone()), cx);
        self.pending = None;
        self.messages.clear();
        self.note("Connect", error, true);
        self.show_panel(Panel::Messages);
        cx.emit(TabEvent::Changed);
        cx.notify();
    }

    fn start(&mut self, plan: Vec<(Range<usize>, String)>, cx: &mut Context<Self>) {
        let Session::Ready(driver) = &self.session else {
            return;
        };
        let driver = Arc::clone(driver);
        let rt = Arc::clone(&cx.global::<Db>().0);
        if self.inline {
            // Earlier inline results stay, but stop paging: the session is needed.
            // So do results moved to the panel.
            self.close_grids(cx);
            self.error_shown = false;
            self.last_block = None;
        } else {
            // Dropping the old grids closes their streams and frees the session.
            self.results.clear();
            self.grid_subs.clear();
        }
        self.messages.clear();
        self.took = None;
        self.running = Some(false);
        cx.emit(TabEvent::Changed);
        cx.notify();
        let count = plan.len();
        cx.spawn(async move |this, cx| {
            let texts: Vec<String> = plan.iter().map(|(_, t)| t.clone()).collect();
            let refusal = match confirm(&this, cx, &rt, &driver, texts, None).await {
                Ok(true) => None,
                Ok(false) => Some(("Not run: cancelled".to_owned(), false)),
                Err(e) => Some((e, true)),
            };
            if let Some((text, error)) = refusal {
                let _ = this.update(cx, |t, cx| {
                    t.note("Run", text, error);
                    t.finish_run(Duration::ZERO, cx);
                });
                return;
            }
            let engine = this
                .read_with(cx, |t, _| t.connection.as_ref().map(|c| c.engine))
                .ok()
                .flatten();
            let names = engine.map_or_else(Vec::new, |e| {
                let mut names: Vec<String> = Vec::new();
                for (_, t) in &plan {
                    for n in split::bind_names(t, e) {
                        if !names.contains(&n) {
                            names.push(n);
                        }
                    }
                }
                names
            });
            let values = if names.is_empty() {
                Vec::new()
            } else {
                let (tx, rx) = tokio::sync::oneshot::channel();
                let window = this.read_with(cx, |t, _| t.window).ok();
                let opened = window.and_then(|w| {
                    cx.update_window(w, |_, window, cx| {
                        this.update(cx, |t, cx| t.ask_binds(names, tx, window, cx))
                    })
                    .ok()
                });
                match (opened, rx.await) {
                    (Some(Ok(())), Ok(Some(v))) => v,
                    _ => {
                        let _ = this.update(cx, |t, cx| {
                            t.note("Run", "Not run: cancelled".into(), false);
                            t.finish_run(Duration::ZERO, cx);
                        });
                        return;
                    }
                }
            };
            let started_all = Instant::now();
            let mut failed_any = false;
            for (i, (range, text)) in plan.into_iter().enumerate() {
                let label = first_line(&text);
                if this
                    .update(cx, |t, cx| {
                        t.begin_block(&range, &text, cx);
                        t.mark_running(Some(range), cx);
                    })
                    .is_err()
                {
                    return;
                }
                let started = Instant::now();
                let d = Arc::clone(&driver);
                let binds: binds::Values = engine
                    .map(|e| split::bind_names(&text, e))
                    .unwrap_or_default()
                    .into_iter()
                    .map(|n| {
                        let v = values
                            .iter()
                            .find(|(k, _)| *k == n)
                            .and_then(|(_, v)| v.clone());
                        (n, v)
                    })
                    .collect();
                // A read can run again for an export of all its rows.
                let rerun = engine
                    .filter(|e| *e != Engine::Redis)
                    .is_some_and(|e| {
                        deadlock_core::classify::classify(&text, e).kind == StatementKind::Read
                    })
                    .then(|| (text.clone(), binds.clone()));
                let statement = text.clone();
                let head = rt.spawn(async move { d.execute_bound(&text, &binds).await });
                let outcome = match head.await {
                    Ok(Ok(stream)) => {
                        follow(&this, cx, &rt, stream, started, i + 1 == count, rerun).await
                    }
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(_) => Err("the query task stopped".into()),
                };
                let failed = outcome.is_err();
                failed_any |= failed;
                let took = started.elapsed();
                let Ok(stop) = this.update(cx, |t, cx| {
                    t.record(statement, took, &outcome, cx);
                    t.finish_statement(label, outcome.map(|(parts, _)| parts), took, cx)
                }) else {
                    return;
                };
                if failed || stop {
                    break;
                }
            }
            let _ = this.update(cx, |t, cx| {
                t.finish_run(started_all.elapsed(), cx);
                t.check_tx(false, failed_any, cx).detach();
            });
        })
        .detach();
    }

    /// Adds a statement that ran to the project's history.
    fn record(
        &self,
        statement: String,
        took: Duration,
        outcome: &Result<(Vec<String>, Outcome), String>,
        cx: &mut Context<Self>,
    ) {
        if !crate::settings::get(cx).keep_history {
            return;
        }
        let (Some(db), Some(c)) = (cx.global::<HistoryDb>().0.clone(), &self.connection) else {
            return;
        };
        let outcome = match outcome {
            Ok((_, o)) => o.clone(),
            Err(e) => Outcome::Failed {
                code: history::error_code(e),
                message: e.clone(),
            },
        };
        let entry = history::Entry {
            id: 0,
            at: chrono::Utc::now().timestamp(),
            connection_id: c.id.clone(),
            connection_name: c.name.clone(),
            statement,
            duration_ms: u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
            outcome,
        };
        let (root, engine) = (self.root.clone(), c.engine);
        let rx = cx.global::<Db>().0.spawn_blocking(move || {
            let mut h = db.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            h.record(&root, engine, &entry)
        });
        cx.spawn(async move |this, cx| match rx.await {
            Ok(Ok(())) => {
                let _ = this.update(cx, |_, cx| cx.emit(TabEvent::Recorded));
            }
            Ok(Err(e)) => eprintln!("deadlock: history: {e}"),
            Err(_) => {}
        })
        .detach();
    }

    /// Adds a line to Messages.
    fn note(&mut self, statement: &str, text: String, error: bool) {
        self.messages.push(Message {
            at: now(),
            statement: statement.to_owned(),
            text,
            error,
        });
    }

    /// The session, when it is open and has transactions.
    fn tx_session(&self) -> Option<Arc<dyn Driver>> {
        match &self.session {
            Session::Ready(d) if d.capabilities().transactions => Some(Arc::clone(d)),
            _ => None,
        }
    }

    /// Stops the grids' streams, so the session is free for a transaction call.
    fn close_grids(&mut self, cx: &mut Context<Self>) {
        let inline: Vec<_> = self
            .blocks
            .iter()
            .filter_map(|b| b.view.read(cx).grid().cloned())
            .collect();
        for t in self.results.iter().chain(&inline) {
            t.update(cx, |t, cx| {
                drop(t.delegate_mut().close());
                t.refresh(cx);
            });
        }
    }

    /// Asks the server whether the session holds an open transaction and shows the
    /// answer. `free` first stops the grids' streams, so the answer comes now; without
    /// it, it comes once the last grid is read. A lost connection is reported, with
    /// what it did to an open transaction, and the session is dropped so the next run
    /// reconnects.
    pub fn check_tx(
        &mut self,
        free: bool,
        after_error: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<bool, String>> {
        let Some(d) = self.tx_session() else {
            return Task::ready(Ok(false));
        };
        if free {
            self.close_grids(cx);
        }
        let rx = cx
            .global::<Db>()
            .0
            .spawn(async move { d.transaction_open().await });
        cx.spawn(async move |this, cx| {
            let state = match rx.await {
                Ok(r) => r.map_err(|e| e.to_string()),
                Err(_) => Err("the transaction check stopped".into()),
            };
            this.update(cx, |t, cx| t.tx_checked(&state, after_error, cx))
                .map_err(|_| "the tab was closed".to_string())?;
            state
        })
    }

    fn tx_checked(
        &mut self,
        state: &Result<bool, String>,
        after_error: bool,
        cx: &mut Context<Self>,
    ) {
        let was_open = self.tx_open;
        match state {
            Ok(open) => {
                self.tx_open = *open;
                if was_open && !open && after_error {
                    self.note(
                        "Transaction",
                        "The server rolled back the open transaction: its changes are gone.".into(),
                        true,
                    );
                }
            }
            Err(e) => {
                self.tx_open = false;
                let text = if was_open {
                    format!(
                        "The connection was lost ({e}). The server rolled back the open \
                         transaction: its uncommitted changes are gone."
                    )
                } else {
                    format!("The connection was lost ({e}). The next run reconnects.")
                };
                self.note("Connection", text, true);
                self.show_panel(Panel::Messages);
                if let Session::Ready(d) = std::mem::replace(&mut self.session, Session::None) {
                    drop(cx.global::<Db>().0.spawn(async move { drop(d) }));
                }
            }
        }
        cx.emit(TabEvent::Changed);
        cx.notify();
    }

    /// Commits (`true`) or rolls back the session's transaction.
    pub fn end_tx(&mut self, commit: bool, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        let Some(d) = self.tx_session() else {
            return Task::ready(Ok(()));
        };
        self.close_grids(cx);
        let rx = cx.global::<Db>().0.spawn(async move {
            let ended = d.end_transaction(commit).await.map_err(|e| e.to_string());
            (ended, d.transaction_open().await.map_err(|e| e.to_string()))
        });
        cx.spawn(async move |this, cx| {
            let (ended, state) = rx
                .await
                .unwrap_or_else(|_| (Err("the transaction task stopped".into()), Ok(true)));
            this.update(cx, |t, cx| {
                let (label, done) = if commit {
                    ("Commit", "Committed")
                } else {
                    ("Roll back", "Rolled back")
                };
                match &ended {
                    Ok(()) => t.note(label, done.into(), false),
                    Err(e) => {
                        t.note(label, e.clone(), true);
                        t.show_panel(Panel::Messages);
                    }
                }
                t.tx_checked(&state, false, cx);
                t.table_tx_ended(cx);
            })
            .map_err(|_| "the tab was closed".to_string())?;
            ended
        })
    }

    /// Switches Auto / Manual. Going to Auto commits an open transaction on MySQL, so
    /// the caller settles it first.
    pub fn set_tx_mode(&mut self, mode: TxMode, cx: &mut Context<Self>) {
        self.tx_mode = mode;
        cx.emit(TabEvent::Changed);
        cx.notify();
        let Some(d) = self.tx_session() else { return };
        let rx = cx
            .global::<Db>()
            .0
            .spawn(async move { d.set_auto_commit(mode == TxMode::Auto).await });
        cx.spawn(async move |this, cx| {
            if let Ok(Err(e)) = rx.await {
                let _ = this.update(cx, |t, cx| {
                    t.note("Transaction mode", e.to_string(), true);
                    t.show_panel(Panel::Messages);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn mark_running(&mut self, range: Option<Range<usize>>, cx: &mut Context<Self>) {
        let p = *cx.global::<Palette>();
        let marks = range
            .map(|r| {
                RangeDecoration::new(r)
                    .with_style(RangeDecorationStyle::Fill)
                    .with_color(p.sel)
            })
            .into_iter()
            .collect();
        self.running_mark.set(marks, cx);
    }

    /// Records a statement in Messages. Returns whether the run was stopped.
    fn finish_statement(
        &mut self,
        statement: String,
        outcome: Result<Vec<String>, String>,
        took: Duration,
        cx: &mut Context<Self>,
    ) -> bool {
        let stopped = self.running == Some(true);
        let (text, error) = match outcome {
            Ok(parts) if parts.is_empty() => (format!("done in {}", duration(took)), false),
            Ok(parts) => (parts.join(", "), false),
            Err(e) => (e, true),
        };
        let text = if stopped && !error {
            format!("{text} (stopped)")
        } else {
            text
        };
        self.end_block(&text, error, cx);
        self.messages.push(Message {
            at: now(),
            statement,
            text,
            error,
        });
        cx.notify();
        stopped
    }

    fn finish_run(&mut self, took: Duration, cx: &mut Context<Self>) {
        self.running = None;
        self.took = Some(took);
        self.mark_running(None, cx);
        let failed = self.messages.iter().any(|m| m.error);
        self.panel = if self.results.is_empty() || failed {
            Panel::Messages
        } else {
            Panel::Result(0)
        };
        cx.emit(TabEvent::Changed);
        cx.notify();
    }

    /// Adds a result set's grid. `stream` is `Some` while more rows can arrive.
    fn add_grid(
        &mut self,
        grid: Grid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TableState<Grid>> {
        let table = cx.new(|cx| {
            TableState::new(grid, window, cx)
                .cell_selectable(true)
                .row_selectable(true)
                .row_header(false)
                .col_movable(false)
                .sortable(true)
        });
        self.grid_subs.push(
            cx.subscribe(&table, |_, table, e: &TableEvent, cx| match e {
                // The row number selects the whole row.
                TableEvent::SelectCell(r, 0) => {
                    let r = *r;
                    table.update(cx, |t, cx| t.set_selected_row(r, cx));
                }
                TableEvent::RightClickedCell(r, c) => {
                    let cell = Some((*r, *c));
                    table.update(cx, |t, _| t.delegate_mut().menu_cell = cell);
                }
                _ => {}
            }),
        );
        self.grid_subs.push(
            cx.subscribe(&table, |this, table, _: &crate::results::Ended, cx| {
                if let Some(next) = table.update(cx, |t, _| t.delegate_mut().close()) {
                    this.follow_later(next, cx);
                }
            }),
        );
        if !self.attach_grid(&table, cx) {
            self.results.push(table.clone());
        }
        cx.notify();
        table
    }

    /// Result sets that arrive after the grid in front was read to its end.
    fn follow_later(&mut self, next: NextSet, cx: &mut Context<Self>) {
        let rt = Arc::clone(&cx.global::<Db>().0);
        cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let outcome = match next.await {
                Ok(Ok(stream)) => follow(&this, cx, &rt, stream, started, true, None).await,
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => return,
            };
            let _ = this.update(cx, |t, cx| {
                t.finish_statement(
                    "(further result sets)".into(),
                    outcome.map(|(parts, _)| parts),
                    started.elapsed(),
                    cx,
                );
            });
        })
        .detach();
    }

    /// Cancels the running statement through the session's driver.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if self.exporting() {
            self.cancel_export(cx);
            return;
        }
        let (Some(stopped), Session::Ready(driver)) = (&mut self.running, &self.session) else {
            return;
        };
        *stopped = true;
        let driver = Arc::clone(driver);
        let rx = cx
            .global::<Db>()
            .0
            .spawn(async move { driver.cancel().await });
        cx.spawn(async move |this, cx| {
            if let Ok(Err(e)) = rx.await {
                let _ = this.update(cx, |t, cx| {
                    t.note("Stop", e.to_string(), true);
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    /// Formats the selection, or the whole buffer, with `sqlformat`.
    pub fn format(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use deadlock_core::settings::{Indent, KeywordCase};
        let s = crate::settings::get(cx);
        let options = sqlformat::FormatOptions {
            indent: match s.indent {
                Indent::Two => sqlformat::Indent::Spaces(2),
                Indent::Four => sqlformat::Indent::Spaces(4),
                Indent::Tab => sqlformat::Indent::Tabs,
            },
            uppercase: match s.keyword_case {
                KeywordCase::Lower => Some(false),
                KeywordCase::Upper => Some(true),
                KeywordCase::Keep => None,
            },
            ..sqlformat::FormatOptions::default()
        };
        let params = sqlformat::QueryParams::None;
        self.editor.update(cx, |e, cx| {
            let sel = e.selected_range();
            if sel.is_empty() {
                let text = sqlformat::format(&e.value(), &params, &options);
                e.replace_all(text, window, cx);
            } else {
                let text = sqlformat::format(&e.value()[sel], &params, &options);
                e.replace(text, window, cx);
            }
        });
    }

    /// A click on a panel tab: shows it, and in inline mode a click on the tab in
    /// front folds the panel down to its tabs.
    fn toggle_panel(&mut self, panel: Panel) {
        if self.inline && self.panel_open && self.panel == panel {
            self.panel_open = false;
        } else {
            self.show_panel(panel);
        }
    }

    /// Inline / Panel: where results go, at the right of the panel's header.
    fn place_switch(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let option = |id: &'static str, label: &'static str, tip: &'static str, inline: bool| {
            let on = self.inline == inline;
            crate::ui::button(id, crate::ui::Kind::Ghost, true, tip, cx)
                .h(px(24.))
                .px(px(8.))
                .when(on, |d| d.bg(p.hover).text_color(p.tx))
                .child(label)
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(TabEvent::ResultsInline(inline))))
        };
        div()
            .flex()
            .flex_none()
            .gap(px(2.))
            .child(option(
                "results-inline",
                "Inline",
                "Show each result under its statement",
                true,
            ))
            .child(option(
                "results-panel",
                "Panel",
                "Show results in this panel",
                false,
            ))
    }

    /// Shows a panel tab, opening the bottom panel in inline mode.
    fn show_panel(&mut self, panel: Panel) {
        self.panel = panel;
        self.panel_open = true;
    }

    /// The editor settings changed: wrap and indent.
    pub fn apply_settings(
        &mut self,
        s: &deadlock_core::settings::Settings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (wrap, tab) = (s.wrap, tab_size(s.indent));
        self.editor.update(cx, |e, cx| {
            e.set_soft_wrap(wrap, window, cx);
            e.set_tab_size(tab, cx);
        });
    }

    /// Sends results inline (`true`) or to the bottom panel. Leaving inline mode
    /// removes the inline results.
    pub fn set_inline(&mut self, inline: bool, cx: &mut Context<Self>) {
        if self.inline == inline {
            return;
        }
        self.inline = inline;
        if !inline {
            self.clear_blocks(cx);
        }
        cx.notify();
    }

    fn clear_blocks(&mut self, cx: &mut Context<Self>) {
        self.running_block = None;
        self.last_block = None;
        if self.blocks.is_empty() {
            return;
        }
        self.blocks.clear();
        self.editor.update(cx, |e, cx| e.clear_blocks(cx));
    }

    /// The line a statement's result goes under: its last line.
    fn end_row(&self, range: &Range<usize>, cx: &App) -> usize {
        let end = range.end.saturating_sub(1).max(range.start);
        self.editor.read(cx).text().offset_to_point(end).row
    }

    fn block_row(&self, view: &Entity<ResultBlock>, cx: &App) -> Option<usize> {
        let b = self.blocks.iter().find(|b| &b.view == view)?;
        self.editor.read(cx).block_row(b.id)
    }

    /// Puts `view` in the editor under line `row`; the block follows the view's height.
    fn insert_block(&mut self, row: usize, view: Entity<ResultBlock>, cx: &mut Context<Self>) {
        let sql = self.engine() != Some(Engine::Redis);
        view.update(cx, |v, _| v.sql = sql);
        let height = view.read(cx).height(cx);
        let render = view.clone();
        let id = self.editor.update(cx, |e, cx| {
            e.insert_block(
                row,
                height,
                move |_, _| render.clone().into_any_element(),
                cx,
            )
        });
        let sync = cx.observe(&view, move |this, view, cx| {
            let h = view.read(cx).height(cx);
            this.editor
                .update(cx, |e, cx| e.set_block_height(id, h, cx));
        });
        let events = cx.subscribe(&view, |this, view, e: &BlockEvent, cx| {
            this.on_block_event(&view, *e, cx);
        });
        self.blocks.push(Inline {
            id,
            view,
            _subs: [sync, events],
        });
    }

    fn remove_block(&mut self, view: &Entity<ResultBlock>, cx: &mut Context<Self>) {
        let Some(i) = self.blocks.iter().position(|b| &b.view == view) else {
            return;
        };
        let b = self.blocks.remove(i);
        self.editor.update(cx, |e, cx| e.remove_block(b.id, cx));
    }

    fn on_block_event(
        &mut self,
        view: &Entity<ResultBlock>,
        e: BlockEvent,
        cx: &mut Context<Self>,
    ) {
        match e {
            BlockEvent::Close => self.remove_block(view, cx),
            BlockEvent::Stop => self.stop(cx),
            BlockEvent::Export(format, all) => {
                if let Some(grid) = view.read(cx).grid().cloned() {
                    self.export(grid, format, all, cx);
                }
            }
            BlockEvent::OpenInPanel => {
                let Some(table) = view.read(cx).grid().cloned() else {
                    return;
                };
                self.remove_block(view, cx);
                self.results.push(table);
                self.show_panel(Panel::Result(self.results.len() - 1));
            }
        }
        cx.notify();
    }

    /// A statement starts: in inline mode its block shows it running, replacing the
    /// blocks it had from an earlier run.
    fn begin_block(&mut self, range: &Range<usize>, text: &str, cx: &mut Context<Self>) {
        if !self.inline {
            return;
        }
        let first = self.editor.read(cx).text().offset_to_point(range.start).row;
        let last = self.end_row(range, cx);
        let old: Vec<_> = self
            .blocks
            .iter()
            .filter(|b| {
                self.editor
                    .read(cx)
                    .block_row(b.id)
                    .is_some_and(|r| (first..=last).contains(&r))
            })
            .map(|b| b.view.clone())
            .collect();
        for v in &old {
            self.remove_block(v, cx);
        }
        let text = text.to_owned();
        let view = cx.new(|cx| ResultBlock::running(text, self.editor.downgrade(), cx));
        self.insert_block(last, view.clone(), cx);
        self.running_block = Some((view, false));
    }

    /// A result set arrived: in inline mode it goes into the running statement's block,
    /// or under it when the block already holds one. `false`: it goes to the panel.
    fn attach_grid(&mut self, table: &Entity<TableState<Grid>>, cx: &mut Context<Self>) -> bool {
        if !self.inline {
            return false;
        }
        if let Some((view, attached)) = &mut self.running_block
            && !*attached
        {
            *attached = true;
            // A single value needs no grid: it starts as one line that shows it.
            let single = {
                let g = table.read(cx).delegate();
                g.columns().len() == 1 && g.row_count() == 1 && g.is_complete()
            };
            view.update(cx, |b, cx| {
                b.collapsed = single;
                b.set_body(Body::Rows(table.clone()), cx);
            });
            return true;
        }
        let under = self
            .running_block
            .as_ref()
            .map(|(v, _)| v)
            .or(self.last_block.as_ref())
            .cloned();
        let Some((under, row)) =
            under.and_then(|v| Some((self.block_row(&v, cx)?, v)).map(|(r, v)| (v, r)))
        else {
            return false;
        };
        let statement = under.read(cx).statement.clone();
        let editor = self.editor.downgrade();
        let table = table.clone();
        let view = cx.new(|cx| {
            let mut b = ResultBlock::running(statement, editor.clone(), cx);
            b.set_body(Body::Rows(table), cx);
            b
        });
        self.insert_block(row, view, cx);
        true
    }

    /// The running statement finished with `text` (a summary, or the error): its block
    /// shows it. The first error of a run is scrolled into view.
    fn end_block(&mut self, text: &str, error: bool, cx: &mut Context<Self>) {
        let Some((view, attached)) = self.running_block.take() else {
            return;
        };
        self.last_block = Some(view.clone());
        let target = match (error, attached) {
            (false, true) => return,
            (false, false) => {
                // The block is one line high: printed output stays in Messages.
                let mut lines = text.lines();
                let first = lines.next().unwrap_or_default();
                let body = Body::Done(match lines.count() {
                    0 => first.to_owned(),
                    1 => format!("{first} · 1 line of output in Messages"),
                    n => format!("{first} · {n} lines of output in Messages"),
                });
                view.update(cx, |b, cx| b.set_body(body, cx));
                return;
            }
            (true, false) => view,
            // The error came after a result set: it gets its own block under it.
            (true, true) => {
                let Some(row) = self.block_row(&view, cx) else {
                    return;
                };
                let statement = view.read(cx).statement.clone();
                let editor = self.editor.downgrade();
                let v = cx.new(|cx| ResultBlock::running(statement, editor.clone(), cx));
                self.insert_block(row, v.clone(), cx);
                v
            }
        };
        // The block is one line high: a multi-line error (Oracle's compilation errors)
        // shows its first two lines; Messages has the rest.
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let first: Vec<&str> = lines.by_ref().take(2).collect();
        let text = if lines.next().is_some() {
            format!("{} … (all of it in Messages)", first.join(" "))
        } else {
            first.join(" ")
        };
        let (code, message) = inline::error_parts(&text);
        target.update(cx, |b, cx| b.set_body(Body::Error { code, message }, cx));
        if !std::mem::replace(&mut self.error_shown, true)
            && let Some(id) = self.blocks.iter().find(|b| b.view == target).map(|b| b.id)
        {
            self.editor.update(cx, |e, cx| e.scroll_to_block(id, cx));
        }
    }

    /// After an edit: drops blocks whose line was deleted, and marks a block outdated
    /// while no statement ending on its line has the text it ran.
    fn refresh_blocks(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.blocks.is_empty() {
            return;
        }
        let editor = self.editor.read(cx);
        let rope = editor.text();
        let ends: Vec<(usize, &str)> = self
            .statements
            .iter()
            .filter_map(|(r, _)| {
                let end = r.end.saturating_sub(1).max(r.start);
                Some((rope.offset_to_point(end).row, text.get(r.clone())?))
            })
            .collect();
        let rows: Vec<Option<usize>> = self.blocks.iter().map(|b| editor.block_row(b.id)).collect();
        let mut i = 0;
        self.blocks.retain(|_| {
            i += 1;
            rows[i - 1].is_some()
        });
        for (b, row) in self.blocks.iter().zip(rows.into_iter().flatten()) {
            let v = b.view.read(cx);
            let stale =
                !v.is_running() && !ends.iter().any(|(r, t)| *r == row && *t == v.statement);
            if v.stale != stale {
                b.view.update(cx, |v, cx| {
                    v.stale = stale;
                    cx.notify();
                });
            }
        }
    }

    fn copy_selection(&mut self, cx: &mut Context<Self>) {
        let Panel::Result(i) = self.panel else { return };
        let Some(table) = self.results.get(i) else {
            return;
        };
        let t = table.read(cx);
        if let Some(text) = t.delegate().copy_text(t.selection()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }
}

/// Reads a statement's result sets: each gets its first page and a grid. The last
/// statement's grid stays open and fetches the rest as the user scrolls; any other
/// grid closes after its first page so the session can run the next statement.
/// Returns the summaries for Messages, or the statement's error.
async fn follow(
    this: &WeakEntity<QueryTab>,
    cx: &mut AsyncApp,
    rt: &Arc<Runtime>,
    mut stream: ResultStream,
    started: Instant,
    keep_open: bool,
    mut source: Option<(String, binds::Values)>,
) -> Result<(Vec<String>, Outcome), String> {
    let mut parts = Vec::new();
    // The first result set is what the history records.
    let mut first: Option<Outcome> = None;
    loop {
        // Only the statement's first result set is what running it again returns.
        let source = source.take();
        let rx = rt.spawn(async move {
            let page = stream.next_page().await;
            (stream, page)
        });
        let Ok((mut s, page)) = rx.await else {
            return Err("the query task stopped".into());
        };
        let rows = match page {
            Some(Ok(rows)) => rows,
            Some(Err(e)) => return Err(e.to_string()),
            None => Vec::new(),
        };
        let took = started.elapsed();
        let next = s.next_result();
        if s.columns.is_empty() {
            let n = s.affected_rows.unwrap_or(0);
            first.get_or_insert(Outcome::Affected(n));
            let noun = if n == 1 { "row" } else { "rows" };
            let mut part = format!("{n} {noun} affected in {}", duration(took));
            for line in &s.output {
                part.push('\n');
                part.push_str(line);
            }
            parts.push(part);
        } else {
            let complete = rows.len() < page_size();
            first.get_or_insert(Outcome::Rows {
                count: rows.len() as u64,
                more: !complete,
            });
            let columns = std::mem::take(&mut s.columns);
            let open = (!complete).then_some(s);
            let mut grid = Grid::new(columns, rows, open, Arc::clone(rt), took);
            grid.source = source;
            let window = this
                .read_with(cx, |t, _| t.window)
                .map_err(|_| "the tab was closed".to_string())?;
            let table = cx
                .update_window(window, |_, window, cx| {
                    this.update(cx, |t, cx| t.add_grid(grid, window, cx))
                })
                .ok()
                .and_then(Result::ok)
                .ok_or("the tab was closed")?;
            if !complete && keep_open {
                let summary = table.update(cx, |t, _| {
                    t.delegate_mut().set_next(next);
                    t.delegate().summary()
                });
                parts.push(summary);
                return Ok((parts, first.unwrap_or(Outcome::Affected(0))));
            }
            parts.push(table.update(cx, |t, cx| {
                t.delegate_mut().close();
                t.refresh(cx);
                t.delegate().summary()
            }));
        }
        let Some(next) = next else {
            return Ok((parts, first.unwrap_or(Outcome::Affected(0))));
        };
        match next.await {
            Ok(Ok(n)) => stream = n,
            Ok(Err(e)) => return Err(e.to_string()),
            Err(_) => return Ok((parts, first.unwrap_or(Outcome::Affected(0)))),
        }
    }
}

/// Classifies the run's statements and asks before the ones that need it: dangerous
/// statements everywhere, changes on production, implicit commits of an open
/// transaction, and `forced` (a drop from the Structure view) unless the connection
/// is read-only and refuses it anyway. `Ok(false)` when the user cancelled.
async fn confirm(
    this: &WeakEntity<QueryTab>,
    cx: &mut AsyncApp,
    rt: &Arc<Runtime>,
    driver: &Arc<dyn Driver>,
    texts: Vec<String>,
    forced: Option<String>,
) -> Result<bool, String> {
    let d = Arc::clone(driver);
    let (texts, classified) = rt
        .spawn(async move {
            let mut out = Vec::with_capacity(texts.len());
            for t in &texts {
                out.push(d.classify(t).await?);
            }
            Ok::<_, deadlock_core::driver::DriverError>((texts, out))
        })
        .await
        .map_err(|_| "the query task stopped".to_string())?
        .map_err(|e| e.to_string())?;
    let (window, ask) = this
        .read_with(cx, |t, cx| {
            let c = t.connection.as_ref();
            let production = c.is_some_and(|c| c.environment == Environment::Production);
            let read_only = c.is_some_and(|c| c.read_only);
            // Production always asks before dangerous statements.
            let ask_danger = production || crate::settings::get(cx).ask_dangerous;
            let found = concerns(
                &texts,
                &classified,
                production,
                read_only,
                t.tx_mode == TxMode::Manual && t.tx_open,
                t.tx_mode == TxMode::Manual,
                ask_danger,
            );
            // A drop from the Structure view: always asked, showing its statement.
            let found = match (forced.filter(|_| !read_only), found) {
                (Some(f), Some(c)) => Some(Concerns {
                    text: format!("{f}\n\n{}", c.text),
                    danger: true,
                    ..c
                }),
                (Some(f), None) => Some(Concerns {
                    text: f,
                    statements: texts.iter().map(|t| excerpt(t)).collect(),
                    danger: true,
                }),
                (None, c) => c,
            };
            let question = match c {
                Some(c) if production => format!("Run on production connection {}?", c.name),
                _ if texts.len() == 1 => "Run this statement?".into(),
                _ => "Run these statements?".into(),
            };
            (
                t.window,
                found.map(|c| {
                    crate::dialog::Ask::new(question, &["Run", "Cancel"])
                        .text(c.text)
                        .statement(c.statements.join("\n"))
                        .production(production)
                        .danger(c.danger)
                }),
            )
        })
        .map_err(|_| "the tab was closed".to_string())?;
    let Some(ask) = ask else {
        return Ok(true);
    };
    let answer = cx
        .update_window(window, |_, window, cx| crate::dialog::ask(window, cx, ask))
        .map_err(|_| "the window was closed".to_string())?;
    Ok(answer.await == Some(0))
}

const CANCELLED: &str = "Not explained: cancelled";

/// Whether a measured explain of `text` may go ahead: reads go straight away; anything
/// else runs for real, so it is refused on a read-only connection and asked about
/// otherwise, with the usual production and danger concerns. `Ok(false)`: cancelled.
async fn measure_ok(
    this: &WeakEntity<QueryTab>,
    cx: &mut AsyncApp,
    rt: &Arc<Runtime>,
    driver: &Arc<dyn Driver>,
    text: &str,
) -> Result<bool, String> {
    let (d, t) = (Arc::clone(driver), text.to_owned());
    let s = rt
        .spawn(async move { d.classify(&t).await })
        .await
        .map_err(|_| "the explain task stopped".to_string())?
        .map_err(|e| e.to_string())?;
    if !s.changes() {
        return confirm(this, cx, rt, driver, vec![text.to_owned()], None).await;
    }
    let (read_only, manual) = this
        .read_with(cx, |t, _| {
            (
                t.connection.as_ref().is_some_and(|c| c.read_only),
                t.tx_mode == TxMode::Manual,
            )
        })
        .map_err(|_| "the tab was closed".to_string())?;
    if read_only {
        return Err(format!(
            "Not explained: the connection is read-only, and measuring the plan would run this \
             {}, which changes data or schema.",
            s.verb
        ));
    }
    let what = if manual {
        "It runs inside the open transaction (Manual mode): commit or roll back afterwards."
    } else {
        "In Auto mode its changes commit as soon as it finishes."
    };
    let forced = format!(
        "Measuring the plan runs this {} for real. {what} The estimated plan does not run it.",
        s.verb
    );
    confirm(this, cx, rt, driver, vec![text.to_owned()], Some(forced)).await
}

/// Why a run needs confirming, as the prompt's detail; `None` when it runs straight
/// away. Writes on a read-only connection are left out: the driver refuses them.
fn concerns(
    texts: &[String],
    statements: &[Statement],
    production: bool,
    read_only: bool,
    tx_open: bool,
    manual: bool,
    ask_danger: bool,
) -> Option<Concerns> {
    let mut found = Concerns::default();
    let add = |found: &mut Concerns, line: String, shown: &String| {
        if !found.text.split("\n\n").any(|l| l == line) {
            if !found.text.is_empty() {
                found.text.push_str("\n\n");
            }
            found.text.push_str(&line);
        }
        if !found.statements.contains(shown) {
            found.statements.push(shown.clone());
        }
    };
    // In Manual mode a write earlier in the run opens a transaction too.
    let mut open = tx_open;
    for (text, s) in texts.iter().zip(statements) {
        if read_only && s.changes() {
            continue;
        }
        let shown = excerpt(text);
        if let Some(d) = s.danger.filter(|_| ask_danger) {
            found.danger = true;
            add(&mut found, format!("{d}."), &shown);
        } else if production && s.changes() {
            let what = if s.kind == StatementKind::Ddl {
                "the schema"
            } else {
                "data"
            };
            add(
                &mut found,
                format!("Changes {what} on a production connection."),
                &shown,
            );
        }
        if open && s.implicit_commit {
            add(
                &mut found,
                format!(
                    "{} commits the open transaction first: its changes become permanent and \
                     can no longer be rolled back.",
                    s.verb
                ),
                &shown,
            );
            open = false;
        }
        open |= manual && s.kind == StatementKind::Write;
    }
    (!found.text.is_empty()).then_some(found)
}

/// What a run asks about: the concerns, the statements they are about, and whether
/// one of them is dangerous (the dialog's action turns red and Enter does not run it).
#[derive(Debug, Default)]
struct Concerns {
    text: String,
    statements: Vec<String>,
    danger: bool,
}

/// The editor's indent for the Indent setting.
fn tab_size(indent: deadlock_core::settings::Indent) -> gpui_kit::component::input::TabSize {
    use deadlock_core::settings::Indent;
    gpui_kit::component::input::TabSize {
        tab_size: if indent == Indent::Four { 4 } else { 2 },
        hard_tabs: indent == Indent::Tab,
    }
}

/// A statement as a prompt shows it: up to 400 characters.
fn excerpt(text: &str) -> String {
    match text.char_indices().nth(400) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_owned(),
    }
}

fn now() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// A statement as Messages lists it: its first line, cut at 100 characters.
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(100) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None if text.lines().nth(1).is_some() => format!("{line} …"),
        None => line.to_owned(),
    }
}

/// `.tab` as the result panel uses it: 36 px, blue underline when on.
fn panel_tab(
    id: impl Into<gpui_kit::ElementId>,
    label: SharedString,
    on: bool,
    enabled: bool,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .h(tab_height())
        .px(px(12.))
        .border_b_2()
        .border_color(if on {
            p.blue
        } else {
            gpui_kit::transparent_black()
        })
        .text_color(if on { p.tx } else { p.tx3 })
        .when(on, |d| d.font_weight(FontWeight::MEDIUM))
        .when(enabled && !on, |d| {
            let tx = p.tx;
            d.hover(move |s| s.text_color(tx))
        })
        .when(!enabled, |d| d.opacity(0.38))
        .whitespace_nowrap()
        .child(label)
}

impl Render for QueryTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if self.table.is_some() {
            table::render(self, cx)
        } else {
            self.render_editor(window, cx)
        };
        let job = self.export.as_ref().map(|j| {
            let this = cx.entity().downgrade();
            let close = this.clone();
            j.render(
                move |_, cx| {
                    let _ = this.update(cx, |t, cx| t.cancel_export(cx));
                },
                move |_, cx| {
                    let _ = close.update(cx, |t, cx| t.close_export(cx));
                },
                cx,
            )
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .children(self.path_field.clone())
            .children(job)
            .child(div().flex_1().min_h_0().child(body))
    }
}

impl QueryTab {
    fn render_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let p = *cx.global::<Palette>();
        let area = Rc::clone(&self.area);
        let origin = area.get().origin;
        let editor = self.editor.read(cx);
        let visible = editor.visible_row_range().unwrap_or(0..0);
        let icons: Vec<_> = if self.running.is_some() {
            Vec::new()
        } else {
            self.statements
                .iter()
                .filter(|(_, row)| visible.contains(row))
                .filter_map(|(r, _)| {
                    let b = editor.range_to_bounds(&(r.start..r.start))?;
                    Some((r.start, b.top() - origin.y, b.size.height))
                })
                .collect()
        };
        let icons = icons.into_iter().map(|(start, top, h)| {
            div()
                .id(("run-statement", start))
                .absolute()
                .left(px(2.))
                .top(top)
                .h(h)
                .w(px(16.))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .child(icon(Icon::Play, p.ok).size(px(12.)))
                .tooltip(|window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new("Run this statement")
                        .build(window, cx)
                })
                .on_click(cx.listener(move |this, _, _, cx| this.run(Scope::At(start), cx)))
        });
        let editor_area = div()
            .relative()
            .size_full()
            .child(
                Editor::new(&self.editor)
                    .bordered(false)
                    .h_full()
                    .font_family(MONO_FONT)
                    .text_size(editor_size())
                    .line_height(px(22.))
                    .py(px(10.)),
            )
            .child(
                canvas(move |bounds, _, _| area.set(bounds), |_, _, _, _| {})
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
            )
            .children(icons);

        let many = self.results.len() > 1;
        let tabs = self.results.iter().enumerate().map(|(i, _)| {
            let label: SharedString = if many {
                format!("Result {}", i + 1).into()
            } else {
                "Result".into()
            };
            panel_tab(
                ("result-tab", i),
                label,
                self.panel == Panel::Result(i),
                true,
                &p,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_panel(Panel::Result(i));
                cx.notify();
            }))
        });
        let tabs: Vec<_> = if self.results.is_empty() {
            vec![
                panel_tab(
                    "result-tab-empty",
                    "Result".into(),
                    matches!(self.panel, Panel::Result(_)),
                    true,
                    &p,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.toggle_panel(Panel::Result(0));
                    cx.notify();
                })),
            ]
        } else {
            tabs.collect()
        };
        let summary: Option<String> = match self.panel {
            Panel::Result(i) => self.results.get(i).map(|t| t.read(cx).delegate().summary()),
            Panel::Messages => self.took.map(|t| {
                let n = self.messages.len();
                let noun = if n == 1 { "statement" } else { "statements" };
                format!("{n} {noun} in {}", duration(t))
            }),
            Panel::Plan => None,
        };
        let summary = if self.running.is_some() {
            Some("Running…".to_owned())
        } else {
            summary
        };
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(16.))
            .pl(px(4.))
            .pr(px(16.))
            .border_b_1()
            .border_color(p.line)
            .child(
                div()
                    .flex()
                    .children(tabs)
                    .child(
                        panel_tab(
                            "messages-tab",
                            "Messages".into(),
                            self.panel == Panel::Messages,
                            true,
                            &p,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.toggle_panel(Panel::Messages);
                            cx.notify();
                        })),
                    )
                    .child(
                        panel_tab(
                            "plan-tab",
                            "Plan".into(),
                            self.panel == Panel::Plan,
                            self.explained.is_some(),
                            &p,
                        )
                        .when(self.explained.is_some(), |d| {
                            d.on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_panel(Panel::Plan);
                                cx.notify();
                            }))
                        }),
                    ),
            )
            .child(div().flex_1())
            .children(summary.map(|s| div().text_color(p.tx3).child(s)))
            .when_some(
                self.explained
                    .as_ref()
                    .filter(|_| self.panel == Panel::Plan && self.running.is_none()),
                |d, v| d.child(plan::header_actions(v, cx)),
            )
            .child(self.place_switch(&p, cx));

        let body = match self.panel {
            Panel::Messages => messages(&self.messages, &p).into_any_element(),
            Panel::Plan => plan::render(self, cx),
            Panel::Result(i) => match self.results.get(i) {
                Some(table) => grid_text(
                    div()
                        .key_context("Results")
                        .on_action(
                            cx.listener(|this, _: &CopySelection, _, cx| this.copy_selection(cx)),
                        )
                        .size_full()
                        .text_color(p.tx)
                        .child(DataTable::new(table).bordered(false)),
                )
                .into_any_element(),
                None => div()
                    .size_full()
                    .p(px(16.))
                    .text_color(p.tx3)
                    .child(if self.connection.is_none() {
                        "Choose a connection in the toolbar to run statements.".to_owned()
                    } else if self.inline {
                        "Results show under their statements. Open in panel moves one here."
                            .to_owned()
                    } else {
                        format!(
                            "Run a statement ({}) to see its result here.",
                            crate::keymap::label("run", cx)
                        )
                    })
                    .into_any_element(),
            },
        };
        let overlay = binds::render(self, window, cx);
        let content = if self.inline && !self.panel_open {
            // Nothing to show: the panel keeps only its tabs, under the editor.
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(div().flex_1().min_h_0().child(editor_area))
                .child(
                    div()
                        .flex_none()
                        .bg(p.bg)
                        .border_t_1()
                        .border_color(p.line)
                        .child(header),
                )
                .into_any_element()
        } else {
            div()
                .size_full()
                .child(
                    v_resizable(SharedString::from(format!(
                        "query-split-{}",
                        cx.entity_id()
                    )))
                    .child(resizable_panel().child(editor_area))
                    .child(
                        resizable_panel().size(px(320.)).child(
                            div()
                                .size_full()
                                .flex()
                                .flex_col()
                                .bg(p.bg)
                                .border_t_1()
                                .border_color(p.line)
                                .child(header)
                                .child(div().flex_1().min_h_0().child(body)),
                        ),
                    ),
                )
                .into_any_element()
        };
        div()
            .size_full()
            .relative()
            .child(content)
            .when_some(overlay, |d, o| d.child(o))
            .into_any_element()
    }
}

fn messages(list: &[Message], p: &Palette) -> impl IntoElement {
    let rows = list.iter().enumerate().map(|(i, m)| {
        div()
            .id(("message", i))
            .flex()
            .gap(px(12.))
            .px(px(16.))
            .py(px(6.))
            .border_b_1()
            .border_color(p.line)
            .child(div().flex_none().text_color(p.tx3).child(m.at.clone()))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .font_family(MONO_FONT)
                            .text_size(small_size())
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(m.statement.clone()),
                    )
                    .child(
                        div()
                            .text_color(if m.error { p.danger } else { p.tx2 })
                            .child(m.text.clone()),
                    ),
            )
    });
    div()
        .id("messages")
        .size_full()
        .overflow_y_scroll()
        .when(list.is_empty(), |d| {
            d.child(
                div()
                    .p(px(16.))
                    .text_color(p.tx3)
                    .child("No statements run yet."),
            )
        })
        .children(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(
        texts: &[&str],
        production: bool,
        read_only: bool,
        tx_open: bool,
        manual: bool,
    ) -> Option<String> {
        let texts: Vec<String> = texts.iter().map(|t| (*t).to_owned()).collect();
        let statements: Vec<_> = texts
            .iter()
            .map(|t| deadlock_core::classify::classify(t, Engine::Mysql))
            .collect();
        concerns(
            &texts,
            &statements,
            production,
            read_only,
            tx_open,
            manual,
            true,
        )
        .map(|c| {
            format!(
                "{}\n--\n{}\n--\n{}",
                c.text,
                c.statements.join("\n"),
                c.danger
            )
        })
    }

    #[test]
    fn runs_ask_only_when_they_must() {
        // Reads and safe writes outside production run straight away.
        assert_eq!(
            ask(
                &["SELECT 1", "UPDATE t SET a = 1 WHERE id = 2"],
                false,
                false,
                false,
                false
            ),
            None
        );
        // Dangerous statements ask in every environment, showing the statement.
        let d = ask(&["DELETE FROM orders"], false, false, false, false).unwrap();
        assert!(
            d.contains("without WHERE") && d.ends_with("DELETE FROM orders\n--\ntrue"),
            "{d}"
        );
        assert!(ask(&["TRUNCATE t"], false, false, false, false).is_some());
        assert!(ask(&["DROP TABLE t"], false, false, false, false).is_some());
        // With the setting off they run; the caller keeps it on for Production.
        let texts = vec!["DELETE FROM orders".to_owned()];
        let st = vec![deadlock_core::classify::classify(&texts[0], Engine::Mysql)];
        assert!(concerns(&texts, &st, false, false, false, false, false).is_none());
        // Production asks before any change, not before reads.
        let d = ask(&["INSERT INTO t VALUES (1)"], true, false, false, false).unwrap();
        assert!(d.contains("production") && d.ends_with("false"), "{d}");
        // The same concern twice is said once; each statement is shown once.
        let d = ask(
            &["DELETE FROM a", "DELETE FROM b"],
            false,
            false,
            false,
            false,
        )
        .unwrap();
        assert_eq!(d.matches("without WHERE").count(), 1, "{d}");
        assert!(d.contains("DELETE FROM a\nDELETE FROM b"), "{d}");
        assert_eq!(
            ask(&["SELECT 1", "SHOW TABLES"], true, false, false, false),
            None
        );
        // Read-only connections refuse writes in the driver instead of asking.
        assert_eq!(ask(&["DELETE FROM t"], true, true, false, false), None);
    }

    #[test]
    fn implicit_commits_warn_inside_manual_transactions() {
        let warn = |texts: &[&str], open: bool, manual: bool| {
            ask(texts, false, false, open, manual)
                .is_some_and(|d| d.contains("commits the open transaction"))
        };
        assert!(warn(&["CREATE TABLE t2 (a INT)"], true, true));
        assert!(warn(&["LOCK TABLES t WRITE"], true, true));
        // A write earlier in the same run opens the transaction.
        assert!(warn(
            &["INSERT INTO t VALUES (1)", "ALTER TABLE t ADD b INT"],
            false,
            true
        ));
        assert!(!warn(&["CREATE TABLE t2 (a INT)"], false, true));
        assert!(!warn(
            &["INSERT INTO t VALUES (1)", "CREATE TABLE t2 (a INT)"],
            false,
            false
        ));
        assert!(!warn(&["CREATE TEMPORARY TABLE t2 (a INT)"], true, true));
    }

    #[test]
    fn messages_show_the_first_line() {
        assert_eq!(first_line("SELECT 1"), "SELECT 1");
        assert_eq!(first_line("SELECT 1\nFROM t"), "SELECT 1 …");
        let long = "x".repeat(150);
        assert_eq!(first_line(&long).chars().count(), 101);
    }
}
