//! The Sessions and locks tab: the server's sessions on a 5 s refresh, lock-wait
//! banners, filters, the session grid and detail pane, Cancel statement / End
//! session, and the last detected deadlock.

use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use deadlock_core::connection::{ConnectionConfig, Engine, Environment};
use deadlock_core::driver::{Driver, connect};
use deadlock_core::sessions::{
    Chain, Deadlock, Filter, LockKind, Session, SessionId, Snapshot, State, duration,
    parse_deadlock,
};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, EventEmitter, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Render, ScrollStrategy, SharedString, StatefulInteractiveElement, Styled, Task,
    UniformListScrollHandle, Window, div, px, relative, uniform_list,
};

use crate::app::Db;
use crate::assets::{Icon, icon};
use crate::query::table::selectable_sql;
use crate::theme::{Palette, RADIUS, data_size, mono_font, small_size};
use crate::ui::Clickable as _;
use crate::ui::{Kind, button};

/// How often the list refreshes, and the status bar's check runs.
pub(crate) const REFRESH: Duration = Duration::from_secs(5);

/// `.sr`: 34 px rows.
const ROW: f32 = 34.;
/// Grid column widths from `.sr`, State and For widened so `idle in transaction` and
/// `2 min 14 s` fit in the mono face; the statement takes the rest.
const WIDTHS: [f32; 6] = [84., 96., 104., 168., 96., 104.];

pub enum SessionsEvent {
    /// Sessions waiting on a lock, after a refresh.
    Waiting(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Col {
    Process,
    User,
    Application,
    State,
    WaitingOn,
    For,
    Statement,
}

const COLUMNS: [(Col, &str); 7] = [
    (Col::Process, "Process"),
    (Col::User, "User"),
    (Col::Application, "Application"),
    (Col::State, "State"),
    (Col::WaitingOn, "Waiting on"),
    (Col::For, "For"),
    (Col::Statement, "Last statement"),
];

enum Conn {
    Connecting,
    Ready(Arc<dyn Driver>),
    Failed(String),
}

/// The server's report text, and the report parsed when it could be.
type Report = (String, Option<Deadlock>);

/// The last-deadlock section.
#[derive(Default)]
struct DeadlockView {
    open: bool,
    raw: bool,
    loading: bool,
    /// The report's text; `Ok(None)`: none detected since the server started.
    /// The error says whether it is only information (`true`): the engine keeps no
    /// deadlock report, as Oracle.
    loaded: Option<Result<Option<Report>, (String, bool)>>,
}

pub struct SessionsTab {
    pub connection: ConnectionConfig,
    conn: Conn,
    snapshot: Option<Snapshot>,
    /// The last refresh failed; the list shown is older.
    error: Option<String>,
    loading: bool,
    paused: bool,
    /// A confirmation is open: refresh waits.
    confirming: bool,
    filter: Filter,
    sort: (Col, bool),
    selected: Option<SessionId>,
    /// The result of the last Cancel / End, and whether it failed.
    message: Option<(String, bool)>,
    /// Session ids in grid order, after filter and sort.
    visible: Vec<SessionId>,
    list: UniformListScrollHandle,
    deadlock: DeadlockView,
    _ticker: Task<()>,
}

impl EventEmitter<SessionsEvent> for SessionsTab {}

impl SessionsTab {
    pub fn new(config: ConnectionConfig, password: String, cx: &mut Context<Self>) -> Self {
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH).await;
                let Ok(()) = this.update(cx, |t, cx| {
                    if !t.paused && !t.confirming {
                        t.refresh(cx);
                    }
                }) else {
                    break;
                };
            }
        });
        // The driver closes its socket on the runtime, not the UI thread.
        cx.on_release(|this, cx| {
            if let Conn::Ready(d) = std::mem::replace(&mut this.conn, Conn::Connecting) {
                drop(cx.global::<Db>().0.spawn(async move { drop(d) }));
            }
        })
        .detach();
        let mut this = Self {
            connection: config.clone(),
            conn: Conn::Connecting,
            snapshot: None,
            error: None,
            loading: false,
            paused: false,
            confirming: false,
            filter: Filter::All,
            sort: (Col::Process, false),
            selected: None,
            message: None,
            visible: Vec::new(),
            list: UniformListScrollHandle::new(),
            deadlock: DeadlockView::default(),
            _ticker: ticker,
        };
        this.open(config, password, cx);
        this
    }

    /// Opens the screen's own server session, then loads.
    fn open(&mut self, config: ConnectionConfig, password: String, cx: &mut Context<Self>) {
        let rx = cx
            .global::<Db>()
            .0
            .spawn(async move { connect(&config, &password).await.map_err(|e| e.to_string()) });
        cx.spawn(async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err("the connection task stopped".into()));
            let _ = this.update(cx, |t, cx| {
                t.conn = match result {
                    Ok(d) => Conn::Ready(d),
                    Err(e) => Conn::Failed(e),
                };
                t.refresh(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        match &self.conn {
            Conn::Ready(d) => Some(Arc::clone(d)),
            _ => None,
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(d) = self.driver().filter(|_| !self.loading) else {
            return;
        };
        self.loading = true;
        let rx = cx
            .global::<Db>()
            .0
            .spawn(async move { d.sessions().await.map_err(|e| e.to_string()) });
        cx.spawn(async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err("the sessions task stopped".into()));
            let _ = this.update(cx, |t, cx| t.loaded(result, cx));
        })
        .detach();
        if self.deadlock.open {
            self.load_deadlock(cx);
        }
    }

    fn loaded(&mut self, result: Result<Snapshot, String>, cx: &mut Context<Self>) {
        self.loading = false;
        match result {
            Ok(s) => {
                self.error = None;
                cx.emit(SessionsEvent::Waiting(s.waiting()));
                self.snapshot = Some(s);
                self.arrange();
            }
            Err(e) => self.error = Some(e),
        }
        cx.notify();
    }

    /// Recomputes the grid's rows from the filter and the sort.
    fn arrange(&mut self) {
        let Some(s) = &self.snapshot else {
            self.visible.clear();
            return;
        };
        let mut rows: Vec<&Session> = s
            .sessions
            .iter()
            .filter(|x| self.filter.matches(x))
            .collect();
        let (col, desc) = self.sort;
        rows.sort_by(|a, b| {
            let o = match col {
                Col::Process => a.id.cmp(&b.id),
                Col::User => a.user.cmp(&b.user),
                Col::Application => application(a).cmp(&application(b)),
                Col::State => state_rank(s, a).cmp(&state_rank(s, b)),
                Col::WaitingOn => s.blockers(a.id).cmp(&s.blockers(b.id)),
                Col::For => for_seconds(s, a).cmp(&for_seconds(s, b)),
                Col::Statement => a.statement.cmp(&b.statement),
            };
            if desc { o.reverse() } else { o }.then(a.id.cmp(&b.id))
        });
        self.visible = rows.iter().map(|x| x.id).collect();
    }

    fn set_filter(&mut self, f: Filter, cx: &mut Context<Self>) {
        self.filter = f;
        self.arrange();
        cx.notify();
    }

    fn set_sort(&mut self, col: Col, cx: &mut Context<Self>) {
        self.sort = match self.sort {
            (c, desc) if c == col => (col, !desc),
            _ => (col, false),
        };
        self.arrange();
        cx.notify();
    }

    /// Selects `id`, showing it in the grid even when the filter hides it.
    fn select(&mut self, id: SessionId, cx: &mut Context<Self>) {
        if !self.visible.contains(&id) {
            self.filter = Filter::All;
            self.arrange();
        }
        self.selected = Some(id);
        if let Some(ix) = self.visible.iter().position(|v| *v == id) {
            self.list.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        self.paused = !self.paused;
        if !self.paused {
            self.refresh(cx);
        }
        cx.notify();
    }

    fn toggle_deadlock(&mut self, cx: &mut Context<Self>) {
        self.deadlock.open = !self.deadlock.open;
        if self.deadlock.open {
            self.load_deadlock(cx);
        }
        cx.notify();
    }

    fn load_deadlock(&mut self, cx: &mut Context<Self>) {
        let Some(d) = self.driver().filter(|_| !self.deadlock.loading) else {
            return;
        };
        self.deadlock.loading = true;
        let rx = cx.global::<Db>().0.spawn(async move {
            d.last_deadlock()
                .await
                .map_err(|e| {
                    let info = matches!(e, deadlock_core::driver::DriverError::Unsupported(_));
                    (e.to_string(), info)
                })
                .map(|raw| {
                    raw.map(|r| {
                        let parsed = parse_deadlock(&r);
                        (r, parsed)
                    })
                })
        });
        cx.spawn(async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err(("the deadlock task stopped".into(), false)));
            let _ = this.update(cx, |t, cx| {
                t.deadlock.loading = false;
                t.deadlock.loaded = Some(result);
                cx.notify();
            });
        })
        .detach();
    }

    /// Cancel statement (`end` false) or End session, after a confirmation that names
    /// the session, its user and its statement. Refresh waits while it is open.
    fn redis(&self) -> bool {
        self.connection.engine == Engine::Redis
    }

    /// Reset on the slow log: asks, then empties it.
    fn reset_slow_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(driver) = self.driver() else { return };
        let production = if self.connection.environment == Environment::Production {
            format!(" on production connection {}", self.connection.name)
        } else {
            String::new()
        };
        self.confirming = true;
        let answer = crate::dialog::ask(
            window,
            cx,
            crate::dialog::Ask::new(
                format!("Reset the slow log{production}?"),
                &["Reset slow log", "Cancel"],
            )
            .text("SLOWLOG RESET deletes every entry, for every client of the server.")
            .production(!production.is_empty())
            .danger(true),
        );
        let rt = Arc::clone(&cx.global::<Db>().0);
        cx.spawn(async move |this, cx| {
            let go = answer.await == Some(0);
            let result = if go {
                Some(
                    rt.spawn(
                        async move { driver.reset_slow_log().await.map_err(|e| e.to_string()) },
                    )
                    .await
                    .unwrap_or_else(|_| Err("the task stopped".into())),
                )
            } else {
                None
            };
            let _ = this.update(cx, |t, cx| {
                t.confirming = false;
                match result {
                    Some(Ok(())) => {
                        t.message = Some(("The slow log was reset.".into(), false));
                        t.refresh(cx);
                    }
                    Some(Err(e)) => t.message = Some((e, true)),
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn act(&mut self, id: SessionId, end: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(s), Some(driver)) = (
            self.snapshot.as_ref().and_then(|s| s.session(id)).cloned(),
            self.driver(),
        ) else {
            return;
        };
        let (question, detail, label) = confirmation(&self.connection, &s, end);
        self.confirming = true;
        let answer = crate::dialog::ask(
            window,
            cx,
            crate::dialog::Ask::new(question, &[label, "Cancel"])
                .text(detail)
                .production(self.connection.environment == Environment::Production)
                .danger(end),
        );
        let rt = Arc::clone(&cx.global::<Db>().0);
        cx.spawn(async move |this, cx| {
            let go = answer.await == Some(0);
            let _ = this.update(cx, |t, cx| {
                t.confirming = false;
                cx.notify();
            });
            if !go {
                return;
            }
            let result = rt
                .spawn(async move {
                    if end {
                        driver.end_session(id).await
                    } else {
                        driver.cancel_statement(id).await
                    }
                    .map_err(|e| e.to_string())
                })
                .await
                .unwrap_or_else(|_| Err("the task stopped".into()));
            let _ = this.update(cx, |t, cx| {
                t.message = Some(match result {
                    Ok(()) if end => (format!("Ended session {id}."), false),
                    Ok(()) if t.redis() => (format!("Unblocked client {id}."), false),
                    Ok(()) => (format!("Cancelled the statement of session {id}."), false),
                    Err(e) => (e, true),
                });
                t.refresh(cx);
                cx.notify();
            });
        })
        .detach();
    }
}

/// The question, the detail and the button of a Cancel / End confirmation.
fn confirmation(c: &ConnectionConfig, s: &Session, end: bool) -> (String, String, &'static str) {
    let production = c.environment == Environment::Production;
    let redis = c.engine == Engine::Redis;
    let question = match (end, production) {
        (false, _) if redis && production => {
            format!(
                "Unblock client {} on production connection {}?",
                s.id, c.name
            )
        }
        (false, _) if redis => format!("Unblock client {}?", s.id),
        (true, _) if s.this_app => format!("End session {}, which this app uses?", s.id),
        (true, true) => format!("End session {} on production connection {}?", s.id, c.name),
        (true, false) => format!("End session {}?", s.id),
        (false, true) => format!(
            "Cancel the statement of session {} on production connection {}?",
            s.id, c.name
        ),
        (false, false) => format!("Cancel the statement of session {}?", s.id),
    };
    let mut detail = format!("User: {}", s.user);
    if let Some(a) = &s.application {
        detail.push_str(&format!(" · {a}"));
    }
    detail.push_str(&format!(
        "\n{}: {}",
        if s.running {
            "Statement"
        } else {
            "Last statement"
        },
        s.statement
            .as_deref()
            .map_or("not reported".into(), |q| ellipsis(&one_line(q), 120))
    ));
    detail.push_str("\n\n");
    detail.push_str(match (end, s.transaction.is_some()) {
        (true, true) => {
            "Its open transaction is rolled back: changes it has not committed are lost."
        }
        (true, false) => "Its connection is closed.",
        (false, _) if redis => {
            "Its blocked command returns as if it timed out (CLIENT UNBLOCK); the client stays."
        }
        (false, _) => "The statement stops; the session and its transaction stay.",
    });
    if redis && end {
        detail.push_str(&format!(" (CLIENT KILL ID {})", s.id));
    }
    if s.this_app && end {
        detail.push_str(
            "\n\nThis is one of this app's own sessions (the sidebar's or a tab's). Whatever it \
             is running stops, and an open transaction in it is rolled back. Deadlock will \
             reconnect automatically.",
        );
    } else if s.this_app {
        detail.push_str("\n\nThis is one of this app's own sessions.");
    } else if s.deadlock {
        detail.push_str("\n\nThis session belongs to Deadlock.");
    }
    if c.read_only {
        detail.push_str("\n\nThe connection is read-only; this changes no data.");
    }
    let label = match (end, redis) {
        (true, _) if s.this_app => "End this app's session",
        (true, _) => "End session",
        (false, true) => "Unblock",
        (false, false) => "Cancel statement",
    };
    (question, detail, label)
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn ellipsis(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{} …", &s[..i]),
        None => s.to_owned(),
    }
}

fn application(s: &Session) -> String {
    match (&s.application, s.own, s.deadlock) {
        (_, true, _) => "Deadlock (this screen)".into(),
        _ if s.this_app => "Deadlock (this app)".into(),
        (Some(a), ..) => a.clone(),
        (None, _, true) => "Deadlock".into(),
        (None, ..) => String::new(),
    }
}

/// Waiting first, then holders, active, idle in transaction, idle.
fn state_rank(snap: &Snapshot, s: &Session) -> u8 {
    match s.state {
        State::Waiting => 0,
        _ if blocks(snap, s.id) => 1,
        State::Active | State::Blocked => 2,
        State::IdleInTransaction => 3,
        State::Idle => 4,
    }
}

fn blocks(snap: &Snapshot, id: SessionId) -> bool {
    snap.waits.iter().any(|w| w.blocker == Some(id))
}

/// The For column: how long it waits, or how long it has been in its state.
fn for_seconds(snap: &Snapshot, s: &Session) -> Option<u64> {
    snap.wait_of(s.id).and_then(|w| w.seconds).or(s.seconds)
}

fn ids(list: &[SessionId]) -> String {
    let names: Vec<String> = list.iter().map(ToString::to_string).collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// `1 session is waiting on a lock`, and the plain-language explanation of a chain.
fn banner_text(snap: &Snapshot, chain: &Chain) -> (String, String) {
    let waiters: usize = if chain.cycle || chain.unknown_blocker {
        chain.levels.iter().map(Vec::len).sum()
    } else {
        chain.levels.iter().skip(1).map(Vec::len).sum()
    };
    let title = if waiters == 1 {
        "1 session is waiting on a lock".to_owned()
    } else {
        format!("{waiters} sessions are waiting on a lock")
    };
    if chain.cycle {
        let all: Vec<SessionId> = chain.levels.iter().flatten().copied().collect();
        return (
            title,
            format!(
                "Sessions {} wait on each other: this is a deadlock. None of them can go on \
                 until the server rolls one back, or you end one of them.",
                ids(&all)
            ),
        );
    }
    let first = &chain.levels[0];
    if chain.unknown_blocker {
        let what = snap
            .wait_of(first[0])
            .map_or("a lock".into(), |w| match w.wants.kind {
                LockKind::Metadata if w.wants.table.is_empty() => "a metadata lock".into(),
                _ => w.wants.describe().to_lowercase(),
            });
        return (
            title,
            format!(
                "Session {} waits for {what}, and the server does not say which session \
                 holds it.",
                ids(first)
            ),
        );
    }
    let root = first[0];
    let direct = chain.levels.get(1).cloned().unwrap_or_default();
    let what = direct
        .iter()
        .find_map(|w| {
            snap.waits
                .iter()
                .find(|x| x.waiter == *w && x.blocker == Some(root))
        })
        .map_or("the lock".to_owned(), |w| match w.wants.kind {
            LockKind::Row => "the row".into(),
            LockKind::Table => format!("a lock on table {}", w.wants.table),
            LockKind::Metadata => format!("a metadata lock on table {}", w.wants.table),
        });
    let verb = if direct.len() == 1 { "waits" } else { "wait" };
    let state = snap.session(root).map(|s| s.state);
    let mut text = match state {
        Some(State::IdleInTransaction) => format!(
            "This is a lock wait, not a deadlock. Session {root} holds {what} and is idle \
             inside an open transaction, so {} {verb} until {root} commits, rolls back or \
             is ended.",
            ids(&direct)
        ),
        Some(State::Active) => format!(
            "This is a lock wait, not a deadlock. Session {root} holds {what} while it runs \
             a statement, so {} {verb} until {root}'s transaction ends or {root} is ended.",
            ids(&direct)
        ),
        _ => format!(
            "This is a lock wait, not a deadlock. Session {root} holds {what}, so {} {verb} \
             until {root} commits, rolls back or is ended.",
            ids(&direct)
        ),
    };
    for pair in chain.levels.windows(2).skip(1) {
        let verb = if pair[1].len() == 1 { "waits" } else { "wait" };
        text.push_str(&format!(
            " {} in turn {verb} on {}.",
            ids(&pair[1]),
            ids(&pair[0])
        ));
    }
    (title, text)
}

impl Render for SessionsTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *cx.global::<Palette>();
        let body: AnyElement = match (&self.conn, &self.snapshot) {
            (Conn::Failed(e), _) => notice(e, p.danger).into_any_element(),
            (_, None) => notice(
                self.error.as_deref().unwrap_or("Loading sessions…"),
                if self.error.is_some() {
                    p.danger
                } else {
                    p.tx3
                },
            )
            .into_any_element(),
            (_, Some(snap)) => {
                let snap = snap.clone();
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .children(snap.chains().iter().map(|c| banner(&snap, c, cx)))
                    // Redis has no locks or transactions: only All would mean something.
                    .when(!self.redis(), |d| d.child(chips(&snap, self.filter, cx)))
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .child(grid(self, &snap, cx))
                            .child(detail(self, &snap, cx)),
                    )
                    .when(!self.redis(), |d| d.child(deadlock_section(self, cx)))
                    .children(snap.slow_log.as_ref().map(|log| slow_log(log, cx)))
                    .into_any_element()
            }
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.bg)
            .child(header(self, cx))
            .child(body)
    }
}

fn notice(text: &str, color: gpui_kit::Hsla) -> Div {
    div()
        .p(px(20.))
        .text_color(color)
        .child(SharedString::from(text.to_owned()))
}

fn header(t: &SessionsTab, cx: &mut Context<SessionsTab>) -> impl IntoElement {
    let p = *cx.global::<Palette>();
    let count = t.snapshot.as_ref().map(|s| {
        let n = s.sessions.len();
        match (s.own_only, n) {
            (Some(p), 1) => format!("Only your own session: 1 (no {p} privilege)"),
            (Some(p), n) => format!("Only your own sessions: {n} (no {p} privilege)"),
            (None, 1) => "1 session".into(),
            (None, n) => format!("{n} sessions"),
        }
    });
    let refresh = if t.paused {
        "Refresh paused"
    } else {
        "Refreshes every 5 seconds"
    };
    let notes: Vec<String> = t
        .snapshot
        .as_ref()
        .map(|s| s.notes.clone())
        .unwrap_or_default();
    div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .px(px(20.))
        .pt(px(20.))
        .pb(px(16.))
        .child(
            div()
                .flex()
                .items_end()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .text_size(px(20.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .line_height(relative(1.2))
                                .child(if t.redis() {
                                    "Clients and slow log"
                                } else {
                                    "Sessions and locks"
                                }),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap_x(px(16.))
                                .text_color(p.tx2)
                                .child(SharedString::from(t.connection.name.clone()))
                                .children(count)
                                .child(refresh),
                        ),
                )
                .child(
                    button(
                        "pause-refresh",
                        Kind::Outline,
                        true,
                        "Stop or restart the 5 second refresh",
                        cx,
                    )
                    .child(if t.paused {
                        "Resume refresh"
                    } else {
                        "Pause refresh"
                    })
                    .clickable(cx.listener(|t, _, _, cx| t.toggle_pause(cx))),
                ),
        )
        .when_some(t.error.as_ref().filter(|_| t.snapshot.is_some()), |d, e| {
            d.child(
                div()
                    .text_color(p.danger)
                    .child(SharedString::from(format!("Refresh failed: {e}"))),
            )
        })
        .when_some(t.message.as_ref(), |d, (m, failed)| {
            d.child(
                div()
                    .text_color(if *failed { p.danger } else { p.tx2 })
                    .child(SharedString::from(m.clone())),
            )
        })
        .children(notes.into_iter().map(|n| {
            div()
                .text_color(p.tx3)
                .text_size(small_size())
                .child(SharedString::from(n))
        }))
}

/// `.box`: a holder or waiter card in a banner.
fn card(
    id: SessionId,
    title: String,
    line: String,
    mono: String,
    waiting: bool,
    cx: &mut Context<SessionsTab>,
) -> impl IntoElement {
    let p = *cx.global::<Palette>();
    div()
        .id(SharedString::from(format!("card-{id}-{waiting}")))
        .flex_1()
        .min_w(px(220.))
        .max_w(px(340.))
        .px(px(14.))
        .py(px(12.))
        .border_1()
        .border_color(if waiting { p.warn } else { p.line2 })
        .rounded(px(8.))
        .bg(p.bg)
        .flex()
        .flex_col()
        .gap(px(4.))
        .clickable(cx.listener(move |t, _, _, cx| t.select(id, cx)))
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child(SharedString::from(title)),
        )
        .child(div().text_color(p.tx3).child(SharedString::from(line)))
        .child(
            div()
                .font_family(mono_font())
                .text_size(data_size())
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(SharedString::from(mono)),
        )
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

fn banner(snap: &Snapshot, chain: &Chain, cx: &mut Context<SessionsTab>) -> AnyElement {
    let p = *cx.global::<Palette>();
    let (title, text) = banner_text(snap, chain);
    let holder = !chain.cycle && !chain.unknown_blocker;
    let mut row: Vec<AnyElement> = Vec::new();
    for (depth, level) in chain.levels.iter().enumerate() {
        if depth > 0 {
            row.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_color(p.warn)
                    .font_weight(FontWeight::MEDIUM)
                    .child(if chain.cycle { "waits on" } else { "blocks" })
                    .child(icon(Icon::Blocks, p.warn))
                    .into_any_element(),
            );
        }
        let cards = level.iter().map(|&id| {
            let s = snap.session(id);
            let wait = snap.wait_of(id);
            if depth == 0 && holder {
                // What it holds that the next level wants.
                let lock = snap
                    .waits
                    .iter()
                    .find(|w| w.blocker == Some(id))
                    .map(|w| w.wants.short())
                    .unwrap_or_default();
                let line = s.map_or(String::new(), |s| match s.seconds {
                    Some(n) => format!("{} for {}", capital(s.state.label()), duration(n)),
                    None => capital(s.state.label()),
                });
                card(
                    id,
                    format!("Session {id} holds the lock"),
                    line,
                    lock,
                    false,
                    cx,
                )
                .into_any_element()
            } else {
                let line = match wait.and_then(|w| w.seconds) {
                    Some(n) => format!("Waiting for {}", duration(n)),
                    None => "Waiting".into(),
                };
                let statement = s
                    .and_then(|s| s.statement.as_deref())
                    .map(|q| ellipsis(&one_line(q), 60))
                    .unwrap_or_default();
                card(
                    id,
                    format!("Session {id} is waiting"),
                    line,
                    statement,
                    true,
                    cx,
                )
                .into_any_element()
            }
        });
        row.push(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .children(cards)
                .into_any_element(),
        );
    }
    div()
        .flex()
        .flex_col()
        .gap(px(12.))
        .px(px(20.))
        .py(px(16.))
        .bg(p.warn_bg)
        .border_t_1()
        .border_b_1()
        .border_color(p.warn_line)
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(SharedString::from(title)),
                )
                .child(
                    div()
                        .max_w(px(620.))
                        .text_color(p.tx2)
                        .line_height(relative(1.5))
                        .child(SharedString::from(text)),
                ),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(12.))
                .children(row),
        )
        .into_any_element()
}

fn chips(snap: &Snapshot, filter: Filter, cx: &mut Context<SessionsTab>) -> impl IntoElement {
    let p = *cx.global::<Palette>();
    let chips = Filter::ALL.map(|f| {
        let on = f == filter;
        let n = snap.count(f);
        let count = div().child(n.to_string());
        let count = match (on, f) {
            (true, _) => count,
            (false, Filter::Waiting) if n > 0 => {
                count.text_color(p.warn).font_weight(FontWeight::MEDIUM)
            }
            (false, _) => count.text_color(p.tx3),
        };
        let id = match f {
            Filter::All => "filter-all",
            Filter::Active => "filter-active",
            Filter::Waiting => "filter-waiting",
            Filter::IdleInTransaction => "filter-idle-tx",
        };
        button(id, Kind::Outline, true, f.label(), cx)
            .when(on, |b| b.bg(p.tx).border_color(p.tx).text_color(p.bg))
            .child(f.label())
            .child(count)
            .clickable(cx.listener(move |t, _, _, cx| t.set_filter(f, cx)))
    });
    div()
        .flex()
        .flex_wrap()
        .gap(px(4.))
        .px(px(20.))
        .py(px(12.))
        .border_b_1()
        .border_color(p.line)
        .children(chips)
}

/// A `.pc` cell of the given width; `None` takes the rest.
fn cell(width: Option<f32>) -> Div {
    let d = div()
        .px(px(12.))
        .overflow_hidden()
        .text_ellipsis()
        .whitespace_nowrap();
    match width {
        Some(w) => d.w(px(w)).flex_none(),
        None => d.flex_1().min_w_0(),
    }
}

fn grid(t: &SessionsTab, snap: &Snapshot, cx: &mut Context<SessionsTab>) -> impl IntoElement {
    let p = *cx.global::<Palette>();
    let (sort_col, desc) = t.sort;
    let redis = t.redis();
    let head = COLUMNS.iter().enumerate().map(|(i, (col, label))| {
        let col = *col;
        let label = match col {
            Col::WaitingOn if redis => "Address",
            Col::Process if redis => "Client",
            _ => *label,
        };
        cell(WIDTHS.get(i).copied())
            .flex()
            .items_center()
            .gap(px(4.))
            .child(label)
            .when(col == sort_col, |d| {
                d.child(icon(
                    if desc { Icon::SortDesc } else { Icon::SortAsc },
                    p.tx3,
                ))
            })
            .id(("sessions-head", i))
            .clickable(cx.listener(move |t, _, _, cx| t.set_sort(col, cx)))
    });
    let empty = t.visible.is_empty();
    let total = snap.sessions.len();
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .h(px(ROW))
                .bg(p.panel)
                .border_b_1()
                .border_color(p.line)
                .text_color(p.tx2)
                .text_size(small_size())
                .font_weight(FontWeight::MEDIUM)
                .children(head),
        )
        .when(empty, |d| {
            d.child(notice(
                if total == 0 {
                    "The server reports no sessions."
                } else {
                    "No sessions match this filter."
                },
                p.tx3,
            ))
        })
        .child(
            uniform_list(
                "sessions-rows",
                t.visible.len(),
                cx.processor(|t, range: Range<usize>, _, cx| {
                    range
                        .filter_map(|ix| {
                            let id = *t.visible.get(ix)?;
                            Some(row(t, id, cx))
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&t.list)
            .w_full()
            .flex_1(),
        )
}

fn row(t: &SessionsTab, id: SessionId, cx: &mut Context<SessionsTab>) -> AnyElement {
    let p = *cx.global::<Palette>();
    let Some(snap) = &t.snapshot else {
        return div().into_any_element();
    };
    let Some(s) = snap.session(id) else {
        return div().into_any_element();
    };
    let waiting = s.state == State::Waiting;
    let flagged = waiting || s.state == State::Blocked || blocks(snap, id);
    let selected = t.selected == Some(id);
    // The detail pane says which one is this screen's; a chip alone marks this app's.
    let app = if s.this_app {
        String::new()
    } else if s.own {
        "Deadlock".into()
    } else {
        application(s)
    };
    let statement = s.statement.as_deref().map(one_line).unwrap_or_default();
    let seconds = for_seconds(snap, s).map(duration).unwrap_or_default();
    let mut process = cell(Some(WIDTHS[0])).child(id.to_string());
    if waiting {
        process = process.pl(px(28.));
    }
    div()
        .id(("session", id))
        .w_full()
        .flex()
        .items_center()
        .h(px(ROW))
        .border_b_1()
        .border_color(p.line)
        .font_family(mono_font())
        .text_size(data_size())
        .when(selected, |d| d.bg(p.sel))
        .when(!selected, |d| d.hover(|s| s.bg(p.panel)))
        .clickable(cx.listener(move |t, _, _, cx| {
            t.selected = Some(id);
            cx.notify();
        }))
        .child(process)
        .child(cell(Some(WIDTHS[1])).child(SharedString::from(s.user.clone())))
        .child(
            cell(Some(WIDTHS[2]))
                .flex()
                .items_center()
                .gap(px(6.))
                .when(s.deadlock || s.own, |d| d.text_color(p.tx2))
                .child(SharedString::from(app))
                .when(s.this_app, |d| d.child(this_app_chip(&p))),
        )
        .child(
            cell(Some(WIDTHS[3]))
                .when(flagged, |d| {
                    d.text_color(p.warn).font_weight(FontWeight::MEDIUM)
                })
                .child(s.state.label()),
        )
        .child(
            cell(Some(WIDTHS[4])).child(SharedString::from(if t.redis() {
                s.host.clone()
            } else {
                snap.blockers(id)
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            })),
        )
        .child(
            cell(Some(WIDTHS[5]))
                .text_color(p.tx3)
                .child(SharedString::from(seconds)),
        )
        .child(cell(None).child(SharedString::from(statement)))
        .into_any_element()
}

/// Marks a session this app holds (the sidebar's, a tab's).
fn this_app_chip(p: &Palette) -> Div {
    div()
        .flex_none()
        .px(px(5.))
        .border_1()
        .border_color(p.line)
        .rounded(px(4.))
        .text_size(small_size())
        .text_color(p.tx2)
        .font_family(crate::theme::ui_font())
        .child("this app")
}

/// `.dl`: a 104 px term column and the value.
fn fact(term: &'static str, value: impl IntoElement, p: &Palette) -> Div {
    div()
        .flex()
        .gap(px(12.))
        .child(div().w(px(104.)).flex_none().text_color(p.tx3).child(term))
        .child(div().flex_1().min_w_0().child(value))
}

fn detail(t: &SessionsTab, snap: &Snapshot, cx: &mut Context<SessionsTab>) -> impl IntoElement {
    let p = *cx.global::<Palette>();
    let aside = div()
        .id("session-detail")
        .flex_none()
        .w(px(304.))
        .h_full()
        .overflow_y_scroll()
        .p(px(20.))
        .flex()
        .flex_col()
        .gap(px(20.))
        .border_l_1()
        .border_color(p.line);
    let Some(id) = t.selected else {
        return aside.child(
            div()
                .text_color(p.tx3)
                .child("Select a session to see its details."),
        );
    };
    let Some(s) = snap.session(id) else {
        return aside.child(
            div()
                .text_color(p.tx3)
                .child(SharedString::from(format!("Session {id} has ended."))),
        );
    };
    let blockers = snap.blockers(id);
    let waiters: Vec<SessionId> = snap
        .waits
        .iter()
        .filter(|w| w.blocker == Some(id))
        .map(|w| w.waiter)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let wait = snap.wait_of(id);
    let for_text = wait
        .and_then(|w| w.seconds)
        .map_or(String::new(), |n| format!(" for {}", duration(n)));
    let (status, warn) = match (blockers.as_slice(), wait) {
        ([], Some(_)) => (format!("Waiting on a lock{for_text}"), true),
        ([one], _) => (format!("Waiting on session {one}{for_text}"), true),
        (many, Some(_)) => (format!("Waiting on sessions {}{for_text}", ids(many)), true),
        _ if !waiters.is_empty() => (format!("Blocks session {}", ids(&waiters)), true),
        _ => (
            match s.seconds {
                Some(n) => format!("{} for {}", capital(s.state.label()), duration(n)),
                None => capital(s.state.label()),
            },
            false,
        ),
    };
    let text = |v: String| SharedString::from(v);
    let mut facts = vec![
        fact("User", text(s.user.clone()), &p),
        fact(
            "Application",
            text(match application(s) {
                a if a.is_empty() => "Not reported".into(),
                a => a,
            }),
            &p,
        ),
        fact(
            "Client",
            div()
                .font_family(mono_font())
                .text_size(data_size())
                .child(text(s.host.clone())),
            &p,
        ),
        fact(
            "Database",
            text(s.database.clone().unwrap_or_else(|| "None".into())),
            &p,
        ),
        fact(
            if t.redis() { "Flags" } else { "Server state" },
            text(s.detail.clone()),
            &p,
        ),
        fact(
            "Transaction",
            text(match (&s.transaction, snap.own_only) {
                (Some(tx), _) => match tx.seconds {
                    Some(n) => format!("Open since {}, {}", tx.started, duration(n)),
                    None => format!("Open since {}", tx.started),
                },
                (None, Some(p)) => format!("Not shown without {p}"),
                (None, None) => "None open".into(),
            }),
            &p,
        ),
    ];
    if let Some(w) = wait {
        facts.push(fact("Wants", text(w.wants.describe()), &p));
    }
    let holds: Vec<String> = s.holds.iter().map(|l| l.describe()).collect();
    // Redis clients hold no transactions or locks: their age instead.
    if let Some(age) = s.age {
        facts.truncate(5);
        facts.push(fact("Connected for", text(duration(age)), &p));
    }
    if s.age.is_none() {
        facts.push(fact(
            "Holds",
            if holds.is_empty() {
                div().child(match snap.own_only {
                    Some(p) => format!("Not shown without {p}"),
                    None => "No locks reported".into(),
                })
            } else {
                div().flex().flex_col().gap(px(4.)).children(
                    holds
                        .into_iter()
                        .map(|h| div().child(SharedString::from(h))),
                )
            },
            &p,
        ));
    }
    let statement_title = if s.running {
        "Statement"
    } else {
        "Last statement"
    };
    let statement = match &s.statement {
        Some(q) => div()
            .font_family(mono_font())
            .text_size(data_size())
            .px(px(12.))
            .py(px(8.))
            .border_1()
            .border_color(p.line)
            .rounded(RADIUS)
            .bg(p.panel)
            .line_height(px(20.))
            .child(selectable_sql("session-statement", q, &p)),
        None => div()
            .text_color(p.tx3)
            .child("The server does not report it."),
    };
    let can_cancel = s.running && !s.own;
    let note = match (blockers.first(), waiters.is_empty()) {
        (Some(&b), _) => Some((
            "Cancelling stops this statement and keeps the session. To let it through, end \
             the blocking session."
                .to_owned(),
            Some(b),
        )),
        (None, false) if s.transaction.is_some() => Some((
            format!(
                "Ending this session rolls back its open transaction and lets session {} \
                 through.",
                ids(&waiters)
            ),
            None,
        )),
        _ => None,
    };
    aside
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .text_size(px(15.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(text(format!("Session {id}"))),
                )
                .child(
                    div()
                        .when(warn, |d| {
                            d.text_color(p.warn).font_weight(FontWeight::MEDIUM)
                        })
                        .when(!warn, |d| d.text_color(p.tx2))
                        .child(text(status)),
                ),
        )
        .child(div().flex().flex_col().gap(px(8.)).children(facts))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(div().font_weight(FontWeight::MEDIUM).child(statement_title))
                .child(statement),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(8.))
                .child(
                    button(
                        "cancel-statement",
                        Kind::Outline,
                        can_cancel,
                        if s.own {
                            "This screen's own session"
                        } else if s.running {
                            "Stop the running statement (KILL QUERY); the session stays"
                        } else {
                            "No statement is running"
                        },
                        cx,
                    )
                    .child(if t.redis() {
                        "Unblock"
                    } else {
                        "Cancel statement"
                    })
                    .when(can_cancel, |b| {
                        b.clickable(cx.listener(move |t, _, window, cx| {
                            t.act(id, false, window, cx);
                        }))
                    }),
                )
                .child(
                    button(
                        "end-session",
                        Kind::Outline,
                        !s.own,
                        if s.own {
                            "This screen's own session cannot be ended here"
                        } else if s.this_app {
                            "One of this app's sessions (KILL); Deadlock reconnects it"
                        } else {
                            "Close the session (KILL); an open transaction rolls back"
                        },
                        cx,
                    )
                    .border_color(p.danger)
                    .text_color(p.danger)
                    .child("End session")
                    .when(!s.own, |b| {
                        b.clickable(cx.listener(move |t, _, window, cx| {
                            t.act(id, true, window, cx);
                        }))
                    }),
                ),
        )
        .when_some(note, |d, (note, show)| {
            d.child(
                div()
                    .pt(px(16.))
                    .border_t_1()
                    .border_color(p.line)
                    .text_color(p.tx2)
                    .line_height(relative(1.5))
                    .child(text(note))
                    .when_some(show, |d, b| {
                        d.child(
                            div()
                                .id("show-blocker")
                                .text_color(p.link)
                                .hover(|s| s.text_color(p.tx))
                                .child(text(format!("Show session {b}")))
                                .clickable(cx.listener(move |t, _, _, cx| t.select(b, cx))),
                        )
                    }),
            )
        })
}

/// Redis's slow log: newest first, with Reset.
fn slow_log(
    log: &[deadlock_core::sessions::SlowEntry],
    cx: &mut Context<SessionsTab>,
) -> AnyElement {
    let p = *cx.global::<Palette>();
    let head = div()
        .flex()
        .items_center()
        .gap(px(12.))
        .px(px(20.))
        .py(px(10.))
        .border_t_1()
        .border_color(p.line)
        .child(div().font_weight(FontWeight::SEMIBOLD).child("Slow log"))
        .child(div().text_color(p.tx3).child(match log.len() {
            0 => "No commands logged".to_owned(),
            1 => "1 command".to_owned(),
            n => format!("{n} commands, newest first"),
        }))
        .child(div().flex_1())
        .child(
            button(
                "reset-slowlog",
                Kind::Outline,
                !log.is_empty(),
                "Delete every slow-log entry (SLOWLOG RESET)",
                cx,
            )
            .child("Reset")
            .when(!log.is_empty(), |b| {
                b.clickable(cx.listener(|t, _, window, cx| t.reset_slow_log(window, cx)))
            }),
        );
    let line = |e: &deadlock_core::sessions::SlowEntry| {
        let at = chrono::DateTime::from_timestamp(e.at, 0)
            .map(|d| {
                d.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_default();
        let took = if e.micros >= 1000 {
            format!("{:.1} ms", e.micros as f64 / 1000.)
        } else {
            format!("{} µs", e.micros)
        };
        div()
            .flex()
            .items_center()
            .h(px(ROW))
            .border_b_1()
            .border_color(p.line)
            .font_family(mono_font())
            .text_size(data_size())
            .child(cell(Some(168.)).text_color(p.tx3).child(at))
            .child(cell(Some(96.)).child(took))
            .child(cell(Some(200.)).text_color(p.tx2).child(e.client.clone()))
            .child(cell(None).child(ellipsis(&e.command, 300)))
    };
    div()
        .id("slow-log")
        .flex_none()
        .max_h(px(ROW * 7. + 48.))
        .overflow_y_scroll()
        .child(head)
        .children(log.iter().map(line))
        .into_any_element()
}

fn deadlock_section(t: &SessionsTab, cx: &mut Context<SessionsTab>) -> impl IntoElement {
    let p = *cx.global::<Palette>();
    let d = &t.deadlock;
    let head = div()
        .id("deadlock-toggle")
        .flex()
        .items_center()
        .gap(px(6.))
        .h(px(36.))
        .px(px(16.))
        .font_weight(FontWeight::MEDIUM)
        .hover(|s| s.bg(p.hover))
        .clickable(cx.listener(|t, _, _, cx| t.toggle_deadlock(cx)))
        .tooltip(|window, cx| Tooltip::new("From SHOW ENGINE INNODB STATUS").build(window, cx))
        .child(icon(
            if d.open {
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
            },
            p.tx3,
        ))
        .child("Last detected deadlock")
        .when_some(
            d.loaded
                .as_ref()
                .and_then(|r| r.as_ref().ok())
                .and_then(|r| r.as_ref())
                .and_then(|(_, parsed)| parsed.as_ref()),
            |h, parsed| {
                h.child(
                    div()
                        .text_color(p.tx3)
                        .font_weight(FontWeight::NORMAL)
                        .child(SharedString::from(parsed.at.clone())),
                )
            },
        );
    let body = d.open.then(|| {
        let mono = |s: String| {
            div()
                .font_family(mono_font())
                .text_size(data_size())
                .child(SharedString::from(s))
        };
        let content: AnyElement = match &d.loaded {
            None => div().text_color(p.tx3).child("Loading…").into_any_element(),
            Some(Err((e, info))) => div()
                .text_color(if *info { p.tx3 } else { p.danger })
                .child(SharedString::from(e.clone()))
                .into_any_element(),
            Some(Ok(None)) => div()
                .text_color(p.tx3)
                .child("The server has detected no deadlock since it started.")
                .into_any_element(),
            Some(Ok(Some((raw, parsed)))) => {
                let raw_box = div()
                    .p(px(12.))
                    .border_1()
                    .border_color(p.line)
                    .rounded(RADIUS)
                    .bg(p.panel)
                    .child(mono(raw.clone()));
                match parsed {
                    None => div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(
                            div()
                                .text_color(p.tx2)
                                .child("The report could not be read; here is the server's text."),
                        )
                        .child(raw_box)
                        .into_any_element(),
                    Some(dl) => {
                        let txs = dl.transactions.iter().map(|tx| {
                            let rolled = dl.rolled_back == Some(tx.number);
                            let list = |title: &'static str, items: &[String]| {
                                fact(
                                    title,
                                    div().flex().flex_col().gap(px(4.)).children(
                                        items
                                            .iter()
                                            .map(|i| div().child(SharedString::from(i.clone()))),
                                    ),
                                    &p,
                                )
                            };
                            div()
                                .flex_1()
                                .min_w(px(280.))
                                .p(px(14.))
                                .border_1()
                                .border_color(if rolled { p.warn } else { p.line2 })
                                .rounded(px(8.))
                                .flex()
                                .flex_col()
                                .gap(px(8.))
                                .child(
                                    div()
                                        .flex()
                                        .gap(px(8.))
                                        .child(div().font_weight(FontWeight::SEMIBOLD).child(
                                            SharedString::from(match tx.thread {
                                                Some(n) => format!(
                                                    "Transaction {} · session {n}",
                                                    tx.number
                                                ),
                                                None => format!("Transaction {}", tx.number),
                                            }),
                                        ))
                                        .when(rolled, |d| {
                                            d.child(
                                                div()
                                                    .text_color(p.warn)
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .child("Rolled back"),
                                            )
                                        }),
                                )
                                .when(!tx.who.is_empty(), |d| {
                                    d.child(fact("User", SharedString::from(tx.who.clone()), &p))
                                })
                                .child(fact(
                                    "Statement",
                                    div().font_family(mono_font()).text_size(data_size()).child(
                                        selectable_sql(
                                            ("deadlock-statement", tx.number as usize),
                                            &tx.statement,
                                            &p,
                                        ),
                                    ),
                                    &p,
                                ))
                                .child(list("Held", &tx.holds))
                                .child(list("Waited for", &tx.waits_for))
                        });
                        let survivor = dl.rolled_back.map(|n| {
                            format!(
                                "The transactions waited on each other. The server rolled back \
                                 transaction {n}, so the other could go on."
                            )
                        });
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(12.))
                            .children(
                                survivor
                                    .map(|s| div().text_color(p.tx2).child(SharedString::from(s))),
                            )
                            .child(div().flex().flex_wrap().gap(px(12.)).children(txs))
                            .child(
                                div()
                                    .id("deadlock-raw")
                                    .text_color(p.link)
                                    .hover(|s| s.text_color(p.tx))
                                    .child(if d.raw {
                                        "Hide the server's text"
                                    } else {
                                        "Show the server's text"
                                    })
                                    .clickable(cx.listener(|t, _, _, cx| {
                                        t.deadlock.raw = !t.deadlock.raw;
                                        cx.notify();
                                    })),
                            )
                            .when(d.raw, |d| d.child(raw_box))
                            .into_any_element()
                    }
                }
            }
        };
        div()
            .id("deadlock-body")
            .max_h(px(360.))
            .overflow_y_scroll()
            .px(px(20.))
            .pb(px(16.))
            .child(content)
    });
    div()
        .flex_none()
        .flex()
        .flex_col()
        .border_t_1()
        .border_color(p.line)
        .child(head)
        .children(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadlock_core::sessions::{Lock, LockWait, Transaction};

    fn session(id: SessionId, state: State) -> Session {
        Session {
            id,
            user: "shop_app".into(),
            host: "10.0.3.17".into(),
            application: None,
            database: None,
            state,
            detail: String::new(),
            seconds: Some(134),
            statement: Some("update orders set total = 1250.00 where id = 48209".into()),
            running: true,
            transaction: Some(Transaction {
                started: "09:12:00".into(),
                seconds: Some(134),
            }),
            holds: Vec::new(),
            own: false,
            deadlock: false,
            this_app: false,
            age: None,
        }
    }

    fn wait(waiter: SessionId, blocker: SessionId, kind: LockKind) -> LockWait {
        LockWait {
            waiter,
            blocker: Some(blocker),
            wants: Lock {
                kind,
                mode: "exclusive".into(),
                schema: "shop".into(),
                table: "orders".into(),
                index: Some("PRIMARY".into()),
                row: Some("48209".into()),
                rows: 1,
            },
            seconds: Some(48),
        }
    }

    #[test]
    fn the_banner_explains_a_row_lock_wait() {
        let mut snap = Snapshot {
            sessions: vec![
                session(4790, State::IdleInTransaction),
                session(4812, State::Active),
            ],
            waits: vec![wait(4812, 4790, LockKind::Row)],
            ..Snapshot::default()
        };
        snap.mark_waiting();
        let (title, text) = banner_text(&snap, &snap.chains()[0]);
        assert_eq!(title, "1 session is waiting on a lock");
        assert_eq!(
            text,
            "This is a lock wait, not a deadlock. Session 4790 holds the row and is idle \
             inside an open transaction, so 4812 waits until 4790 commits, rolls back or is \
             ended."
        );
    }

    #[test]
    fn the_banner_walks_a_longer_chain_and_names_a_cycle() {
        let snap = Snapshot {
            sessions: vec![
                session(1, State::Active),
                session(2, State::Waiting),
                session(3, State::Waiting),
            ],
            waits: vec![
                wait(2, 1, LockKind::Metadata),
                wait(3, 2, LockKind::Metadata),
            ],
            ..Snapshot::default()
        };
        let (title, text) = banner_text(&snap, &snap.chains()[0]);
        assert_eq!(title, "2 sessions are waiting on a lock");
        assert!(text.contains("Session 1 holds a metadata lock on table orders while it runs"));
        assert!(text.ends_with(" 3 in turn waits on 2."), "{text}");

        let cycle = Snapshot {
            waits: vec![wait(5, 6, LockKind::Row), wait(6, 5, LockKind::Row)],
            ..Snapshot::default()
        };
        let (_, text) = banner_text(&cycle, &cycle.chains()[0]);
        assert!(text.contains("this is a deadlock"), "{text}");
    }

    #[test]
    fn ending_an_open_transaction_says_it_rolls_back() {
        let mut c = ConnectionConfig::new(deadlock_core::connection::Engine::Mysql);
        c.name = "shop_prod".into();
        c.environment = Environment::Production;
        c.read_only = true;
        let s = session(4790, State::IdleInTransaction);
        let (q, detail, label) = confirmation(&c, &s, true);
        assert_eq!(q, "End session 4790 on production connection shop_prod?");
        assert_eq!(label, "End session");
        assert!(detail.contains("User: shop_app"));
        assert!(detail.contains("update orders set total"));
        assert!(detail.contains("rolled back"));
        assert!(detail.contains("changes no data"));
        let (q, detail, _) = confirmation(&c, &s, false);
        assert!(q.starts_with("Cancel the statement of session 4790"));
        assert!(detail.contains("transaction stay"));
    }
}
