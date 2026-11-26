//! Search everywhere: Shift twice (or its shortcut) opens a palette that finds object
//! names, columns and source text on the active connection, or on every connection of
//! the project. Names and columns from the loaded tree show at once; each server's
//! answer ([`deadlock_core::driver::Driver::search_everywhere`]) then replaces its
//! connection's results, so tables never loaded into the tree are found too.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use deadlock_core::edit::TableRef;
use deadlock_core::search::{self, Group, Hit, LIMIT, Line};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{self, Input, InputEvent, InputState, Position};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, FontWeight, InteractiveElement,
    IntoElement, Modifiers, ModifiersChangedEvent, ParentElement, ScrollHandle,
    StatefulInteractiveElement, Styled, Subscription, Window, div, hsla, px,
};

use super::sidebar::label;
use super::{Conn, EverywhereNext, EverywherePrev, Front, Shell};
use crate::app::Db;
use crate::assets::{Icon, icon};
use crate::query::{Kind, View};
use crate::theme::{Palette, RADIUS, mono_font, small_size};
use crate::ui::Clickable as _;

/// Two Shift taps this close (release to release) open the palette; a tap is a Shift
/// press shorter than this, with no key or click while it is down.
const DOUBLE_SHIFT: Duration = Duration::from_millis(400);

/// A pause in typing before the servers are asked.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// The palette's key context: its arrow keys are in [`crate::keymap`].
const CONTEXT: &str = "Everywhere";

/// Results a group shows until its "more" row is picked.
const SHOWN: usize = 8;

/// Shift tapped twice on its own. gpui reports a lone modifier only as a
/// [`ModifiersChangedEvent`], so taps are read from those; key downs and clicks
/// while Shift is held [`interrupt`](Self::interrupt).
#[derive(Debug, Default)]
pub(super) struct DoubleShift {
    /// When Shift went down alone, while it is held.
    down: Option<Instant>,
    /// Nothing else happened since Shift went down.
    clean: bool,
    /// When the last clean tap ended.
    last: Option<Instant>,
}

impl DoubleShift {
    /// The modifiers are now `m`. Returns true when this ends the second tap in time.
    pub(super) fn modifiers(&mut self, m: Modifiers, now: Instant) -> bool {
        let others = m.control || m.alt || m.platform || m.function;
        if m.shift && !others {
            if self.down.is_none() {
                self.down = Some(now);
                self.clean = true;
            }
            return false;
        }
        if others {
            self.interrupt();
            return false;
        }
        let Some(down) = self.down.take() else {
            return false;
        };
        if !self.clean || now.saturating_duration_since(down) > DOUBLE_SHIFT {
            self.last = None;
            return false;
        }
        match self.last.take() {
            Some(last) if now.saturating_duration_since(last) <= DOUBLE_SHIFT => true,
            _ => {
                self.last = Some(now);
                false
            }
        }
    }

    /// A key or a click: Shift held for it was no tap, and a pair starts over.
    pub(super) fn interrupt(&mut self) {
        self.clean = false;
        self.last = None;
    }
}

/// A line of the result list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    /// A group's title and how many results it has.
    Head(Group, usize),
    /// `hits[i]`.
    Hit(usize),
    /// Shows the group's other results.
    More(Group, usize),
}

impl Item {
    fn pickable(self) -> bool {
        !matches!(self, Self::Head(..))
    }
}

/// The list for results in `groups` (one per hit, grouped, in order): each group's
/// title, its first [`SHOWN`] results unless `expanded`, then a "more" row.
fn items(groups: &[Group], expanded: &HashSet<Group>) -> Vec<Item> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < groups.len() {
        let g = groups[i];
        let n = groups[i..].iter().take_while(|x| **x == g).count();
        out.push(Item::Head(g, n));
        let shown = if expanded.contains(&g) {
            n
        } else {
            n.min(SHOWN)
        };
        out.extend((i..i + shown).map(Item::Hit));
        if shown < n {
            out.push(Item::More(g, n - shown));
        }
        i += n;
    }
    out
}

/// Ranks `hits` and keeps at most [`LIMIT`] per group.
fn settle(hits: &mut Vec<(String, Hit)>, text: &str) {
    search::sort_by(hits, text, |(_, h)| h);
    let mut seen: Option<(Group, usize)> = None;
    hits.retain(|(_, h)| {
        let n = match seen {
            Some((g, n)) if g == h.group => n + 1,
            _ => 1,
        };
        seen = Some((h.group, n));
        n <= LIMIT
    });
}

/// The palette while it is open.
pub(super) struct Everywhere {
    input: Entity<InputState>,
    /// Every connection of the project, not only the active one.
    all: bool,
    /// The text the results are for, trimmed.
    text: String,
    /// Connection id and result, ranked.
    hits: Vec<(String, Hit)>,
    /// Connections whose server has not answered yet.
    waiting: HashSet<String>,
    /// Per connection: why its server was not asked, or how it failed.
    notes: Vec<(String, String)>,
    expanded: HashSet<Group>,
    /// The picked line of [`items`].
    selected: usize,
    /// Set when a newer search starts: the older one stops between queries.
    stop: Arc<AtomicBool>,
    scroll: ScrollHandle,
    /// Where the focus goes back to on close.
    restore: Option<FocusHandle>,
    _subs: Vec<Subscription>,
}

impl Everywhere {
    fn items(&self) -> Vec<Item> {
        let groups: Vec<Group> = self.hits.iter().map(|(_, h)| h.group).collect();
        items(&groups, &self.expanded)
    }

    /// Keeps the pick on a pickable line, the first one when it is gone.
    fn clamp(&mut self) {
        let items = self.items();
        if !items.get(self.selected).is_some_and(|i| i.pickable()) {
            self.selected = items.iter().position(|i| i.pickable()).unwrap_or(0);
        }
    }
}

/// Whether the focus is in a text field or the editor, where Shift belongs to typing.
fn typing(window: &Window) -> bool {
    window.context_stack().iter().any(|c| c.contains("Input"))
}

impl Shell {
    pub(super) fn on_modifiers(
        &mut self,
        e: &ModifiersChangedEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.shift.modifiers(e.modifiers, Instant::now())
            && self.everywhere.is_none()
            && !typing(window)
        {
            self.open_everywhere(window, cx);
        }
    }

    pub(super) fn open_everywhere(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(e) = &self.everywhere {
            e.input.update(cx, |s, cx| s.focus(window, cx));
            return;
        }
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search tables, columns and code"));
        let sub = cx.subscribe_in(
            &input,
            window,
            |this, _, e: &InputEvent, window, cx| match e {
                InputEvent::Change => this.run_everywhere(cx),
                InputEvent::PressEnter { .. } => this.pick_everywhere(window, cx),
                _ => {}
            },
        );
        let restore = window.focused(cx);
        input.update(cx, |s, cx| s.focus(window, cx));
        self.everywhere = Some(Everywhere {
            input,
            all: false,
            text: String::new(),
            hits: Vec::new(),
            waiting: HashSet::new(),
            notes: Vec::new(),
            expanded: HashSet::new(),
            selected: 0,
            stop: Arc::new(AtomicBool::new(false)),
            scroll: ScrollHandle::new(),
            restore,
            _subs: vec![sub],
        });
        // The server answers for what the tree has not loaded: connect if needed.
        if let Some(id) = self.active(cx)
            && !self.conns.contains_key(&id)
        {
            self.connect(&id, cx);
        }
        cx.notify();
    }

    fn close_everywhere(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(e) = self.everywhere.take() {
            e.stop.store(true, Ordering::Relaxed);
            if let Some(f) = e.restore {
                window.focus(&f, cx);
            }
        }
        cx.notify();
    }

    /// The connections searched: the active one, or all of the project's.
    fn everywhere_conns(&self, all: bool, cx: &Context<Self>) -> Vec<String> {
        if all {
            self.project
                .config
                .connections
                .iter()
                .map(|c| c.id.clone())
                .collect()
        } else {
            self.active(cx).into_iter().collect()
        }
    }

    /// The text or the scope changed (or a connection came up): the loaded tree's
    /// matches now, each connected server's after a pause in typing. The search
    /// before stops.
    pub(super) fn run_everywhere(&mut self, cx: &mut Context<Self>) {
        let Some(all) = self.everywhere.as_ref().map(|e| e.all) else {
            return;
        };
        let conns = self.everywhere_conns(all, cx);
        let hidden = self.hidden_by_conn();
        let Some(e) = &mut self.everywhere else {
            return;
        };
        let text = e.input.read(cx).value().trim().to_owned();
        e.stop.store(true, Ordering::Relaxed);
        let stop = Arc::new(AtomicBool::new(false));
        e.stop = Arc::clone(&stop);
        e.text.clone_from(&text);
        e.hits.clear();
        e.waiting.clear();
        e.notes.clear();
        e.expanded.clear();
        e.selected = 0;
        if text.is_empty() {
            cx.notify();
            return;
        }
        let mut drivers = Vec::new();
        for id in conns {
            let hidden = hidden.get(&id).map_or(&[][..], Vec::as_slice);
            if let Some(nodes) = self.tree.children(std::slice::from_ref(&id)) {
                let mut found = search::tree_hits(nodes, &text);
                search::retain_visible(&mut found, hidden);
                e.hits.extend(found.into_iter().map(|h| (id.clone(), h)));
            }
            match self.conns.get(&id) {
                Some(Conn::Connected(d)) => {
                    e.waiting.insert(id.clone());
                    drivers.push((id, Arc::clone(d)));
                }
                // `connected` runs the search again.
                Some(Conn::Connecting) => e.notes.push((
                    id,
                    "connecting: names and columns of the cached tree until then".into(),
                )),
                _ => e.notes.push((
                    id,
                    "not connected: names and columns of the cached tree only".into(),
                )),
            }
        }
        settle(&mut e.hits, &text);
        e.clamp();
        let rt = Arc::clone(&cx.global::<Db>().0);
        for (id, driver) in drivers {
            let (text, stop) = (text.clone(), Arc::clone(&stop));
            let skip = hidden.get(&id).cloned().unwrap_or_default();
            let rt = Arc::clone(&rt);
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(DEBOUNCE).await;
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let asked = text.clone();
                let rx = rt.spawn(async move {
                    driver
                        .search_everywhere(&asked, &skip, &stop)
                        .await
                        .map_err(|e| e.to_string())
                });
                let result = rx
                    .await
                    .unwrap_or_else(|_| Err("the search task stopped".into()));
                let _ = this.update(cx, |this, cx| {
                    this.everywhere_answer(&id, &text, result, cx)
                });
            })
            .detach();
        }
        cx.notify();
    }

    fn everywhere_answer(
        &mut self,
        conn: &str,
        text: &str,
        result: Result<Vec<Hit>, String>,
        cx: &mut Context<Self>,
    ) {
        let hidden = self.hidden_by_conn().remove(conn).unwrap_or_default();
        let Some(e) = self.everywhere.as_mut().filter(|e| e.text == text) else {
            return;
        };
        e.waiting.remove(conn);
        match result {
            Ok(mut hits) => {
                // Safety net: the server was told to skip them, but the list may have changed since.
                search::retain_visible(&mut hits, &hidden);
                // The server knows everything the tree knew, and more.
                e.hits.retain(|(c, _)| c != conn);
                e.hits
                    .extend(hits.into_iter().map(|h| (conn.to_owned(), h)));
                settle(&mut e.hits, text);
            }
            Err(err) => e.notes.push((conn.to_owned(), err)),
        }
        e.clamp();
        cx.notify();
    }

    fn everywhere_nav(&mut self, down: bool, cx: &mut Context<Self>) {
        let Some(e) = &mut self.everywhere else {
            return;
        };
        let items = e.items();
        let next = if down {
            (e.selected + 1..items.len()).find(|&i| items[i].pickable())
        } else {
            (0..e.selected).rev().find(|&i| items[i].pickable())
        };
        if let Some(i) = next {
            e.selected = i;
            e.scroll.scroll_to_item(i);
        }
        cx.notify();
    }

    /// Enter or a click: a "more" row shows the rest of its group; a result opens.
    fn pick_everywhere(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(e) = &mut self.everywhere else {
            return;
        };
        match e.items().get(e.selected).copied() {
            Some(Item::More(g, _)) => {
                e.expanded.insert(g);
                cx.notify();
            }
            Some(Item::Hit(i)) => {
                let (conn, hit) = e.hits[i].clone();
                let text = e.text.clone();
                self.close_everywhere(window, cx);
                self.open_hit(conn, hit, &text, window, cx);
            }
            _ => {}
        }
    }

    /// Opens a result as the sidebar would: tables and views in their tab (columns
    /// and view code in its Structure view), keys in theirs, anything else as its
    /// `CREATE` statement in a new query tab, the caret on the match.
    fn open_hit(
        &mut self,
        conn: String,
        hit: Hit,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_redis(&conn) {
            return self.open_key(&[conn, hit.schema, hit.name], window, cx);
        }
        let table = TableRef {
            database: hit.schema.clone(),
            table: hit.name.clone(),
        };
        if let Some((column, _)) = hit.column {
            self.open_table(&conn, table, View::Structure, window, cx);
            if let Some(tab) = self.front_tab().cloned() {
                tab.update(cx, |t, cx| t.focus_column(column, cx));
            }
        } else if hit.is_table() {
            // shortcut: view code opens the Structure view's DDL, without moving to the
            // match; a definition tab would need the view's text reformatted to help.
            let view = if hit.group == Group::Code {
                View::Structure
            } else {
                View::Data
            };
            self.open_table(&conn, table, view, window, cx);
        } else {
            self.open_definition(conn, hit, text.to_owned(), window, cx);
        }
    }

    fn open_definition(
        &mut self,
        conn: String,
        hit: Hit,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Conn::Connected(driver)) = self.conns.get(&conn) else {
            let name = self.config(&conn).map_or(conn.clone(), |c| c.name.clone());
            drop(crate::dialog::ask(
                window,
                cx,
                crate::dialog::Ask::new(format!("Connect to {name} first"), &["OK"])
                    .text("The definition is read from the server."),
            ));
            return;
        };
        let driver = Arc::clone(driver);
        let (schema, name, kind) = (hit.schema.clone(), hit.name.clone(), hit.kind.clone());
        let rx = cx.global::<Db>().0.spawn(async move {
            driver
                .definition(&schema, &name, &kind)
                .await
                .map_err(|e| e.to_string())
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err("the definition task stopped".into()));
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(def) => {
                    // A name match has no line: the caret goes to the first mention.
                    let line = hit.line.clone().unwrap_or(Line {
                        number: 1,
                        text: String::new(),
                        hits: Vec::new(),
                    });
                    let at = search::locate(&def, &line, &text);
                    let binding = this.binding(Some(&conn));
                    let i = this.add_tab(Kind::File(None), binding, def, window, cx);
                    this.show_tab(Front::Query(i), window, cx);
                    if let (Some((line, ch)), Some((tab, _))) = (at, this.tabs.get(i)) {
                        let editor = tab.read(cx).editor.clone();
                        let at = Position::new(line as u32, ch as u32);
                        editor.update(cx, |e, cx| e.set_cursor_position(at, window, cx));
                    }
                }
                Err(e) => drop(crate::dialog::ask(
                    window,
                    cx,
                    crate::dialog::Ask::new("The definition could not be read", &["OK"]).text(e),
                )),
            });
        })
        .detach();
    }
}

fn hit_icon(hit: &Hit, p: &Palette) -> Option<gpui_kit::Svg> {
    Some(match (hit.group, hit.kind.as_str()) {
        (Group::Columns, _) => icon(Icon::Table, p.tx3),
        _ if hit.is_table() => icon(Icon::Table, p.tx3),
        (_, "string" | "hash" | "list" | "set" | "zset" | "stream" | "json") => return None,
        _ => icon(Icon::Routine, p.tx3),
    })
}

fn group_title(g: Group) -> &'static str {
    match g {
        Group::Objects => "Objects",
        Group::Columns => "Columns",
        Group::Code => "Code",
    }
}

/// The palette over the window, while it is open.
pub(super) fn palette(shell: &Shell, cx: &mut Context<Shell>) -> Option<AnyElement> {
    let p = *cx.global::<Palette>();
    let e = shell.everywhere.as_ref()?;
    let name = |id: &str| shell.config(id).map_or(id.to_owned(), |c| c.name.clone());
    let scope = if e.all {
        format!(
            "In all {} connections of the project",
            shell.project.config.connections.len()
        )
    } else {
        shell
            .active(cx)
            .map_or("No connection".into(), |id| format!("In {}", name(&id)))
    };
    let status = if !e.waiting.is_empty() {
        "Searching the server…".to_owned()
    } else if e.text.is_empty() {
        "Names, columns and the code of routines, triggers, views and events".to_owned()
    } else if e.hits.is_empty() {
        "Nothing found".to_owned()
    } else {
        String::new()
    };
    let rows: Vec<AnyElement> = e
        .items()
        .into_iter()
        .enumerate()
        .map(|(ix, item)| {
            let on = ix == e.selected;
            let pick = cx.listener(move |this, _, window, cx| {
                if let Some(e) = &mut this.everywhere {
                    e.selected = ix;
                }
                this.pick_everywhere(window, cx);
            });
            match item {
                Item::Head(g, n) => div()
                    .px(px(8.))
                    .pt(px(8.))
                    .pb(px(2.))
                    .text_size(small_size())
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.tx3)
                    .child(if n >= LIMIT {
                        format!("{}  {n}+", group_title(g))
                    } else {
                        format!("{}  {n}", group_title(g))
                    })
                    .into_any_element(),
                Item::More(_, n) => div()
                    .id(("everywhere-more", ix))
                    .px(px(8.))
                    .py(px(3.))
                    .rounded(RADIUS)
                    .text_size(small_size())
                    .text_color(p.link)
                    .when(on, |d| d.bg(p.sel))
                    .hover(|s| s.bg(p.hover))
                    .child(format!("Show {n} more"))
                    .clickable(pick)
                    .into_any_element(),
                Item::Hit(i) => {
                    let (conn, hit) = &e.hits[i];
                    hit_row(ix, hit, &e.text, on, e.all.then(|| name(conn)), &p)
                        .clickable(pick)
                        .into_any_element()
                }
            }
        })
        .collect();
    let notes: Vec<AnyElement> = e
        .notes
        .iter()
        .map(|(id, note)| {
            div()
                .text_size(small_size())
                .text_color(p.tx3)
                .child(format!("{}: {note}", name(id)))
                .into_any_element()
        })
        .collect();
    Some(
        div()
            .id("everywhere")
            .key_context(CONTEXT)
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .justify_center()
            .pt(px(72.))
            .bg(hsla(0., 0., 0., 0.3))
            .on_action(cx.listener(|this, _: &input::Escape, window, cx| {
                this.close_everywhere(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &EverywherePrev, _, cx| this.everywhere_nav(false, cx)),
            )
            .on_action(cx.listener(|this, _: &EverywhereNext, _, cx| this.everywhere_nav(true, cx)))
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_everywhere(window, cx)),
            )
            .child(
                div()
                    .id("everywhere-panel")
                    .w(px(640.))
                    .max_h(px(560.))
                    .p(px(12.))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .bg(p.bg)
                    .text_color(p.tx)
                    .border_1()
                    .border_color(p.line)
                    .rounded(RADIUS)
                    // Clicks inside stay inside: the backdrop's closes the palette.
                    .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation()
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .child(
                                div()
                                    .flex_1()
                                    .child(Styled::h(Input::new(&e.input), px(32.))),
                            )
                            .child(
                                Checkbox::new("everywhere-all")
                                    .checked(e.all)
                                    .label("All connections")
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                        if let Some(e) = &mut this.everywhere {
                                            e.all = *checked;
                                        }
                                        this.run_everywhere(cx);
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .gap(px(8.))
                            .text_size(small_size())
                            .text_color(p.tx3)
                            .child(scope)
                            .child(status),
                    )
                    .children(notes)
                    .child(
                        div()
                            .id("everywhere-list")
                            .track_scroll(&e.scroll)
                            .overflow_y_scroll()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .children(rows),
                    )
                    .child(
                        div()
                            .text_size(small_size())
                            .text_color(p.tx3)
                            .child("↑ ↓ move   Enter open   Esc close"),
                    ),
            )
            .into_any_element(),
    )
}

/// One result: icon, name with the match highlighted, what and where it is; code
/// results add the matching line.
fn hit_row(
    ix: usize,
    hit: &Hit,
    text: &str,
    on: bool,
    connection: Option<String>,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let (title, hits) = match &hit.column {
        Some((column, _)) => {
            let shift = hit.name.len() + 1;
            let hits = search::find_all(column, text)
                .into_iter()
                .map(|r| r.start + shift..r.end + shift)
                .collect::<Vec<_>>();
            (format!("{}.{column}", hit.name), hits)
        }
        None => (hit.name.clone(), search::find_all(&hit.name, text)),
    };
    let mut detail = vec![
        hit.column
            .as_ref()
            .map_or(hit.kind.clone(), |(_, ty)| ty.clone()),
    ];
    detail.push(hit.schema.clone());
    if let Some(l) = &hit.line {
        detail.push(format!("line {}", l.number));
    }
    detail.extend(connection);
    let snippet = hit.line.as_ref().map(|l| search::snippet(l, 90));
    div()
        .id(("everywhere-hit", ix))
        .px(px(8.))
        .py(px(3.))
        .rounded(RADIUS)
        .flex()
        .flex_col()
        .gap(px(2.))
        .when(on, |d| d.bg(p.sel))
        .hover(|s| s.bg(p.hover))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .children(hit_icon(hit, p))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(label(&title, &hits, p)),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(small_size())
                        .text_color(p.tx3)
                        .child(detail.join(" · ")),
                ),
        )
        .when_some(snippet, |d, (s, hits)| {
            d.child(
                div()
                    .pl(px(24.))
                    .font_family(mono_font())
                    .text_size(small_size())
                    .text_color(p.tx2)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(label(&s, &hits, p)),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shift(on: bool) -> Modifiers {
        Modifiers {
            shift: on,
            ..Modifiers::default()
        }
    }

    fn ms(t0: Instant, n: u64) -> Instant {
        t0 + Duration::from_millis(n)
    }

    #[test]
    fn two_quick_shift_taps_trigger_once() {
        let t0 = Instant::now();
        let mut d = DoubleShift::default();
        assert!(!d.modifiers(shift(true), t0));
        assert!(!d.modifiers(shift(false), ms(t0, 80)));
        assert!(!d.modifiers(shift(true), ms(t0, 200)));
        assert!(d.modifiers(shift(false), ms(t0, 280)));
        // The pair is used up: a third tap starts a new one.
        assert!(!d.modifiers(shift(true), ms(t0, 350)));
        assert!(!d.modifiers(shift(false), ms(t0, 400)));
    }

    #[test]
    fn slow_taps_long_holds_and_chords_do_not() {
        let t0 = Instant::now();
        let mut d = DoubleShift::default();
        // Too far apart.
        d.modifiers(shift(true), t0);
        d.modifiers(shift(false), ms(t0, 50));
        d.modifiers(shift(true), ms(t0, 600));
        assert!(!d.modifiers(shift(false), ms(t0, 650)));
        // A held Shift is no tap.
        let mut d = DoubleShift::default();
        d.modifiers(shift(true), t0);
        d.modifiers(shift(false), ms(t0, 50));
        d.modifiers(shift(true), ms(t0, 100));
        assert!(!d.modifiers(shift(false), ms(t0, 900)));
        // Shift+A typed in between.
        let mut d = DoubleShift::default();
        d.modifiers(shift(true), t0);
        d.modifiers(shift(false), ms(t0, 50));
        d.modifiers(shift(true), ms(t0, 100));
        d.interrupt();
        assert!(!d.modifiers(shift(false), ms(t0, 150)));
        // Ctrl+Shift.
        let mut d = DoubleShift::default();
        d.modifiers(shift(true), t0);
        d.modifiers(shift(false), ms(t0, 50));
        d.modifiers(shift(true), ms(t0, 100));
        let both = Modifiers {
            control: true,
            ..shift(true)
        };
        d.modifiers(both, ms(t0, 120));
        assert!(!d.modifiers(shift(false), ms(t0, 150)));
    }

    #[test]
    fn items_cap_each_group_with_a_more_row() {
        let mut groups = vec![Group::Objects; 10];
        groups.extend([Group::Columns; 2]);
        let list = items(&groups, &HashSet::new());
        assert_eq!(list[0], Item::Head(Group::Objects, 10));
        assert_eq!(list[SHOWN + 1], Item::More(Group::Objects, 2));
        assert_eq!(list[SHOWN + 2], Item::Head(Group::Columns, 2));
        assert_eq!(list.last(), Some(&Item::Hit(11)));
        let open: HashSet<Group> = [Group::Objects].into();
        let list = items(&groups, &open);
        assert_eq!(list.len(), 1 + 10 + 1 + 2);
        assert!(!list.iter().any(|i| matches!(i, Item::More(..))));
    }

    #[test]
    fn settle_ranks_and_caps_per_group() {
        let mut hits: Vec<(String, Hit)> = (0..LIMIT + 5)
            .map(|i| {
                (
                    "c".to_owned(),
                    Hit::object("s".into(), format!("x_orders_{i}"), "table".into()),
                )
            })
            .collect();
        hits.push((
            "c".into(),
            Hit::object("s".into(), "orders".into(), "table".into()),
        ));
        settle(&mut hits, "orders");
        assert_eq!(hits.len(), LIMIT);
        assert_eq!(hits[0].1.name, "orders");
    }
}
