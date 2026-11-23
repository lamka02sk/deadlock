//! The Sessions and locks tabs, the sidebar row under each connection, and the light
//! periodic check behind the "waiting on a lock" indicators.

use std::sync::Arc;

use deadlock_core::schema::{Row, RowKind};
use gpui_kit::{AppContext as _, Context, Entity, Task, Window};

use super::{Conn, Front, Shell};
use crate::app::Db;
use crate::sessions::{REFRESH, SessionsEvent, SessionsTab};

/// The sidebar row's id under its connection. No database name holds a NUL.
pub(super) const ROW_NAME: &str = "\0sessions";

/// The Server configuration row's id under its connection.
pub(super) const CONFIG_ROW: &str = "\0config";

/// A screen row (Sessions and locks, Server configuration), not a tree node.
pub(super) fn is_screen_row(id: &[String]) -> bool {
    id.len() == 2 && (id[1] == ROW_NAME || id[1] == CONFIG_ROW)
        || id.len() == 3 && id[2] == super::diagram::ROW_NAME
}

impl Shell {
    /// Every few seconds, while the window is open: how many sessions wait on a lock,
    /// for each connected connection that has a sessions screen.
    pub(super) fn watch_waits(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH).await;
                let Ok((rt, drivers)) = this.update(cx, |t, cx| {
                    let drivers: Vec<_> = t
                        .conns
                        .iter()
                        .filter_map(|(id, c)| match c {
                            Conn::Connected(d) if d.capabilities().sessions => {
                                Some((id.clone(), Arc::clone(d)))
                            }
                            _ => None,
                        })
                        .collect();
                    (Arc::clone(&cx.global::<Db>().0), drivers)
                }) else {
                    return;
                };
                for (id, d) in drivers {
                    let n = rt.spawn(async move { d.waiting_sessions().await }).await;
                    // A failed check shows nothing rather than a stale count.
                    let n = match n {
                        Ok(Ok(n)) => n,
                        _ => 0,
                    };
                    if this.update(cx, |t, cx| t.set_waiting(&id, n, cx)).is_err() {
                        return;
                    }
                }
            }
        })
    }

    fn set_waiting(&mut self, id: &str, n: usize, cx: &mut Context<Self>) {
        if self.waiting.get(id).copied().unwrap_or(0) != n {
            self.waiting.insert(id.to_owned(), n);
            self.rebuild(cx);
        }
    }

    /// Waiting sessions on connection `id`, while it is connected.
    pub(super) fn waiting_on(&self, id: &str) -> usize {
        match self.conns.get(id) {
            Some(Conn::Connected(_)) => self.waiting.get(id).copied().unwrap_or(0),
            _ => 0,
        }
    }

    /// Puts "Server configuration" and "Sessions and locks" rows after the databases
    /// of each open, connected connection with those screens. Not while searching.
    pub(super) fn add_sessions_rows(&self, rows: &mut Vec<Row>) {
        let screen = |conn: &str, name: &str, kind: RowKind, label: &str, detail: String| Row {
            id: vec![conn.to_owned(), name.to_owned()],
            kind,
            label: label.into(),
            detail,
            hits: Vec::new(),
            expandable: false,
            expanded: false,
            dim: false,
            primary_key: false,
            foreign_key: false,
        };
        let mut i = 0;
        while i < rows.len() {
            let r = &rows[i];
            let conn = r.id[0].clone();
            let caps = match self.conns.get(&conn) {
                Some(Conn::Connected(d)) if r.kind == RowKind::Connection && r.expanded => {
                    d.capabilities()
                }
                _ => {
                    i += 1;
                    continue;
                }
            };
            i += 1;
            while i < rows.len() && rows[i].id[0] == conn {
                i += 1;
            }
            if caps.config {
                let row = screen(
                    &conn,
                    CONFIG_ROW,
                    RowKind::Config,
                    "Server configuration",
                    String::new(),
                );
                rows.insert(i, row);
                i += 1;
            }
            if caps.sessions {
                let n = self.waiting_on(&conn);
                let detail = if n > 0 {
                    format!("{n} waiting")
                } else {
                    String::new()
                };
                let label = if self.is_redis(&conn) {
                    "Clients and slow log"
                } else {
                    "Sessions and locks"
                };
                let row = screen(&conn, ROW_NAME, RowKind::Sessions, label, detail);
                rows.insert(i, row);
                i += 1;
            }
        }
    }

    /// Opens connection `id`'s Sessions and locks tab, or brings it forward. It opens
    /// its own server session, with the password this window already has.
    pub(super) fn open_sessions(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(i) = self
            .sessions
            .iter()
            .position(|(t, _)| t.read(cx).connection.id == id)
        {
            self.show_tab(Front::Sessions(i), window, cx);
            return;
        }
        let (Some(config), Some(password)) = (self.config(id).cloned(), self.passwords.get(id))
        else {
            return;
        };
        let password = password.clone();
        let tab = cx.new(|cx| SessionsTab::new(config, password, cx));
        let sub = cx.subscribe(&tab, Self::on_sessions_event);
        self.sessions.push((tab, sub));
        self.show_tab(Front::Sessions(self.sessions.len() - 1), window, cx);
    }

    fn on_sessions_event(
        &mut self,
        tab: Entity<SessionsTab>,
        e: &SessionsEvent,
        cx: &mut Context<Self>,
    ) {
        let SessionsEvent::Waiting(n) = e;
        let id = tab.read(cx).connection.id.clone();
        self.set_waiting(&id, *n, cx);
    }

    pub(super) fn close_sessions(&mut self, i: usize, cx: &mut Context<Self>) {
        if i >= self.sessions.len() {
            return;
        }
        drop(self.sessions.remove(i));
        self.front = match self.front {
            Front::Sessions(f) if f == i => match (self.sessions.len(), self.tabs.len()) {
                (0, 0) => Front::None,
                (0, n) => Front::Query(n - 1),
                (n, _) => Front::Sessions(i.min(n - 1)),
            },
            Front::Sessions(f) if f > i => Front::Sessions(f - 1),
            other => other,
        };
        self.follow_front(cx);
    }
}
