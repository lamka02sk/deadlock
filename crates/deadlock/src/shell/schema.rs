//! The toolbar's schema chip and its menu: the schema the front tab's statements run
//! in, switched with `USE` / `ALTER SESSION SET CURRENT_SCHEMA` / `SELECT n` on the
//! tab's own session. Also where the console opens: on the connection and schema of
//! what the user clicked last, a sidebar row or the tab in front.

use std::time::Duration;

use deadlock_core::connection::ConnectionConfig;
use deadlock_core::driver::NodeKind;
use deadlock_core::schema::RowKind;
use deadlock_core::session_schema::{default_schema, switch_verb};
use gpui_kit::component::input::{self, Input, InputEvent, InputState};
use gpui_kit::component::popover::Popover;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Animation, AnimationExt as _, AnyElement, AppContext as _, Context, Entity, FocusHandle,
    Focusable as _, FontWeight, InteractiveElement, IntoElement, ParentElement, Pixels,
    SharedString, StatefulInteractiveElement as _, Styled, StyledText, Subscription, TextRun,
    Transformation, Window, actions, div, font, percentage, px,
};

use super::{Conn, Front, Shell, Trigger, text_width};
use crate::assets::{Icon, icon};
use crate::query::QueryTab;
use crate::theme::{Palette, mono_font, small_size, ui_font};
use crate::ui::{Clickable as _, Kind, button};

actions!(schema_menu, [SchemaPrev, SchemaNext, SchemaPick]);

/// The key context of the open menu.
pub(crate) const CONTEXT: &str = "SchemaMenu";

/// The longest schema name the chip shows before it cuts.
const NAME_MAX: f32 = 160.;

/// More schemas than this: the menu shows a filter field.
const FILTER_FROM: usize = 8;

/// The schema menu's state, kept between openings.
pub(super) struct SchemaMenu {
    pub(super) open: bool,
    filter: Entity<InputState>,
    /// Takes the keys while the menu has no filter field.
    focus: FocusHandle,
    /// The highlighted row of the filtered list.
    selected: usize,
    _sub: Subscription,
}

impl SchemaMenu {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Shell>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter schemas"));
        let sub = cx.subscribe_in(
            &filter,
            window,
            |this: &mut Shell, _, e: &InputEvent, _, cx| match e {
                InputEvent::Change => {
                    this.schema_menu.selected = 0;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.pick_highlighted(cx),
                _ => {}
            },
        );
        Self {
            open: false,
            filter,
            focus: cx.focus_handle(),
            selected: 0,
            _sub: sub,
        }
    }
}

/// What a click or a front tab says the console should open on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Target {
    pub(super) connection: String,
    /// `None`: keep the console's schema as it is.
    pub(super) schema: Option<String>,
}

/// The schema a sidebar row belongs to: its database node, the second step of its
/// path. Connection rows and the Sessions / configuration screens have none.
pub(crate) fn row_schema(kind: RowKind, id: &[String]) -> Option<String> {
    if matches!(kind, RowKind::Sessions | RowKind::Config) {
        return None;
    }
    id.get(1).filter(|s| !s.is_empty()).cloned()
}

impl Shell {
    /// The front tab, when it has a schema to pick (a file or console on a connection).
    fn schema_tab(&self, cx: &gpui_kit::App) -> Option<&Entity<QueryTab>> {
        self.front_tab().filter(|t| t.read(cx).has_schema())
    }

    /// The schemas the sidebar lists for `config`, without hidden ones. `None` until
    /// the connection's tree has loaded.
    fn schema_names(&self, config: &ConnectionConfig) -> Option<Vec<String>> {
        let nodes = self.tree.children(std::slice::from_ref(&config.id))?;
        Some(
            nodes
                .iter()
                .filter(|n| n.kind == NodeKind::Database && !config.is_hidden(&n.name))
                .map(|n| n.name.clone())
                .collect(),
        )
    }

    /// The menu's rows after the filter.
    fn schema_rows(&self, names: &[String], cx: &gpui_kit::App) -> Vec<String> {
        let text = self
            .schema_menu
            .filter
            .read(cx)
            .value()
            .trim()
            .to_lowercase();
        names
            .iter()
            .filter(|n| names.len() <= FILTER_FROM || n.to_lowercase().contains(&text))
            .cloned()
            .collect()
    }

    fn set_schema_menu(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.schema_menu.open = open;
        if open {
            self.schema_menu
                .filter
                .update(cx, |s, cx| s.set_value("", window, cx));
            let tab = self.schema_tab(cx).map(|t| t.read(cx));
            let (config, current) = match tab {
                Some(t) => (t.connection.clone(), t.schema().map(str::to_owned)),
                None => (None, None),
            };
            if let Some(config) = config {
                let names = self.schema_names(&config);
                self.schema_menu.selected = names
                    .iter()
                    .flatten()
                    .position(|n| Some(n) == current.as_ref())
                    .unwrap_or(0);
                // The list comes from the sidebar's tree: with nothing cached, load it.
                if names.is_none()
                    && !matches!(
                        self.conns.get(&config.id),
                        Some(Conn::Connected(_) | Conn::Connecting)
                    )
                {
                    self.connect(&config.id, cx);
                }
            }
        }
        cx.notify();
    }

    fn schema_nav(&mut self, down: bool, cx: &mut Context<Self>) {
        let n = self.menu_rows(cx).len();
        if n == 0 {
            return;
        }
        let s = &mut self.schema_menu.selected;
        *s = if down {
            (*s + 1).min(n - 1)
        } else {
            s.saturating_sub(1)
        };
        cx.notify();
    }

    fn menu_rows(&self, cx: &gpui_kit::App) -> Vec<String> {
        let Some(config) = self
            .schema_tab(cx)
            .and_then(|t| t.read(cx).connection.clone())
        else {
            return Vec::new();
        };
        let names = self.schema_names(&config).unwrap_or_default();
        self.schema_rows(&names, cx)
    }

    fn pick_highlighted(&mut self, cx: &mut Context<Self>) {
        if let Some(name) = self.menu_rows(cx).get(self.schema_menu.selected).cloned() {
            self.pick_schema(name, cx);
        }
    }

    /// Closes the menu and switches the front tab to `schema`.
    fn pick_schema(&mut self, schema: String, cx: &mut Context<Self>) {
        self.schema_menu.open = false;
        if let Some(tab) = self.schema_tab(cx).cloned()
            && tab.read(cx).schema() != Some(schema.as_str())
        {
            tab.update(cx, |t, cx| t.switch_schema(schema, cx));
        }
        cx.notify();
    }

    /// Where the Console button and Ctrl+Shift+N open the console: the connection and
    /// schema of what was clicked last, a sidebar row or the tab in front; else the
    /// active connection.
    pub(super) fn console_target(&self, cx: &gpui_kit::App) -> Option<Target> {
        let picked = if self.sidebar_last {
            self.selected.as_ref().and_then(|id| {
                let row = self.rows.iter().find(|r| &r.id == id);
                let kind = row.map_or(RowKind::Connection, |r| r.kind);
                Some(Target {
                    connection: self.config(id.first()?)?.id.clone(),
                    schema: row_schema(kind, id),
                })
            })
        } else {
            self.front_target(cx)
        };
        picked.or_else(|| {
            Some(Target {
                connection: self.active(cx)?,
                schema: None,
            })
        })
    }

    /// The front tab's connection and schema: a table, object or diagram tab names
    /// its schema, a file or console has its current one.
    fn front_target(&self, cx: &gpui_kit::App) -> Option<Target> {
        let (connection, schema) = match self.front {
            Front::Query(i) => {
                let t = self.tabs.get(i)?.0.read(cx);
                let schema = match &t.kind {
                    crate::query::Kind::Table(r) => Some(r.database.clone()),
                    _ => t.schema().map(str::to_owned),
                };
                (t.connection.as_ref()?.id.clone(), schema)
            }
            Front::Object(i) => {
                let t = self.objects.get(i)?.0.read(cx);
                (t.connection.id.clone(), Some(t.object.schema.clone()))
            }
            Front::Diagram(i) => {
                let t = self.diagrams.get(i)?.0.read(cx);
                (t.connection.id.clone(), Some(t.schema.clone()))
            }
            Front::Sessions(i) => (self.sessions.get(i)?.0.read(cx).connection.id.clone(), None),
            Front::Config(i) => (self.configs.get(i)?.0.read(cx).connection.id.clone(), None),
            Front::None | Front::Form | Front::Import | Front::Settings => return None,
        };
        Some(Target {
            connection,
            schema: schema.filter(|s| !s.is_empty()),
        })
    }
}

/// The chip's text and whether it is the "No schema" placeholder; `None` hides the
/// chip (no front tab with a schema to pick).
pub(super) fn chip_label(shell: &Shell, cx: &gpui_kit::App) -> Option<(String, bool)> {
    let t = shell.schema_tab(cx)?.read(cx);
    Some(match t.schema() {
        Some(s) => (s.to_owned(), false),
        None => ("No schema".to_owned(), true),
    })
}

/// The chip's width, as the toolbar's label-dropping maths counts it.
pub(super) fn chip_width(window: &Window, label: &str) -> Pixels {
    // `.b`: 1 px border and 8 px padding a side, two 16 px icons, two 6 px gaps.
    px(2. + 16. + 16. + 6. + 16. + 6.)
        + text_width(window, label, FontWeight::NORMAL).min(px(NAME_MAX))
}

/// The schema chip and its menu, before Run.
pub(super) fn chip(shell: &Shell, cx: &mut Context<Shell>) -> AnyElement {
    let p = *cx.global::<Palette>();
    let Some((label, none)) = chip_label(shell, cx) else {
        return div().into_any_element();
    };
    let Some(tab) = shell.schema_tab(cx) else {
        return div().into_any_element();
    };
    let t = tab.read(cx);
    let switching = t.is_switching();
    let enabled = !t.is_running() && !switching;
    let config = t.connection.clone();
    let current = t.schema().map(str::to_owned);
    let open = shell.schema_menu.open && enabled;
    let lead = if switching {
        icon(Icon::Spinner, p.tx3)
            .with_animation(
                "schema-switching",
                Animation::new(Duration::from_millis(800)).repeat(),
                |s, d| s.with_transformation(Transformation::rotate(percentage(d))),
            )
            .into_any_element()
    } else {
        icon(Icon::Database, p.tx3).into_any_element()
    };
    let trigger = button("schema", Kind::Ghost, enabled, "", cx)
        .text_color(p.tx)
        .when(open, |b| b.bg(p.sel))
        .child(lead)
        .child(
            div()
                .max_w(px(NAME_MAX))
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .when(none, |d| d.text_color(p.tx3))
                .child(SharedString::from(label.clone())),
        )
        .child(icon(Icon::ChevronDown, p.tx3));
    let tip = SharedString::from(if none {
        "Schema statements run in: none".to_owned()
    } else {
        format!("Schema statements run in: {label}")
    });
    if !enabled {
        return trigger
            .tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
            })
            .into_any_element();
    }
    let trigger = trigger.when(!open, |b| {
        b.tooltip(move |window, cx| {
            gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
        })
    });
    let Some(config) = config else {
        return trigger.into_any_element();
    };
    let names = shell.schema_names(&config);
    let loading = names.is_none();
    let names = names.unwrap_or_default();
    let filtered = names.len() > FILTER_FROM;
    let rows = shell.schema_rows(&names, cx);
    let default = default_schema(&config);
    let verb = switch_verb(config.engine);
    let selected = shell.schema_menu.selected;
    let filter = shell.schema_menu.filter.clone();
    let focus = if filtered {
        filter.read(cx).focus_handle(cx)
    } else {
        shell.schema_menu.focus.clone()
    };
    let menu_focus = shell.schema_menu.focus.clone();
    let this = cx.entity().downgrade();
    Popover::new("schema-menu")
        .p(px(6.))
        .w(px(280.))
        .open(open)
        .track_focus(&focus)
        .on_open_change(cx.listener(|this, open: &bool, window, cx| {
            this.set_schema_menu(*open, window, cx);
        }))
        .trigger(Trigger(trigger, open, ""))
        .content(move |_, _, _| {
            let list = rows.iter().enumerate().map(|(ix, name)| {
                let this = this.clone();
                let pick = name.clone();
                let on = Some(name) == current.as_ref();
                div()
                    .id(("schema", ix))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(30.))
                    .px(px(12.))
                    .rounded(px(4.))
                    .text_color(p.tx)
                    .when(ix == selected, |d| d.bg(p.hover))
                    .hover(|s| s.bg(p.hover))
                    .when(on, |d| d.font_weight(FontWeight::MEDIUM))
                    .clickable(move |_, _, cx| {
                        let _ = this.update(cx, |this, cx| this.pick_schema(pick.clone(), cx));
                    })
                    .child(if on {
                        icon(Icon::Commit, p.blue).into_any_element()
                    } else {
                        div().w(px(16.)).flex_none().into_any_element()
                    })
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(SharedString::from(name.clone())),
                    )
                    .when(default.as_ref() == Some(name), |d| {
                        d.child(
                            div()
                                .flex_none()
                                .px(px(5.))
                                .rounded(px(4.))
                                .border_1()
                                .border_color(p.line2)
                                .text_size(px(11.))
                                .line_height(px(16.))
                                .text_color(p.tx3)
                                .font_weight(FontWeight::NORMAL)
                                .child("default"),
                        )
                    })
            });
            let empty = if loading {
                Some("Loading schemas…")
            } else if rows.is_empty() {
                Some(if names.is_empty() {
                    "No schemas to show."
                } else {
                    "No schema matches."
                })
            } else {
                None
            };
            let (prev, next, pick) = (this.clone(), this.clone(), this.clone());
            let close = this.clone();
            div()
                .key_context(CONTEXT)
                .track_focus(&menu_focus)
                .flex()
                .flex_col()
                .on_action(move |_: &SchemaPrev, _, cx| {
                    let _ = prev.update(cx, |this, cx| this.schema_nav(false, cx));
                })
                .on_action(move |_: &SchemaNext, _, cx| {
                    let _ = next.update(cx, |this, cx| this.schema_nav(true, cx));
                })
                .on_action(move |_: &SchemaPick, _, cx| {
                    let _ = pick.update(cx, |this, cx| this.pick_highlighted(cx));
                })
                .on_action(move |_: &input::Escape, _, cx| {
                    let _ = close.update(cx, |this, cx| {
                        this.schema_menu.open = false;
                        cx.notify();
                    });
                })
                .when(filtered, |d| {
                    d.child(
                        div()
                            .mb(px(6.))
                            .child(Styled::h(Input::new(&filter), px(30.))),
                    )
                })
                .children(list)
                .when_some(empty, |d, text| {
                    d.child(
                        div()
                            .px(px(12.))
                            .h(px(30.))
                            .flex()
                            .items_center()
                            .text_color(p.tx3)
                            .child(text),
                    )
                })
                .child(div().h(px(1.)).bg(p.line).mx(px(4.)).my(px(6.)))
                .child(
                    div()
                        .px(px(12.))
                        .pt(px(2.))
                        .pb(px(4.))
                        .text_size(small_size())
                        .child(footer(verb, &p)),
                )
        })
        .into_any_element()
}

/// "Runs `verb` on this tab’s session…", the statement word in the mono font.
fn footer(verb: &str, p: &Palette) -> StyledText {
    let (before, after) = (
        "Runs ",
        " on this tab’s session. ↑↓ to move, Enter to pick.",
    );
    let run = |len, family: SharedString, color| TextRun {
        len,
        font: font(family),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    StyledText::new(format!("{before}{verb}{after}")).with_runs(vec![
        run(before.len(), ui_font(), p.tx3),
        run(verb.len(), mono_font(), p.tx2),
        run(after.len(), ui_font(), p.tx3),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_name_the_schema_they_are_under() {
        let id = |p: &[&str]| p.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(row_schema(RowKind::Connection, &id(&["c"])), None);
        assert_eq!(
            row_schema(RowKind::Node(NodeKind::Database), &id(&["c", "shop"])).as_deref(),
            Some("shop")
        );
        assert_eq!(
            row_schema(
                RowKind::Node(NodeKind::Table),
                &id(&["c", "HR", "Tables", "EMPLOYEES"])
            )
            .as_deref(),
            Some("HR")
        );
        assert_eq!(
            row_schema(RowKind::Diagram, &id(&["c", "shop", "diagram"])).as_deref(),
            Some("shop")
        );
        assert_eq!(row_schema(RowKind::Sessions, &id(&["c", "sessions"])), None);
        assert_eq!(row_schema(RowKind::Config, &id(&["c", "config"])), None);
        assert_eq!(row_schema(RowKind::Loading, &id(&["c", ""])), None);
    }
}
