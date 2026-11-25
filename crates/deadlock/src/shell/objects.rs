//! The object editor tabs in the window: opening one per object (or a new one from a
//! template), their strip entries, closing with the Discard prompt, the tree reloads
//! after an apply, and the sidebar's object and table menus.

use std::rc::Rc;
use std::sync::Arc;

use deadlock_core::driver::{DriverError, NodeKind};
use deadlock_core::edit::TableRef;
use deadlock_core::objects::{Flavor, ObjectKind, ObjectRef, Properties, template};
use deadlock_core::schema::{Row, RowKind};
use gpui_kit::component::menu::PopupMenu;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, ClipboardItem, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};

use super::{Conn, Front, Shell};
use crate::app::Db;
use crate::assets::{Icon, icon, menu_item, menu_item_danger, menu_item_disabled, menu_item_hint};
use crate::object::{ObjectEvent, ObjectTab, Target, Then};
use crate::query::{Kind, TableAction, View};
use crate::theme::{Palette, tab_height};
use crate::ui::Clickable as _;
use gpui_kit::{MouseButton, MouseUpEvent};

impl Shell {
    /// The object dialect of connection `conn`, while it is connected; `None` on Redis.
    pub(super) fn flavor(&self, conn: &str) -> Option<Flavor> {
        match self.conns.get(conn) {
            Some(Conn::Connected(d)) => d.object_flavor(),
            _ => None,
        }
    }

    /// Opens `object` of `conn` in its editor tab, or brings the open one forward;
    /// `then` runs once it is loaded (the menu's Rename, Drop, Recompile).
    pub(super) fn open_object(
        &mut self,
        conn: &str,
        object: ObjectRef,
        then: Option<Then>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(i) = self
            .objects
            .iter()
            .position(|(t, _)| t.read(cx).is(conn, &object))
        {
            if let Some(then) = then {
                self.objects[i].0.update(cx, |t, cx| match then {
                    Then::Rename => t.start_rename(window, cx),
                    Then::Drop => t.ask_drop(window, cx),
                    Then::Recompile => t.recompile(window, cx),
                });
            }
            self.show_tab(Front::Object(i), window, cx);
            return;
        }
        if let Some(i) = self.add_object(conn, Target::Existing(object, then), window, cx) {
            self.show_tab(Front::Object(i), window, cx);
        }
    }

    /// Connects the next connection that restored object tabs wait for, one at a time
    /// and never over a password prompt.
    pub(super) fn connect_restored(&mut self, cx: &mut Context<Self>) {
        if self.prompt.is_some() || self.conns.values().any(|c| matches!(c, Conn::Connecting)) {
            return;
        }
        let next = self
            .restored_objects
            .iter()
            .map(|(c, _)| c)
            .find(|c| !self.conns.contains_key(*c))
            .cloned();
        if let Some(id) = next {
            self.connect(&id, cx);
        }
    }

    /// Connection `id` connected: opens the object tabs `state.json` listed for it,
    /// behind the tab in front. Objects that are gone are left out without a word.
    pub(super) fn reopen_objects(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(Conn::Connected(driver)) = self.conns.get(id) else {
            return;
        };
        let (due, rest) = std::mem::take(&mut self.restored_objects)
            .into_iter()
            .partition(|(c, _)| c == id);
        self.restored_objects = rest;
        if due.is_empty() {
            return;
        }
        let (driver, handle, id) = (Arc::clone(driver), self.window, id.to_owned());
        let rt = Arc::clone(&cx.global::<Db>().0);
        cx.spawn(async move |this, cx| {
            for (_, object) in due {
                let (driver, o) = (Arc::clone(&driver), object.clone());
                // Only an object that is gone is left out; on any other failure the
                // tab still opens and shows the error, so it is not lost.
                if matches!(
                    rt.spawn(async move { driver.object(&o).await }).await,
                    Ok(Err(DriverError::NotFound(_)))
                ) {
                    continue;
                }
                let _ = cx.update_window(handle, |_, window, cx| {
                    this.update(cx, |this, cx| {
                        let open = this
                            .objects
                            .iter()
                            .any(|(t, _)| t.read(cx).is(&id, &object));
                        if !open {
                            this.add_object(&id, Target::Existing(object, None), window, cx);
                            cx.notify();
                        }
                    })
                });
            }
        })
        .detach();
    }

    /// A New … menu item: the editor on `kind`'s template in `schema`, for `table`'s
    /// triggers when given.
    pub(super) fn new_object(
        &mut self,
        conn: &str,
        kind: ObjectKind,
        schema: &str,
        table: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(flavor) = self.flavor(conn) else {
            return;
        };
        let mut draft = template(flavor, kind, schema);
        if let (Properties::Trigger(t), Some(table)) = (&mut draft.props, table) {
            t.table = table;
        }
        if let Some(i) = self.add_object(conn, Target::New(Box::new(draft)), window, cx) {
            self.show_tab(Front::Object(i), window, cx);
        }
    }

    /// New table…: a table tab in draft state for `schema`, on its Structure view.
    pub(super) fn new_table(
        &mut self,
        conn: &str,
        schema: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(binding) = self.binding(Some(conn)) else {
            return;
        };
        let mariadb = self.flavor(conn) == Some(Flavor::Mariadb);
        let table = TableRef {
            database: schema.into(),
            table: String::new(),
        };
        let i = self.add_tab(Kind::Table(table), Some(binding), String::new(), window, cx);
        self.tabs[i]
            .0
            .update(cx, |t, cx| t.start_draft(mariadb, window, cx));
        self.show_tab(Front::Query(i), window, cx);
    }

    /// Adds an object tab at the end of the strip; its index, once `conn` is connected.
    fn add_object(
        &mut self,
        conn: &str,
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let (Some(config), Some(Conn::Connected(driver)), Some(flavor)) = (
            self.config(conn).cloned(),
            self.conns.get(conn),
            self.flavor(conn),
        ) else {
            return None;
        };
        let driver = Arc::clone(driver);
        let root = self.project.root().to_owned();
        let shell = cx.entity().downgrade();
        let binding = config.clone();
        let complete: crate::object::Complete = Rc::new(move |editor, cx| {
            let binding = binding.clone();
            super::complete::install_editor(
                shell.clone(),
                editor,
                Rc::new(move |_| Some(binding.clone())),
                cx,
            );
        });
        let tab =
            cx.new(|cx| ObjectTab::new(config, driver, flavor, root, target, complete, window, cx));
        let sub = cx.subscribe_in(&tab, window, Self::on_object_event);
        self.objects.push((tab, sub));
        Some(self.objects.len() - 1)
    }

    fn on_object_event(
        &mut self,
        tab: &Entity<ObjectTab>,
        e: &ObjectEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let conn = tab.read(cx).connection.id.clone();
        match e {
            ObjectEvent::Changed => cx.notify(),
            ObjectEvent::Refresh(paths) => {
                if matches!(self.conns.get(&conn), Some(Conn::Connected(_))) {
                    for path in paths {
                        let mut id = vec![conn.clone()];
                        id.extend(path.0.iter().cloned());
                        self.load(id, cx);
                    }
                }
            }
            ObjectEvent::Close => {
                if let Some(i) = self.objects.iter().position(|(t, _)| t == tab) {
                    self.close_object(i, window, cx);
                }
            }
            ObjectEvent::OpenFile(path) => self.open_path(path.clone(), window, cx),
            ObjectEvent::Dropped => {
                if let Some(i) = self.objects.iter().position(|(t, _)| t == tab) {
                    self.remove_object(i, cx);
                }
            }
        }
    }

    /// Table `table` of connection `conn` was dropped: closes the tabs of its triggers,
    /// asking once first about those with unapplied edits.
    pub(super) fn close_objects_on_table(
        &mut self,
        conn: &str,
        table: &TableRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let on: Vec<_> = self
            .objects
            .iter()
            .map(|(t, _)| t.clone())
            .filter(|t| t.read(cx).is_on_table(conn, table) && !t.read(cx).is_running())
            .collect();
        let (dirty, clean): (Vec<_>, Vec<_>) = on.into_iter().partition(|t| t.read(cx).dirty());
        for tab in clean {
            if let Some(i) = self.objects.iter().position(|(t, _)| *t == tab) {
                self.remove_object(i, cx);
            }
        }
        if dirty.is_empty() {
            return;
        }
        let titles: Vec<_> = dirty.iter().map(|t| t.read(cx).title()).collect();
        let answer = crate::dialog::ask(
            window,
            cx,
            crate::dialog::Ask::new(
                format!("Discard changes to {}?", titles.join(", ")),
                &["Discard", "Cancel"],
            )
            .text(format!(
                "Table {} was dropped, and with it its triggers. The edits have not been applied.",
                table.table
            )),
        );
        cx.spawn(async move |this, cx| {
            if answer.await != Some(0) {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                for tab in dirty {
                    if let Some(i) = this.objects.iter().position(|(t, _)| *t == tab) {
                        this.remove_object(i, cx);
                    }
                }
            });
        })
        .detach();
    }

    /// Closes object tab `i`; unapplied edits ask Discard / Cancel first. A tab whose
    /// apply is running stays: stopping in the middle could leave the object missing.
    pub(super) fn close_object(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.objects.get(i).map(|(t, _)| t.clone()) else {
            return;
        };
        if tab.read(cx).is_running() {
            return;
        }
        if !tab.read(cx).dirty() {
            self.remove_object(i, cx);
            return;
        }
        let title = tab.read(cx).title();
        let answer = crate::dialog::ask(
            window,
            cx,
            crate::dialog::Ask::new(
                format!("Discard changes to {title}?"),
                &["Discard", "Cancel"],
            )
            .text("The edits have not been applied."),
        );
        cx.spawn(async move |this, cx| {
            if answer.await != Some(0) {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                if let Some(i) = this.objects.iter().position(|(t, _)| *t == tab) {
                    this.remove_object(i, cx);
                }
            });
        })
        .detach();
    }

    pub(super) fn remove_object(&mut self, i: usize, cx: &mut Context<Self>) {
        if i >= self.objects.len() {
            return;
        }
        drop(self.objects.remove(i));
        self.front = match self.front {
            Front::Object(f) if f == i => match (self.objects.len(), self.tabs.len()) {
                (0, 0) => Front::None,
                (0, n) => Front::Query(n - 1),
                (n, _) => Front::Object(i.min(n - 1)),
            },
            Front::Object(f) if f > i => Front::Object(f - 1),
            other => other,
        };
        self.follow_front(cx);
    }

    /// Object tabs with edits not applied, for the window's close prompt.
    pub(super) fn dirty_objects(&self, cx: &gpui_kit::App) -> Vec<String> {
        self.objects
            .iter()
            .map(|(t, _)| t.read(cx))
            .filter(|t| t.dirty())
            .map(ObjectTab::title)
            .collect()
    }

    /// Reads the object's CREATE statement from the server onto the clipboard.
    fn copy_create(&mut self, conn: &str, object: ObjectRef, cx: &mut Context<Self>) {
        self.copy_create_with(conn, cx, async move |d| {
            d.object(&object).await.map(|def| def.source)
        });
    }

    /// The table's CREATE statement, as its Structure view shows it, onto the clipboard.
    fn copy_table_create(&mut self, conn: &str, table: TableRef, cx: &mut Context<Self>) {
        self.copy_create_with(conn, cx, async move |d| {
            d.structure(&table).await.map(|s| s.ddl)
        });
    }

    /// Runs `read` on the connection's driver and puts what it returns on the
    /// clipboard, or says why it could not.
    fn copy_create_with<F>(
        &mut self,
        conn: &str,
        cx: &mut Context<Self>,
        read: impl FnOnce(Arc<dyn deadlock_core::driver::Driver>) -> F + Send + 'static,
    ) where
        F: Future<Output = Result<String, deadlock_core::driver::DriverError>> + Send + 'static,
    {
        let Some(Conn::Connected(driver)) = self.conns.get(conn) else {
            return;
        };
        let driver = Arc::clone(driver);
        let rx = cx
            .global::<Db>()
            .0
            .spawn(async move { read(driver).await.map_err(|e| e.to_string()) });
        let window = self.window;
        cx.spawn(async move |_, cx| {
            match rx
                .await
                .unwrap_or_else(|_| Err("the object task stopped".into()))
            {
                Ok(sql) => cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string(sql))),
                Err(e) => {
                    let _ = cx.update_window(window, |_, window, cx| {
                        drop(crate::dialog::ask(
                            window,
                            cx,
                            crate::dialog::Ask::new(
                                "The CREATE statement could not be read",
                                &["OK"],
                            )
                            .text(e),
                        ));
                    });
                }
            }
        })
        .detach();
    }

    /// Whether `row` gets object menu items: object rows, their groups and schemas of
    /// a connected SQL connection.
    pub(super) fn has_object_menu(&self, row: &Row) -> bool {
        self.flavor(&row.id[0]).is_some()
            && match row.kind {
                RowKind::Node(NodeKind::Routine | NodeKind::View) => {
                    ObjectRef::from_node(&row.id).is_some()
                }
                RowKind::Node(NodeKind::Group) => {
                    new_kind(&row.id).is_some() || is_tables_group(&row.id)
                }
                RowKind::Node(NodeKind::Database) => true,
                _ => false,
            }
    }

    /// The object items of `row`'s context menu, before the row's other items.
    pub(super) fn object_menu(
        &self,
        row: &Row,
        mut menu: PopupMenu,
        shell: Entity<Shell>,
        cx: &gpui_kit::App,
    ) -> PopupMenu {
        let conn = row.id[0].clone();
        let Some(flavor) = self.flavor(&conn) else {
            return menu;
        };
        let read_only = self.config(&conn).is_some_and(|c| c.read_only);
        let new_item = |kind: ObjectKind, schema: String, table: Option<String>| {
            let (shell, conn) = (shell.clone(), conn.clone());
            menu_item(format!("New {}…", kind.label()), Icon::Plus)
                .disabled(read_only)
                .on_click(move |_, window, cx| {
                    let table = table.clone();
                    shell.update(cx, |s, cx| {
                        s.new_object(&conn, kind, &schema, table, window, cx)
                    });
                })
        };
        let new_table = |schema: String| {
            let (shell, conn) = (shell.clone(), conn.clone());
            menu_item("New table…", Icon::Plus)
                .disabled(read_only)
                .on_click(move |_, window, cx| {
                    shell.update(cx, |s, cx| s.new_table(&conn, &schema, window, cx));
                })
        };
        match row.kind {
            RowKind::Node(NodeKind::Database) => {
                menu = menu.item(new_table(row.id[1].clone()));
                for kind in ObjectKind::managed(flavor) {
                    menu = menu.item(new_item(*kind, row.id[1].clone(), None));
                }
                menu.separator()
            }
            RowKind::Node(NodeKind::Group) => match new_kind(&row.id) {
                Some((kind, table)) => menu.item(new_item(kind, row.id[1].clone(), table)),
                None if is_tables_group(&row.id) => menu.item(new_table(row.id[1].clone())),
                None => menu,
            },
            _ => {
                let Some(object) = ObjectRef::from_node(&row.id) else {
                    return menu;
                };
                let item = |label: String, ic: Icon, then: Option<Then>, off: bool| {
                    let (shell, conn, object) = (shell.clone(), conn.clone(), object.clone());
                    // Disabled items keep the plain look: a red one reads as available.
                    let it = if then == Some(Then::Drop) && !off {
                        menu_item_danger(label, ic, cx)
                    } else {
                        menu_item(label, ic)
                    };
                    it.disabled(off).on_click(move |_, window, cx| {
                        shell.update(cx, |s, cx| {
                            s.open_object(&conn, object.clone(), then, window, cx)
                        });
                    })
                };
                let view = matches!(object.kind, ObjectKind::View | ObjectKind::MaterializedView);
                menu = menu.item(item(
                    if view {
                        "Edit definition".into()
                    } else {
                        "Open".into()
                    },
                    if view { Icon::Rename } else { Icon::Open },
                    None,
                    false,
                ));
                let name = object.name.clone();
                menu = menu.item(
                    menu_item("Copy name", Icon::Copy).on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(name.clone()));
                    }),
                );
                {
                    let (shell, conn, object) = (shell.clone(), conn.clone(), object.clone());
                    menu = menu.item(menu_item("Copy CREATE statement", Icon::Copy).on_click(
                        move |_, _, cx| {
                            shell.update(cx, |s, cx| s.copy_create(&conn, object.clone(), cx));
                        },
                    ));
                }
                menu = menu.separator();
                menu = match object.kind.can_rename(flavor) {
                    Ok(()) => menu.item(item(
                        "Rename…".into(),
                        Icon::Rename,
                        Some(Then::Rename),
                        read_only,
                    )),
                    Err(reason) => menu.item(menu_item_disabled("Rename…", reason, Icon::Rename)),
                };
                if flavor == Flavor::Oracle && row.detail == "invalid" {
                    menu = menu.item(item(
                        "Recompile".into(),
                        Icon::Refresh,
                        Some(Then::Recompile),
                        read_only,
                    ));
                }
                menu.item(item(
                    format!("Drop {}…", object.kind.label()),
                    Icon::Trash,
                    Some(Then::Drop),
                    read_only,
                ))
            }
        }
    }
}

impl Shell {
    /// The items of a sidebar table row (`[conn, schema, "Tables", table]`), after the
    /// object items.
    pub(super) fn table_menu(
        &self,
        row: &Row,
        menu: PopupMenu,
        shell: Entity<Shell>,
        cx: &gpui_kit::App,
    ) -> PopupMenu {
        let [conn, schema, _, name] = row.id.as_slice() else {
            return menu;
        };
        let table = TableRef {
            database: schema.clone(),
            table: name.clone(),
        };
        let read_only = self.config(conn).is_some_and(|c| c.read_only);
        let (s, c, t) = (shell.clone(), conn.clone(), table.clone());
        let menu = menu.item(menu_item_hint("Open", "Enter", Icon::Open).on_click(
            move |_, window, cx| {
                s.update(cx, |s, cx| {
                    s.open_table(&c, t.clone(), View::Data, window, cx)
                });
            },
        ));
        let name = name.clone();
        let menu = menu.item(
            menu_item("Copy name", Icon::Copy).on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(name.clone()));
            }),
        );
        let (s, c, t) = (shell.clone(), conn.clone(), table.clone());
        let menu = menu.item(menu_item("Copy CREATE statement", Icon::Copy).on_click(
            move |_, _, cx| {
                s.update(cx, |s, cx| s.copy_table_create(&c, t.clone(), cx));
            },
        ));
        let (s, c, t) = (shell.clone(), conn.clone(), table.clone());
        let menu = menu.separator().item(
            menu_item("Import CSV…", Icon::Import)
                .disabled(read_only)
                .on_click(move |_, window, cx| {
                    s.update(cx, |s, cx| s.open_import(&c, Some(t.clone()), window, cx));
                }),
        );
        table_actions(menu, &shell, conn, &table, read_only, cx)
    }

    /// Opens (or brings forward) the table's tab on its Structure view, which asks.
    pub(super) fn table_action(
        &mut self,
        conn: &str,
        table: TableRef,
        action: TableAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_table(conn, table.clone(), View::Structure, window, cx);
        let tab = self.tabs.iter().map(|(t, _)| t).find(|t| {
            let t = t.read(cx);
            t.kind == Kind::Table(table.clone())
                && t.connection.as_ref().is_some_and(|c| c.id == conn)
        });
        if let Some(tab) = tab.cloned() {
            tab.update(cx, |t, cx| t.start_table_action(action, window, cx));
        }
    }

    /// Table `from` was renamed (`to`) or dropped: its saved diagram card follows, in
    /// the open diagram when it shows that schema.
    pub(super) fn table_changed(
        &mut self,
        conn: &str,
        from: &TableRef,
        to: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let tabs: Vec<_> = self.tabs.iter().map(|(t, _)| t.clone()).collect();
        for tab in tabs {
            tab.update(cx, |t, cx| t.other_table_changed(conn, from, cx));
        }
        let open = self.diagrams.iter().map(|(d, _)| d).find(|d| {
            let d = d.read(cx);
            d.connection.id == conn && d.schema == from.database
        });
        match open.cloned() {
            Some(d) => d.update(cx, |d, cx| d.table_changed(&from.table, to, cx)),
            None => {
                let root = self.project.root();
                let renamed = deadlock_core::diagram::rename_table(
                    root,
                    conn,
                    &from.database,
                    &from.table,
                    to,
                );
                if let Err(e) = renamed {
                    eprintln!("deadlock: {e}");
                }
            }
        }
    }
}

/// Rename…, Truncate… and Drop table… for `table` of `conn`; only Drop is red.
pub(super) fn table_actions(
    menu: PopupMenu,
    shell: &Entity<Shell>,
    conn: &str,
    table: &TableRef,
    read_only: bool,
    cx: &gpui_kit::App,
) -> PopupMenu {
    let item = |label: &'static str, ic: Icon, action: TableAction| {
        let (shell, conn, table) = (shell.clone(), conn.to_owned(), table.clone());
        // Disabled items keep the plain look: a red one reads as available.
        let it = if action == TableAction::Drop && !read_only {
            menu_item_danger(label, ic, cx)
        } else {
            menu_item(label, ic)
        };
        it.disabled(read_only).on_click(move |_, window, cx| {
            shell.update(cx, |s, cx| {
                s.table_action(&conn, table.clone(), action, window, cx)
            });
        })
    };
    menu.item(item("Rename…", Icon::Rename, TableAction::Rename))
        .item(item("Truncate…", Icon::Minus, TableAction::Truncate))
        .item(item("Drop table…", Icon::Trash, TableAction::Drop))
}

/// A schema's Tables group: `[conn, schema, "Tables"]`.
fn is_tables_group(id: &[String]) -> bool {
    matches!(id, [_, _, g] if g == "Tables")
}

/// The kind a group row's New item makes, and the table for triggers under a table:
/// `[conn, schema, "Procedures"]`, `[conn, schema, "Tables", table, "Triggers"]`.
fn new_kind(id: &[String]) -> Option<(ObjectKind, Option<String>)> {
    match id {
        [_, _, group] => Some((ObjectKind::from_group(group)?, None)),
        [_, _, tables, table, triggers] if tables == "Tables" && triggers == "Triggers" => {
            Some((ObjectKind::Trigger, Some(table.clone())))
        }
        _ => None,
    }
}

/// The strip entries of the object tabs.
pub(super) fn object_tabs(shell: &Shell, cx: &mut Context<Shell>) -> Vec<impl IntoElement> {
    let p = *cx.global::<Palette>();
    shell
        .objects
        .iter()
        .enumerate()
        .map(|(i, (t, _))| {
            let on = shell.front == Front::Object(i);
            let t = t.read(cx);
            let tip = format!(
                "{} {}.{} · {}",
                t.object.kind.label(),
                t.object.schema,
                t.title(),
                t.connection.name
            );
            let env = t.connection.environment;
            let tint = shell.color_of(&t.connection.id);
            let view = matches!(
                t.object.kind,
                ObjectKind::View | ObjectKind::MaterializedView
            );
            let group: SharedString = format!("object-tab-{i}").into();
            div()
                .id(group.clone())
                .group(group.clone())
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
                .when(on, |d| d.font_weight(gpui_kit::FontWeight::MEDIUM))
                .hover(|s| s.text_color(p.tx))
                .whitespace_nowrap()
                .flex_none()
                .tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                })
                .on_mouse_up(
                    MouseButton::Middle,
                    cx.listener(move |this, _: &MouseUpEvent, window, cx| {
                        this.close_object(i, window, cx);
                    }),
                )
                .clickable(cx.listener(move |this, _, window, cx| {
                    this.show_tab(Front::Object(i), window, cx)
                }))
                .child(icon(
                    if view { Icon::Table } else { Icon::Routine },
                    if on { p.tx } else { p.tx3 },
                ))
                .child(t.title())
                .children(super::env_marker(env, &p))
                .when(t.marked(), |d| {
                    d.child(div().size(px(6.)).rounded_full().bg(p.warn))
                })
                .child(
                    div()
                        .id(("close-object", i))
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
                            this.close_object(i, window, cx);
                        })),
                )
                .map(|d| super::tabs::tinted(d, tint, on, &p))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn group_rows_offer_their_kind() {
        assert_eq!(
            new_kind(&id(&["c", "shop", "Procedures"])),
            Some((ObjectKind::Procedure, None))
        );
        assert_eq!(new_kind(&id(&["c", "shop", "Tables"])), None);
        assert_eq!(
            new_kind(&id(&["c", "shop", "Tables", "orders", "Triggers"])),
            Some((ObjectKind::Trigger, Some("orders".into())))
        );
        assert_eq!(
            new_kind(&id(&["c", "shop", "Tables", "orders", "Columns"])),
            None
        );
    }

    #[test]
    fn tables_groups_offer_new_table() {
        assert!(is_tables_group(&id(&["c", "shop", "Tables"])));
        assert!(!is_tables_group(&id(&["c", "shop", "Views"])));
        assert!(!is_tables_group(&id(&["c", "shop", "Tables", "orders"])));
    }

    #[test]
    fn object_rows_name_their_object() {
        // A trigger under its table and under the schema's group is the same object.
        let under_table =
            ObjectRef::from_node(&id(&["c", "shop", "Tables", "orders", "Triggers", "t1"]));
        let in_group = ObjectRef::from_node(&id(&["c", "shop", "Triggers", "t1"]));
        assert_eq!(under_table, in_group);
        assert_eq!(in_group.map(|o| o.kind), Some(ObjectKind::Trigger));
        assert_eq!(
            ObjectRef::from_node(&id(&["c", "shop", "Tables", "orders"])),
            None
        );
    }
}
