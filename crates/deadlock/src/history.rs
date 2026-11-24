//! The History panel from the design's `History` artboard: the project's statements,
//! newest first, with search, the three filters, day headings and the footer actions.
//! The list is virtualized and loads older statements as it scrolls.

use std::path::PathBuf;
use std::sync::PoisonError;

use chrono::{DateTime, Local};
use deadlock_core::history::{Entry, Filter, KEEP, Outcome};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    FontWeight, InteractiveElement, IntoElement, ListAlignment, ListState, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Window, actions, div, list, px,
};

use crate::app::{Db, HistoryDb};
use crate::assets::{Icon, icon};
use crate::theme::{MONO_FONT, Palette, RADIUS, data_size, row_height, small_size};
use crate::ui::Clickable as _;
use crate::ui::{Kind, button, square_button};

actions!(history, [Up, Down, InsertSelected]);

const CONTEXT: &str = "History";

/// Statements read per page.
const PAGE: usize = 200;

/// What the panel asks of the window.
pub enum HistoryEvent {
    Close,
    /// Insert this statement at the caret of the query tab in front.
    Insert(String),
    /// Open this statement in a new query tab on its connection.
    Open {
        connection: String,
        statement: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    All,
    This,
    Failed,
}

/// A line of the list: a day heading, or a statement by index into `entries`.
#[derive(Debug, Clone, PartialEq)]
enum Item {
    Day(String),
    Entry(usize),
}

pub struct HistoryPanel {
    root: PathBuf,
    search: Entity<InputState>,
    scope: Scope,
    /// The connection of the tab in front, for "This connection".
    current: Option<String>,
    entries: Vec<Entry>,
    items: Vec<Item>,
    list: ListState,
    selected: Option<usize>,
    focus: FocusHandle,
    loading: bool,
    /// Every matching statement is loaded.
    done: bool,
    /// Bumped on every new query: older answers are dropped.
    generation: u64,
    _sub: Subscription,
}

impl EventEmitter<HistoryEvent> for HistoryPanel {}

impl HistoryPanel {
    pub fn new(root: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search statements"));
        let sub = cx.subscribe(&search, |this, _, e: &InputEvent, cx| {
            if matches!(e, InputEvent::Change) {
                this.reload(cx);
            }
        });
        let list = ListState::new(0, ListAlignment::Top, px(400.));
        let weak = cx.entity().downgrade();
        list.set_scroll_handler(move |e, _, cx| {
            // Near the end: read the next page.
            if e.visible_range.end + 20 >= e.count {
                let _ = weak.update(cx, |this, cx| this.load_more(cx));
            }
        });
        let mut this = Self {
            root,
            search,
            scope: Scope::All,
            current: None,
            entries: Vec::new(),
            items: Vec::new(),
            list,
            selected: None,
            focus: cx.focus_handle(),
            loading: false,
            done: false,
            generation: 0,
            _sub: sub,
        };
        this.reload(cx);
        this
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.search.update(cx, |s, cx| s.focus(window, cx));
    }

    /// The tab in front changed connection.
    pub fn set_current(&mut self, connection: Option<String>, cx: &mut Context<Self>) {
        if self.current != connection {
            self.current = connection;
            if self.scope == Scope::This {
                self.reload(cx);
            }
        }
    }

    /// A statement was recorded: show it on top.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.entries.clear();
        self.items.clear();
        self.list.reset(0);
        self.selected = None;
        self.loading = false;
        self.done = false;
        self.load_more(cx);
        cx.notify();
    }

    fn filter(&self, cx: &App) -> Filter {
        Filter {
            text: self.search.read(cx).value().to_string(),
            connection: match self.scope {
                // No connection in front: nothing is "this connection's".
                Scope::This => Some(self.current.clone().unwrap_or_default()),
                _ => None,
            },
            failed: self.scope == Scope::Failed,
        }
    }

    fn load_more(&mut self, cx: &mut Context<Self>) {
        if self.loading || self.done {
            return;
        }
        let Some(db) = cx.global::<HistoryDb>().0.clone() else {
            self.done = true;
            return;
        };
        self.loading = true;
        let (root, filter) = (self.root.clone(), self.filter(cx));
        let before = self.entries.last().map(|e| e.id);
        let generation = self.generation;
        let rx = cx.global::<Db>().0.spawn_blocking(move || {
            let h = db.lock().unwrap_or_else(PoisonError::into_inner);
            h.page(&root, &filter, before, PAGE)
        });
        cx.spawn(async move |this, cx| {
            let Ok(page) = rx.await else { return };
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                match page {
                    Ok(page) => this.append(page),
                    Err(e) => {
                        eprintln!("deadlock: history: {e}");
                        this.done = true;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn append(&mut self, page: Vec<Entry>) {
        self.done = page.len() < PAGE;
        let old = self.items.len();
        let today = Local::now().date_naive();
        for e in page {
            let day = day(e.at, today);
            let same = self
                .entries
                .last()
                .is_some_and(|last| self::day(last.at, today) == day);
            if !same {
                self.items.push(Item::Day(day));
            }
            self.entries.push(e);
            self.items.push(Item::Entry(self.entries.len() - 1));
        }
        self.list.splice(old..old, self.items.len() - old);
    }

    fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        // A pressed chip lets go again: back to all connections.
        self.scope = if self.scope == scope {
            Scope::All
        } else {
            scope
        };
        self.reload(cx);
    }

    fn select(&mut self, i: usize, cx: &mut Context<Self>) {
        self.selected = Some(i);
        if let Some(ix) = self.items.iter().position(|it| *it == Item::Entry(i)) {
            self.list.scroll_to_reveal_item(ix);
        }
        cx.notify();
    }

    fn step(&mut self, down: bool, cx: &mut Context<Self>) {
        let n = self.entries.len();
        if n == 0 {
            return;
        }
        let i = match (self.selected, down) {
            (None, _) => 0,
            (Some(i), true) => (i + 1).min(n - 1),
            (Some(i), false) => i.saturating_sub(1),
        };
        if down && i + 20 >= n {
            self.load_more(cx);
        }
        self.select(i, cx);
    }

    fn chosen(&self) -> Option<&Entry> {
        self.selected.and_then(|i| self.entries.get(i))
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some(e) = self.chosen() {
            cx.write_to_clipboard(ClipboardItem::new_string(e.statement.clone()));
        }
    }

    fn insert(&mut self, cx: &mut Context<Self>) {
        if let Some(e) = self.chosen() {
            cx.emit(HistoryEvent::Insert(e.statement.clone()));
        }
    }

    fn open(&mut self, cx: &mut Context<Self>) {
        if let Some(e) = self.chosen() {
            cx.emit(HistoryEvent::Open {
                connection: e.connection_id.clone(),
                statement: e.statement.clone(),
            });
        }
    }

    fn render_item(&mut self, ix: usize, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let p = *cx.global::<Palette>();
        match self.items.get(ix) {
            Some(Item::Day(label)) => div()
                .pt(px(10.))
                .pb(px(6.))
                .px(px(16.))
                .text_size(small_size())
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.tx2)
                .bg(p.panel)
                .border_b_1()
                .border_color(p.line)
                .child(SharedString::from(label.clone()))
                .into_any_element(),
            Some(Item::Entry(i)) => {
                let i = *i;
                let Some(e) = self.entries.get(i) else {
                    return div().into_any_element();
                };
                let on = self.selected == Some(i);
                let time = DateTime::from_timestamp(e.at, 0)
                    .map(|t| t.with_timezone(&Local).format("%H:%M").to_string())
                    .unwrap_or_default();
                let mut meta = vec![
                    div().child(time),
                    div().child(SharedString::from(e.connection_name.clone())),
                ];
                match &e.outcome {
                    Outcome::Rows { count, more } => {
                        let n = usize::try_from(*count).unwrap_or(usize::MAX);
                        let noun = if n == 1 && !more { "row" } else { "rows" };
                        let more = if *more { "+" } else { "" };
                        meta.push(
                            div().child(format!("{}{more} {noun}", crate::export::grouped(n))),
                        );
                    }
                    Outcome::Affected(n) => {
                        let n = usize::try_from(*n).unwrap_or(usize::MAX);
                        let noun = if n == 1 { "row" } else { "rows" };
                        meta.push(
                            div().child(format!("{} {noun} affected", crate::export::grouped(n))),
                        );
                    }
                    Outcome::Failed { code, .. } => meta.push(
                        div()
                            .text_color(p.danger)
                            .font_weight(FontWeight::MEDIUM)
                            .child(code.clone().unwrap_or_else(|| "Failed".into())),
                    ),
                }
                if !matches!(e.outcome, Outcome::Failed { .. }) {
                    meta.push(div().child(crate::results::duration(
                        std::time::Duration::from_millis(e.duration_ms),
                    )));
                }
                // The whole statement on two lines at most: line breaks become spaces.
                let flat: String = e.statement.split_whitespace().collect::<Vec<_>>().join(" ");
                div()
                    .id(("history-entry", i))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .w_full()
                    .py(px(10.))
                    .px(px(16.))
                    .border_b_1()
                    .border_color(p.line)
                    .when(on, |d| d.bg(p.sel))
                    .when(!on, |d| d.hover(|s| s.bg(p.panel)))
                    .child(
                        div()
                            .font_family(MONO_FONT)
                            .text_size(data_size())
                            .line_height(px(18.))
                            .text_color(p.tx)
                            .line_clamp(2)
                            .overflow_hidden()
                            .child(crate::query::table::highlighted(&flat, &p)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_x(px(10.))
                            .gap_y(px(4.))
                            .text_size(small_size())
                            .text_color(p.tx3)
                            .children(meta),
                    )
                    .when_some(failure(e), |d, msg| {
                        let msg = msg.to_owned();
                        d.tooltip(move |window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(msg.clone())
                                .build(window, cx)
                        })
                    })
                    .clickable(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                        window.focus(&this.focus, cx);
                        this.select(i, cx);
                        if ev.click_count() >= 2 {
                            this.insert(cx);
                        }
                    }))
                    .into_any_element()
            }
            None => div().into_any_element(),
        }
    }
}

/// The server's message of a failed statement, for its tooltip.
fn failure(e: &Entry) -> Option<&str> {
    match &e.outcome {
        Outcome::Failed { message, .. } => Some(message),
        _ => None,
    }
}

fn day(at: i64, today: chrono::NaiveDate) -> String {
    crate::welcome::day_label(u64::try_from(at).unwrap_or(0), today)
}

impl Render for HistoryPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *cx.global::<Palette>();
        // `.b.bo`, filled with `--tx` while pressed.
        let chip = |id: &'static str, label: &'static str, scope: Scope, cx: &mut Context<Self>| {
            let on = self.scope == scope;
            let (bg, fg, border, hover) = if on {
                (p.tx, p.bg, p.tx, p.tx)
            } else {
                (p.bg, p.tx, p.line2, p.hover)
            };
            div()
                .id(id)
                .flex()
                .items_center()
                .h(row_height())
                .px(px(8.))
                .border_1()
                .border_color(border)
                .rounded(RADIUS)
                .bg(bg)
                .text_color(fg)
                .whitespace_nowrap()
                .hover(move |s| s.bg(hover))
                .child(label)
                .clickable(cx.listener(move |this, _, _, cx| this.set_scope(scope, cx)))
        };
        let picked = self.selected.is_some();
        let action = |id: &'static str, label: &'static str, tip: &'static str, cx: &App| {
            button(id, Kind::Outline, picked, tip, cx).child(label)
        };
        let empty = self.entries.is_empty() && !self.loading;
        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Up, _, cx| this.step(false, cx)))
            .on_action(cx.listener(|this, _: &Down, _, cx| this.step(true, cx)))
            .on_action(cx.listener(|this, _: &InsertSelected, _, cx| this.insert(cx)))
            .size_full()
            .flex()
            .flex_col()
            .bg(p.bg)
            .border_l_1()
            .border_color(p.line)
            .text_color(p.tx)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.))
                    .h(px(44.))
                    .pl(px(16.))
                    .pr(px(8.))
                    .border_b_1()
                    .border_color(p.line)
                    .child(
                        div()
                            .flex_grow(1.)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("History"),
                    )
                    .child(
                        square_button(
                            "history-close",
                            Icon::Close,
                            format!("Close ({})", crate::keymap::label("history", cx)),
                            cx,
                        )
                        .clickable(cx.listener(|_, _, _, cx| cx.emit(HistoryEvent::Close))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .flex_col()
                    .gap(px(8.))
                    .py(px(10.))
                    .px(px(16.))
                    .border_b_1()
                    .border_color(p.line)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .h(row_height())
                            .px(px(8.))
                            .border_1()
                            .border_color(p.line2)
                            .rounded(RADIUS)
                            .child(icon(Icon::Search, p.tx3))
                            .child(div().flex_1().min_w_0().child(
                                Input::new(&self.search).appearance(false).h(px(24.)).px_0(),
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(4.))
                            .child(chip("history-all", "All connections", Scope::All, cx))
                            .child(chip("history-this", "This connection", Scope::This, cx))
                            .child(chip("history-failed", "Failed only", Scope::Failed, cx)),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .when(empty, |d| {
                        d.p(px(16.)).text_color(p.tx3).child(
                            if self.search.read(cx).value().is_empty() && self.scope == Scope::All {
                                "Statements you run appear here."
                            } else {
                                "No statement matches."
                            },
                        )
                    })
                    .when(!empty, |d| {
                        d.child(
                            list(
                                self.list.clone(),
                                cx.processor(|this, ix, _, cx| this.render_item(ix, cx)),
                            )
                            .size_full(),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.))
                    .py(px(10.))
                    .px(px(16.))
                    .border_t_1()
                    .border_color(p.line)
                    .bg(p.panel)
                    .child(
                        div()
                            .flex_grow(1.)
                            .text_size(small_size())
                            .text_color(p.tx3)
                            .child(format!(
                                "Kept for this project, newest {} statements",
                                crate::export::grouped(KEEP)
                            )),
                    )
                    .child(
                        action("history-copy", "Copy", "Copy the statement", cx)
                            .when(picked, |b| {
                                b.clickable(cx.listener(|this, _, _, cx| this.copy(cx)))
                            }),
                    )
                    .child(
                        action(
                            "history-insert",
                            "Insert at caret",
                            "Insert into the query in front (Enter, double-click)",
                            cx,
                        )
                        .when(picked, |b| {
                            b.clickable(cx.listener(|this, _, _, cx| this.insert(cx)))
                        }),
                    )
                    .child(
                        action(
                            "history-open",
                            "Open in new query",
                            "Open in a new query tab on its connection",
                            cx,
                        )
                        .when(picked, |b| {
                            b.clickable(cx.listener(|this, _, _, cx| this.open(cx)))
                        }),
                    ),
            )
    }
}
