//! What exports share between screens: the format menu, the save location (the native
//! dialog, or a typed path when no dialog portal is available) and the progress line
//! of an export that runs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use deadlock_core::export::Format;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Context, Entity, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Render, Styled, Subscription, Task, Window, div, px,
};
use tokio::sync::oneshot;

use crate::theme::{Palette, RADIUS, row_height, small_size};
use crate::ui::Clickable as _;
use crate::ui::{Kind, button};

/// A view that can show the typed-path field.
pub trait PathHost: 'static + Sized {
    fn path_field(&mut self) -> &mut Option<Entity<PathField>>;
}

/// Where to save `name`: the native save dialog, or, when it cannot open, a field in
/// `host` for a typed path. `None` when the user cancelled.
pub fn save_path<T: PathHost>(
    dir: PathBuf,
    name: String,
    window: AnyWindowHandle,
    cx: &mut Context<T>,
) -> Task<Option<PathBuf>> {
    let picked = cx.prompt_for_new_path(&dir, Some(&name));
    cx.spawn(async move |this, cx| {
        if let Ok(Ok(path)) = picked.await {
            return path;
        }
        // No portal (or it failed): fall back to the field.
        let (tx, rx) = oneshot::channel();
        let opened = cx.update_window(window, |_, window, cx| {
            this.update(cx, |host, cx| {
                let field = cx.new(|cx| PathField::new(&dir, &name, tx, window, cx));
                let focus = field.read(cx).input.clone();
                focus.update(cx, |f, cx| f.focus(window, cx));
                *host.path_field() = Some(field);
                cx.notify();
            })
        });
        if !matches!(opened, Ok(Ok(()))) {
            return None;
        }
        let path = rx.await.ok().flatten();
        let _ = this.update(cx, |host, cx| {
            *host.path_field() = None;
            cx.notify();
        });
        path
    })
}

/// The typed-path fallback: a line with the path, Save and Cancel.
pub struct PathField {
    input: Entity<InputState>,
    dir: PathBuf,
    answer: Option<oneshot::Sender<Option<PathBuf>>>,
    error: Option<String>,
    /// The path exists and the user was told: the next Save replaces it.
    replace: Option<PathBuf>,
    _sub: Subscription,
}

impl PathField {
    fn new(
        dir: &Path,
        name: &str,
        answer: oneshot::Sender<Option<PathBuf>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let start = dir.join(name).display().to_string();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(start));
        let sub = cx.subscribe(&input, |this, _, e: &InputEvent, cx| match e {
            InputEvent::PressEnter { .. } => this.save(cx),
            InputEvent::Change => {
                this.error = None;
                this.replace = None;
                cx.notify();
            }
            _ => {}
        });
        Self {
            input,
            dir: dir.to_owned(),
            answer: Some(answer),
            error: None,
            replace: None,
            _sub: sub,
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        match resolve(&self.dir, &text) {
            Err(e) => self.error = Some(e),
            Ok(path) if path.exists() && self.replace.as_ref() != Some(&path) => {
                self.error = Some("The file exists: Save again to replace it.".into());
                self.replace = Some(path);
            }
            Ok(path) => {
                if let Some(tx) = self.answer.take() {
                    let _ = tx.send(Some(path));
                }
            }
        }
        cx.notify();
    }

    fn cancel(&mut self) {
        if let Some(tx) = self.answer.take() {
            let _ = tx.send(None);
        }
    }
}

/// A typed path: `~` is the home folder, a relative path is under `dir`; the folder
/// must exist and the path must not be one.
fn resolve(dir: &Path, text: &str) -> Result<PathBuf, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("Type a file path.".into());
    }
    let path = match text.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().ok_or("No home folder.")?.join(rest),
        None => dir.join(text),
    };
    if path.is_dir() {
        return Err("That is a folder: add a file name.".into());
    }
    match path.parent() {
        Some(p) if p.is_dir() => Ok(path),
        _ => Err("The folder does not exist.".into()),
    }
}

impl Render for PathField {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *cx.global::<Palette>();
        div()
            .flex()
            .flex_wrap()
            .flex_none()
            .items_center()
            .gap_x(px(8.))
            .gap_y(px(6.))
            .px(px(16.))
            .py(px(8.))
            .bg(p.panel)
            .border_b_1()
            .border_color(p.line)
            .child(div().text_color(p.tx2).child("Save as"))
            .child(
                div()
                    .flex_grow(1.)
                    .flex_basis(px(260.))
                    .min_w(px(160.))
                    .child(Input::new(&self.input).h(row_height())),
            )
            .child(
                button(
                    "path-save",
                    Kind::Primary,
                    true,
                    "Save to this path (Enter)",
                    cx,
                )
                .child("Save")
                .clickable(cx.listener(|this, _, _, cx| this.save(cx))),
            )
            .child(
                button("path-cancel", Kind::Outline, true, "Do not save", cx)
                    .child("Cancel")
                    .clickable(cx.listener(|this, _, _, cx| {
                        this.cancel();
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .w_full()
                    .text_size(small_size())
                    .text_color(if self.error.is_some() {
                        p.danger
                    } else {
                        p.tx3
                    })
                    .child(self.error.clone().unwrap_or_else(|| {
                        "No save dialog is available on this desktop: type where to save.".into()
                    })),
            )
    }
}

/// What the format menu offers for one grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Offer {
    /// Rows in the grid now.
    pub loaded: usize,
    /// Every row is loaded: there is no choice to make.
    pub complete: bool,
    /// All rows can be fetched again (a read, or a table).
    pub rerun: bool,
    /// SQL INSERT statements make sense (not Redis).
    pub sql: bool,
}

/// The menu: which rows, then a format. `all` is the current rows choice; `on_all`
/// changes it, `on_pick` starts the export.
pub fn menu(
    offer: Offer,
    all: bool,
    on_all: impl Fn(bool, &mut App) + Clone + 'static,
    on_pick: impl Fn(Format, bool, &mut App) + Clone + 'static,
    p: &Palette,
) -> gpui_kit::AnyElement {
    let all = all && offer.rerun && !offer.complete;
    let row = |id: &'static str, on: bool| {
        div()
            .id(id)
            .flex()
            .items_center()
            .gap(px(8.))
            .h(row_height())
            .px(px(8.))
            .rounded(RADIUS)
            .whitespace_nowrap()
            .text_color(p.tx)
            .when(on, |d| d.bg(p.sel))
            .hover(|s| s.bg(p.hover))
    };
    let note = |t: String| div().px(px(8.)).py(px(4.)).text_color(p.tx3).child(t);
    let loaded = grouped(offer.loaded);
    let scope: Vec<gpui_kit::AnyElement> = if offer.complete {
        vec![note(format!("All {loaded} rows")).into_any_element()]
    } else if !offer.rerun {
        vec![
            note(format!(
                "The {loaded} loaded rows: running this statement again could change data"
            ))
            .into_any_element(),
        ]
    } else {
        let choice = |id, value: bool, label: &'static str, what: String| {
            let on_all = on_all.clone();
            row(id, all == value)
                .child(div().font_weight(FontWeight::MEDIUM).child(label))
                .child(div().ml_auto().pl(px(8.)).text_color(p.tx3).child(what))
                .clickable(move |_, _, cx| on_all(value, cx))
                .into_any_element()
        };
        vec![
            choice(
                "export-all",
                true,
                "All rows",
                "fetched again from the server".into(),
            ),
            choice(
                "export-loaded",
                false,
                "Loaded rows only",
                format!("{loaded} loaded now"),
            ),
        ]
    };
    let formats = Format::ALL
        .into_iter()
        .filter(|f| offer.sql || *f != Format::SqlInsert)
        .map(|f| {
            let on_pick = on_pick.clone();
            let what = match f {
                Format::Csv => "comma-separated, RFC 4180",
                Format::Tsv => "tab-separated",
                Format::Json => "an array of objects",
                Format::JsonLines => "one object per line",
                Format::SqlInsert => "INSERT statements",
            };
            row(f.label(), false)
                .child(div().font_weight(FontWeight::MEDIUM).child(f.label()))
                .child(div().ml_auto().pl(px(8.)).text_color(p.tx3).child(what))
                .clickable(move |_, _, cx| on_pick(f, all, cx))
        });
    let heading = |t: &'static str| {
        div()
            .px(px(8.))
            .pt(px(4.))
            .text_size(small_size())
            .text_color(p.tx3)
            .child(t)
    };
    div()
        .flex()
        .flex_col()
        .gap(px(1.))
        .min_w(px(300.))
        .child(heading("Rows"))
        .children(scope)
        .child(div().h(px(1.)).my(px(4.)).bg(p.line))
        .child(heading("Format"))
        .children(formats)
        .into_any_element()
}

/// `1234567` → `1,234,567`.
pub fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// An export that runs or has ended, for the progress line.
pub struct Job {
    pub file: String,
    pub rows: Arc<AtomicU64>,
    started: Instant,
    cancel: Option<oneshot::Sender<()>>,
    /// The outcome once it ended, and whether it failed.
    pub done: Option<(String, bool)>,
    _tick: Option<Task<()>>,
}

impl Job {
    /// A running job; the line redraws while it runs.
    pub fn start<T: 'static>(
        file: String,
        rows: Arc<AtomicU64>,
        cancel: Option<oneshot::Sender<()>>,
        cx: &mut Context<T>,
    ) -> Self {
        let tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        Self {
            file,
            rows,
            started: Instant::now(),
            cancel,
            done: None,
            _tick: Some(tick),
        }
    }

    pub fn is_running(&self) -> bool {
        self.done.is_none()
    }

    /// Asks the export to stop; it ends with "cancelled".
    pub fn cancel(&mut self) {
        if let Some(tx) = self.cancel.take() {
            let _ = tx.send(());
        }
    }

    /// Records the outcome of the export.
    pub fn finish(&mut self, outcome: Result<u64, String>) {
        self._tick = None;
        let took = crate::results::duration(self.started.elapsed());
        self.done = Some(match outcome {
            Ok(n) => (
                format!(
                    "Exported {} rows to {} in {took}",
                    grouped(usize::try_from(n).unwrap_or(usize::MAX)),
                    self.file
                ),
                false,
            ),
            Err(e) if e == "cancelled" => ("Export cancelled: nothing was saved".into(), false),
            Err(e) => (format!("Export failed: {e}"), true),
        });
    }

    /// The progress line. `on_cancel` stops a running export; `on_close` hides an
    /// ended one.
    pub fn render(
        &self,
        on_cancel: impl Fn(&mut Window, &mut App) + 'static,
        on_close: impl Fn(&mut Window, &mut App) + 'static,
        cx: &App,
    ) -> impl IntoElement {
        let p = *cx.global::<Palette>();
        let (text, bad) = self.done.clone().unwrap_or_else(|| {
            let n = usize::try_from(self.rows.load(Ordering::Relaxed)).unwrap_or(usize::MAX);
            (
                format!(
                    "Exporting to {}: {} rows written, {}",
                    self.file,
                    grouped(n),
                    crate::results::duration(self.started.elapsed())
                ),
                false,
            )
        });
        let running = self.is_running();
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(16.))
            .py(px(6.))
            .bg(p.panel)
            .border_b_1()
            .border_color(p.line)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_color(if bad { p.danger } else { p.tx2 })
                    .child(text),
            )
            .child(if running {
                button("export-cancel", Kind::Outline, true, "Stop the export", cx)
                    .child("Cancel")
                    .clickable(move |_, window, cx| on_cancel(window, cx))
            } else {
                button("export-close", Kind::Ghost, true, "Hide", cx)
                    .child("Close")
                    .clickable(move |_, window, cx| on_close(window, cx))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_paths_resolve_under_the_project() {
        let dir = std::env::temp_dir();
        assert_eq!(resolve(&dir, " out.csv "), Ok(dir.join("out.csv")));
        assert!(resolve(&dir, "").is_err());
        assert!(resolve(&dir, ".").is_err());
        assert!(resolve(&dir, "no-such-folder-p14/out.csv").is_err());
        let abs = dir.join("x.csv");
        assert_eq!(
            resolve(Path::new("/elsewhere"), abs.to_str().unwrap()),
            Ok(abs)
        );
    }

    #[test]
    fn counts_are_grouped() {
        assert_eq!(grouped(1_000_000), "1,000,000");
        assert_eq!(grouped(12), "12");
    }
}
