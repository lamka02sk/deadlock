//! The Redis key browser in the sidebar: Load more, opening keys, the server-side
//! search, New key, and deleting keys or a whole namespace in batches.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use deadlock_core::connection::{Engine, Environment};
use deadlock_core::driver::{Driver, NodeKind, ObjectPath};
use deadlock_core::edit::TableRef;
use deadlock_core::redis::{self as rk, Command, KeyType};
use deadlock_core::redis_driver::SearchEnd;
use deadlock_core::schema::{NodeId, Row, RowKind};
use gpui_kit::component::input::{self, Input, InputEvent, InputState};
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, ClipboardItem, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, StatefulInteractiveElement as _, Styled, Subscription, Window, div, px,
};

use super::{Conn, Shell};
use crate::app::Db;
use crate::query::View;
use crate::theme::{MONO_FONT, Palette, small_size};
use crate::ui::{Kind, button, labelled, modal};

/// Keys per `UNLINK` when deleting many.
const DELETE_BATCH: usize = 1000;

/// The key browser's state in the window.
#[derive(Default)]
pub(super) struct Keys {
    /// Key rows picked with Ctrl+click, for Delete keys.
    pub marked: BTreeSet<NodeId>,
    /// The server-side search, per connection.
    pub scans: HashMap<String, Scan>,
    /// A namespace count or delete running in batches.
    pub job: Option<Job>,
    pub new_key: Option<NewKey>,
}

/// A search match: database name, key, type.
type Found = (String, Vec<u8>, KeyType);

/// A context-menu action.
type MenuAction = Box<dyn Fn(&mut Shell, &mut Window, &mut Context<Shell>)>;

pub(super) struct Scan {
    text: String,
    /// The matches, in the order found.
    found: Vec<Found>,
    running: bool,
    end: Option<SearchEnd>,
    error: Option<String>,
    stop: Arc<AtomicBool>,
}

pub(super) struct Job {
    what: String,
    done: usize,
    total: Option<usize>,
    stop: Arc<AtomicBool>,
}

pub(super) struct NewKey {
    conn: String,
    db: String,
    kind: KeyType,
    json: bool,
    name: Entity<InputState>,
    first: Entity<InputState>,
    second: Entity<InputState>,
    error: Option<String>,
    fresh: bool,
    _subs: Vec<Subscription>,
}

/// `db3` → 3.
fn db_index(name: &str) -> u32 {
    name.strip_prefix("db")
        .and_then(|d| d.parse().ok())
        .unwrap_or(0)
}

/// Shows up to ten keys of a command for a confirmation.
fn command_preview(name: &str, keys: &[Vec<u8>]) -> String {
    let shown: Vec<String> = keys.iter().take(10).map(|k| rk::quote(k)).collect();
    let more = keys.len().saturating_sub(10);
    if more > 0 {
        format!(
            "{name} {} … and {} more",
            shown.join(" "),
            rk::grouped(more as u64)
        )
    } else {
        format!("{name} {}", shown.join(" "))
    }
}

impl Shell {
    pub(super) fn is_redis(&self, conn: &str) -> bool {
        self.config(conn).is_some_and(|c| c.engine == Engine::Redis)
    }

    fn redis_driver(&self, conn: &str) -> Option<Arc<dyn Driver>> {
        match self.conns.get(conn) {
            Some(Conn::Connected(d)) if d.as_redis().is_some() => Some(Arc::clone(d)),
            _ => None,
        }
    }

    /// Reloads database `db_id` (`[conn, db]`) and its open namespaces.
    fn reload_db(&mut self, db_id: &[String], cx: &mut Context<Self>) {
        let open: Vec<NodeId> = self
            .tree
            .expanded
            .iter()
            .filter(|e| e.len() > 2 && e.starts_with(db_id))
            .cloned()
            .collect();
        self.load(db_id.to_vec(), cx);
        for id in open {
            self.load(id, cx);
        }
    }

    /// A click on a "Load more" row: the browser scans further or shows more of a
    /// namespace, then the database's rows reload.
    pub(super) fn load_more(&mut self, id: NodeId, cx: &mut Context<Self>) {
        let Some(d) = self.redis_driver(&id[0]) else {
            return;
        };
        let path = ObjectPath(id[1..].to_vec());
        let rx = cx
            .global::<Db>()
            .0
            .spawn(async move { d.children(&path).await });
        let db_id = id[..2].to_vec();
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(Ok(_)) => this.reload_db(&db_id, cx),
                Ok(Err(e)) => {
                    this.tree
                        .status
                        .insert(db_id, deadlock_core::schema::Load::Failed(e.to_string()));
                    this.rebuild(cx);
                }
                Err(_) => {}
            });
        })
        .detach();
    }

    /// Opens the key of row `id` (`[conn, db, …, key]`) in its tab.
    pub(super) fn open_key(&mut self, id: &[String], window: &mut Window, cx: &mut Context<Self>) {
        let (Some(conn), Some(db), Some(key)) = (id.first(), id.get(1), id.last()) else {
            return;
        };
        let table = TableRef {
            database: db.clone(),
            table: key.clone(),
        };
        let conn = conn.clone();
        self.open_table(&conn, table, View::Data, window, cx);
    }

    /// A key tab deleted or renamed keys in database `db`: the browser forgets the old
    /// names, learns the new one, and the database's rows reload.
    pub(super) fn keys_changed(
        &mut self,
        conn: &str,
        db: &str,
        removed: Vec<Vec<u8>>,
        added: Option<(Vec<u8>, KeyType)>,
        cx: &mut Context<Self>,
    ) {
        let Some(d) = self.redis_driver(conn) else {
            return;
        };
        let n = db_index(db);
        let rx = cx.global::<Db>().0.spawn(async move {
            if let Some(r) = d.as_redis() {
                r.forget(n, &removed, None).await;
                if let Some((k, t)) = added {
                    r.remember(n, k, t).await;
                }
            }
        });
        let db_id = vec![conn.to_owned(), db.to_owned()];
        cx.spawn(async move |this, cx| {
            let _ = rx.await;
            let _ = this.update(cx, |this, cx| this.reload_db(&db_id, cx));
        })
        .detach();
    }

    /// Ctrl+click on a key row: adds it to or drops it from the picked keys.
    pub(super) fn toggle_mark(&mut self, id: NodeId, cx: &mut Context<Self>) {
        if !self.keys.marked.remove(&id) {
            // Picks stay within one database.
            self.keys.marked.retain(|m| m[..2] == id[..2]);
            self.keys.marked.insert(id);
        }
        cx.notify();
    }

    /// The search text changed: restarts the server-side search of each connected
    /// Redis connection, after a pause in typing.
    pub(super) fn search_keys(&mut self, text: &str, cx: &mut Context<Self>) {
        let conns: Vec<String> = self
            .tree
            .connections
            .iter()
            .map(|c| c.id.clone())
            .filter(|c| self.redis_driver(c).is_some())
            .collect();
        for conn in conns {
            if self.keys.scans.get(&conn).is_some_and(|s| s.text == text) {
                continue;
            }
            if let Some(old) = self.keys.scans.remove(&conn) {
                old.stop.store(true, Ordering::Relaxed);
            }
            if text.trim().is_empty() {
                continue;
            }
            let stop = Arc::new(AtomicBool::new(false));
            self.keys.scans.insert(
                conn.clone(),
                Scan {
                    text: text.to_owned(),
                    found: Vec::new(),
                    running: true,
                    end: None,
                    error: None,
                    stop: Arc::clone(&stop),
                },
            );
            let dbs: Vec<String> = self
                .tree
                .children(std::slice::from_ref(&conn))
                .unwrap_or_default()
                .iter()
                .filter(|n| n.kind == NodeKind::Database)
                .map(|n| n.name.clone())
                .collect();
            self.run_search(conn, text.to_owned(), dbs, stop, cx);
        }
    }

    fn run_search(
        &mut self,
        conn: String,
        text: String,
        dbs: Vec<String>,
        stop: Arc<AtomicBool>,
        cx: &mut Context<Self>,
    ) {
        let Some(d) = self.redis_driver(&conn) else {
            return;
        };
        let rt = Arc::clone(&cx.global::<Db>().0);
        cx.spawn(async move |this, cx| {
            // A pause in typing first: each keystroke would start a scan otherwise.
            cx.background_executor()
                .timer(std::time::Duration::from_millis(300))
                .await;
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let pattern = rk::search_pattern(&text);
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let flag = Arc::clone(&stop);
            let done = rt.spawn(async move {
                let Some(r) = d.as_redis() else {
                    return Ok(SearchEnd::Complete);
                };
                for db in dbs {
                    let end = r.search(db_index(&db), &pattern, &flag, &tx).await?;
                    if end != SearchEnd::Complete {
                        return Ok(end);
                    }
                }
                Ok::<_, deadlock_core::driver::DriverError>(SearchEnd::Complete)
            });
            let mut batch = Vec::new();
            while rx.recv_many(&mut batch, 256).await > 0 {
                let items = std::mem::take(&mut batch);
                let current = this.update(cx, |this, cx| {
                    let Some(s) = this.keys.scans.get_mut(&conn).filter(|s| s.text == text) else {
                        return false;
                    };
                    s.found.extend(
                        items
                            .into_iter()
                            .map(|(db, k, t)| (format!("db{db}"), k, t)),
                    );
                    this.rebuild(cx);
                    true
                });
                if !matches!(current, Ok(true)) {
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
            }
            let end = done.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(s) = this.keys.scans.get_mut(&conn).filter(|s| s.text == text) {
                    s.running = false;
                    match end {
                        Ok(Ok(e)) => s.end = Some(e),
                        Ok(Err(e)) => s.error = Some(e.to_string()),
                        Err(_) => s.error = Some("the search task stopped".into()),
                    }
                    this.rebuild(cx);
                }
            });
        })
        .detach();
    }

    /// Stop on the search line: every running search ends where it is.
    fn stop_searches(&mut self, cx: &mut Context<Self>) {
        for s in self.keys.scans.values() {
            s.stop.store(true, Ordering::Relaxed);
        }
        cx.notify();
    }

    /// Ends every search (the search field closed).
    pub(super) fn clear_key_searches(&mut self) {
        for (_, s) in self.keys.scans.drain() {
            s.stop.store(true, Ordering::Relaxed);
        }
    }

    /// While searching, each Redis connection shows the keys its server-side search
    /// found under their databases, instead of the loaded tree filtered.
    pub(super) fn search_rows(&self, rows: &mut Vec<Row>, found: &mut usize, text: &str) {
        for (conn, scan) in &self.keys.scans {
            let Some(start) = rows
                .iter()
                .position(|r| r.kind == RowKind::Connection && r.id[0] == *conn)
            else {
                continue;
            };
            let end = rows[start + 1..]
                .iter()
                .position(|r| r.id[0] != *conn)
                .map_or(rows.len(), |e| start + 1 + e);
            *found -= rows[start + 1..end]
                .iter()
                .filter(|r| !r.hits.is_empty())
                .count();
            let mut head = rows[start].clone();
            head.expanded = true;
            head.expandable = true;
            head.dim = scan.found.is_empty() && !scan.running;
            head.detail = if head.dim {
                "no matches".into()
            } else {
                String::new()
            };
            let mut out = vec![head];
            let mut by_db: BTreeMap<&str, Vec<&Found>> = BTreeMap::new();
            for f in &scan.found {
                by_db.entry(f.0.as_str()).or_default().push(f);
            }
            let row = |id: NodeId, kind, label: String, detail: String| Row {
                id,
                kind,
                label,
                detail,
                hits: Vec::new(),
                expandable: false,
                expanded: false,
                dim: false,
                primary_key: false,
                foreign_key: false,
            };
            for (db, keys) in by_db {
                let mut d = row(
                    vec![conn.clone(), db.to_owned()],
                    RowKind::Node(NodeKind::Database),
                    db.to_owned(),
                    format!("{} found", rk::grouped(keys.len() as u64)),
                );
                d.expandable = true;
                d.expanded = true;
                out.push(d);
                for (_, key, kind) in keys {
                    let label = rk::key_label(key);
                    let mut r = row(
                        vec![conn.clone(), db.to_owned(), label.clone()],
                        RowKind::Node(NodeKind::Key),
                        label.clone(),
                        kind.label().to_owned(),
                    );
                    r.hits = rk::search_hits(&label, text);
                    out.push(r);
                }
            }
            *found += scan.found.len();
            rows.splice(start..end, out);
        }
    }

    /// Right click on a Redis row: New key, Delete namespace, Delete key(s).
    pub(super) fn key_menu(
        &self,
        row: &Row,
        menu: gpui_kit::component::menu::PopupMenu,
        shell: Entity<Shell>,
    ) -> gpui_kit::component::menu::PopupMenu {
        let read_only = self.config(&row.id[0]).is_some_and(|c| c.read_only);
        let item = |label: String, f: MenuAction| {
            let shell = shell.clone();
            PopupMenuItem::new(label).on_click(move |_, window, cx| {
                shell.update(cx, |s, cx| f(s, window, cx));
            })
        };
        let id = row.id.clone();
        match row.kind {
            RowKind::Node(NodeKind::Database | NodeKind::Namespace) if !read_only => {
                let prefix = if row.kind == RowKind::Node(NodeKind::Namespace) {
                    row.label.clone()
                } else {
                    String::new()
                };
                let new_label = if prefix.is_empty() {
                    "New key…".to_owned()
                } else {
                    format!("New key in {prefix}…")
                };
                let (i, p) = (id.clone(), prefix.clone());
                let menu = menu.item(item(
                    new_label,
                    Box::new(move |s, w, cx| s.new_key(&i[..2], &p, w, cx)),
                ));
                if prefix.is_empty() {
                    menu
                } else {
                    menu.item(item(
                        format!("Delete namespace {prefix}…"),
                        Box::new(move |s, _, cx| s.delete_namespace(id.clone(), cx)),
                    ))
                }
            }
            RowKind::Node(NodeKind::Key) => {
                let (i, label) = (id.clone(), row.label.clone());
                let menu = menu
                    .item(item(
                        "Open".into(),
                        Box::new(move |s, w, cx| s.open_key(&i, w, cx)),
                    ))
                    .item(item(
                        "Copy name".into(),
                        Box::new(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(label.clone()));
                        }),
                    ));
                if read_only {
                    return menu;
                }
                let picked: Vec<NodeId> = if self.keys.marked.contains(&id) {
                    self.keys.marked.iter().cloned().collect()
                } else {
                    vec![id]
                };
                let label = if picked.len() == 1 {
                    "Delete key…".to_owned()
                } else {
                    format!("Delete {} keys…", picked.len())
                };
                menu.separator().item(item(
                    label,
                    Box::new(move |s, _, cx| s.delete_keys(picked.clone(), cx)),
                ))
            }
            _ => menu,
        }
    }

    fn is_production(&self, conn: &str) -> bool {
        self.config(conn)
            .is_some_and(|c| c.environment == Environment::Production)
    }

    /// The question and detail of a write confirmation, with the production warning.
    fn write_question(&self, conn: &str, question: String) -> String {
        match self.config(conn) {
            Some(c) if c.environment == Environment::Production => {
                format!("{question} (production connection {})", c.name)
            }
            _ => question,
        }
    }

    /// Asks, then `UNLINK`s the keys of rows `ids` (one database).
    fn delete_keys(&mut self, ids: Vec<NodeId>, cx: &mut Context<Self>) {
        let Some(first) = ids.first().cloned() else {
            return;
        };
        let Some(d) = self.redis_driver(&first[0]) else {
            return;
        };
        let db_id = first[..2].to_vec();
        let keys: Vec<Vec<u8>> = ids
            .iter()
            .filter_map(|i| i.last())
            .map(|l| rk::label_key(l))
            .collect();
        let n = keys.len();
        let question = self.write_question(
            &first[0],
            if n == 1 {
                format!("Delete key {}?", ids[0].last().cloned().unwrap_or_default())
            } else {
                format!("Delete {n} keys?")
            },
        );
        let preview = command_preview("UNLINK", &keys);
        let production = self.is_production(&first[0]);
        let window = self.window;
        let rt = Arc::clone(&cx.global::<Db>().0);
        cx.spawn(async move |this, cx| {
            let answer = cx.update_window(window, |_, window, cx| {
                crate::dialog::ask(
                    window,
                    cx,
                    crate::dialog::Ask::new(question, &["Delete", "Cancel"])
                        .text("This cannot be undone.")
                        .statement(preview)
                        .production(production)
                        .danger(true),
                )
            });
            let Ok(answer) = answer else { return };
            if answer.await != Some(0) {
                return;
            }
            let db = db_index(&db_id[1]);
            let result = rt
                .spawn(async move {
                    let Some(r) = d.as_redis() else {
                        return Ok(());
                    };
                    for chunk in keys.chunks(DELETE_BATCH) {
                        let mut c = Command::new("UNLINK");
                        for k in chunk {
                            c = c.arg(k);
                        }
                        r.run_in(db, &[c]).await?;
                    }
                    r.forget(db, &keys, None).await;
                    Ok::<_, deadlock_core::driver::DriverError>(())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.keys.marked.clear();
                if let Ok(Err(e)) = result {
                    this.tree.status.insert(
                        db_id.clone(),
                        deadlock_core::schema::Load::Failed(e.to_string()),
                    );
                }
                this.reload_db(&db_id, cx);
            });
        })
        .detach();
    }

    /// Counts the keys under namespace row `id`, asks with the count, then deletes them
    /// with `SCAN MATCH` and `UNLINK` in batches, with progress and Cancel.
    fn delete_namespace(&mut self, id: NodeId, cx: &mut Context<Self>) {
        if self.keys.job.is_some() {
            return;
        }
        let Some(d) = self.redis_driver(&id[0]) else {
            return;
        };
        let Some(label) = id.last().cloned() else {
            return;
        };
        let prefix = rk::label_key(&label);
        let pattern = rk::prefix_pattern(&prefix);
        let db_id = id[..2].to_vec();
        let db = db_index(&db_id[1]);
        let stop = Arc::new(AtomicBool::new(false));
        self.keys.job = Some(Job {
            what: format!("Counting keys in {label}"),
            done: 0,
            total: None,
            stop: Arc::clone(&stop),
        });
        cx.notify();
        let rt = Arc::clone(&cx.global::<Db>().0);
        let window = self.window;
        let conn = id[0].clone();
        cx.spawn(async move |this, cx| {
            // One pass of SCAN steps, each on the runtime; `delete` unlinks what it finds.
            let pass = async |delete: bool,
                              this: &gpui_kit::WeakEntity<Shell>,
                              cx: &mut gpui_kit::AsyncApp| {
                let mut cursor = 0;
                let mut n = 0usize;
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return Ok(None);
                    }
                    let (d, pattern) = (Arc::clone(&d), pattern.clone());
                    let step = rt
                        .spawn(async move {
                            let Some(r) = d.as_redis() else {
                                return Ok((0, 0));
                            };
                            let (next, keys) = r.scan_keys(db, cursor, &pattern).await?;
                            if delete && !keys.is_empty() {
                                let mut c = Command::new("UNLINK");
                                for k in &keys {
                                    c = c.arg(k);
                                }
                                r.run_in(db, &[c]).await?;
                            }
                            Ok::<_, deadlock_core::driver::DriverError>((next, keys.len()))
                        })
                        .await
                        .map_err(|_| "the task stopped".to_owned())?
                        .map_err(|e| e.to_string())?;
                    n += step.1;
                    cursor = step.0;
                    let _ = this.update(cx, |this, cx| {
                        if let Some(j) = &mut this.keys.job {
                            j.done = n;
                        }
                        cx.notify();
                    });
                    if cursor == 0 {
                        return Ok::<_, String>(Some(n));
                    }
                }
            };
            let counted = pass(false, &this, cx).await;
            let total = match counted {
                Ok(Some(t)) => t,
                Ok(None) | Err(_) => {
                    let _ = this.update(cx, |this, cx| {
                        this.keys.job = None;
                        if let Err(e) = counted {
                            this.tree
                                .status
                                .insert(db_id.clone(), deadlock_core::schema::Load::Failed(e));
                            this.rebuild(cx);
                        }
                        cx.notify();
                    });
                    return;
                }
            };
            let question = this
                .read_with(cx, |this, _| {
                    this.write_question(
                        &conn,
                        format!("Delete {} keys in {label}?", rk::grouped(total as u64)),
                    )
                })
                .unwrap_or_default();
            let detail = format!(
                "SCAN MATCH {} with UNLINK in batches of {SCAN}. Keys created under {label} \
                 meanwhile are deleted too. This cannot be undone.",
                rk::quote(&pattern),
                SCAN = rk::grouped(deadlock_core::redis_driver::SCAN_BATCH as u64)
            );
            let production = this
                .read_with(cx, |this, _| this.is_production(&conn))
                .unwrap_or(false);
            let answer = cx.update_window(window, |_, window, cx| {
                crate::dialog::ask(
                    window,
                    cx,
                    crate::dialog::Ask::new(question, &["Delete", "Cancel"])
                        .text(detail)
                        .production(production)
                        .danger(true),
                )
            });
            let go = match answer {
                Ok(a) => total > 0 && a.await == Some(0),
                Err(_) => false,
            };
            if go {
                let _ = this.update(cx, |this, cx| {
                    if let Some(j) = &mut this.keys.job {
                        j.what = format!("Deleting {label}");
                        j.done = 0;
                        j.total = Some(total);
                    }
                    cx.notify();
                });
                let deleted = pass(true, &this, cx).await;
                let d2 = Arc::clone(&d);
                let p2 = prefix.clone();
                let _ = rt
                    .spawn(async move {
                        if let Some(r) = d2.as_redis() {
                            r.forget(db, &[], Some(&p2)).await;
                        }
                    })
                    .await;
                if let Err(e) = deleted {
                    let _ = this.update(cx, |this, _| {
                        this.tree
                            .status
                            .insert(db_id.clone(), deadlock_core::schema::Load::Failed(e));
                    });
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.keys.job = None;
                if go {
                    this.reload_db(&db_id, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Opens the New key dialog for database `db_id` (`[conn, db]`), the name
    /// starting with `prefix`.
    fn new_key(
        &mut self,
        db_id: &[String],
        prefix: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = |window: &mut Window, cx: &mut Context<Self>, value: &str| {
            let value = value.to_owned();
            cx.new(|cx| InputState::new(window, cx).default_value(value))
        };
        let name = input(window, cx, prefix);
        let first = input(window, cx, "");
        let second = input(window, cx, "");
        let subs = [&name, &first, &second]
            .into_iter()
            .map(|s| {
                cx.subscribe_in(s, window, |this, _, e: &InputEvent, window, cx| {
                    if matches!(e, InputEvent::PressEnter { .. }) {
                        this.create_key(window, cx);
                    }
                })
            })
            .collect();
        self.keys.new_key = Some(NewKey {
            conn: db_id[0].clone(),
            db: db_id[1].clone(),
            kind: KeyType::String,
            json: false,
            name,
            first,
            second,
            error: None,
            fresh: true,
            _subs: subs,
        });
        // JSON is offered only where the module is loaded.
        if let Some(d) = self.redis_driver(&db_id[0]) {
            let rx = cx.global::<Db>().0.spawn(async move {
                match d.as_redis() {
                    Some(r) => r.has_json().await.unwrap_or(false),
                    None => false,
                }
            });
            cx.spawn(async move |this, cx| {
                let json = rx.await.unwrap_or(false);
                let _ = this.update(cx, |this, cx| {
                    if let Some(n) = &mut this.keys.new_key {
                        n.json = json;
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }

    /// Create in the New key dialog: checks the name is free, confirms on production,
    /// writes the first value and opens the key.
    fn create_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(n) = &self.keys.new_key else { return };
        let name = n.name.read(cx).value().to_string();
        let (a, b) = (
            n.first.read(cx).value().to_string(),
            n.second.read(cx).value().to_string(),
        );
        let (conn, db_name, kind) = (n.conn.clone(), n.db.clone(), n.kind.clone());
        let key = rk::label_key(&name);
        let fail = |this: &mut Self, e: String, cx: &mut Context<Self>| {
            if let Some(n) = &mut this.keys.new_key {
                n.error = Some(e);
            }
            cx.notify();
        };
        if name.is_empty() {
            return fail(self, "Enter the key's name.".into(), cx);
        }
        let command = match rk::new_key(&key, &kind, &a, &b) {
            Ok(c) => c,
            Err(e) => return fail(self, e, cx),
        };
        let Some(d) = self.redis_driver(&conn) else {
            return fail(self, "The connection is not open.".into(), cx);
        };
        let production = self
            .config(&conn)
            .is_some_and(|c| c.environment == Environment::Production);
        let question = self.write_question(&conn, format!("Create key {name}?"));
        let text = command.text();
        let db = db_index(&db_name);
        let rt = Arc::clone(&cx.global::<Db>().0);
        let handle = self.window;
        let _ = window;
        cx.spawn(async move |this, cx| {
            let (d2, k2) = (Arc::clone(&d), key.clone());
            let exists = rt
                .spawn(async move { d2.as_redis()?.exists_in(db, &k2).await.ok() })
                .await
                .ok()
                .flatten();
            if exists == Some(true) {
                let _ = this.update(cx, |this, cx| {
                    fail(this, format!("A key named {name} already exists."), cx);
                });
                return;
            }
            if production {
                let answer = cx.update_window(handle, |_, window, cx| {
                    crate::dialog::ask(
                        window,
                        cx,
                        crate::dialog::Ask::new(question, &["Create", "Cancel"])
                            .statement(text)
                            .production(true),
                    )
                });
                let Ok(answer) = answer else { return };
                if answer.await != Some(0) {
                    return;
                }
            }
            let k3 = key.clone();
            let result = rt
                .spawn(async move {
                    let r = d.as_redis().ok_or("not a Redis connection".to_owned())?;
                    r.run_in(db, &[command]).await.map_err(|e| e.to_string())?;
                    r.remember(db, k3, kind).await;
                    Ok::<_, String>(())
                })
                .await
                .unwrap_or_else(|_| Err("the task stopped".into()));
            let _ = cx.update_window(handle, |_, window, cx| {
                this.update(cx, |this, cx| match result {
                    Ok(()) => {
                        this.keys.new_key = None;
                        let db_id = vec![conn.clone(), db_name.clone()];
                        this.reload_db(&db_id, cx);
                        this.open_key(&[conn, db_name, rk::key_label(&key)], window, cx);
                    }
                    Err(e) => fail(this, e, cx),
                })
            });
        })
        .detach();
    }
}

/// `Scanning… 12 found` with Stop, or how the search ended, under the search field.
pub(super) fn search_status(shell: &Shell, cx: &mut Context<Shell>) -> Option<impl IntoElement> {
    let p = *cx.global::<Palette>();
    if shell.keys.scans.is_empty() || !shell.query_active {
        return None;
    }
    let running = shell.keys.scans.values().any(|s| s.running);
    let n: usize = shell.keys.scans.values().map(|s| s.found.len()).sum();
    let note = shell.keys.scans.values().find_map(|s| {
        s.error.clone().or(match s.end {
            Some(SearchEnd::Capped) => Some(format!(
                "Stopped at {} matches: refine the search.",
                rk::grouped(deadlock_core::redis_driver::SEARCH_CAP as u64)
            )),
            Some(SearchEnd::TooLong) => {
                Some("Stopped after scanning about two million keys: refine the search.".into())
            }
            Some(SearchEnd::Stopped) => Some("Stopped.".into()),
            _ => None,
        })
    });
    let text = if running {
        format!("Scanning… {} found", rk::grouped(n as u64))
    } else {
        format!("{} found on the server", rk::grouped(n as u64))
    };
    Some(
        div()
            .flex()
            .flex_none()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .px(px(8.))
            .text_size(small_size())
            .text_color(p.tx3)
            .child(text)
            .when(running, |d| {
                d.child(
                    button(
                        "stop-search",
                        Kind::Ghost,
                        true,
                        "Stop the server-side search",
                        cx,
                    )
                    .h(px(22.))
                    .child("Stop")
                    .on_click(cx.listener(|this, _, _, cx| this.stop_searches(cx))),
                )
            })
            .children(note.map(|n| div().w_full().child(n))),
    )
}

/// The namespace job's progress line at the bottom of the sidebar.
pub(super) fn job_status(shell: &Shell, cx: &mut Context<Shell>) -> Option<impl IntoElement> {
    let p = *cx.global::<Palette>();
    let job = shell.keys.job.as_ref()?;
    let progress = match job.total {
        Some(t) => format!(
            "{}… {} of {}",
            job.what,
            rk::grouped(job.done as u64),
            rk::grouped(t as u64)
        ),
        None => format!("{}… {}", job.what, rk::grouped(job.done as u64)),
    };
    let stop = Arc::clone(&job.stop);
    Some(
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(8.))
            .py(px(4.))
            .border_t_1()
            .border_color(p.line)
            .text_size(small_size())
            .text_color(p.tx2)
            .child(div().flex_1().min_w_0().child(progress))
            .child(
                button(
                    "cancel-job",
                    Kind::Outline,
                    true,
                    "Stop after the current batch",
                    cx,
                )
                .h(px(22.))
                .child("Cancel")
                .on_click(move |_, _, _| stop.store(true, Ordering::Relaxed)),
            ),
    )
}

/// The New key dialog.
pub(super) fn new_key_dialog(
    shell: &mut Shell,
    window: &mut Window,
    cx: &mut Context<Shell>,
) -> Option<gpui_kit::AnyElement> {
    let p = *cx.global::<Palette>();
    let n = shell.keys.new_key.as_mut()?;
    if std::mem::take(&mut n.fresh) {
        n.name.update(cx, |s, cx| s.focus(window, cx));
    }
    let mut types = vec![
        KeyType::String,
        KeyType::Hash,
        KeyType::List,
        KeyType::Set,
        KeyType::ZSet,
        KeyType::Stream,
    ];
    if n.json {
        types.push(KeyType::Json);
    }
    let chosen = n.kind.clone();
    let segments =
        div()
            .flex()
            .flex_wrap()
            .gap(px(4.))
            .children(types.into_iter().enumerate().map(|(i, t)| {
                let on = t == chosen;
                div()
                    .id(("key-type", i))
                    .px(px(8.))
                    .h(px(28.))
                    .flex()
                    .items_center()
                    .rounded(crate::theme::RADIUS)
                    .border_1()
                    .border_color(if on { p.tx } else { p.line2 })
                    .when(on, |d| d.bg(p.tx).text_color(p.bg))
                    .font_family(MONO_FONT)
                    .text_size(small_size())
                    .child(t.label().to_owned())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(n) = &mut this.keys.new_key {
                            n.kind = t.clone();
                            n.error = None;
                        }
                        cx.notify();
                    }))
            }));
    let (first, second): (&'static str, Option<&'static str>) = match chosen {
        KeyType::Hash => ("First field", Some("Its value")),
        KeyType::ZSet => ("First member", Some("Its score")),
        KeyType::List => ("First element", None),
        KeyType::Set => ("First member", None),
        KeyType::Stream => (
            "First entry's fields, as JSON: {\"field\": \"value\"}",
            None,
        ),
        KeyType::Json => ("Document (JSON)", None),
        _ => ("Value", None),
    };
    let field = |s: &Entity<InputState>, mono: bool| {
        let i = Styled::h(Input::new(s), px(32.));
        if mono { i.font_family(MONO_FONT) } else { i }
    };
    let mut body = vec![
        labelled("Key name", field(&n.name, true)).into_any_element(),
        labelled("Type", segments).into_any_element(),
        labelled(first, field(&n.first, true)).into_any_element(),
    ];
    if let Some(label) = second {
        body.push(labelled(label, field(&n.second, true)).into_any_element());
    }
    if let Some(e) = &n.error {
        body.push(
            div()
                .text_size(small_size())
                .text_color(p.danger)
                .child(e.clone())
                .into_any_element(),
        );
    }
    body.push(
        div()
            .flex()
            .justify_end()
            .gap(px(8.))
            .child(
                button("new-key-cancel", Kind::Outline, true, "Cancel (Esc)", cx)
                    .child("Cancel")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.keys.new_key = None;
                        cx.notify();
                    })),
            )
            .child(
                button(
                    "new-key-create",
                    Kind::Primary,
                    true,
                    "Create the key (Enter)",
                    cx,
                )
                .child("Create key")
                .on_click(cx.listener(|this, _, window, cx| this.create_key(window, cx))),
            )
            .into_any_element(),
    );
    Some(
        modal(
            "new-key",
            440.,
            format!("New key in {}", n.db),
            Some("The key is created with its first value: Redis has no empty keys.".into()),
            body,
            &p,
        )
        .on_action(cx.listener(|this, _: &input::Escape, _, cx| {
            this.keys.new_key = None;
            cx.notify();
        }))
        .into_any_element(),
    )
}
