//! The Schema diagram tabs: the sidebar row under each database, opening one tab per
//! connection, its strip entry and closing it.

use std::sync::Arc;

use deadlock_core::connection::Environment;
use deadlock_core::driver::NodeKind;
use deadlock_core::schema::{Row, RowKind};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};

use super::{Conn, Front, Shell};
use crate::assets::{Icon, icon};
use crate::diagram::{DiagramEvent, DiagramTab};
use crate::theme::{Palette, tab_height};

/// The row's last id part under its database. No table name holds a NUL.
pub(super) const ROW_NAME: &str = "\0diagram";

impl Shell {
    /// Puts a "Schema diagram" row after the contents of each open database of a
    /// connected connection whose engine draws diagrams.
    pub(super) fn add_diagram_rows(&self, rows: &mut Vec<Row>) {
        let mut i = 0;
        while i < rows.len() {
            let r = &rows[i];
            let on = r.kind == RowKind::Node(NodeKind::Database)
                && r.expanded
                && matches!(self.conns.get(&r.id[0]), Some(Conn::Connected(d)) if d.capabilities().diagram);
            let id = r.id.clone();
            i += 1;
            if !on {
                continue;
            }
            while i < rows.len() && rows[i].id.len() > 2 && rows[i].id[..2] == id[..] {
                i += 1;
            }
            let mut row_id = id;
            row_id.push(ROW_NAME.to_owned());
            rows.insert(
                i,
                Row {
                    id: row_id,
                    kind: RowKind::Diagram,
                    label: "Schema diagram".into(),
                    detail: String::new(),
                    hits: Vec::new(),
                    expandable: false,
                    expanded: false,
                    dim: false,
                    primary_key: false,
                    foreign_key: false,
                },
            );
            i += 1;
        }
    }

    /// Opens connection `conn`'s diagram on `schema`, or brings its tab forward and
    /// switches it to `schema`. It loads on the sidebar's server session.
    pub(super) fn open_diagram(
        &mut self,
        conn: &str,
        schema: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(i) = self
            .diagrams
            .iter()
            .position(|(t, _)| t.read(cx).connection.id == conn)
        {
            self.diagrams[i]
                .0
                .update(cx, |t, cx| t.set_schema(schema, cx));
            self.show_tab(Front::Diagram(i), window, cx);
            return;
        }
        let (Some(config), Some(Conn::Connected(driver))) =
            (self.config(conn).cloned(), self.conns.get(conn))
        else {
            return;
        };
        let (driver, root) = (Arc::clone(driver), self.project.root().to_owned());
        let tab = cx.new(|cx| DiagramTab::new(config, root, driver, schema, window, cx));
        let sub = cx.subscribe_in(&tab, window, Self::on_diagram_event);
        self.diagrams.push((tab, sub));
        self.show_tab(Front::Diagram(self.diagrams.len() - 1), window, cx);
    }

    fn on_diagram_event(
        &mut self,
        tab: &Entity<DiagramTab>,
        e: &DiagramEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let DiagramEvent::Open(table, view) = e;
        let conn = tab.read(cx).connection.id.clone();
        self.open_table(&conn, table.clone(), *view, window, cx);
    }

    pub(super) fn close_diagram(&mut self, i: usize, cx: &mut Context<Self>) {
        if i >= self.diagrams.len() {
            return;
        }
        drop(self.diagrams.remove(i));
        self.front = match self.front {
            Front::Diagram(f) if f == i => match (
                self.diagrams.len(),
                self.configs.len(),
                self.sessions.len(),
                self.tabs.len(),
            ) {
                (0, 0, 0, 0) => Front::None,
                (0, 0, 0, n) => Front::Query(n - 1),
                (0, 0, n, _) => Front::Sessions(n - 1),
                (0, n, ..) => Front::Config(n - 1),
                (n, ..) => Front::Diagram(i.min(n - 1)),
            },
            Front::Diagram(f) if f > i => Front::Diagram(f - 1),
            other => other,
        };
        self.follow_front(cx);
    }
}

/// The strip entries of the diagram tabs.
pub(super) fn diagram_tabs(shell: &Shell, cx: &mut Context<Shell>) -> Vec<impl IntoElement> {
    let p = *cx.global::<Palette>();
    shell
        .diagrams
        .iter()
        .enumerate()
        .map(|(i, (t, _))| {
            let on = shell.front == Front::Diagram(i);
            let t = t.read(cx);
            let tip = format!("Schema diagram · {} · {}", t.schema, t.connection.name);
            let production = t.connection.environment == Environment::Production;
            let group: SharedString = format!("diagram-tab-{i}").into();
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
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.show_tab(Front::Diagram(i), window, cx)
                }))
                .child(icon(Icon::Diagram, if on { p.tx } else { p.tx3 }))
                .child("Schema diagram")
                .when(production, |d| d.child(super::production_marker(&p)))
                .child(
                    div()
                        .id(("close-diagram", i))
                        .size(px(18.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.))
                        .opacity(if on { 1. } else { 0. })
                        .group_hover(group, |s| s.opacity(1.))
                        .hover(|s| s.bg(p.hover))
                        .child(icon(Icon::Close, p.tx3).size(px(12.)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_diagram(i, cx);
                        })),
                )
        })
        .collect()
}
