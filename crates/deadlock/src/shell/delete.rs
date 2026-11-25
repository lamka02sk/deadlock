//! Delete connection and Delete project: what Deadlock stored goes, the user's files
//! stay.

use std::sync::PoisonError;

use deadlock_core::connection::Environment;
use deadlock_core::connection::keychain::{self, ssh_entry};
use gpui_kit::{AppContext as _, Context, Window};

use super::{Cache, CloseThen, Shell};
use crate::app::{Db, HistoryDb};
use crate::dialog::{Ask, ask};
use crate::query::Kind;

/// `1 tab`, `3 tabs`.
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

impl Shell {
    /// Delete connection…: refused while one of its tabs has an open transaction or
    /// pending changes, else asks, then removes it with its tabs, saved secrets, cache
    /// rows, diagrams and console buffer. File tabs stay, without a connection.
    pub(crate) fn delete_connection(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(config) = self.config(id).cloned() else {
            return;
        };
        let on =
            |c: Option<&deadlock_core::connection::ConnectionConfig>| c.is_some_and(|c| c.id == id);
        let busy = self
            .tabs
            .iter()
            .map(|(t, _)| t.read(cx))
            .find(|t| on(t.connection.as_ref()) && (t.tx_open || t.pending_changes(cx) > 0))
            .map(|t| self.tab_title(t));
        if let Some(title) = busy {
            drop(ask(
                window,
                cx,
                Ask::new(format!("Commit or roll back in {title} first."), &["OK"]).text(format!(
                    "{} cannot be deleted while a tab has an open transaction or pending changes.",
                    config.name
                )),
            ));
            return;
        }
        let closing = self
            .tabs
            .iter()
            .filter(|(t, _)| {
                let t = t.read(cx);
                on(t.connection.as_ref()) && !matches!(t.kind, Kind::File(_))
            })
            .count()
            + self
                .sessions
                .iter()
                .filter(|(t, _)| on(Some(&t.read(cx).connection)))
                .count()
            + self
                .configs
                .iter()
                .filter(|(t, _)| on(Some(&t.read(cx).connection)))
                .count()
            + self
                .diagrams
                .iter()
                .filter(|(t, _)| on(Some(&t.read(cx).connection)))
                .count()
            + self
                .objects
                .iter()
                .filter(|(t, _)| on(Some(&t.read(cx).connection)))
                .count()
            + usize::from(
                self.import
                    .as_ref()
                    .is_some_and(|(t, _)| on(Some(&t.read(cx).connection))),
            );
        let tabs = match closing {
            0 => "No tabs close.".to_owned(),
            n => format!(
                "{} {}.",
                count(n, "tab", "tabs"),
                if n == 1 { "closes" } else { "close" }
            ),
        };
        let answer = ask(
            window,
            cx,
            Ask::new(
                format!("Delete connection {}?", config.name),
                &["Delete connection", "Cancel"],
            )
            .text(format!(
                "Its saved password, schema cache, diagrams and console are removed. {tabs} \
                 File tabs stay open without a connection, and query history keeps its statements."
            ))
            .production(config.environment == Environment::Production)
            .danger(true),
        );
        let id = id.to_owned();
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Some(0) {
                return;
            }
            let _ = this.update_in(cx, |this, window, cx| {
                this.remove_connection(&id, window, cx)
            });
        })
        .detach();
    }

    /// Removes connection `id` from `project.toml` first; when that write fails nothing
    /// else changes and the error shows.
    fn remove_connection(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let list = &mut self.project.config.connections;
        let Some(at) = list.iter().position(|c| c.id == id) else {
            return;
        };
        let removed = list.remove(at);
        if let Err(e) = self.project.save_config() {
            let title = format!("{} cannot be deleted", removed.name);
            self.project.config.connections.insert(at, removed);
            drop(ask(
                window,
                cx,
                Ask::new(title, &["OK"]).text(e.to_string()),
            ));
            return;
        }
        self.close_connection_tabs(id, cx);
        if let Some(old) = self.conns.remove(id) {
            // Drivers close their sockets on the runtime, not the UI thread.
            drop(cx.global::<Db>().0.spawn(async move { drop(old) }));
        }
        self.passwords.remove(id);
        self.prompted.remove(id);
        self.waiting.remove(id);
        if self.prompt.as_ref().is_some_and(|p| p.id == id) {
            self.prompt = None;
        }
        if self
            .form
            .as_ref()
            .is_some_and(|(f, _)| f.read(cx).editing().is_some_and(|c| c.id == id))
        {
            self.close_form(cx);
        }
        self.keys.scans.remove(id);
        let under = |n: &Vec<String>| n.first().is_some_and(|c| c == id);
        self.keys.marked.retain(|n| !under(n));
        self.restored_objects.retain(|(c, _)| c != id);
        self.tree.expanded.retain(|n| !under(n));
        self.tree.status.retain(|n, _| !under(n));
        self.fresh.retain(|n| !under(n));
        if self.selected.as_ref().is_some_and(under) {
            self.selected = None;
        }
        self.tree_connections();
        self.rebuild(cx);
        self.schedule_save(cx);
        cx.notify();
        self.forget_files(id, cx);
    }

    /// Closes every tab of connection `id` except file tabs, which lose the connection.
    fn close_connection_tabs(&mut self, id: &str, cx: &mut Context<Self>) {
        for i in (0..self.tabs.len()).rev() {
            let tab = self.tabs[i].0.clone();
            if !tab.read(cx).connection.as_ref().is_some_and(|c| c.id == id) {
                continue;
            }
            if matches!(tab.read(cx).kind, Kind::File(_)) {
                tab.update(cx, |t, cx| t.set_connection(None, cx));
            } else {
                // Its buffer is deleted next: no last save.
                tab.update(cx, |t, _| t.dirty = false);
                self.remove_tab(i, cx);
            }
        }
        for i in (0..self.sessions.len()).rev() {
            if self.sessions[i].0.read(cx).connection.id == id {
                self.close_sessions(i, cx);
            }
        }
        for i in (0..self.configs.len()).rev() {
            if self.configs[i].0.read(cx).connection.id == id {
                self.remove_config(i, cx);
            }
        }
        for i in (0..self.diagrams.len()).rev() {
            if self.diagrams[i].0.read(cx).connection.id == id {
                self.close_diagram(i, cx);
            }
        }
        for i in (0..self.objects.len()).rev() {
            if self.objects[i].0.read(cx).connection.id == id {
                self.remove_object(i, cx);
            }
        }
        if self
            .import
            .as_ref()
            .is_some_and(|(t, _)| t.read(cx).connection.id == id)
        {
            self.close_import(cx);
        }
        self.follow_front(cx);
    }

    /// Removes the secrets, cache rows, diagrams and console buffer of a deleted
    /// connection. Failures are logged: the connection is already gone.
    fn forget_files(&self, id: &str, cx: &mut Context<Self>) {
        let (root, id) = (self.project.root().to_owned(), id.to_owned());
        let cache = cx.global::<Cache>().0.clone();
        let rt = std::sync::Arc::clone(&cx.global::<Db>().0);
        // From a task: closed diagram tabs save their layout as they are released,
        // which happens after this update; the forget must come after that.
        cx.spawn(async move |_, _| {
            let done = rt.spawn_blocking(move || {
                for entry in [id.clone(), ssh_entry(&id)] {
                    if let Err(e) = keychain::delete(&root, &entry) {
                        eprintln!("deadlock: keychain: {e}");
                    }
                }
                if let Some(cache) = cache
                    && let Err(e) = cache
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .forget(&root, Some(&id))
                {
                    eprintln!("deadlock: {e}");
                }
                if let Err(e) = deadlock_core::diagram::forget(&root, &id) {
                    eprintln!("deadlock: {e}");
                }
                match deadlock_core::project::console_path(&root, &id).map(std::fs::remove_file) {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Ok(Err(e)) => eprintln!("deadlock: console: {e}"),
                    Err(e) => eprintln!("deadlock: console: {e}"),
                }
            });
            let _ = done.await;
        })
        .detach();
    }

    /// Delete project…: asks, runs the close checks (Cancel there keeps the project),
    /// then [`Self::finish_delete`].
    pub(crate) fn delete_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.project.config.connections.len();
        let answer = ask(
            window,
            cx,
            Ask::new(
                format!("Delete project {}?", self.project.name()),
                &["Delete project", "Cancel"],
            )
            .text(format!(
                "Removes Deadlock's data for this folder: {} and their saved passwords, query \
                 history, schema cache, diagrams and console buffers. The folder {} and your \
                 files stay.",
                count(n, "connection", "connections"),
                self.project.root().display()
            ))
            .danger(true),
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Some(0) {
                return;
            }
            let _ = this.update_in(cx, |this, window, cx| {
                this.close_then = CloseThen::Delete;
                if this.should_close(window, cx) {
                    this.finish_close(window, cx);
                }
            });
        })
        .detach();
    }

    /// The close checks passed: deletes the project's data on the blocking pool, then
    /// drops it from the recent list and closes the window, showing the open-project
    /// window when no project window is left. On a failure the window stays and says why.
    pub(super) fn finish_delete(&mut self, cx: &mut Context<Self>) {
        self.deleted = true;
        self.save = None;
        // Diagrams save their layout as they close: into `.deadlock/` before it goes.
        for i in (0..self.diagrams.len()).rev() {
            self.close_diagram(i, cx);
        }
        for (t, _) in &self.tabs {
            t.update(cx, |t, _| {
                if matches!(t.kind, Kind::Console(_)) {
                    t.dirty = false;
                }
            });
        }
        let root = self.project.root().to_owned();
        let ids: Vec<String> = self
            .project
            .config
            .connections
            .iter()
            .map(|c| c.id.clone())
            .collect();
        let history = cx.global::<HistoryDb>().0.clone();
        let cache = cx.global::<Cache>().0.clone();
        let rt = std::sync::Arc::clone(&cx.global::<Db>().0);
        let handle = self.window;
        cx.spawn(async move |this, cx| {
            let target = root.clone();
            let done = rt
                .spawn_blocking(move || {
                    let (Some(history), Some(cache)) = (history, cache) else {
                        return Err("the query history or the schema cache could not be opened \
                                    when Deadlock started"
                            .to_owned());
                    };
                    let mut history = history.lock().unwrap_or_else(PoisonError::into_inner);
                    let cache = cache.lock().unwrap_or_else(PoisonError::into_inner);
                    deadlock_core::project::delete(&target, &ids, &mut history, &cache)
                        .map_err(|e| e.to_string())
                })
                .await
                .unwrap_or_else(|_| Err("the delete task stopped".to_owned()));
            let _ = cx.update_window(handle, |_, window, cx| {
                let Err(e) = done else {
                    crate::app::forget_recent(&root, cx);
                    let others = cx
                        .global::<crate::app::Registry>()
                        .projects
                        .keys()
                        .any(|p| *p != root);
                    // Before the window goes: the app quits with its last window.
                    if !others {
                        crate::app::show_welcome(cx);
                    }
                    window.remove_window();
                    return;
                };
                // History may be gone already: open panels list what is left.
                crate::shell::history_cleared(cx);
                let _ = this.update(cx, |this, cx| {
                    this.deleted = false;
                    this.may_close = false;
                    drop(ask(
                        window,
                        cx,
                        Ask::new("The project could not be deleted", &["OK"]).text(format!(
                            "{e}. What was removed before this step stays removed; \
                             Delete project again to remove the rest."
                        )),
                    ));
                });
            });
        })
        .detach();
    }
}
