//! The connections sidebar: the lazy schema tree, its keyboard navigation, refresh,
//! the schema cache and the search field.

use std::collections::HashSet;
use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};

use deadlock_core::cache::SchemaCache;
use deadlock_core::driver::{NodeKind, ObjectNode, ObjectPath};
use deadlock_core::schema::{ConnectionTree, Load, NodeId, Query, Row, RowKind};
use gpui_kit::component::input::{self, Input, InputEvent, InputState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, ClickEvent, Context, FontWeight, Global, HighlightStyle,
    InteractiveElement, IntoElement, ParentElement, Pixels, Point, ScrollStrategy, SharedString,
    StatefulInteractiveElement, Styled, StyledText, Subscription, Window, div, px, relative,
    uniform_list,
};

use super::{Conn, Shell, type_label};
use crate::app::Db;
use crate::assets::{Icon, icon};
use crate::theme::{Palette, RADIUS, row_height, small_size};
use crate::ui::square_button;

/// The schema cache shared by every window; `None` when it could not be opened.
pub(crate) struct Cache(pub Option<Arc<Mutex<SchemaCache>>>);

impl Global for Cache {}

/// The search field while it is open.
pub(super) struct Search {
    input: gpui_kit::Entity<InputState>,
    /// Nodes opened or closed by hand against the search's own choice.
    flipped: HashSet<NodeId>,
    /// The selection and scroll position to restore on close.
    saved: (Option<NodeId>, Point<Pixels>),
    _sub: Subscription,
}

impl Shell {
    pub(super) fn tree_connections(&mut self) {
        self.tree.connections = self
            .project
            .config
            .connections
            .iter()
            .map(|c| ConnectionTree {
                id: c.id.clone(),
                name: c.name.clone(),
                nodes: self
                    .tree
                    .connections
                    .iter_mut()
                    .find(|t| t.id == c.id)
                    .and_then(|t| t.nodes.take()),
            })
            .collect();
    }

    /// Paints the tree from the cache. Nodes the server already sent win.
    pub(super) fn load_cache(&mut self, cx: &mut Context<Self>) {
        let Some(cache) = cx.global::<Cache>().0.clone() else {
            return;
        };
        let root = self.project.root().to_owned();
        let ids: Vec<String> = self.tree.connections.iter().map(|c| c.id.clone()).collect();
        let rx = cx.global::<Db>().0.spawn_blocking(move || {
            let cache = cache.lock().unwrap_or_else(PoisonError::into_inner);
            ids.into_iter()
                .map(|id| {
                    let rows = cache.load(&root, &id);
                    (id, rows)
                })
                .collect::<Vec<_>>()
        });
        cx.spawn(async move |this, cx| {
            let Ok(list) = rx.await else { return };
            let _ = this.update(cx, |this, cx| {
                for (conn, rows) in list {
                    match rows {
                        Ok(rows) => {
                            for (path, nodes) in rows {
                                let mut id = vec![conn.clone()];
                                id.extend(path);
                                if !this.fresh.contains(&id) {
                                    this.tree.set_children(&id, nodes);
                                }
                            }
                        }
                        Err(e) => eprintln!("deadlock: {e}"),
                    }
                }
                this.rebuild(cx);
            });
        })
        .detach();
    }

    fn save_cache(&self, id: &[String], nodes: &[ObjectNode], cx: &Context<Self>) {
        let Some(cache) = cx.global::<Cache>().0.clone() else {
            return;
        };
        // Keys change too often for a cached tree to be worth showing.
        if self.is_redis(&id[0]) {
            return;
        }
        let (root, path) = (self.project.root().to_owned(), id.to_vec());
        // The databases are saved by name; each database keeps its own row.
        let nodes: Vec<ObjectNode> = if id.len() == 1 {
            nodes
                .iter()
                .map(|n| ObjectNode {
                    children: None,
                    ..n.clone()
                })
                .collect()
        } else {
            nodes.to_vec()
        };
        drop(cx.global::<Db>().0.spawn_blocking(move || {
            let mut cache = cache.lock().unwrap_or_else(PoisonError::into_inner);
            if let Err(e) = cache.save(&root, &path[0], &path[1..], &nodes) {
                eprintln!("deadlock: {e}");
            }
        }));
    }

    /// Asks the connection's driver for the children of `id`.
    pub(super) fn load(&mut self, id: NodeId, cx: &mut Context<Self>) {
        let Some(Conn::Connected(driver)) = id.first().and_then(|c| self.conns.get(c)) else {
            return;
        };
        let driver = Arc::clone(driver);
        self.tree.status.insert(id.clone(), Load::Loading);
        let path = ObjectPath(id[1..].to_vec());
        let rx = cx
            .global::<Db>()
            .0
            .spawn(async move { driver.children(&path).await.map_err(|e| e.to_string()) });
        cx.spawn(async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err("schema task failed".into()));
            let _ = this.update(cx, |this, cx| this.loaded(id, result, cx));
        })
        .detach();
    }

    fn loaded(
        &mut self,
        id: NodeId,
        result: Result<Vec<ObjectNode>, String>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(nodes) => {
                self.tree.status.remove(&id);
                self.save_cache(&id, &nodes, cx);
                if self.tree.set_children(&id, nodes) {
                    self.fresh.insert(id.clone());
                }
                if id.len() == 1 {
                    // A reloaded connection reloads its open databases.
                    let open: Vec<NodeId> = self
                        .tree
                        .expanded
                        .iter()
                        .filter(|e| e.len() == 2 && e[0] == id[0] && !self.tree.is_gone(e))
                        .cloned()
                        .collect();
                    for db in open {
                        self.load(db, cx);
                    }
                }
                self.tree.prune();
                while let Some(sel) = &self.selected
                    && sel.len() > 1
                    && !super::sessions::is_screen_row(sel)
                    && self.tree.is_gone(sel)
                {
                    self.selected = Some(sel[..sel.len() - 1].to_vec());
                }
            }
            Err(e) => {
                self.tree.status.insert(id, Load::Failed(e));
            }
        }
        self.rebuild(cx);
    }

    /// Reloads a connection after it connects or on Refresh.
    pub(super) fn reload(&mut self, conn: &str, cx: &mut Context<Self>) {
        self.fresh.retain(|f| f[0] != conn);
        self.load(vec![conn.to_owned()], cx);
    }

    /// F5 and the toolbar button: reloads the active connection, connecting it if needed.
    pub(super) fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.active.clone() else {
            return;
        };
        match self.conns.get(&id) {
            Some(Conn::Connected(_)) => self.reload(&id, cx),
            Some(Conn::Connecting) => {}
            _ => self.connect(&id, cx),
        }
    }

    /// Loads what an opened node needs: a connection connects, a database not fetched
    /// this session reloads, anything without children asks the driver.
    fn ensure_loaded(&mut self, id: &NodeId, cx: &mut Context<Self>) {
        if id.len() == 1 {
            match self.conns.get(&id[0]) {
                Some(Conn::Connected(_)) if !self.fresh.contains(id) => self.load(id.clone(), cx),
                Some(Conn::Connected(_) | Conn::Connecting) => {}
                _ => self.connect(&id[0], cx),
            }
            return;
        }
        let needed = self.tree.node(id).is_some_and(|n| {
            n.children.is_none() || n.kind == NodeKind::Database && !self.fresh.contains(id)
        });
        if needed && self.tree.status.get(id) != Some(&Load::Loading) {
            self.load(id.clone(), cx);
        }
    }

    fn toggle(&mut self, id: &NodeId, cx: &mut Context<Self>) {
        let row_open = self.rows.iter().find(|r| &r.id == id).map(|r| r.expanded);
        if let Some(search) = self.search.as_mut().filter(|_| self.query_active) {
            if !search.flipped.remove(id) {
                search.flipped.insert(id.clone());
            }
        } else if !self.tree.expanded.remove(id) {
            self.tree.expanded.insert(id.clone());
        }
        if row_open == Some(false) {
            self.ensure_loaded(id, cx);
        }
        self.rebuild(cx);
    }

    /// Rebuilds the rows from the tree and the search, then repaints.
    pub(super) fn rebuild(&mut self, cx: &mut Context<Self>) {
        let query = self
            .search
            .as_ref()
            .and_then(|s| Query::new(&s.input.read(cx).value()));
        self.query_active = query.is_some();
        let text = self
            .search
            .as_ref()
            .map(|s| s.input.read(cx).value().trim().to_owned())
            .unwrap_or_default();
        self.search_keys(&text, cx);
        (self.rows, self.found) = match (&query, &self.search) {
            (Some(q), Some(s)) => {
                let (mut rows, mut found) = self.tree.search(q, &s.flipped);
                self.search_rows(&mut rows, &mut found, &text);
                (rows, found)
            }
            _ => {
                let mut rows = self.tree.rows();
                self.add_diagram_rows(&mut rows);
                self.add_sessions_rows(&mut rows);
                (rows, 0)
            }
        };
        cx.notify();
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix) else { return };
        self.selected = Some(row.id.clone());
        if row.kind == RowKind::Connection {
            let id = row.id[0].clone();
            self.activate(&id, false, cx);
        }
        self.list.scroll_to_item(ix, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn selected_ix(&self) -> Option<usize> {
        let sel = self.selected.as_ref()?;
        self.rows.iter().position(|r| &r.id == sel)
    }

    fn on_click_row(
        &mut self,
        ix: usize,
        e: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.tree_focus, cx);
        let row = self.rows.get(ix).map(|r| (r.kind, r.id.clone()));
        match row {
            Some((RowKind::Node(NodeKind::More), id)) => return self.load_more(id, cx),
            Some((RowKind::Node(NodeKind::Key), id))
                if e.modifiers().control || e.modifiers().platform =>
            {
                return self.toggle_mark(id, cx);
            }
            Some((RowKind::Node(NodeKind::Key), _)) if e.click_count() < 2 => {
                self.keys.marked.clear();
            }
            _ => {}
        }
        self.select(ix, cx);
        // A link, as in the design: one click opens it.
        let link = self.rows.get(ix).is_some_and(|r| {
            matches!(
                r.kind,
                RowKind::Sessions | RowKind::Config | RowKind::Diagram
            )
        });
        if (link || e.click_count() >= 2) && self.open_selected_table(window, cx) {
            return;
        }
        if e.click_count() >= 2
            && let Some(row) = self.rows.get(ix).filter(|r| r.expandable)
        {
            let id = row.id.clone();
            self.toggle(&id, cx);
        }
    }

    /// Double-click or Enter on a table or view: opens its data tab. Returns whether
    /// the selected row was one.
    pub(super) fn open_selected_table(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(row) = self.selected_ix().and_then(|ix| self.rows.get(ix)) else {
            return false;
        };
        if row.kind == RowKind::Sessions {
            let conn = row.id[0].clone();
            self.open_sessions(&conn, window, cx);
            return true;
        }
        if row.kind == RowKind::Config {
            let conn = row.id[0].clone();
            self.open_config(&conn, window, cx);
            return true;
        }
        if row.kind == RowKind::Diagram {
            let (conn, schema) = (row.id[0].clone(), row.id[1].clone());
            self.open_diagram(&conn, schema, window, cx);
            return true;
        }
        if row.kind == RowKind::Node(NodeKind::Key) {
            let id = row.id.clone();
            self.open_key(&id, window, cx);
            return true;
        }
        let (RowKind::Node(NodeKind::Table | NodeKind::View), [conn, db, _, table]) =
            (row.kind, row.id.as_slice())
        else {
            return false;
        };
        let (conn, table) = (
            conn.clone(),
            deadlock_core::edit::TableRef {
                database: db.clone(),
                table: table.clone(),
            },
        );
        self.open_table(&conn, table, crate::query::View::Data, window, cx);
        true
    }

    pub(super) fn on_nav(&mut self, nav: Nav, cx: &mut Context<Self>) {
        let last = self.rows.len().saturating_sub(1);
        let current = self.selected_ix();
        let row = current.and_then(|ix| self.rows.get(ix));
        let target = match (nav, current, row) {
            (_, _, _) if self.rows.is_empty() => return,
            (Nav::First, ..) | (Nav::Next | Nav::Prev, None, _) => 0,
            (Nav::Last, ..) => last,
            (Nav::Next, Some(ix), _) => (ix + 1).min(last),
            (Nav::Prev, Some(ix), _) => ix.saturating_sub(1),
            (Nav::Expand, Some(ix), Some(r)) if r.expandable && !r.expanded => {
                let id = r.id.clone();
                self.toggle(&id, cx);
                ix
            }
            (Nav::Expand, Some(ix), Some(r)) if r.expanded => (ix + 1).min(last),
            (Nav::Collapse, Some(ix), Some(r)) if r.expanded => {
                let id = r.id.clone();
                self.toggle(&id, cx);
                ix
            }
            (Nav::Collapse, Some(ix), Some(r)) if r.id.len() > 1 => {
                let parent = &r.id[..r.id.len() - 1];
                self.rows.iter().position(|p| p.id == parent).unwrap_or(ix)
            }
            (Nav::Toggle, Some(ix), Some(r)) if r.expandable => {
                let id = r.id.clone();
                self.toggle(&id, cx);
                ix
            }
            (_, Some(ix), _) => ix,
            (_, None, _) => return,
        };
        self.select(target, cx);
    }

    pub(super) fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(s) = &self.search {
            s.input.update(cx, |s, cx| s.focus(window, cx));
            return;
        }
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search connections and objects"));
        let sub = cx.subscribe(&input, |this, _, e: &InputEvent, cx| {
            if matches!(e, InputEvent::Change) {
                if let Some(s) = &mut this.search {
                    s.flipped.clear();
                }
                this.rebuild(cx);
            }
        });
        input.update(cx, |s, cx| s.focus(window, cx));
        self.search = Some(Search {
            input,
            flipped: HashSet::new(),
            saved: (
                self.selected.clone(),
                self.list.0.borrow().base_handle.offset(),
            ),
            _sub: sub,
        });
        cx.notify();
    }

    /// Esc and the clear button: the tree comes back as it was before the search.
    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(search) = self.search.take() else {
            return;
        };
        self.clear_key_searches();
        let (selected, offset) = search.saved;
        self.selected = selected;
        self.rebuild(cx);
        self.list.0.borrow().base_handle.set_offset(offset);
        window.focus(&self.tree_focus, cx);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Nav {
    Prev,
    Next,
    First,
    Last,
    Expand,
    Collapse,
    Toggle,
}

/// `.t` rows: chevron at `4 + 16 × depth`; columns sit at 112 px, or 90 px with a key icon.
fn indent(row: &Row) -> Pixels {
    let key = row.primary_key || row.foreign_key;
    match row.kind {
        RowKind::Node(NodeKind::Column) if key => px(90.),
        RowKind::Node(NodeKind::Column) => px(112.),
        // Redis keys line up with their namespace's name, as in the design.
        RowKind::Node(NodeKind::Key | NodeKind::More) => {
            px((22. + 16. * row.depth().saturating_sub(1) as f32).max(58.))
        }
        _ => px(4. + 16. * row.depth() as f32),
    }
}

fn label(text: &str, hits: &[Range<usize>], p: &Palette) -> StyledText {
    // `.hit`: semibold in the link color.
    let style = HighlightStyle {
        font_weight: Some(FontWeight::SEMIBOLD),
        color: Some(p.link),
        ..HighlightStyle::default()
    };
    StyledText::new(SharedString::from(text.to_owned()))
        .with_highlights(hits.iter().map(|r| (r.clone(), style)))
}

fn row_icon(row: &Row, p: &Palette) -> Option<gpui_kit::Svg> {
    Some(match row.kind {
        RowKind::Connection => icon(Icon::Database, p.tx3),
        RowKind::Node(NodeKind::Database) => icon(Icon::Folder, p.tx3),
        RowKind::Node(NodeKind::Table | NodeKind::View) => icon(Icon::Table, p.tx3),
        RowKind::Node(NodeKind::Routine) => icon(Icon::Routine, p.tx3),
        RowKind::Node(NodeKind::Column) if row.primary_key => icon(Icon::PrimaryKey, p.warn),
        RowKind::Node(NodeKind::Column) if row.foreign_key => icon(Icon::ForeignKey, p.link),
        RowKind::Sessions => icon(Icon::Sessions, p.tx3),
        RowKind::Config => icon(Icon::Config, p.tx3),
        RowKind::Diagram => icon(Icon::Diagram, p.tx3),
        _ => return None,
    })
}

fn render_row(shell: &Shell, ix: usize, row: &Row, cx: &mut Context<Shell>) -> AnyElement {
    let p = *cx.global::<Palette>();
    // A screen row is on while its tab is in front.
    let selected = match (row.kind, shell.front) {
        (RowKind::Sessions, super::Front::Sessions(i)) => shell
            .sessions
            .get(i)
            .is_some_and(|(t, _)| t.read(cx).connection.id == row.id[0]),
        (RowKind::Sessions, _) => false,
        (RowKind::Config, super::Front::Config(i)) => shell
            .configs
            .get(i)
            .is_some_and(|(t, _)| t.read(cx).connection.id == row.id[0]),
        (RowKind::Config, _) => false,
        (RowKind::Diagram, super::Front::Diagram(i)) => {
            shell.diagrams.get(i).is_some_and(|(t, _)| {
                let t = t.read(cx);
                t.connection.id == row.id[0] && t.schema == row.id[1]
            })
        }
        (RowKind::Diagram, _) => false,
        _ => shell.selected.as_ref() == Some(&row.id),
    };
    let is_conn = row.kind == RowKind::Connection;
    let mono = matches!(row.kind, RowKind::Node(NodeKind::Key | NodeKind::Namespace));
    let more = row.kind == RowKind::Node(NodeKind::More);
    let marked = shell.keys.marked.contains(&row.id);
    let redis_menu = matches!(
        row.kind,
        RowKind::Node(NodeKind::Key | NodeKind::Namespace | NodeKind::Database)
    ) && shell.is_redis(&row.id[0]);
    // "Load more of user:" under a namespace, "Load more keys" under a database.
    let shown_label = if more && row.label == deadlock_core::redis::MORE {
        format!(
            "Load more of {}",
            row.id
                .get(row.id.len().saturating_sub(2))
                .cloned()
                .unwrap_or_default()
        )
    } else if more {
        "Load more".to_owned()
    } else {
        row.label.clone()
    };
    let active = is_conn && shell.active.as_deref() == Some(row.id[0].as_str());
    let color = match row.kind {
        RowKind::Error => p.danger,
        RowKind::Node(NodeKind::More) => p.link,
        RowKind::Loading => p.tx3,
        _ if row.dim => p.tx3,
        RowKind::Connection if !row.expanded && !active => p.tx2,
        _ => p.tx,
    };
    let detail = if is_conn && !row.dim {
        shell.state_label(&row.id[0])
    } else {
        row.detail.clone()
    };
    let chevron = row.expandable.then(|| {
        let id = row.id.clone();
        div()
            .id(("chevron", ix))
            .flex_none()
            .child(icon(
                if row.expanded {
                    Icon::ChevronDown
                } else {
                    Icon::ChevronRight
                },
                p.tx3,
            ))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                window.focus(&this.tree_focus, cx);
                this.select(ix, cx);
                this.toggle(&id, cx);
            }))
    });
    // Leaves above the column level keep the chevron's place, so icons line up.
    let spacer = !row.expandable
        && !matches!(
            row.kind,
            RowKind::Node(NodeKind::Column | NodeKind::Key | NodeKind::More)
        );
    let tip: Option<SharedString> = match (row.kind, shell.conns.get(&row.id[0])) {
        (RowKind::Connection, Some(Conn::Failed(e))) => Some(e.clone().into()),
        (RowKind::Error, _) => Some(row.label.clone().into()),
        // The count may not fit beside the label.
        (RowKind::Node(NodeKind::More), _) => {
            Some(format!("{shown_label}: {} keys loaded", row.detail).into())
        }
        _ => None,
    };
    let element = div()
        .id(("tree-row", ix))
        .h(row_height() + px(1.))
        .pb(px(1.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .size_full()
                .pl(indent(row))
                .pr(px(8.))
                .rounded(RADIUS)
                .whitespace_nowrap()
                .text_color(color)
                .when(selected || marked, |d| {
                    d.bg(p.sel).font_weight(FontWeight::MEDIUM)
                })
                .when(is_conn && row.expanded, |d| {
                    d.font_weight(FontWeight::MEDIUM)
                })
                .when(!selected, |d| d.hover(|s| s.bg(p.hover)))
                .children(chevron)
                .when(spacer, |d| d.child(div().w(px(16.)).flex_none()))
                .children(row_icon(row, &p))
                // Names keep their width up to three quarters of the row; the type
                // label gives way first.
                .child(
                    div()
                        // The waiting count matters more than the screen's name.
                        .when(row.kind == RowKind::Sessions, |d| {
                            d.flex_shrink(1.).min_w_0()
                        })
                        .when(row.kind != RowKind::Sessions, |d| d.flex_shrink_0())
                        .max_w(relative(0.75))
                        .overflow_hidden()
                        .text_ellipsis()
                        .when(mono, |d| {
                            d.font_family(crate::theme::MONO_FONT)
                                .text_size(crate::theme::data_size())
                        })
                        .child(label(&shown_label, &row.hits, &p)),
                )
                // The type label gives way before the name does.
                .when(!detail.is_empty(), |d| {
                    d.child(
                        type_label(&detail, &p)
                            .when(row.kind == RowKind::Sessions, |d| d.flex_none())
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis(),
                    )
                }),
        )
        .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
            this.on_click_row(ix, e, window, cx);
        }))
        .when_some(tip, |d, tip| {
            d.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
        });
    if !redis_menu {
        return element.into_any_element();
    }
    let (row, shell_entity) = (row.clone(), cx.entity());
    element
        .context_menu(move |menu, _, cx| {
            shell_entity
                .read(cx)
                .key_menu(&row, menu, shell_entity.clone())
        })
        .into_any_element()
}

/// The search field from the design's search-open state.
fn search_field(
    search: &Search,
    found: Option<usize>,
    cx: &mut Context<Shell>,
) -> impl IntoElement {
    let p = *cx.global::<Palette>();
    // The 2 px `--sel` outline sits outside the 1 px blue border.
    div()
        .flex_none()
        .mx(px(-2.))
        .mt(px(-2.))
        .mb(px(2.))
        .border_2()
        .border_color(p.sel)
        .rounded(px(8.))
        .on_action(cx.listener(|this, _: &input::Escape, window, cx| {
            this.close_search(window, cx);
        }))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .h(row_height())
                .pl(px(8.))
                .pr(px(4.))
                .border_1()
                .border_color(p.blue)
                .rounded(RADIUS)
                .bg(p.bg)
                .child(icon(Icon::Search, p.tx3))
                .child(
                    div().flex_1().min_w_0().child(
                        Input::new(&search.input)
                            .appearance(false)
                            .h(px(24.))
                            .px_0(),
                    ),
                )
                .when_some(found, |d, n| {
                    d.child(
                        div()
                            .flex_none()
                            .text_size(small_size())
                            .text_color(p.tx3)
                            .child(format!("{n} found")),
                    )
                })
                .child(
                    div()
                        .id("clear-search")
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .size(px(20.))
                        .rounded(px(4.))
                        .hover(|s| s.bg(p.hover))
                        .child(icon(Icon::Close, p.tx3))
                        .tooltip(|window, cx| Tooltip::new("Clear search (Esc)").build(window, cx))
                        .on_click(cx.listener(|this, _, window, cx| this.close_search(window, cx))),
                ),
        )
}

pub(super) fn sidebar(shell: &Shell, cx: &mut Context<Shell>) -> impl IntoElement {
    let p = *cx.global::<Palette>();
    let empty = shell.project.config.connections.is_empty();
    let found = shell.query_active.then_some(shell.found);
    div()
        .size_full()
        .flex()
        .flex_col()
        .gap(px(4.))
        .p(px(8.))
        .bg(p.panel)
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_between()
                .h(px(28.))
                .pl(px(8.))
                .child(
                    div()
                        .text_size(small_size())
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.tx2)
                        .child("Connections"),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(2.))
                        .child(
                            square_button(
                                "search",
                                Icon::Search,
                                format!(
                                    "Search connections and objects ({})",
                                    crate::keymap::label("search", cx)
                                ),
                                cx,
                            )
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.open_search(window, cx);
                                },
                            )),
                        )
                        .child(
                            square_button("add-connection", Icon::Plus, "Add connection", cx)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.new_connection(window, cx);
                                })),
                        ),
                ),
        )
        .when_some(shell.search.as_ref(), |d, s| {
            d.child(search_field(s, found, cx))
        })
        .children(super::keys::search_status(shell, cx))
        .when(empty, |d| {
            d.child(
                div()
                    .px(px(8.))
                    .py(px(4.))
                    .text_color(p.tx3)
                    .line_height(gpui_kit::relative(1.5))
                    .child("No connections yet. Add one with +."),
            )
        })
        .child(
            div()
                .id("tree")
                .key_context("Tree")
                .track_focus(&shell.tree_focus)
                .flex_1()
                .min_h_0()
                .child(
                    uniform_list(
                        "tree-rows",
                        shell.rows.len(),
                        cx.processor(|this, range: Range<usize>, _, cx| {
                            range
                                .filter_map(|ix| {
                                    let row = this.rows.get(ix)?.clone();
                                    Some(render_row(this, ix, &row, cx))
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .track_scroll(&shell.list)
                    .size_full(),
                ),
        )
        .children(super::keys::job_status(shell, cx))
}
