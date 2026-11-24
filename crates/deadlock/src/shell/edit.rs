//! Saving an edited connection: in place by id, reconnecting only when the change
//! reaches the server.

use deadlock_core::connection::{ConnectionConfig, keychain};
use gpui_kit::{Context, Window};

use super::{Conn, Shell, ssh_entry};
use crate::app::Db;

/// Whether the edit changes how the connection reaches the server. Name, environment,
/// colour and the keychain choice apply without reconnecting.
fn needs_reconnect(old: &ConnectionConfig, new: &ConnectionConfig) -> bool {
    let mut same = new.clone();
    same.name.clone_from(&old.name);
    same.environment = old.environment;
    same.color.clone_from(&old.color);
    same.keychain = old.keychain;
    same != *old
}

/// Replaces the connection with `config`'s id, keeping its place. False when none has it.
fn replace_connection(list: &mut [ConnectionConfig], config: ConnectionConfig) -> bool {
    match list.iter_mut().find(|c| c.id == config.id) {
        Some(c) => {
            *c = config;
            true
        }
        None => false,
    }
}

impl Shell {
    /// The edit form's Save: applies at once, or after a confirmation when the
    /// connection is open and the change needs a new connection.
    pub(super) fn save_edit(
        &mut self,
        config: ConnectionConfig,
        password: Option<String>,
        ssh_secret: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(old) = self.config(&config.id).cloned() else {
            return self.close_form(cx);
        };
        // The form never edits this; its copy may predate a hide.
        let mut config = config;
        config.hidden_schemas.clone_from(&old.hidden_schemas);
        let open = matches!(
            self.conns.get(&config.id),
            Some(Conn::Connected(_) | Conn::Connecting)
        );
        let secrets = password.is_some() || ssh_secret.is_some();
        if !open || !(needs_reconnect(&old, &config) || secrets) {
            self.apply_edit(config, password, ssh_secret, false, window, cx);
            return;
        }
        let answer = crate::dialog::ask(
            window,
            cx,
            crate::dialog::Ask::new(
                format!("Reconnect {} with the new settings?", old.name),
                &["Save and reconnect", "Cancel"],
            )
            .text(
                "The connection closes and opens again. Its Sessions, configuration, \
                 diagram and import tabs close; editor tabs open new sessions.",
            )
            .production(old.environment == deadlock_core::connection::Environment::Production),
        );
        let tabs: Vec<_> = self
            .tabs
            .iter()
            .map(|(t, _)| t.clone())
            .filter(|t| {
                t.read(cx)
                    .connection
                    .as_ref()
                    .is_some_and(|c| c.id == old.id)
            })
            .collect();
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Some(0) {
                return;
            }
            let _ = this.update(cx, move |this, cx| {
                let question = format!(
                    "Submit the pending changes before reconnecting {}?",
                    old.name
                );
                this.settle_pending(tabs.clone(), question, cx, move |this, _, cx| {
                    let question = format!("Commit or roll back before reconnecting {}?", old.name);
                    this.settle(tabs, question, cx, move |this, window, cx| {
                        this.apply_edit(config, password, ssh_secret, true, window, cx);
                    });
                });
            });
        })
        .detach();
    }

    /// Saves `config` in place, refreshes every view of it, and reconnects when asked.
    fn apply_edit(
        &mut self,
        config: ConnectionConfig,
        password: Option<String>,
        ssh_secret: Option<String>,
        reconnect: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = config.id.clone();
        if config.keychain && (password.is_some() || ssh_secret.is_some()) {
            let (root, id, pw, ssh) = (
                self.project.root().to_owned(),
                id.clone(),
                password.clone(),
                ssh_secret,
            );
            drop(cx.global::<Db>().0.spawn_blocking(move || {
                for (entry, secret) in [(id.clone(), pw), (ssh_entry(&id), ssh)] {
                    if let Some(s) = secret
                        && let Err(e) = keychain::save(&root, &entry, &s)
                    {
                        eprintln!("deadlock: keychain: {e}");
                    }
                }
            }));
        }
        if let Some(p) = password {
            self.passwords.insert(id.clone(), p);
        }
        if !replace_connection(&mut self.project.config.connections, config.clone()) {
            return self.close_form(cx);
        }
        if let Err(e) = self.project.save_config() {
            eprintln!("deadlock: {e}");
        }
        // Tabs keep a copy of the settings: the new name and badge show at once.
        for (t, _) in &self.tabs {
            t.update(cx, |t, cx| {
                if t.connection.as_ref().is_some_and(|c| c.id == id) {
                    t.connection = Some(config.clone());
                    if reconnect {
                        t.close_session(cx);
                    }
                    cx.notify();
                }
            });
        }
        for (t, _) in &self.sessions {
            t.update(cx, |t, _| {
                if t.connection.id == id {
                    t.connection = config.clone();
                }
            });
        }
        for (t, _) in &self.configs {
            t.update(cx, |t, _| {
                if t.connection.id == id {
                    t.connection = config.clone();
                }
            });
        }
        for (t, _) in &self.diagrams {
            t.update(cx, |t, _| {
                if t.connection.id == id {
                    t.connection = config.clone();
                }
            });
        }
        if let Some((t, _)) = &self.import {
            t.update(cx, |t, _| {
                if t.connection.id == id {
                    t.connection = config.clone();
                }
            });
        }
        self.close_form(cx);
        if reconnect {
            // Their sessions are the old connection's: they close, newest first.
            for i in (0..self.sessions.len()).rev() {
                if self.sessions[i].0.read(cx).connection.id == id {
                    self.close_sessions(i, cx);
                }
            }
            for i in (0..self.diagrams.len()).rev() {
                if self.diagrams[i].0.read(cx).connection.id == id {
                    self.close_diagram(i, cx);
                }
            }
            for i in (0..self.configs.len()).rev() {
                if self.configs[i].0.read(cx).connection.id == id {
                    self.close_config(i, window, cx);
                }
            }
            if self
                .import
                .as_ref()
                .is_some_and(|(t, _)| t.read(cx).connection.id == id)
            {
                self.close_import(cx);
            }
            if let Some(old) = self.conns.remove(&id) {
                // Drivers close their sockets on the runtime, not the UI thread.
                drop(cx.global::<Db>().0.spawn(async move { drop(old) }));
            }
            self.connect(&id, cx);
        }
        self.tree_connections();
        self.rebuild(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadlock_core::connection::{Engine, Environment};

    fn conn(id: &str) -> ConnectionConfig {
        ConnectionConfig {
            id: id.into(),
            name: id.into(),
            ..ConnectionConfig::new(Engine::Mysql)
        }
    }

    #[test]
    fn name_and_environment_apply_live() {
        let old = conn("a");
        let mut new = old.clone();
        new.name = "renamed".into();
        new.environment = Environment::Production;
        new.keychain = !old.keychain;
        new.color = Some("#3b82f6".into());
        assert!(!needs_reconnect(&old, &new));
        new.host = "db.example.com".into();
        assert!(needs_reconnect(&old, &new));
        let mut tunnel = old.clone();
        tunnel.ssh_tunnel = true;
        assert!(needs_reconnect(&old, &tunnel));
    }

    #[test]
    fn edits_replace_in_place_by_id() {
        let mut list = vec![conn("a"), conn("b"), conn("c")];
        let mut b = conn("b");
        b.name = "Bee".into();
        assert!(replace_connection(&mut list, b));
        assert_eq!(list.len(), 3);
        assert_eq!(list[1].id, "b");
        assert_eq!(list[1].name, "Bee");
        assert!(!replace_connection(&mut list, conn("gone")));
        assert_eq!(list.len(), 3);
    }
}
