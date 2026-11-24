//! The Server configuration tab: the server's global variables by category, filters,
//! search, the detail pane with an editor per type, and the pending-changes panel
//! that applies them through the tab's own session.

use std::ops::Range;
use std::sync::Arc;

use deadlock_core::config::{
    Applied, Applies, Category, ConfigChange, ConfigParam, Mode, NewValue, Scope, ServerConfig,
    ValueType, hint, scale, unscale,
};
use deadlock_core::connection::{ConnectionConfig, Engine, Environment};
use deadlock_core::driver::{Driver, connect};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AnyWindowHandle, AppContext as _, ClipboardItem, Context, Div, Entity, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Render, ScrollStrategy, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, UniformListScrollHandle, Window, div,
    px, relative, uniform_list,
};

use crate::app::Db;
use crate::assets::{Icon, icon};
use crate::query::table::highlighted;
use crate::theme::{MONO_FONT, Palette, RADIUS, data_size, small_size};
use crate::ui::Clickable as _;
use crate::ui::{Kind, button};

/// `.pr`: 34 px rows.
const ROW: f32 = 34.;
/// Default, Set in and Applies on, from `.pr`; Parameter and Value share the rest
/// 1.3 : 1.
const FIXED: [f32; 3] = [84., 150., 76.];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Modified,
    Pending,
    Restart,
}

impl Filter {
    const ALL: [Self; 4] = [Self::All, Self::Modified, Self::Pending, Self::Restart];

    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Modified => "Modified",
            Self::Pending => "Pending",
            Self::Restart => "Needs restart",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Col {
    Name,
    Value,
    Default,
    Source,
    Applies,
}

const COLUMNS: [(Col, &str); 5] = [
    (Col::Name, "Parameter"),
    (Col::Value, "Value"),
    (Col::Default, "Default"),
    (Col::Source, "Set in"),
    (Col::Applies, "Applies on"),
];

/// A grid line: a category heading or a variable, by index into the params.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line {
    Group(Category),
    Param(usize),
}

enum Conn {
    Connecting,
    Ready(Arc<dyn Driver>),
    Failed(String),
}

pub struct ConfigTab {
    pub connection: ConnectionConfig,
    conn: Conn,
    window: AnyWindowHandle,
    config: Option<ServerConfig>,
    /// The last load failed; what is shown is older.
    error: Option<String>,
    loading: bool,
    /// Apply is running.
    busy: bool,
    filter: Filter,
    /// `None`: all categories.
    category: Option<Category>,
    sort: (Col, bool),
    selected: Option<String>,
    search: Entity<InputState>,
    /// The search text, lower case.
    query: String,
    /// The New value field, for numbers and text.
    value: Entity<InputState>,
    /// The unit button the field counts in, as an index into [`Unit::steps`].
    unit: usize,
    edit_error: Option<String>,
    pending: Vec<ConfigChange>,
    /// Redis: apply with `CONFIG REWRITE` too.
    rewrite: bool,
    /// The result of the last apply, and whether it failed.
    message: Option<(String, bool)>,
    lines: Vec<Line>,
    list: UniformListScrollHandle,
    _subs: Vec<Subscription>,
}

impl ConfigTab {
    pub fn new(
        config: ConnectionConfig,
        password: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Filter parameters"));
        let value = cx.new(|cx| InputState::new(window, cx));
        let subs = vec![
            cx.subscribe(&search, |t, s, e: &InputEvent, cx| {
                if matches!(e, InputEvent::Change) {
                    t.query = s.read(cx).value().trim().to_lowercase();
                    t.arrange();
                    cx.notify();
                }
            }),
            cx.subscribe(&value, |t, _, e: &InputEvent, cx| {
                if matches!(e, InputEvent::Change) {
                    t.edited(cx);
                }
            }),
        ];
        cx.on_release(|this, cx| {
            if let Conn::Ready(d) = std::mem::replace(&mut this.conn, Conn::Connecting) {
                drop(cx.global::<Db>().0.spawn(async move { drop(d) }));
            }
        })
        .detach();
        let mut this = Self {
            connection: config.clone(),
            conn: Conn::Connecting,
            window: window.window_handle(),
            config: None,
            error: None,
            loading: false,
            busy: false,
            filter: Filter::All,
            category: None,
            sort: (Col::Name, false),
            selected: None,
            search,
            query: String::new(),
            value,
            unit: 0,
            edit_error: None,
            pending: Vec::new(),
            rewrite: false,
            message: None,
            lines: Vec::new(),
            list: UniformListScrollHandle::new(),
            _subs: subs,
        };
        this.open(config, password, cx);
        this
    }

    /// Opens the tab's own server session, then loads.
    fn open(&mut self, config: ConnectionConfig, password: String, cx: &mut Context<Self>) {
        let rx = cx
            .global::<Db>()
            .0
            .spawn(async move { connect(&config, &password).await.map_err(|e| e.to_string()) });
        cx.spawn(async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err("the connection task stopped".into()));
            let _ = this.update(cx, |t, cx| {
                t.conn = match result {
                    Ok(d) => Conn::Ready(d),
                    Err(e) => Conn::Failed(e),
                };
                t.reload(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        match &self.conn {
            Conn::Ready(d) => Some(Arc::clone(d)),
            _ => None,
        }
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Reload from server: keeps the pending changes that still apply.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(d) = self.driver().filter(|_| !self.loading) else {
            return;
        };
        self.loading = true;
        cx.notify();
        let rx = cx
            .global::<Db>()
            .0
            .spawn(async move { d.config().await.map_err(|e| e.to_string()) });
        let window = self.window;
        cx.spawn(async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err("the configuration task stopped".into()));
            let _ = cx.update_window(window, |_, window, cx| {
                this.update(cx, |t, cx| t.loaded(result, window, cx))
            });
        })
        .detach();
    }

    fn loaded(
        &mut self,
        result: Result<ServerConfig, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.loading = false;
        match result {
            Ok(cfg) => {
                self.error = None;
                let read_only = self.connection.read_only;
                self.pending.retain(|c| {
                    let Some(p) = cfg.param(&c.name) else {
                        return false;
                    };
                    let differs = match &c.value {
                        NewValue::Default => true,
                        NewValue::Set(v) => p
                            .check(v)
                            .is_ok_and(|v| Some(v.as_str()) != p.value.as_deref()),
                    };
                    differs && !read_only && cfg.statements(c).is_ok()
                });
                if self
                    .selected
                    .as_ref()
                    .is_some_and(|s| cfg.param(s).is_none())
                {
                    self.selected = None;
                }
                self.config = Some(cfg);
                self.arrange();
                self.sync_editor(window, cx);
            }
            Err(e) => self.error = Some(e),
        }
        cx.notify();
    }

    fn param(&self, name: &str) -> Option<&ConfigParam> {
        self.config.as_ref()?.param(name)
    }

    fn pending_of(&self, name: &str) -> Option<&ConfigChange> {
        self.pending.iter().find(|c| c.name == name)
    }

    /// Why `p` cannot be changed here.
    fn locked(&self, p: &ConfigParam) -> Option<String> {
        if self.connection.read_only {
            return Some("Read-only connection: nothing can be changed from here.".into());
        }
        self.config.as_ref()?.locked(p)
    }

    fn needs_restart(&self, p: &ConfigParam) -> bool {
        p.restart_pending() || p.applies == Applies::Restart && self.pending_of(&p.name).is_some()
    }

    fn matches(&self, f: Filter, p: &ConfigParam) -> bool {
        match f {
            Filter::All => true,
            Filter::Modified => p.is_modified(),
            Filter::Pending => self.pending_of(&p.name).is_some(),
            Filter::Restart => self.needs_restart(p),
        }
    }

    fn count(&self, f: Filter) -> usize {
        self.config.as_ref().map_or(0, |c| {
            c.params.iter().filter(|p| self.matches(f, p)).count()
        })
    }

    /// Recomputes the grid's lines: filter, category, search, then grouped by category
    /// and sorted within each.
    fn arrange(&mut self) {
        let Some(cfg) = &self.config else {
            self.lines.clear();
            return;
        };
        let needle = self.query.as_str();
        let mut shown: Vec<usize> = cfg
            .params
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                self.matches(self.filter, p)
                    && self.category.is_none_or(|c| c == p.category)
                    && (needle.is_empty()
                        || p.name.contains(needle)
                        || p.description
                            .as_ref()
                            .is_some_and(|d| d.to_lowercase().contains(needle)))
            })
            .map(|(i, _)| i)
            .collect();
        let (col, desc) = self.sort;
        let key = |p: &ConfigParam| -> String {
            match col {
                Col::Name => p.name.clone(),
                Col::Value => p.value.clone().unwrap_or_default(),
                Col::Default => p.default.clone().unwrap_or_default(),
                Col::Source => p.source.short(),
                Col::Applies => p.applies.short().to_owned(),
            }
        };
        shown.sort_by(|&a, &b| {
            let (pa, pb) = (&cfg.params[a], &cfg.params[b]);
            let (ka, kb) = (key(pa), key(pb));
            let o = match (ka.parse::<f64>(), kb.parse::<f64>()) {
                (Ok(x), Ok(y)) => x.total_cmp(&y),
                _ => ka.cmp(&kb),
            };
            let o = if desc { o.reverse() } else { o };
            o.then(pa.name.cmp(&pb.name))
        });
        self.lines.clear();
        for c in Category::ALL {
            let group: Vec<usize> = shown
                .iter()
                .copied()
                .filter(|&i| cfg.params[i].category == c)
                .collect();
            if !group.is_empty() {
                self.lines.push(Line::Group(c));
                self.lines.extend(group.into_iter().map(Line::Param));
            }
        }
    }

    fn set_filter(&mut self, f: Filter, cx: &mut Context<Self>) {
        self.filter = f;
        self.arrange();
        cx.notify();
    }

    fn set_category(&mut self, c: Option<Category>, cx: &mut Context<Self>) {
        self.category = c;
        self.arrange();
        cx.notify();
    }

    fn set_sort(&mut self, col: Col, cx: &mut Context<Self>) {
        self.sort = match self.sort {
            (c, desc) if c == col => (col, !desc),
            _ => (col, false),
        };
        self.arrange();
        cx.notify();
    }

    fn select(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(name);
        self.sync_editor(window, cx);
        cx.notify();
    }

    /// Selects `name`, showing it even when the filters hide it.
    fn show(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let visible = |t: &Self| {
            t.lines.iter().any(|l| match l {
                Line::Param(i) => t.config.as_ref().is_some_and(|c| c.params[*i].name == name),
                Line::Group(_) => false,
            })
        };
        if !visible(self) {
            self.filter = Filter::All;
            self.category = None;
            self.search.update(cx, |s, cx| s.set_value("", window, cx));
            self.query.clear();
            self.arrange();
        }
        let ix = self.lines.iter().position(|l| match l {
            Line::Param(i) => self
                .config
                .as_ref()
                .is_some_and(|c| c.params[*i].name == name),
            Line::Group(_) => false,
        });
        if let Some(ix) = ix {
            self.list.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
        self.select(name, window, cx);
    }

    /// Puts the selected variable's pending or current value into the New value field,
    /// in the largest unit that holds it exactly.
    fn sync_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_error = None;
        let Some(p) = self.selected.as_deref().and_then(|s| self.param(s)) else {
            return;
        };
        let raw = match self.pending_of(&p.name).map(|c| &c.value) {
            Some(NewValue::Set(v)) => p.check(v).unwrap_or_else(|_| v.clone()),
            _ => p.value.clone().unwrap_or_default(),
        };
        let (unit, text) = match (p.unit, raw.parse::<i128>()) {
            (Some(u), Ok(n)) => u
                .steps()
                .iter()
                .enumerate()
                .rev()
                .find(|(_, (_, step))| n != 0 && n % step == 0)
                .map_or((0, raw.clone()), |(i, (_, step))| {
                    (i, (n / step).to_string())
                }),
            _ => (0, raw),
        };
        self.unit = unit;
        self.value.update(cx, |s, cx| s.set_value(text, window, cx));
    }

    /// The New value field changed: check it, then make it pending or drop the change.
    fn edited(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self
            .selected
            .as_deref()
            .and_then(|s| self.param(s))
            .cloned()
        else {
            return;
        };
        if self.locked(&p).is_some() {
            return;
        }
        let text = self.value.read(cx).value().to_string();
        let raw = match p.unit {
            Some(u) => {
                let step = u.steps().get(self.unit).map_or(1, |s| s.1);
                match scale(&text, step) {
                    Some(n) => n.to_string(),
                    None => {
                        self.edit_error = Some(if text.trim().is_empty() {
                            "Enter a value.".into()
                        } else {
                            "Enter a whole number of bytes in this unit.".into()
                        });
                        self.drop_pending(&p.name);
                        cx.notify();
                        return;
                    }
                }
            }
            None => text,
        };
        match p.check(&raw) {
            Ok(_) => {
                self.edit_error = None;
                self.set_pending(&p, NewValue::Set(raw));
            }
            Err(e) => {
                self.edit_error = Some(e);
                self.drop_pending(&p.name);
            }
        }
        self.arrange();
        cx.notify();
    }

    fn drop_pending(&mut self, name: &str) {
        self.pending.retain(|c| c.name != name);
    }

    /// Makes `value` pending for `p`, or drops the change when it is the running value.
    fn set_pending(&mut self, p: &ConfigParam, value: NewValue) {
        let same = match &value {
            NewValue::Set(v) => p.check(v).ok().as_deref() == p.value.as_deref(),
            NewValue::Default => false,
        };
        if same {
            self.drop_pending(&p.name);
            return;
        }
        match self.pending.iter_mut().find(|c| c.name == p.name) {
            Some(c) => c.value = value,
            None => self.pending.push(ConfigChange {
                name: p.name.clone(),
                value,
                // Redis: CONFIG REWRITE only when asked, for every change at once.
                mode: if self.connection.engine == Engine::Redis && !self.rewrite {
                    Mode::RunOnly
                } else {
                    Mode::default()
                },
            }),
        }
    }

    /// A switch, list or Revert button picked a value.
    fn pick(&mut self, value: NewValue, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self
            .selected
            .as_deref()
            .and_then(|s| self.param(s))
            .cloned()
        else {
            return;
        };
        self.set_pending(&p, value);
        self.arrange();
        self.sync_editor(window, cx);
        cx.notify();
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if let Some(name) = &self.selected
            && let Some(c) = self.pending.iter_mut().find(|c| &c.name == name)
        {
            c.mode = mode;
        }
        cx.notify();
    }

    /// A unit button: the field converts to the new unit, keeping the value.
    fn set_unit(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(u) = self
            .selected
            .as_deref()
            .and_then(|s| self.param(s))
            .and_then(|p| p.unit)
        else {
            return;
        };
        let steps = u.steps();
        let (Some(from), Some(to)) = (steps.get(self.unit), steps.get(i)) else {
            return;
        };
        let text = self.value.read(cx).value().to_string();
        let converted = scale(&text, from.1).and_then(|n| unscale(n, to.1));
        self.unit = i;
        if let Some(c) = converted {
            self.value.update(cx, |s, cx| s.set_value(c, window, cx));
        } else {
            self.edited(cx);
        }
        cx.notify();
    }

    /// Every pending change's statements, as they will run.
    fn statements(&self) -> Vec<String> {
        let Some(cfg) = &self.config else {
            return Vec::new();
        };
        // Redis command lines end without a semicolon.
        let end = if cfg.redis { "" } else { ";" };
        let mut out: Vec<String> = self
            .pending
            .iter()
            .flat_map(|c| match cfg.statements(c) {
                Ok(s) => s.into_iter().map(|s| format!("{s}{end}")).collect(),
                Err(e) => vec![format!("-- {}: {e}", c.name)],
            })
            .collect();
        out.extend(cfg.trailer(&self.pending).map(|t| format!("{t}{end}")));
        out
    }

    /// Redis's "Also rewrite the config file" choice, for every pending change.
    fn set_rewrite(&mut self, on: bool, cx: &mut Context<Self>) {
        self.rewrite = on;
        let mode = if on { Mode::Persist } else { Mode::RunOnly };
        for c in &mut self.pending {
            c.mode = mode;
        }
        cx.notify();
    }

    fn discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.pending.len();
        if n == 0 {
            return;
        }
        let answer = crate::dialog::ask(
            window,
            cx,
            crate::dialog::Ask::new(
                format!("Discard {n} pending {}?", noun(n)),
                &["Discard", "Cancel"],
            )
            .text("Nothing has been applied to the server.")
            .danger(true),
        );
        let handle = self.window;
        cx.spawn(async move |this, cx| {
            if answer.await == Some(0) {
                let _ = cx.update_window(handle, |_, window, cx| {
                    this.update(cx, |t, cx| {
                        t.pending.clear();
                        t.arrange();
                        t.sync_editor(window, cx);
                        cx.notify();
                    })
                });
            }
        })
        .detach();
    }

    /// Runs the pending changes in order; on Production after a confirmation that
    /// shows the statements. Stops at the first error, then reloads. The task ends when
    /// the server has answered.
    pub fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Task<()> {
        let (Some(driver), Some(cfg)) = (self.driver(), self.config.clone()) else {
            return Task::ready(());
        };
        if self.pending.is_empty() || self.busy || self.connection.read_only {
            return Task::ready(());
        }
        let changes = self.pending.clone();
        let n = changes.len();
        let answer = (self.connection.environment == Environment::Production).then(|| {
            crate::dialog::ask(
                window,
                cx,
                crate::dialog::Ask::new(
                    format!(
                        "Apply {n} {} on production connection {}?",
                        noun(n),
                        self.connection.name
                    ),
                    &["Apply", "Cancel"],
                )
                .statement(self.statements().join("\n"))
                .production(true),
            )
        });
        let rt = Arc::clone(&cx.global::<Db>().0);
        cx.spawn(async move |this, cx| {
            if let Some(a) = answer
                && a.await != Some(0)
            {
                return;
            }
            let _ = this.update(cx, |t, cx| {
                t.busy = true;
                cx.notify();
            });
            let result = rt
                .spawn(async move { driver.apply_config(&cfg, &changes).await })
                .await
                .unwrap_or_else(|_| Err(deadlock_core::driver::DriverError::Gone));
            let _ = this.update(cx, |t, cx| {
                t.busy = false;
                t.message = Some(match result {
                    Ok(Applied { done, error: None }) => {
                        t.pending.drain(..done.min(t.pending.len()));
                        (format!("Applied {done} {}.", noun(done)), false)
                    }
                    // Every change went through; what ran after them failed.
                    Ok(Applied {
                        done,
                        error: Some(e),
                    }) if done == n => {
                        t.pending.clear();
                        (format!("Applied {done} {}. {e}", noun(done)), true)
                    }
                    Ok(Applied {
                        done,
                        error: Some(e),
                    }) => {
                        let failed = t.pending.get(done).map(|c| c.name.clone());
                        t.pending.drain(..done.min(t.pending.len()));
                        (
                            format!(
                                "Applied {done} of {n} {}. {} failed: {e} The rest did not run.",
                                noun(n),
                                failed.map_or("A change".into(), |f| format!("The change to {f}"))
                            ),
                            true,
                        )
                    }
                    Err(e) => (e.to_string(), true),
                });
                t.reload(cx);
                cx.notify();
            });
        })
    }
}

fn noun(n: usize) -> &'static str {
    if n == 1 { "change" } else { "changes" }
}

/// The config file can be opened here: the server runs on this machine and the file
/// is readable.
fn local_file(c: &ConnectionConfig, path: &str) -> bool {
    matches!(c.host.as_str(), "localhost" | "127.0.0.1" | "::1" | "")
        && std::fs::File::open(path).is_ok()
}

impl Render for ConfigTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *cx.global::<Palette>();
        let body: AnyElement = match (&self.conn, &self.config) {
            (Conn::Failed(e), _) => notice(e, p.danger).into_any_element(),
            (_, None) => notice(
                self.error
                    .as_deref()
                    .unwrap_or("Loading server configuration…"),
                if self.error.is_some() {
                    p.danger
                } else {
                    p.tx3
                },
            )
            .into_any_element(),
            (_, Some(_)) => div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(filters(self, cx))
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .child(categories(self, cx))
                        .child(grid(self, cx))
                        .child(detail(self, cx)),
                )
                .children(pending_panel(self, cx))
                .into_any_element(),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.bg)
            .child(header(self, cx))
            .child(body)
    }
}

fn notice(text: &str, color: gpui_kit::Hsla) -> Div {
    div()
        .p(px(20.))
        .text_color(color)
        .child(SharedString::from(text.to_owned()))
}

fn header(t: &ConfigTab, cx: &mut Context<ConfigTab>) -> impl IntoElement {
    let p = *cx.global::<Palette>();
    let version = t.driver().map(|d| d.server_version().to_owned());
    let file = t.config.as_ref().and_then(|c| c.config_file.clone());
    let openable = file
        .as_deref()
        .filter(|f| local_file(&t.connection, f))
        .map(str::to_owned);
    let read_only = t
        .connection
        .read_only
        .then(|| "Read-only connection: variables are shown, nothing can be changed.".to_owned())
        .or_else(|| t.config.as_ref().and_then(|c| c.read_only.clone()));
    let container = t.config.as_ref().and_then(|c| c.container.clone());
    div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .px(px(20.))
        .pt(px(20.))
        .pb(px(16.))
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_end()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .text_size(px(20.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .line_height(relative(1.2))
                                .child("Server configuration"),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap_x(px(16.))
                                .gap_y(px(2.))
                                .text_color(p.tx2)
                                .child(SharedString::from(t.connection.name.clone()))
                                .children(version.map(SharedString::from))
                                .when_some(file.clone(), |d, f| {
                                    let copy = f.clone();
                                    d.child(
                                        div()
                                            .id("config-file")
                                            .font_family(MONO_FONT)
                                            .text_size(data_size())
                                            .hover(|s| s.text_color(p.tx))
                                            .tooltip(|window, cx| {
                                                Tooltip::new("Copy the path").build(window, cx)
                                            })
                                            .clickable(cx.listener(move |_, _, _, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    copy.clone(),
                                                ));
                                            }))
                                            .child(SharedString::from(f)),
                                    )
                                }),
                        ),
                )
                .when_some(openable, |d, f| {
                    let name = f.rsplit('/').next().unwrap_or(&f).to_owned();
                    d.child(
                        button(
                            "open-config-file",
                            Kind::Outline,
                            true,
                            "Open the server's option file",
                            cx,
                        )
                        .child(SharedString::from(format!("Open {name}")))
                        .clickable(cx.listener(move |_, _, _, cx| {
                            cx.open_with_system(std::path::Path::new(&f));
                        })),
                    )
                })
                .child(
                    button(
                        "reload-config",
                        Kind::Outline,
                        !t.loading && !t.busy,
                        "Read the variables again; pending changes that still apply stay",
                        cx,
                    )
                    .child("Reload from server")
                    .clickable(cx.listener(|t, _, _, cx| t.reload(cx))),
                ),
        )
        .when_some(t.error.as_ref().filter(|_| t.config.is_some()), |d, e| {
            d.child(
                div()
                    .text_color(p.danger)
                    .child(SharedString::from(format!("Reload failed: {e}"))),
            )
        })
        .when_some(t.message.as_ref(), |d, (m, failed)| {
            d.child(
                div()
                    .text_color(if *failed { p.danger } else { p.tx2 })
                    .child(SharedString::from(m.clone())),
            )
        })
        .when_some(read_only, |d, r| {
            d.child(div().text_color(p.tx2).child(SharedString::from(r)))
        })
        .when_some(container, |d, c| {
            d.child(div().text_color(p.tx2).child(SharedString::from(c)))
        })
}

fn filters(t: &ConfigTab, cx: &mut Context<ConfigTab>) -> impl IntoElement {
    let p = *cx.global::<Palette>();
    let chips = Filter::ALL.map(|f| {
        let on = f == t.filter;
        let n = t.count(f);
        let id = match f {
            Filter::All => "config-all",
            Filter::Modified => "config-modified",
            Filter::Pending => "config-pending",
            Filter::Restart => "config-restart",
        };
        let count = (f != Filter::All).then(|| {
            let d = div().child(n.to_string());
            match (on, f) {
                (true, _) => d,
                (false, Filter::Pending) if n > 0 => {
                    d.text_color(p.warn).font_weight(FontWeight::MEDIUM)
                }
                (false, _) => d.text_color(p.tx3),
            }
        });
        button(id, Kind::Outline, true, f.label(), cx)
            .when(on, |b| b.bg(p.tx).border_color(p.tx).text_color(p.bg))
            .child(f.label())
            .children(count)
            .clickable(cx.listener(move |t, _, _, cx| t.set_filter(f, cx)))
    });
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(16.))
        .gap_y(px(8.))
        .px(px(20.))
        .pb(px(16.))
        .border_b_1()
        .border_color(p.line)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .flex_grow(1.)
                .flex_basis(px(240.))
                .max_w(px(360.))
                .h(px(28.))
                .px(px(8.))
                .border_1()
                .border_color(p.line2)
                .rounded(RADIUS)
                .child(icon(Icon::Search, p.tx3))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(&t.search).appearance(false).h(px(26.)).px_0()),
                ),
        )
        .child(div().flex().flex_wrap().gap(px(4.)).children(chips))
}

/// `.t` row of the category list.
fn nav_row(
    id: SharedString,
    label: &'static str,
    modified: usize,
    on: bool,
    p: &Palette,
) -> gpui_kit::Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(6.))
        .h(px(28.))
        .pl(px(8.))
        .pr(px(8.))
        .rounded(RADIUS)
        .whitespace_nowrap()
        .when(on, |d| d.bg(p.sel).font_weight(FontWeight::MEDIUM))
        .when(!on, |d| d.hover(|s| s.bg(p.hover)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .child(label),
        )
        .when(modified > 0, |d| {
            d.child(
                div()
                    .flex_none()
                    .font_family(MONO_FONT)
                    .text_size(px(11.5))
                    .text_color(p.tx3)
                    .font_weight(FontWeight::NORMAL)
                    .child(format!("{modified} modified")),
            )
        })
}

fn categories(t: &ConfigTab, cx: &mut Context<ConfigTab>) -> impl IntoElement {
    let p = *cx.global::<Palette>();
    let params = t
        .config
        .as_ref()
        .map(|c| c.params.as_slice())
        .unwrap_or_default();
    let rows = Category::ALL.into_iter().filter_map(|c| {
        let in_c: Vec<&ConfigParam> = params.iter().filter(|x| x.category == c).collect();
        if in_c.is_empty() {
            return None;
        }
        let modified = in_c.iter().filter(|x| x.is_modified()).count();
        Some(
            nav_row(
                format!("category-{}", c.label()).into(),
                c.label(),
                modified,
                t.category == Some(c),
                &p,
            )
            .clickable(cx.listener(move |t, _, _, cx| t.set_category(Some(c), cx))),
        )
    });
    div()
        .id("config-categories")
        .flex_none()
        .w(px(240.))
        .h_full()
        .overflow_y_scroll()
        .p(px(8.))
        .flex()
        .flex_col()
        .gap(px(1.))
        .border_r_1()
        .border_color(p.line)
        .child(
            nav_row(
                "category-all".into(),
                "All categories",
                0,
                t.category.is_none(),
                &p,
            )
            .clickable(cx.listener(|t, _, _, cx| t.set_category(None, cx))),
        )
        .children(rows)
}

/// A `.pc` cell: fixed width, or a share of the rest.
fn cell(width: Result<f32, f32>) -> Div {
    let d = div()
        .px(px(12.))
        .overflow_hidden()
        .text_ellipsis()
        .whitespace_nowrap();
    match width {
        Ok(w) => d.w(px(w)).flex_none(),
        Err(grow) => d.flex_grow(grow).flex_basis(px(0.)).min_w_0(),
    }
}

fn widths(col: usize) -> Result<f32, f32> {
    match col {
        0 => Err(1.3),
        1 => Err(1.),
        n => Ok(FIXED[n - 2]),
    }
}

fn grid(t: &ConfigTab, cx: &mut Context<ConfigTab>) -> impl IntoElement {
    let p = *cx.global::<Palette>();
    let (sort_col, desc) = t.sort;
    let head = COLUMNS.iter().enumerate().map(|(i, (col, label))| {
        let col = *col;
        cell(widths(i))
            .flex()
            .items_center()
            .gap(px(4.))
            .child(*label)
            .when(col == sort_col, |d| {
                d.child(icon(
                    if desc { Icon::SortDesc } else { Icon::SortAsc },
                    p.tx3,
                ))
            })
            .id(("config-head", i))
            .clickable(cx.listener(move |t, _, _, cx| t.set_sort(col, cx)))
    });
    let empty = t.lines.is_empty();
    div()
        .flex_1()
        .min_w(px(420.))
        .flex()
        .flex_col()
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .h(px(ROW))
                .bg(p.panel)
                .border_b_1()
                .border_color(p.line)
                .text_color(p.tx2)
                .text_size(small_size())
                .font_weight(FontWeight::MEDIUM)
                .children(head),
        )
        .when(empty, |d| d.child(notice("No parameters match.", p.tx3)))
        .child(
            uniform_list(
                "config-rows",
                t.lines.len(),
                cx.processor(|t, range: Range<usize>, _, cx| {
                    range
                        .filter_map(|ix| Some(line(t, *t.lines.get(ix)?, cx)))
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&t.list)
            .w_full()
            .flex_1(),
        )
}

fn line(t: &ConfigTab, l: Line, cx: &mut Context<ConfigTab>) -> AnyElement {
    let p = *cx.global::<Palette>();
    let i = match l {
        // `.gh`, at the grid's row height.
        Line::Group(c) => {
            return div()
                .h(px(ROW))
                .flex()
                .items_end()
                .px(px(12.))
                .pb(px(6.))
                .border_b_1()
                .border_color(p.line)
                .text_size(small_size())
                .font_weight(FontWeight::SEMIBOLD)
                .child(c.label())
                .into_any_element();
        }
        Line::Param(i) => i,
    };
    let Some(x) = t.config.as_ref().and_then(|c| c.params.get(i)) else {
        return div().into_any_element();
    };
    let selected = t.selected.as_deref() == Some(x.name.as_str());
    let pending = t.pending_of(&x.name);
    let current = x.display(x.value.as_deref());
    let value = match pending.map(|c| &c.value) {
        None => cell(widths(1))
            .when(x.is_modified(), |d| d.font_weight(FontWeight::MEDIUM))
            .when(!x.is_modified(), |d| d.text_color(p.tx3))
            .child(SharedString::from(current)),
        Some(v) => {
            let next = match v {
                NewValue::Set(v) => x.display(x.check(v).ok().as_deref().or(Some(v))),
                NewValue::Default => match &x.default {
                    Some(d) => x.display(Some(d)),
                    None => "default".into(),
                },
            };
            cell(widths(1))
                .flex()
                .items_center()
                .gap(px(6.))
                .child(
                    div()
                        .text_color(p.tx3)
                        .line_through()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(SharedString::from(current)),
                )
                .child(icon(Icon::ArrowRight, p.warn))
                .child(
                    div()
                        .text_color(p.warn)
                        .font_weight(FontWeight::MEDIUM)
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(SharedString::from(next)),
                )
        }
    };
    let restart = x.applies != Applies::Now && x.applies != Applies::Unknown;
    let name = x.name.clone();
    div()
        .id(("config-row", i))
        .w_full()
        .flex()
        .items_center()
        .h(px(ROW))
        .border_b_1()
        .border_color(p.line)
        .font_family(MONO_FONT)
        .text_size(data_size())
        .when(pending.is_some() && !selected, |d| d.bg(p.warn_bg))
        .when(selected, |d| d.bg(p.sel))
        .when(!selected && pending.is_none(), |d| {
            d.hover(|s| s.bg(p.panel))
        })
        .clickable(cx.listener(move |t, _, window, cx| t.select(name.clone(), window, cx)))
        .child(cell(widths(0)).child(SharedString::from(x.name.clone())))
        .child(value)
        .child(
            cell(widths(2)).text_color(p.tx3).child(SharedString::from(
                x.default
                    .as_deref()
                    .map(|d| x.display(Some(d)))
                    .unwrap_or_default(),
            )),
        )
        .child(
            cell(widths(3))
                .text_color(p.tx3)
                .child(SharedString::from(x.source.short())),
        )
        .child(
            cell(widths(4))
                .when(restart, |d| d.font_weight(FontWeight::MEDIUM))
                .when(!restart, |d| d.text_color(p.tx3))
                .child(
                    match (x.applies, t.config.as_ref().is_some_and(|c| c.oracle)) {
                        // Oracle: a pluggable database cannot change it at all.
                        (Applies::Fixed, true) => "Root only",
                        (a, _) => a.short(),
                    },
                ),
        )
        .into_any_element()
}

/// `.dl`: a 96 px term column and the value.
fn fact(term: &'static str, value: impl IntoElement, p: &Palette) -> Div {
    div()
        .flex()
        .gap(px(12.))
        .child(div().w(px(96.)).flex_none().text_color(p.tx3).child(term))
        .child(div().flex_1().min_w_0().child(value))
}

fn mono(s: String) -> Div {
    div()
        .font_family(MONO_FONT)
        .text_size(data_size())
        .child(SharedString::from(s))
}

fn applies_long(cfg: &ServerConfig, x: &ConfigParam) -> &'static str {
    match (x.applies, x.scope) {
        (Applies::Now, Some(Scope::GlobalAndSession)) => {
            "Immediately for new sessions; open sessions keep their own value"
        }
        (Applies::Now, _) if x.deferred => "For sessions that start after the change",
        (Applies::Now, _) => "Immediately, no restart needed",
        (Applies::Restart, _) if cfg.oracle && cfg.persist => {
            "Next server restart, from the spfile (SCOPE = SPFILE)"
        }
        (Applies::Fixed, _) if cfg.oracle => "Only from the root container (CDB$ROOT)",
        (Applies::Restart, _) if cfg.persist => "Next server restart",
        (Applies::Restart, _) => "Next server restart, from the option file",
        (Applies::Fixed, _) => "Server start only, from the option file or command line",
        (Applies::Unknown, _) => "Not reported by the server",
    }
}

/// The editor for `x`: a switch, a list, or a field with unit buttons.
fn editor(t: &ConfigTab, x: &ConfigParam, cx: &mut Context<ConfigTab>) -> AnyElement {
    let p = *cx.global::<Palette>();
    let chosen = match t.pending_of(&x.name).map(|c| &c.value) {
        Some(NewValue::Set(v)) => x.check(v).ok(),
        Some(NewValue::Default) => x.default.clone(),
        None => x.value.clone(),
    }
    .unwrap_or_default();
    match &x.value_type {
        ValueType::Boolean => {
            let on = chosen.eq_ignore_ascii_case("ON");
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    Switch::new("config-switch")
                        .checked(on)
                        .color(p.blue)
                        .cursor_pointer()
                        .on_change(cx.listener(|t, v: &bool, window, cx| {
                            let v = if *v { "ON" } else { "OFF" };
                            t.pick(NewValue::Set(v.into()), window, cx);
                        })),
                )
                .child(mono(if on { "ON".into() } else { "OFF".into() }))
                .into_any_element()
        }
        ValueType::Enum(list) | ValueType::Set(list) => {
            let set = matches!(x.value_type, ValueType::Set(_));
            let picked: Vec<String> = if set {
                chosen.split(',').map(str::to_owned).collect()
            } else {
                vec![chosen.clone()]
            };
            let rows = list.iter().enumerate().map(|(i, v)| {
                let on = picked.iter().any(|c| c.eq_ignore_ascii_case(v));
                let value = v.clone();
                let picked = picked.clone();
                let list = list.clone();
                div()
                    .id(("config-option", i))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(28.))
                    .px(px(8.))
                    .rounded(RADIUS)
                    .font_family(MONO_FONT)
                    .text_size(data_size())
                    .when(on, |d| d.bg(p.sel).font_weight(FontWeight::MEDIUM))
                    .when(!on, |d| d.hover(|s| s.bg(p.hover)))
                    .child(
                        div()
                            .w(px(16.))
                            .flex_none()
                            .when(on, |d| d.child(icon(Icon::Commit, p.link))),
                    )
                    .child(SharedString::from(v.clone()))
                    .clickable(cx.listener(move |t, _, window, cx| {
                        let next = if set {
                            list.iter()
                                .filter(|o| {
                                    let had = picked.iter().any(|c| c.eq_ignore_ascii_case(o));
                                    if **o == value { !had } else { had }
                                })
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(",")
                        } else {
                            value.clone()
                        };
                        t.pick(NewValue::Set(next), window, cx);
                    }))
            });
            div()
                .id("config-options")
                .max_h(px(240.))
                .overflow_y_scroll()
                .p(px(4.))
                .border_1()
                .border_color(p.line2)
                .rounded(RADIUS)
                .flex()
                .flex_col()
                .gap(px(1.))
                .children(rows)
                .into_any_element()
        }
        ValueType::Integer | ValueType::Float | ValueType::Text => {
            let field = div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .h(px(32.))
                .px(px(10.))
                .border_1()
                .border_color(if t.edit_error.is_some() {
                    p.danger
                } else {
                    p.line2
                })
                .rounded(RADIUS)
                .bg(p.bg)
                .font_family(MONO_FONT)
                .text_size(data_size())
                .child(Input::new(&t.value).appearance(false).h(px(30.)).px_0());
            let units = x.unit.map(|u| {
                let text = t.value.read(cx).value().to_string();
                let from = u.steps().get(t.unit).map_or(1, |s| s.1);
                let buttons = u.steps().iter().enumerate().map(|(i, (label, step))| {
                    let on = i == t.unit;
                    let ok = on
                        || scale(&text, from).and_then(|n| unscale(n, *step)).is_some()
                        || scale(&text, from).is_none();
                    let id: SharedString = format!("unit-{label}").into();
                    div()
                        .id(id)
                        .flex()
                        .items_center()
                        .h(px(32.))
                        .px(px(8.))
                        .border_1()
                        .border_color(if on { p.tx } else { p.line2 })
                        .rounded(RADIUS)
                        .font_family(MONO_FONT)
                        .text_size(data_size())
                        .when(on, |d| d.bg(p.tx).text_color(p.bg))
                        .when(!on && ok, |d| d.hover(|s| s.bg(p.hover)))
                        .when(!ok, |d| d.opacity(0.38))
                        .child(*label)
                        .when(ok && !on, |d| {
                            d.clickable(cx.listener(move |t, _, window, cx| {
                                t.set_unit(i, window, cx);
                            }))
                        })
                });
                div().flex().gap(px(4.)).children(buttons)
            });
            div()
                .flex()
                .gap(px(6.))
                .child(field)
                .children(units)
                .into_any_element()
        }
    }
}

fn detail(t: &ConfigTab, cx: &mut Context<ConfigTab>) -> impl IntoElement {
    let p = *cx.global::<Palette>();
    let aside = div()
        .id("config-detail")
        .flex_none()
        .w(px(288.))
        .h_full()
        .overflow_y_scroll()
        .p(px(20.))
        .flex()
        .flex_col()
        .gap(px(20.))
        .border_l_1()
        .border_color(p.line);
    let (Some(cfg), Some(x)) = (
        t.config.as_ref(),
        t.selected.as_deref().and_then(|s| t.param(s)),
    ) else {
        return aside.child(
            div()
                .text_color(p.tx3)
                .child("Select a parameter to see and change it."),
        );
    };
    let pending = t.pending_of(&x.name);
    let locked = t.locked(x);
    let text = |s: String| SharedString::from(s);
    let mut facts = vec![fact(
        "Current",
        mono(match (&x.value, x.unit) {
            (Some(v), Some(_)) if x.display(Some(v)) != *v => {
                format!("{} ({v})", x.display(Some(v)))
            }
            (Some(v), _) if v.is_empty() => "(empty)".into(),
            (v, _) => x.display(v.as_deref()),
        }),
        &p,
    )];
    if let Some(d) = &x.default {
        facts.push(fact("Default", mono(x.display(Some(d))), &p));
    }
    facts.push(fact("Set in", text(x.source.long()), &p));
    if let Some(v) = &x.persisted {
        facts.push(fact(
            "Persisted",
            mono(if x.restart_pending() {
                format!("{}, used from the next restart", x.display(Some(v)))
            } else {
                x.display(Some(v))
            }),
            &p,
        ));
    }
    facts.push(fact("Applies on", applies_long(cfg, x), &p));
    if let Some(s) = x.scope {
        facts.push(fact(
            "Scope",
            match s {
                Scope::GlobalOnly => "Global only",
                Scope::GlobalAndSession => "Global, and each session's own copy",
            },
            &p,
        ));
    }
    if let Some(w) = cfg.who_can_set(x) {
        facts.push(fact("Who can set", w, &p));
    }
    let mode = (cfg.persist && !cfg.redis && locked.is_none() && x.applies != Applies::Restart)
        .then(|| {
            let current = pending.map_or(Mode::Persist, |c| c.mode);
            let enabled = pending.is_some();
            let seg = |id: &'static str, label: &'static str, m: Mode, tip: &'static str| {
                let on = enabled && current == m;
                button(id, Kind::Outline, enabled, tip, cx)
                    .when(on, |b| b.bg(p.tx).border_color(p.tx).text_color(p.bg))
                    .child(label)
                    .when(enabled, |b| {
                        b.clickable(cx.listener(move |t, _, _, cx| t.set_mode(m, cx)))
                    })
            };
            div()
                .flex()
                .gap(px(4.))
                .child(seg(
                    "mode-persist",
                    "Keep after restart",
                    Mode::Persist,
                    if cfg.oracle {
                        "SCOPE = BOTH: applies now and is written to the spfile"
                    } else {
                        "SET PERSIST: applies now and is written to mysqld-auto.cnf"
                    },
                ))
                .child(seg(
                    "mode-run",
                    "This run only",
                    Mode::RunOnly,
                    if cfg.oracle {
                        "SCOPE = MEMORY: applies now and is lost at the next restart"
                    } else {
                        "SET GLOBAL: applies now and is lost at the next restart"
                    },
                ))
        });
    // Revert: to the reported default, or to the server's own default when it does
    // not say what that is.
    let revert = locked.is_none().then(|| match &x.default {
        Some(d) if x.value.as_deref() != Some(d.as_str()) || x.persisted.is_some() => {
            Some(format!("Revert to {}", x.display(Some(d))))
        }
        Some(_) => None,
        None if x.source.is_modified() || x.persisted.is_some() => {
            Some("Revert to default".to_owned())
        }
        None => None,
    });
    let revert = revert.flatten().filter(|_| {
        !matches!(pending.map(|c| &c.value), Some(NewValue::Default))
            && cfg
                .statements(&ConfigChange {
                    name: x.name.clone(),
                    value: NewValue::Default,
                    mode: Mode::Persist,
                })
                .is_ok()
    });
    let related = hint(cfg, x);
    aside
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .font_family(MONO_FONT)
                        .text_size(px(15.))
                        .font_weight(FontWeight::MEDIUM)
                        .child(text(x.name.clone())),
                )
                .when_some(x.description.clone(), |d, desc| {
                    d.child(
                        div()
                            .text_color(p.tx2)
                            .line_height(relative(1.5))
                            .child(text(desc)),
                    )
                }),
        )
        .child(match &locked {
            Some(why) => div()
                .text_color(p.tx2)
                .line_height(relative(1.5))
                .child(text(why.clone())),
            None => div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(div().font_weight(FontWeight::MEDIUM).child("New value"))
                .child(editor(t, x, cx))
                .when_some(t.edit_error.clone(), |d, e| {
                    d.child(div().text_color(p.danger).child(text(e)))
                })
                .when_some(x.allowed().filter(|_| t.edit_error.is_none()), |d, a| {
                    d.child(div().text_color(p.tx3).child(text(a)))
                })
                .children(mode),
        })
        .child(div().flex().flex_col().gap(px(8.)).children(facts))
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(8.))
                .when_some(revert, |d, label| {
                    d.child(
                        button(
                            "config-revert",
                            Kind::Outline,
                            true,
                            "Make the default value pending",
                            cx,
                        )
                        .child(text(label))
                        .clickable(cx.listener(|t, _, window, cx| {
                            t.pick(NewValue::Default, window, cx);
                        })),
                    )
                })
                .when(pending.is_some(), |d| {
                    let name = x.name.clone();
                    d.child(
                        button(
                            "config-keep",
                            Kind::Ghost,
                            true,
                            "Drop this pending change",
                            cx,
                        )
                        .child("Keep current value")
                        .clickable(cx.listener(move |t, _, window, cx| {
                            t.drop_pending(&name);
                            t.arrange();
                            t.sync_editor(window, cx);
                            cx.notify();
                        })),
                    )
                }),
        )
        .when_some(related, |d, (hint, target)| {
            let label = format!("Show {target}");
            d.child(
                div()
                    .pt(px(16.))
                    .border_t_1()
                    .border_color(p.line)
                    .text_color(p.tx2)
                    .line_height(relative(1.5))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(text(hint))
                    .child(
                        div()
                            .id("config-related")
                            .text_color(p.link)
                            .hover(|s| s.text_color(p.tx))
                            .child(text(label))
                            .clickable(cx.listener(move |t, _, window, cx| {
                                t.show(target.clone(), window, cx);
                            })),
                    ),
            )
        })
}

fn pending_panel(t: &ConfigTab, cx: &mut Context<ConfigTab>) -> Option<impl IntoElement> {
    let p = *cx.global::<Palette>();
    let cfg = t.config.as_ref()?;
    let n = t.pending.len();
    if n == 0 {
        return None;
    }
    let names = |f: &dyn Fn(&ConfigChange, &ConfigParam) -> bool| -> Vec<String> {
        t.pending
            .iter()
            .filter(|c| cfg.param(&c.name).is_some_and(|x| f(c, x)))
            .map(|c| c.name.clone())
            .collect()
    };
    let restart = names(&|_, x| x.applies == Applies::Restart);
    let run_only =
        names(&|c, x| cfg.persist && x.applies != Applies::Restart && c.mode == Mode::RunOnly);
    let mut note = "Nothing is written until you apply.".to_owned();
    if !restart.is_empty() {
        note.push_str(&format!(
            " {} {} effect after the next server restart.",
            restart.join(", "),
            if restart.len() == 1 { "takes" } else { "take" }
        ));
    }
    if !run_only.is_empty() {
        note.push_str(&format!(
            " {} {} only until the server restarts.",
            run_only.join(", "),
            if run_only.len() == 1 { "lasts" } else { "last" }
        ));
    }
    let lines: Vec<String> = if cfg.persist || cfg.redis {
        Vec::new()
    } else {
        t.pending
            .iter()
            .filter_map(|c| cfg.config_line(c))
            .collect()
    };
    let sql = t.statements().join("\n");
    let can_apply = !t.busy && !t.connection.read_only && cfg.read_only.is_none();
    let copy = format!("[mariadb]\n{}\n", lines.join("\n"));
    Some(
        div()
            .id("config-pending")
            .flex()
            .flex_wrap()
            .flex_none()
            .items_start()
            .gap_x(px(24.))
            .gap_y(px(12.))
            .px(px(20.))
            .py(px(16.))
            .bg(p.warn_bg)
            .border_t_1()
            .border_color(p.warn_line)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .min_w_0()
                    .flex_grow(1.)
                    .flex_basis(px(200.))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("{n} pending {}", noun(n))),
                    )
                    .child(
                        div()
                            .text_color(p.tx2)
                            .line_height(relative(1.5))
                            .child(SharedString::from(note)),
                    )
                    .when(cfg.redis, |d| {
                        d.child(
                            div()
                                .text_color(p.warn)
                                .font_weight(FontWeight::MEDIUM)
                                .line_height(relative(1.5))
                                .child(match &cfg.config_file {
                                    Some(f) => format!(
                                        "CONFIG SET changes apply at once and are lost when the \
                                         server restarts, unless CONFIG REWRITE writes them to \
                                         {f}."
                                    ),
                                    None => "CONFIG SET changes apply at once and are lost when \
                                             the server restarts. The server reports no config \
                                             file, so CONFIG REWRITE will fail."
                                        .to_owned(),
                                }),
                        )
                        .child(
                            gpui_kit::component::checkbox::Checkbox::new("config-rewrite")
                                .checked(t.rewrite)
                                .label("Also rewrite the config file (CONFIG REWRITE)")
                                .cursor_pointer()
                                .on_click(
                                    cx.listener(|t, on: &bool, _, cx| t.set_rewrite(*on, cx)),
                                ),
                        )
                    })
                    .when(!cfg.persist && !cfg.redis, |d| {
                        d.child(
                            div()
                                .text_color(p.warn)
                                .font_weight(FontWeight::MEDIUM)
                                .line_height(relative(1.5))
                                .child(
                                    "MariaDB has no SET PERSIST: these changes are lost when the \
                                     server restarts. To keep them, add these lines to the \
                                     server's option file:",
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .items_start()
                                .gap(px(8.))
                                .child(
                                    div()
                                        .id("config-lines")
                                        .flex_1()
                                        .min_w_0()
                                        .px(px(12.))
                                        .py(px(8.))
                                        .bg(p.bg)
                                        .border_1()
                                        .border_color(p.warn_line)
                                        .rounded(RADIUS)
                                        .font_family(MONO_FONT)
                                        .text_size(data_size())
                                        .line_height(px(20.))
                                        .whitespace_nowrap()
                                        .overflow_x_scroll()
                                        .child(SharedString::from(format!(
                                            "[mariadb]\n{}",
                                            lines.join("\n")
                                        ))),
                                )
                                .child(
                                    button(
                                        "copy-config-lines",
                                        Kind::Outline,
                                        true,
                                        "Copy the option-file lines",
                                        cx,
                                    )
                                    .child("Copy")
                                    .clickable(cx.listener(
                                        move |_, _, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy.clone(),
                                            ));
                                        },
                                    )),
                                ),
                        )
                    }),
            )
            .child(
                div()
                    .id("config-sql")
                    .flex_grow(2.)
                    .flex_shrink(1.)
                    .flex_basis(px(340.))
                    .min_w_0()
                    .max_h(px(20. * 8. + 18.))
                    .overflow_scroll()
                    .px(px(12.))
                    .py(px(8.))
                    .bg(p.bg)
                    .border_1()
                    .border_color(p.warn_line)
                    .rounded(RADIUS)
                    .font_family(MONO_FONT)
                    .text_size(data_size())
                    .line_height(px(20.))
                    .whitespace_nowrap()
                    .child(highlighted(&sql, &p)),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.))
                    .child(
                        button(
                            "config-discard",
                            Kind::Outline,
                            !t.busy,
                            "Drop every pending change",
                            cx,
                        )
                        .child("Discard changes")
                        .when(!t.busy, |b| {
                            b.clickable(cx.listener(|t, _, window, cx| t.discard(window, cx)))
                        }),
                    )
                    .child(
                        button(
                            "config-apply",
                            Kind::Primary,
                            can_apply,
                            "Run the statements in order; the first error stops the rest",
                            cx,
                        )
                        .child(if t.busy {
                            "Applying…".to_owned()
                        } else {
                            format!("Apply {n} {}", noun(n))
                        })
                        .when(can_apply, |b| {
                            b.clickable(
                                cx.listener(|t, _, window, cx| t.apply(window, cx).detach()),
                            )
                        }),
                    ),
            ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadlock_core::config::Unit;

    #[test]
    fn units_round_trip_through_the_buttons() {
        let steps = Unit::Bytes.steps();
        // 64 MB typed, then KB, then GB.
        let bytes = scale("64", steps[2].1).unwrap();
        assert_eq!(bytes, 67_108_864);
        assert_eq!(unscale(bytes, steps[1].1).as_deref(), Some("65536"));
        assert_eq!(unscale(bytes, steps[3].1).as_deref(), Some("0.0625"));
        assert_eq!(scale("0.0625", steps[3].1), Some(bytes));
    }
}
