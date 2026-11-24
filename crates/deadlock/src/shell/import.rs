//! The Import tab in a project window: what the toolbar's Import opens, and the table
//! tab that reloads after an import.

use deadlock_core::connection::Engine;
use deadlock_core::driver::NodeKind;
use deadlock_core::edit::TableRef;
use gpui_kit::{AppContext as _, Context, Entity, Window};

use super::{Conn, Front, Shell};
use crate::import::{ImportEvent, ImportTab};
use crate::query::Kind;

/// Why a table cannot take an import.
const REDIS: &str = "Import CSV needs a MySQL, MariaDB or Oracle table: Redis has none";
const VIEW: &str = "Import CSV needs a table: a view cannot be imported into";

impl Shell {
    /// What Import opens now: the front table tab's table, else the active connection
    /// with a table to pick; or why it cannot open.
    pub(super) fn import_target(
        &self,
        cx: &gpui_kit::App,
    ) -> Result<(String, Option<TableRef>), &'static str> {
        if let Some(t) = self.front_tab().map(|t| t.read(cx))
            && let (Some(data), Some(c)) = (&t.table, &t.connection)
        {
            if c.engine == Engine::Redis {
                return Err(REDIS);
            }
            if data.info.as_ref().is_some_and(|i| i.view) {
                return Err(VIEW);
            }
            return self
                .importable(&c.id)
                .map(|id| (id, Some(data.table.clone())));
        }
        let id = self
            .active(cx)
            .ok_or("Import CSV: choose a connection first")?;
        self.importable(&id).map(|id| (id, None))
    }

    fn importable(&self, id: &str) -> Result<String, &'static str> {
        let c = self
            .config(id)
            .ok_or("Import CSV: choose a connection first")?;
        if c.engine == Engine::Redis {
            return Err(REDIS);
        }
        if c.read_only {
            return Err("Import CSV: the connection is read-only");
        }
        match self.conns.get(id) {
            Some(Conn::Connected(_)) => Ok(id.to_owned()),
            _ => Err("Import CSV: connect to the connection first"),
        }
    }

    /// Opens the Import tab for `table` of connection `id`, replacing an Import tab
    /// that is not running.
    pub(super) fn open_import(
        &mut self,
        id: &str,
        table: Option<TableRef>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .import
            .as_ref()
            .is_some_and(|(t, _)| t.read(cx).is_running())
        {
            self.show_tab(Front::Import, window, cx);
            return;
        }
        let (Some(config), Some(Conn::Connected(driver))) =
            (self.config(id).cloned(), self.conns.get(id))
        else {
            return;
        };
        let driver = std::sync::Arc::clone(driver);
        let password = self.passwords.get(id).cloned().unwrap_or_default();
        let tables = if table.is_none() {
            self.tables_of(id)
        } else {
            Vec::new()
        };
        let tab = cx.new(|cx| ImportTab::new(config, password, driver, table, tables, window, cx));
        let sub = cx.subscribe(&tab, Self::on_import);
        self.import = Some((tab, sub));
        self.show_tab(Front::Import, window, cx);
    }

    /// The connection's tables the sidebar has loaded, for the table picker.
    fn tables_of(&self, id: &str) -> Vec<TableRef> {
        let mut out = Vec::new();
        for db in self.tree.children(&[id.to_owned()]).unwrap_or_default() {
            let path = [id.to_owned(), db.name.clone()];
            for group in self.tree.children(&path).unwrap_or_default() {
                for t in group.children.iter().flatten() {
                    if t.kind == NodeKind::Table {
                        out.push(TableRef {
                            database: db.name.clone(),
                            table: t.name.clone(),
                        });
                    }
                }
            }
        }
        out
    }

    fn on_import(&mut self, _: Entity<ImportTab>, e: &ImportEvent, cx: &mut Context<Self>) {
        let ImportEvent::Imported(table) = e;
        for (t, _) in &self.tabs {
            t.update(cx, |t, cx| {
                if matches!(&t.kind, Kind::Table(r) if r == table) {
                    t.load_table(false, cx);
                }
            });
        }
    }

    pub(super) fn close_import(&mut self, cx: &mut Context<Self>) {
        if self
            .import
            .as_ref()
            .is_some_and(|(t, _)| t.read(cx).is_running())
        {
            return;
        }
        self.import = None;
        if self.front == Front::Import {
            self.front = match self.tabs.len() {
                0 => Front::None,
                n => Front::Query(n - 1),
            };
            self.follow_front(cx);
        }
        cx.notify();
    }
}
