//! A Redis key tab: the table tab's grid, pending changes and editor, with a facts
//! header (type, size, memory, expiry), a `match` filter, a text editor for strings
//! and JSON, and the key's actions (Set expiry, Rename, Delete key). Submits run in
//! `MULTI` / `EXEC` under the `WATCH` the load set, so a concurrent change aborts.

use std::sync::Arc;
use std::time::Instant;

use deadlock_core::connection::Environment;
use deadlock_core::driver::{Value, page_size};
use deadlock_core::redis::{self as rk, Command, KeyType, Part};
use deadlock_core::redis_driver::{KeyFacts, KeyText};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{self, Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, HighlightStyle,
    InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement as _,
    Styled, StyledText, Subscription, Task, Window, div, px,
};

use super::{QueryTab, Session, TabEvent};
use crate::app::Db;
use crate::assets::Icon;
use crate::dialog::Ask;
use crate::theme::{MONO_FONT, Palette, RADIUS, data_size, small_size};
use crate::ui::{Kind as ButtonKind, button, icon_button, labelled, modal};

/// A key tab's Redis state.
pub struct KeyState {
    pub key: Vec<u8>,
    pub facts: Option<KeyFacts>,
    /// When `facts` arrived: the expiry counts down from there.
    loaded_at: Instant,
    /// A string's or JSON document's value as loaded.
    text: Option<KeyText>,
    /// Its editor; read-only for binary and cut values.
    editor: Option<Entity<EditorState>>,
    /// The editor's text as loaded, to tell an edit.
    original: String,
    /// The edited JSON does not parse.
    json_error: Option<String>,
    dialog: Option<Dialog>,
    /// The key is gone: deleted here, or expired.
    pub gone: bool,
    /// Repaints the expiry every second.
    _tick: Option<Task<()>>,
    _editor_sub: Option<Subscription>,
}

impl KeyState {
    pub fn new(label: &str) -> Self {
        Self {
            key: rk::label_key(label),
            facts: None,
            loaded_at: Instant::now(),
            text: None,
            editor: None,
            original: String::new(),
            json_error: None,
            dialog: None,
            gone: false,
            _tick: None,
            _editor_sub: None,
        }
    }

    fn kind(&self) -> Option<&KeyType> {
        self.facts.as_ref().map(|f| &f.kind)
    }

    /// Milliseconds to expiry now; `None` without one.
    fn ttl_now(&self) -> Option<i64> {
        let f = self.facts.as_ref()?;
        (f.pttl >= 0).then(|| {
            f.pttl - i64::try_from(self.loaded_at.elapsed().as_millis()).unwrap_or(i64::MAX)
        })
    }
}

enum Dialog {
    Expiry {
        input: Entity<InputState>,
        _sub: Subscription,
    },
    Rename {
        input: Entity<InputState>,
        overwrite: bool,
        error: Option<String>,
        _sub: Subscription,
    },
}

/// The value as the editor shows it: JSON pretty-printed, binary as hex.
fn editor_text(kind: &KeyType, v: &Value) -> String {
    match (kind, v) {
        (_, Value::Bytes(b)) => b
            .iter()
            .map(|x| format!("{x:02x}"))
            .collect::<Vec<_>>()
            .join(" "),
        (KeyType::Json, v) => {
            let raw = v.to_string();
            serde_json::from_str::<serde_json::Value>(&raw)
                .ok()
                .and_then(|j| serde_json::to_string_pretty(&j).ok())
                .unwrap_or(raw)
        }
        (_, v) => v.to_string(),
    }
}

/// Whether a string value reads as JSON: an object or array that parses.
fn looks_json(s: &str) -> bool {
    let t = s.trim_start();
    (t.starts_with('{') || t.starts_with('['))
        && serde_json::from_str::<serde_json::Value>(s).is_ok()
}

/// `One field updated and one deleted.`, with the type's own noun.
fn summary(kind: &KeyType, updated: usize, deleted: usize, added: usize) -> String {
    let noun = match kind {
        KeyType::Hash => ("field", "fields"),
        KeyType::List => ("element", "elements"),
        KeyType::Set | KeyType::ZSet => ("member", "members"),
        KeyType::Stream => ("entry", "entries"),
        _ => ("value", "values"),
    };
    let word = |n: usize| match n {
        1 => "one".to_owned(),
        2 => "two".to_owned(),
        3 => "three".to_owned(),
        n => n.to_string(),
    };
    let mut parts = Vec::new();
    for (n, verb) in [(updated, "updated"), (deleted, "deleted"), (added, "added")] {
        if n == 0 {
            continue;
        }
        if parts.is_empty() {
            let name = if n == 1 { noun.0 } else { noun.1 };
            parts.push(format!("{} {name} {verb}", word(n)));
        } else {
            parts.push(format!("{} {verb}", word(n)));
        }
    }
    let text = match parts.as_slice() {
        [] => return String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    let mut c = text.chars();
    c.next().map_or_else(String::new, |f| {
        format!("{}{}.", f.to_uppercase(), c.as_str())
    })
}

/// Commands with the design's colors: the name bold in the link color, values green.
fn highlighted(commands: &[Command], p: &Palette) -> StyledText {
    let mut text = String::new();
    let mut marks = Vec::new();
    for (i, c) in commands.iter().enumerate() {
        if i > 0 {
            text.push('\n');
        }
        for (piece, part) in c.parts() {
            let start = text.len();
            text.push_str(&piece);
            let style = match part {
                Part::Name => HighlightStyle {
                    color: Some(p.link),
                    font_weight: Some(FontWeight::MEDIUM),
                    ..HighlightStyle::default()
                },
                Part::Value => HighlightStyle {
                    color: Some(p.ok),
                    ..HighlightStyle::default()
                },
                Part::Arg => continue,
            };
            // The value's leading space stays plain.
            let start = if part == Part::Value {
                start + 1
            } else {
                start
            };
            marks.push((start..text.len(), style));
        }
    }
    StyledText::new(SharedString::from(text)).with_highlights(marks)
}

impl QueryTab {
    /// A Redis key tab.
    pub fn is_key(&self) -> bool {
        self.table.as_ref().is_some_and(|t| t.redis.is_some())
    }

    fn key_state(&self) -> Option<&KeyState> {
        self.table.as_ref()?.redis.as_ref()
    }

    fn key_state_mut(&mut self) -> Option<&mut KeyState> {
        self.table.as_mut()?.redis.as_mut()
    }

    /// Loads the key's facts (setting the `WATCH`), then its first page of elements
    /// or its text. `keep` reloads into the grid in place.
    pub(super) fn load_key(&mut self, keep: bool, cx: &mut Context<Self>) {
        let Session::Ready(driver) = &self.session else {
            return;
        };
        let driver = Arc::clone(driver);
        let Some(t) = &mut self.table else { return };
        let Some(k) = &t.redis else { return };
        let key = k.key.clone();
        let filter = t.filter.read(cx).value().trim().to_owned();
        let rows_on_screen = if keep {
            t.grid
                .as_ref()
                .map_or(0, |g| g.read(cx).delegate().row_count())
        } else {
            0
        };
        t.busy = true;
        if !keep {
            t.notice = None;
        }
        t.generation += 1;
        let generation = t.generation;
        let rx = cx.global::<Db>().0.spawn(async move {
            let r = driver.as_redis().ok_or("not a Redis connection")?;
            let facts = r.facts(&key).await.map_err(|e| e.to_string())?;
            if facts.kind.columns().is_empty() {
                let text = match facts.kind {
                    KeyType::String | KeyType::Json => {
                        Some(r.text(&key, &facts).await.map_err(|e| e.to_string())?)
                    }
                    _ => None,
                };
                return Ok((facts, None, text));
            }
            let pattern = (!filter.is_empty()).then_some(filter);
            let mut stream = r
                .rows(&key, &facts.kind, pattern)
                .map_err(|e| e.to_string())?;
            let mut rows = Vec::new();
            loop {
                match stream.next_page().await {
                    Some(Ok(page)) => {
                        let full = page.len() >= page_size();
                        rows.extend(page);
                        if !full || rows.len() >= rows_on_screen {
                            let more = full.then_some(stream);
                            return Ok((facts, Some((rows, more)), None));
                        }
                    }
                    Some(Err(e)) => return Err(e.to_string()),
                    None => return Ok((facts, Some((rows, None)), None)),
                }
            }
        });
        cx.spawn(async move |this, cx| {
            let result: Result<_, String> = rx
                .await
                .unwrap_or_else(|_| Err("the key task stopped".into()));
            let window = this.read_with(cx, |t, _| t.window).ok();
            let Some(window) = window else { return };
            let _ = cx.update_window(window, |_, window, cx| {
                this.update(cx, |this, cx| {
                    this.key_loaded(generation, keep, result, window, cx)
                })
            });
        })
        .detach();
        cx.notify();
    }

    #[allow(clippy::type_complexity)]
    fn key_loaded(
        &mut self,
        generation: u64,
        keep: bool,
        result: Result<
            (
                KeyFacts,
                Option<(
                    Vec<deadlock_core::driver::Row>,
                    Option<deadlock_core::driver::ResultStream>,
                )>,
                Option<KeyText>,
            ),
            String,
        >,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let read_only = self.connection.as_ref().is_some_and(|c| c.read_only);
        let Some(t) = &mut self.table else { return };
        if t.generation != generation {
            return;
        }
        let (facts, rows, text) = match result {
            Ok(r) => r,
            Err(e) => {
                t.busy = false;
                let gone = e.contains("does not exist");
                if let Some(k) = &mut t.redis {
                    k.gone = gone;
                    if gone {
                        k.facts = None;
                    }
                }
                t.notice = Some((e, true));
                cx.notify();
                return;
            }
        };
        let kind = facts.kind.clone();
        let ticking = facts.pttl > 0;
        if let Some(k) = &mut t.redis {
            k.facts = Some(facts);
            k.loaded_at = Instant::now();
            k.gone = false;
        }
        match rows {
            Some((rows, stream)) => {
                let info = kind.table_info();
                self.table_loaded(generation, keep, Ok((info, rows, stream)), window, cx);
            }
            None => {
                t.busy = false;
                self.set_text(&kind, text, read_only, window, cx);
                self.changed(cx);
            }
        }
        if ticking {
            let tick = cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(1))
                        .await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        return;
                    }
                }
            });
            if let Some(k) = self.key_state_mut() {
                k._tick = Some(tick);
            }
        }
        cx.emit(TabEvent::Changed);
        cx.notify();
    }

    /// Puts a string's or JSON document's value in the editor.
    fn set_text(
        &mut self,
        kind: &KeyType,
        text: Option<KeyText>,
        read_only: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(text) = text else {
            if let Some(k) = self.key_state_mut() {
                k.text = None;
                k.editor = None;
            }
            return;
        };
        let shown = editor_text(kind, &text.value);
        let json = *kind == KeyType::Json || matches!(&text.value, Value::Text(s) if looks_json(s));
        let fixed = read_only || text.truncated || matches!(text.value, Value::Bytes(_));
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language(if json { "json" } else { "text" })
                .line_number(true)
                .soft_wrap(true)
                .default_value(shown.clone())
        });
        editor.update(cx, |e, cx| e.set_readonly(fixed, cx));
        let sub = cx.subscribe(&editor, |this, _, e: &InputEvent, cx| {
            if matches!(e, InputEvent::Change) {
                this.text_edited(cx);
            }
        });
        if let Some(k) = self.key_state_mut() {
            k.text = Some(text);
            k.editor = Some(editor);
            k.original = shown;
            k.json_error = None;
            k._editor_sub = Some(sub);
        }
    }

    fn text_edited(&mut self, cx: &mut Context<Self>) {
        let Some(k) = self.key_state() else { return };
        let (Some(editor), Some(kind)) = (&k.editor, k.kind()) else {
            return;
        };
        let value = editor.read(cx).value().to_string();
        let error = (*kind == KeyType::Json)
            .then(|| serde_json::from_str::<serde_json::Value>(&value).err())
            .flatten()
            .map(|e| format!("Not valid JSON: {e}."));
        if let Some(k) = self.key_state_mut() {
            k.json_error = error;
        }
        self.changed(cx);
    }

    /// The edited text, when it differs from the loaded value.
    fn edited_text(&self, cx: &App) -> Option<String> {
        let k = self.key_state()?;
        let value = k.editor.as_ref()?.read(cx).value().to_string();
        (value != k.original).then_some(value)
    }

    /// A string or JSON key's pending edit: one change.
    pub(super) fn key_text_pending(&self, cx: &App) -> usize {
        usize::from(self.edited_text(cx).is_some())
    }

    /// The commands a submit would run.
    pub(super) fn key_commands(&self, cx: &App) -> Result<Vec<Command>, String> {
        let k = self.key_state().ok_or("not a key tab")?;
        let kind = k.kind().ok_or("the key has not loaded")?;
        if let Some(text) = self.edited_text(cx) {
            if let Some(e) = &k.json_error {
                return Err(e.clone());
            }
            // RedisJSON keeps the parsed document: send it compact.
            let text = match kind {
                KeyType::Json => serde_json::from_str::<serde_json::Value>(&text)
                    .map(|j| j.to_string())
                    .unwrap_or(text),
                _ => text,
            };
            return rk::text_command(&k.key, kind, &text)
                .map(|c| vec![c])
                .ok_or_else(|| "this type has no text value".into());
        }
        let grid = self
            .table
            .as_ref()
            .and_then(|t| t.grid.as_ref())
            .ok_or("nothing loaded")?
            .read(cx)
            .delegate();
        let Some(e) = &grid.edit else {
            return Ok(Vec::new());
        };
        let tombstone = format!(
            "\0deadlock-deleted:{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        );
        rk::commands(&k.key, kind, &e.pending, grid.rows(), tombstone.as_bytes())
    }

    /// Runs the pending changes in `MULTI` / `EXEC` under the load's `WATCH`;
    /// production connections confirm first.
    pub(super) fn submit_key(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), String>> {
        if !self.commit_edit(window, cx) {
            return Task::ready(Err("a cell holds a value its column cannot take".into()));
        }
        let Session::Ready(driver) = &self.session else {
            return Task::ready(Err("the key is not loaded".into()));
        };
        let driver = Arc::clone(driver);
        let commands = match self.key_commands(cx) {
            Ok(c) if c.is_empty() => return Task::ready(Ok(())),
            Ok(c) => c,
            Err(e) => {
                self.table_note(e.clone(), true, cx);
                return Task::ready(Err(e));
            }
        };
        let n = self.pending_changes(cx).max(1);
        let production = self
            .connection
            .as_ref()
            .filter(|c| c.environment == Environment::Production)
            .map(|c| c.name.clone());
        let preview: String = commands
            .iter()
            .map(Command::text)
            .collect::<Vec<_>>()
            .join("\n");
        let title = self
            .table
            .as_ref()
            .map(|t| t.table.table.clone())
            .unwrap_or_default();
        let rt = Arc::clone(&cx.global::<Db>().0);
        if let Some(t) = &mut self.table {
            t.busy = true;
        }
        cx.notify();
        let window = self.window;
        cx.spawn(async move |this, cx| {
            if let Some(name) = production
                && !confirm(
                    cx,
                    window,
                    Ask::new(
                        format!(
                            "Submit {n} {} to {title} on production connection {name}?",
                            noun(n)
                        ),
                        &["Submit", "Cancel"],
                    )
                    .statement(preview)
                    .production(true),
                )
                .await
            {
                let _ = this.update(cx, |this, cx| {
                    if let Some(t) = &mut this.table {
                        t.busy = false;
                    }
                    cx.notify();
                });
                return Err("cancelled".to_owned());
            }
            let result = rt
                .spawn(async move {
                    let r = driver
                        .as_redis()
                        .ok_or("not a Redis connection".to_owned())?;
                    r.transact(&commands).await.map_err(|e| e.to_string())
                })
                .await
                .unwrap_or_else(|_| Err("the submit task stopped".into()));
            this.update(cx, |this, cx| {
                if let Some(t) = &mut this.table {
                    t.busy = false;
                }
                match &result {
                    Ok(()) => {
                        this.table_note(format!("{n} {} submitted.", noun(n)), false, cx);
                        this.reload_key(cx);
                    }
                    Err(e) => this.table_note(e.clone(), true, cx),
                }
                cx.notify();
            })
            .map_err(|_| "the tab was closed".to_string())?;
            result
        })
    }

    /// Reload after a submit or an action: the grid in place, the editor afresh.
    pub(super) fn reload_key(&mut self, cx: &mut Context<Self>) {
        if let Some(k) = self.key_state_mut() {
            k.editor = None;
            k.text = None;
        }
        let keep = self.table.as_ref().is_some_and(|t| t.grid.is_some());
        self.load_key(keep, cx);
    }

    /// What an export of all elements reads: the key, its type and the `match` text.
    pub(super) fn key_source(&self, cx: &App) -> Option<(Vec<u8>, KeyType, Option<String>)> {
        let t = self.table.as_ref()?;
        let k = t.redis.as_ref()?;
        let filter = t.filter.read(cx).value().trim().to_owned();
        Some((
            k.key.clone(),
            k.kind()?.clone(),
            (!filter.is_empty()).then_some(filter),
        ))
    }

    /// `Fields 1 to 7 of 7`, for the status bar.
    pub(super) fn key_status(&self, cx: &App) -> Option<String> {
        let t = self.table.as_ref()?;
        let k = t.redis.as_ref()?;
        let f = k.facts.as_ref()?;
        let noun = f.kind.noun();
        if f.kind.columns().is_empty() {
            return f.len.map(|n| format!("{noun} {}", rk::grouped(n)));
        }
        let shown = t.grid.as_ref()?.read(cx).delegate().row_count();
        if shown == 0 {
            return Some(format!("No {}", noun.to_lowercase()));
        }
        let filtered = !t.filter.read(cx).value().trim().is_empty();
        let total = match f.len {
            Some(n) if !filtered => format!(" of {}", rk::grouped(n)),
            _ => " matching".into(),
        };
        Some(format!("{noun} 1 to {}{total}", rk::grouped(shown as u64)))
    }

    /// Why a cell cannot change, beyond the table's rules: stream entries.
    pub(super) fn key_locked(&self) -> Option<String> {
        (self.key_state()?.kind()? == &KeyType::Stream).then(|| {
            "Streams are append-only: entries cannot be edited, only added or deleted.".into()
        })
    }

    /// Push to head or tail: an added list element at that end.
    fn push(&mut self, head: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.add_row(window, cx);
        let end = if head { rk::HEAD } else { rk::TAIL };
        let added = self.table.as_ref().and_then(|t| {
            let g = t.grid.as_ref()?.read(cx).delegate();
            let e = g.edit.as_ref()?;
            e.pending.added().last().map(|(id, _)| id)
        });
        if let Some(id) = added {
            let row = deadlock_core::edit::RowId::Added(id);
            self.with_pending(cx, |p| {
                p.set(
                    row,
                    0,
                    deadlock_core::edit::Edit::Value(Value::Text(end.into())),
                    None,
                );
            });
        }
    }

    fn open_dialog(&mut self, expiry: bool, window: &mut Window, cx: &mut Context<Self>) {
        let label = self
            .table
            .as_ref()
            .map(|t| t.table.table.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| {
            let s = InputState::new(window, cx);
            if expiry {
                s.placeholder("90s, 15m, 2h, 1d, 2026-12-31 18:00 or never")
            } else {
                s.default_value(label)
            }
        });
        input.update(cx, |s, cx| s.focus(window, cx));
        let sub = cx.subscribe_in(
            &input,
            window,
            move |this, _, e: &InputEvent, window, cx| match e {
                InputEvent::PressEnter { .. } => this.confirm_dialog(window, cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            },
        );
        let dialog = if expiry {
            Dialog::Expiry { input, _sub: sub }
        } else {
            Dialog::Rename {
                input,
                overwrite: false,
                error: None,
                _sub: sub,
            }
        };
        if let Some(k) = self.key_state_mut() {
            k.dialog = Some(dialog);
        }
        cx.notify();
    }

    fn close_dialog(&mut self, cx: &mut Context<Self>) {
        if let Some(k) = self.key_state_mut() {
            k.dialog = None;
        }
        cx.notify();
    }

    /// The dialog's command, or why there is none yet.
    fn dialog_command(&self, cx: &App) -> Result<Command, String> {
        let k = self.key_state().ok_or("")?;
        match &k.dialog {
            Some(Dialog::Expiry { input, .. }) => {
                let e =
                    rk::parse_expiry(&input.read(cx).value(), chrono::Local::now().naive_local())?;
                Ok(rk::expiry_command(&k.key, e))
            }
            Some(Dialog::Rename {
                input, overwrite, ..
            }) => {
                let to = rk::label_key(&input.read(cx).value());
                if to.is_empty() {
                    return Err("Enter the new name.".into());
                }
                if to == k.key {
                    return Err("That is the key's name now.".into());
                }
                Ok(Command::new(if *overwrite { "RENAME" } else { "RENAMENX" })
                    .arg(&k.key)
                    .arg(to))
            }
            None => Err(String::new()),
        }
    }

    /// Set expiry or Rename: runs the dialog's command, then reloads.
    fn confirm_dialog(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Ok(command) = self.dialog_command(cx) else {
            return;
        };
        let Session::Ready(driver) = &self.session else {
            return;
        };
        let driver = Arc::clone(driver);
        let rename = command.name.starts_with("RENAME");
        let overwrite = command.name == "RENAME";
        let new_label = command.args().nth(1).map(rk::key_label);
        let new_key = command.args().nth(1).map(<[u8]>::to_vec);
        let key = command
            .args()
            .next()
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        let rt = Arc::clone(&cx.global::<Db>().0);
        let to = new_key.clone().unwrap_or_default();
        cx.spawn(async move |this, cx| {
            // `Ok(false)`: RENAMENX found the name taken.
            let result = rt
                .spawn(async move {
                    let r = driver
                        .as_redis()
                        .ok_or("not a Redis connection".to_owned())?;
                    if rename {
                        r.rename(&key, &to, overwrite)
                            .await
                            .map_err(|e| e.to_string())
                    } else {
                        r.run(&[command])
                            .await
                            .map(|_| true)
                            .map_err(|e| e.to_string())
                    }
                })
                .await
                .unwrap_or_else(|_| Err("the task stopped".into()));
            let _ = this.update(cx, |this, cx| match result {
                Err(e) => this.dialog_error(e, cx),
                Ok(false) => this.dialog_error(
                    "A key with that name exists. Tick Overwrite to replace it.".into(),
                    cx,
                ),
                Ok(true) => {
                    this.close_dialog(cx);
                    if rename && let (Some(label), Some(to)) = (new_label, new_key) {
                        this.renamed(label, to, cx);
                    }
                    this.reload_key(cx);
                }
            });
        })
        .detach();
    }

    fn dialog_error(&mut self, e: String, cx: &mut Context<Self>) {
        if let Some(k) = self.key_state_mut()
            && let Some(Dialog::Rename { error, .. }) = &mut k.dialog
        {
            *error = Some(e);
        } else {
            self.close_dialog(cx);
            self.table_note(e, true, cx);
        }
        cx.notify();
    }

    /// The key now has another name: the tab follows it and the sidebar reloads.
    fn renamed(&mut self, label: String, to: Vec<u8>, cx: &mut Context<Self>) {
        let Some(t) = &mut self.table else { return };
        let old = t.redis.as_ref().map(|k| k.key.clone()).unwrap_or_default();
        let kind = t.redis.as_ref().and_then(|k| k.kind().cloned());
        t.table.table = label;
        self.kind = super::Kind::Table(t.table.clone());
        if let Some(k) = &mut t.redis {
            k.key = to.clone();
        }
        let db = t.table.database.clone();
        cx.emit(TabEvent::KeysChanged(db, vec![old], kind.map(|k| (to, k))));
        cx.emit(TabEvent::Changed);
    }

    /// Delete key: asks with the command, then `UNLINK`s it.
    fn delete_key(&mut self, cx: &mut Context<Self>) {
        let (Session::Ready(driver), Some(t)) = (&self.session, &self.table) else {
            return;
        };
        let Some(k) = &t.redis else { return };
        let driver = Arc::clone(driver);
        let key = k.key.clone();
        let db = t.table.database.clone();
        let command = Command::new("UNLINK").arg(&key);
        let production = self
            .connection
            .as_ref()
            .filter(|c| c.environment == Environment::Production)
            .map(|c| format!(" on production connection {}", c.name))
            .unwrap_or_default();
        let ask = Ask::new(
            format!("Delete key {}{production}?", t.table.table),
            &["Delete key", "Cancel"],
        )
        .text("This cannot be undone.")
        .statement(command.text())
        .production(!production.is_empty())
        .danger(true);
        let rt = Arc::clone(&cx.global::<Db>().0);
        let window = self.window;
        cx.spawn(async move |this, cx| {
            if !confirm(cx, window, ask).await {
                return;
            }
            let result = rt
                .spawn(async move {
                    let r = driver
                        .as_redis()
                        .ok_or("not a Redis connection".to_owned())?;
                    r.run(&[command]).await.map_err(|e| e.to_string())
                })
                .await
                .unwrap_or_else(|_| Err("the task stopped".into()));
            let _ = this.update(cx, |this, cx| match result {
                Ok(_) => {
                    this.discard_now(cx);
                    if let Some(t) = &mut this.table {
                        t.grid = None;
                        if let Some(k) = &mut t.redis {
                            k.gone = true;
                            k.facts = None;
                            k.editor = None;
                            k.text = None;
                        }
                    }
                    this.table_note("The key was deleted.".into(), false, cx);
                    cx.emit(TabEvent::KeysChanged(db, vec![key], None));
                    cx.notify();
                }
                Err(e) => this.table_note(e, true, cx),
            });
        })
        .detach();
    }
}

fn noun(n: usize) -> &'static str {
    if n == 1 { "change" } else { "changes" }
}

/// The dialog `ask`; `true` for its first button.
async fn confirm(cx: &mut gpui_kit::AsyncApp, window: gpui_kit::AnyWindowHandle, ask: Ask) -> bool {
    let answer = cx.update_window(window, |_, window, cx| crate::dialog::ask(window, cx, ask));
    match answer {
        Ok(a) => a.await == Some(0),
        Err(_) => false,
    }
}

/// A key tab's column widths: names narrow, values wide (the design's 1 : 2.4); the
/// list index and scores narrow.
pub(super) fn widths(info: &deadlock_core::edit::TableInfo) -> Vec<f32> {
    info.columns
        .iter()
        .map(|c| match c.name.as_str() {
            "Index" | "Score" => 120.,
            "Field" | "ID" => 220.,
            "Member" if info.columns.len() == 1 => 640.,
            "Member" => 420.,
            "Value" | "Fields" => 520.,
            _ => 190.,
        })
        .collect()
}

/// `.fact`: a small label over its value.
fn fact(label: &'static str, value: impl IntoElement, p: &Palette) -> gpui_kit::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(div().text_size(small_size()).text_color(p.tx3).child(label))
        .child(value)
}

/// The facts header and, for collections, the `match` row.
pub(super) fn header(
    this: &QueryTab,
    read_only: bool,
    cx: &mut Context<QueryTab>,
) -> gpui_kit::Div {
    let p = *cx.global::<Palette>();
    let Some(t) = &this.table else { return div() };
    let Some(k) = &t.redis else { return div() };
    let facts = k.facts.as_ref();
    let can_act = !read_only && facts.is_some() && !k.gone && !t.busy;
    let kind = facts.map(|f| f.kind.clone());
    let mut row = div()
        .flex()
        .flex_wrap()
        .flex_none()
        .items_center()
        .gap_x(px(28.))
        .gap_y(px(12.))
        .px(px(16.))
        .py(px(14.))
        .border_b_1()
        .border_color(p.line)
        .child(
            fact(
                "Key",
                div()
                    .font_family(MONO_FONT)
                    .text_size(px(14.))
                    .font_weight(FontWeight::MEDIUM)
                    .child(t.table.table.clone()),
                &p,
            )
            .flex_grow(1.)
            .flex_shrink(1.)
            .flex_basis(px(200.))
            .min_w_0(),
        );
    if let Some(f) = facts {
        let encoding: SharedString = f.encoding.clone().map_or_else(
            || "Encoding not reported".into(),
            |e| format!("Encoding: {e}").into(),
        );
        row = row
            .child(fact(
                "Type",
                div()
                    .id("key-type")
                    .child(f.kind.label().to_owned())
                    .tooltip(move |window, cx| Tooltip::new(encoding.clone()).build(window, cx)),
                &p,
            ))
            .children(
                f.len
                    .map(|n| fact(f.kind.noun(), div().child(rk::grouped(n)), &p)),
            )
            .children(
                f.dump_len
                    .map(|n| fact("DUMP length", div().child(rk::bytes_size(n)), &p)),
            )
            .children(
                f.memory
                    .map(|m| fact("Memory", div().child(rk::bytes_size(m)), &p)),
            )
            .child(fact(
                "Expires",
                match k.ttl_now() {
                    Some(ms) if ms > 0 => div()
                        .text_color(p.warn)
                        .font_weight(FontWeight::MEDIUM)
                        .child(rk::expires_in(ms)),
                    Some(_) => div()
                        .text_color(p.danger)
                        .font_weight(FontWeight::MEDIUM)
                        .child("expired"),
                    None => div().child("never"),
                },
                &p,
            ));
    }
    let actions = div()
        .flex()
        .flex_wrap()
        .gap(px(4.))
        .child(
            button(
                "set-expiry",
                ButtonKind::Outline,
                can_act,
                "Set or remove the key's expiry",
                cx,
            )
            .child("Set expiry")
            .when(can_act, |b| {
                b.on_click(cx.listener(|this, _, window, cx| this.open_dialog(true, window, cx)))
            }),
        )
        .child(
            button(
                "rename-key",
                ButtonKind::Outline,
                can_act,
                "Rename the key",
                cx,
            )
            .child("Rename")
            .when(can_act, |b| {
                b.on_click(cx.listener(|this, _, window, cx| this.open_dialog(false, window, cx)))
            }),
        )
        .child(
            button(
                "delete-key",
                ButtonKind::Outline,
                can_act,
                "Delete the key (UNLINK)",
                cx,
            )
            .text_color(p.danger)
            .child("Delete key")
            .when(can_act, |b| {
                b.on_click(cx.listener(|this, _, _, cx| this.delete_key(cx)))
            }),
        )
        .child(
            icon_button(
                "reload-key",
                Icon::Refresh,
                None,
                ButtonKind::Ghost,
                true,
                format!("Reload key ({})", crate::keymap::label("refresh", cx)),
                cx,
            )
            .w(px(28.))
            .px_0()
            .justify_center()
            .on_click(cx.listener(|this, _, window, cx| {
                this.after_discard(window, cx, |this, _, cx| this.reload_key(cx));
            })),
        );
    let head = div()
        .flex()
        .flex_col()
        .flex_none()
        .child(row.child(actions));
    let Some(kind) = kind.filter(|k| !k.columns().is_empty()) else {
        return head;
    };
    let can_edit = !read_only && t.grid.is_some();
    let matching = matches!(kind, KeyType::Hash | KeyType::Set | KeyType::ZSet);
    let (add, delete) = match kind {
        KeyType::Hash => ("Add field", "Delete field"),
        KeyType::List => ("Push to tail", "Delete element"),
        KeyType::Set | KeyType::ZSet => ("Add member", "Delete member"),
        _ => ("Add entry", "Delete entry"),
    };
    let field = div()
        .flex()
        .items_center()
        .gap(px(8.))
        .h(px(28.))
        .px(px(8.))
        .border_1()
        .border_color(if t.filter_error.is_some() {
            p.danger
        } else {
            p.line2
        })
        .rounded(RADIUS)
        .font_family(MONO_FONT)
        .text_size(data_size())
        .when(!matching, |d| d.opacity(0.5))
        .child(div().flex_none().text_color(p.tx3).child("match"))
        .child(
            div().flex_1().min_w_0().child(
                Input::new(&t.filter)
                    .appearance(false)
                    .h(px(26.))
                    .px_0()
                    .disabled(!matching),
            ),
        );
    let tools = div()
        .flex()
        .flex_wrap()
        .flex_none()
        .items_center()
        .gap_x(px(12.))
        .gap_y(px(8.))
        .px(px(16.))
        .py(px(10.))
        .border_b_1()
        .border_color(p.line)
        .child(
            div()
                .flex_grow(1.)
                .flex_shrink(1.)
                .flex_basis(px(260.))
                .min_w(px(160.))
                .child(field),
        )
        .when(kind == KeyType::List, |d| {
            d.child(
                icon_button(
                    "push-head",
                    Icon::Plus,
                    Some("Push to head"),
                    ButtonKind::Outline,
                    can_edit,
                    "Add an element at the head (LPUSH)",
                    cx,
                )
                .when(can_edit, |b| {
                    b.on_click(cx.listener(|this, _, window, cx| this.push(true, window, cx)))
                }),
            )
        })
        .child(
            icon_button(
                "add-row",
                Icon::Plus,
                Some(add),
                ButtonKind::Outline,
                can_edit,
                "Add (Alt+Insert)",
                cx,
            )
            .when(can_edit, |b| {
                b.on_click(cx.listener(move |this, _, window, cx| {
                    if kind_is_list(this) {
                        this.push(false, window, cx);
                    } else {
                        this.add_row(window, cx);
                    }
                }))
            }),
        )
        .child(
            icon_button(
                "delete-row",
                Icon::Minus,
                Some(delete),
                ButtonKind::Outline,
                can_edit,
                "Delete the selected rows (Delete)",
                cx,
            )
            .when(can_edit, |b| {
                b.on_click(cx.listener(|this, _, _, cx| this.delete_rows(cx)))
            }),
        );
    let note = (kind == KeyType::Stream).then(|| {
        div()
            .flex_none()
            .px(px(16.))
            .py(px(6.))
            .border_b_1()
            .border_color(p.line)
            .bg(p.panel)
            .text_size(small_size())
            .text_color(p.tx2)
            .child("Streams are append-only: entries cannot be edited, only added or deleted. Newest first.")
    });
    head.child(tools).children(note)
}

fn kind_is_list(this: &QueryTab) -> bool {
    this.key_state().and_then(KeyState::kind) == Some(&KeyType::List)
}

/// The body for keys without rows: the text editor, or what Deadlock cannot show.
pub(super) fn body(this: &QueryTab, cx: &mut Context<QueryTab>) -> Option<AnyElement> {
    let p = *cx.global::<Palette>();
    let t = this.table.as_ref()?;
    let k = t.redis.as_ref()?;
    let message = |text: String| {
        div()
            .size_full()
            .p(px(16.))
            .text_color(p.tx3)
            .child(text)
            .into_any_element()
    };
    if k.gone {
        return Some(message("The key does not exist.".into()));
    }
    let f = k.facts.as_ref()?;
    if !f.kind.columns().is_empty() {
        return None;
    }
    if let KeyType::Other(name) = &f.kind {
        return Some(message(format!(
            "Deadlock cannot show {name} values: it is a module's type. It opens read-only; use \
             the module's own commands in the console.{}",
            f.dump_len
                .map(|n| format!(" Its DUMP is {} long.", rk::bytes_size(n)))
                .unwrap_or_default()
        )));
    }
    let editor = k.editor.as_ref()?;
    let text = k.text.as_ref()?;
    let note = if matches!(text.value, Value::Bytes(_)) {
        Some("Binary value, shown as hex: read-only here.".to_owned())
    } else if text.truncated {
        Some(format!(
            "Only the first {} are shown, read-only: the value is {} long.",
            rk::bytes_size(deadlock_core::redis_driver::TEXT_LIMIT),
            rk::bytes_size(f.len.unwrap_or(0))
        ))
    } else {
        None
    };
    Some(
        div()
            .size_full()
            .flex()
            .flex_col()
            .children(note.map(|n| {
                div()
                    .flex_none()
                    .px(px(16.))
                    .py(px(6.))
                    .bg(p.panel)
                    .border_b_1()
                    .border_color(p.line)
                    .text_size(small_size())
                    .text_color(p.tx2)
                    .child(n)
            }))
            .children(k.json_error.clone().map(|e| {
                div()
                    .flex_none()
                    .px(px(16.))
                    .py(px(6.))
                    .border_b_1()
                    .border_color(p.line)
                    .text_size(small_size())
                    .text_color(p.danger)
                    .child(e)
            }))
            .child(
                div().flex_1().min_h_0().child(
                    Editor::new(editor)
                        .bordered(false)
                        .h_full()
                        .font_family(MONO_FONT)
                        .text_size(data_size())
                        .line_height(px(20.))
                        .py(px(10.)),
                ),
            )
            .into_any_element(),
    )
}

/// The pending panel's sentence and commands.
pub(super) fn panel_text(this: &QueryTab, cx: &App) -> (String, Result<Vec<Command>, String>) {
    let commands = this.key_commands(cx);
    let Some(k) = this.key_state() else {
        return (String::new(), commands);
    };
    let kind = k.kind().cloned().unwrap_or(KeyType::String);
    let what = if this.edited_text(cx).is_some() {
        "The value was edited.".to_owned()
    } else {
        this.table
            .as_ref()
            .and_then(|t| t.grid.as_ref())
            .and_then(|g| {
                let e = g.read(cx).delegate().edit.as_ref()?;
                let p = &e.pending;
                Some(summary(
                    &kind,
                    p.updated().count(),
                    p.deleted().count(),
                    p.added_len(),
                ))
            })
            .unwrap_or_default()
    };
    (
        format!(
            "{what} Redis has no rollback: they run together in MULTI and take effect at once."
        ),
        commands,
    )
}

/// The panel's commands, highlighted, or the error that stops them.
pub(super) fn panel_commands(commands: &Result<Vec<Command>, String>, p: &Palette) -> AnyElement {
    match commands {
        Ok(c) => highlighted(c, p).into_any_element(),
        Err(e) => div()
            .text_color(p.danger)
            .child(e.clone())
            .into_any_element(),
    }
}

/// Set expiry or Rename, over the tab.
pub(super) fn dialog(this: &QueryTab, cx: &mut Context<QueryTab>) -> Option<AnyElement> {
    let p = *cx.global::<Palette>();
    let k = this.key_state()?;
    let dialog = k.dialog.as_ref()?;
    let command = this.dialog_command(cx);
    let production = this
        .connection
        .as_ref()
        .filter(|c| c.environment == Environment::Production)
        .map(|c| format!("Runs on production connection {}.", c.name));
    let (title, sub, field, extra, go) = match dialog {
        Dialog::Expiry { input, .. } => (
            "Set expiry".to_owned(),
            "A duration (90s, 15m, 2h, 1d), a local date and time, or never to keep the key.",
            labelled(
                "Expires",
                Styled::h(Input::new(input), px(32.)).font_family(MONO_FONT),
            ),
            None,
            "Set expiry",
        ),
        Dialog::Rename {
            input,
            overwrite,
            error,
            ..
        } => (
            format!(
                "Rename {}",
                this.table
                    .as_ref()
                    .map(|t| t.table.table.clone())
                    .unwrap_or_default()
            ),
            "RENAMENX refuses a name that is taken; Overwrite uses RENAME, which replaces that key.",
            labelled(
                "New name",
                Styled::h(Input::new(input), px(32.)).font_family(MONO_FONT),
            ),
            Some((
                Checkbox::new("rename-overwrite")
                    .checked(*overwrite)
                    .label("Overwrite a key with that name")
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        if let Some(k) = this.key_state_mut()
                            && let Some(Dialog::Rename {
                                overwrite, error, ..
                            }) = &mut k.dialog
                        {
                            *overwrite = *checked;
                            *error = None;
                        }
                        cx.notify();
                    })),
                error.clone(),
            )),
            "Rename",
        ),
    };
    let ok = command.is_ok();
    let preview = match &command {
        Ok(c) => div()
            .px(px(12.))
            .py(px(8.))
            .border_1()
            .border_color(p.line)
            .rounded(RADIUS)
            .bg(p.panel)
            .font_family(MONO_FONT)
            .text_size(data_size())
            .child(highlighted(std::slice::from_ref(c), &p)),
        Err(e) => div()
            .text_size(small_size())
            .text_color(p.danger)
            .child(e.clone()),
    };
    let mut body: Vec<AnyElement> = vec![field.into_any_element()];
    if let Some((check, error)) = extra {
        body.push(check.into_any_element());
        if let Some(e) = error {
            body.push(
                div()
                    .text_size(small_size())
                    .text_color(p.danger)
                    .child(e)
                    .into_any_element(),
            );
        }
    }
    body.push(preview.into_any_element());
    if let Some(note) = production {
        body.push(
            div()
                .text_size(small_size())
                .text_color(p.warn)
                .child(note)
                .into_any_element(),
        );
    }
    body.push(
        div()
            .flex()
            .justify_end()
            .gap(px(8.))
            .child(
                button(
                    "key-dialog-cancel",
                    ButtonKind::Outline,
                    true,
                    "Cancel (Esc)",
                    cx,
                )
                .child("Cancel")
                .on_click(cx.listener(|this, _, _, cx| this.close_dialog(cx))),
            )
            .child(
                button(
                    "key-dialog-go",
                    ButtonKind::Primary,
                    ok,
                    "Run the command shown (Enter)",
                    cx,
                )
                .child(go)
                .when(ok, |b| {
                    b.on_click(cx.listener(|this, _, window, cx| this.confirm_dialog(window, cx)))
                }),
            )
            .into_any_element(),
    );
    Some(
        modal("key-dialog", 440., title, Some(sub.into()), body, &p)
            .on_action(cx.listener(|this, _: &input::Escape, _, cx| this.close_dialog(cx)))
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_use_the_types_noun() {
        assert_eq!(
            summary(&KeyType::Hash, 1, 1, 0),
            "One field updated and one deleted."
        );
        assert_eq!(
            summary(&KeyType::List, 0, 2, 1),
            "Two elements deleted and one added."
        );
        assert_eq!(
            summary(&KeyType::Set, 1, 1, 1),
            "One member updated, one deleted and one added."
        );
        assert_eq!(summary(&KeyType::Stream, 0, 0, 0), "");
        assert!(looks_json(r#"{"a":1}"#));
        assert!(!looks_json("{not json"));
        assert_eq!(
            editor_text(&KeyType::String, &Value::Bytes(vec![0, 255])),
            "00 ff"
        );
        assert_eq!(
            editor_text(&KeyType::Json, &Value::Text(r#"{"a":1}"#.into())),
            "{\n  \"a\": 1\n}"
        );
    }
}
