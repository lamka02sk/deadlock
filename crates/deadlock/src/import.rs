//! The Import tab from the design's `Import` artboard: a CSV file into a table, with
//! separator and encoding detection, the column mapping, a check of every row before
//! anything is written, and the import itself in one transaction on a session of its
//! own, with progress and Cancel.

use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use deadlock_core::connection::{ConnectionConfig, Environment};
use deadlock_core::driver::{Driver, connect};
use deadlock_core::edit::{TableInfo, TableRef};
use deadlock_core::import::{self, Checked, ENCODINGS, Field, Job, Mapping, Options, SEPARATORS};
use encoding_rs::Encoding;
use gpui_kit::component::IndexPath;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Context, Entity, EventEmitter, FontWeight,
    InteractiveElement, IntoElement, ParentElement, PathPromptOptions, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, Window, div, px, relative, white,
};
use tokio::sync::oneshot;

use crate::app::Db;
use crate::export::grouped;
use crate::theme::{MONO_FONT, Palette, RADIUS, data_size, small_size};

type Choice = Entity<SelectState<Vec<SharedString>>>;

/// What the tab tells the window.
pub enum ImportEvent {
    /// Rows went into this table: its data tab reloads.
    Imported(TableRef),
}

/// The import running now.
struct Running {
    progress: Arc<AtomicU64>,
    total: u64,
    cancel: Option<oneshot::Sender<()>>,
}

/// A check of the file running now.
struct Checking {
    progress: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
}

pub struct ImportTab {
    pub connection: ConnectionConfig,
    password: String,
    /// The window's session to the connection: table definitions come from it.
    driver: Arc<dyn Driver>,
    window: AnyWindowHandle,
    /// The table picker, when the tab was opened without a table.
    picker: Option<(Choice, Vec<TableRef>)>,
    pub table: Option<TableRef>,
    info: Option<Result<TableInfo, String>>,
    file: Entity<InputState>,
    path: Option<PathBuf>,
    /// Why the file cannot be read, or the note that no file dialog opened.
    file_note: Option<(String, bool)>,
    separator: Choice,
    encoding: Choice,
    empty: Choice,
    header: bool,
    skip_bad: bool,
    /// The file's column names (or `Column n`) and its first row's values.
    names: Vec<String>,
    first: Vec<Field>,
    targets: Vec<Choice>,
    checking: Option<Checking>,
    checked: Option<Result<Checked, String>>,
    running: Option<Running>,
    /// The last import's outcome.
    done: Option<Result<u64, String>>,
    _ticker: Option<Task<()>>,
    _subs: Vec<Subscription>,
    _target_subs: Vec<Subscription>,
}

impl EventEmitter<ImportEvent> for ImportTab {}

fn choice(
    labels: Vec<SharedString>,
    at: usize,
    window: &mut Window,
    cx: &mut Context<ImportTab>,
) -> Choice {
    cx.new(|cx| SelectState::new(labels, Some(IndexPath::default().row(at)), window, cx))
}

fn encoding_label(e: &'static Encoding) -> SharedString {
    // `windows-1250` → `Windows-1250`.
    let name = e.name();
    match name.strip_prefix("windows") {
        Some(rest) => format!("Windows{rest}").into(),
        None => name.into(),
    }
}

impl ImportTab {
    pub fn new(
        connection: ConnectionConfig,
        password: String,
        driver: Arc<dyn Driver>,
        table: Option<TableRef>,
        tables: Vec<TableRef>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let file =
            cx.new(|cx| InputState::new(window, cx).placeholder("Path of a .csv file, then Enter"));
        let separator = choice(
            SEPARATORS
                .iter()
                .map(|(_, l)| SharedString::from(*l))
                .collect(),
            0,
            window,
            cx,
        );
        let encoding = choice(
            ENCODINGS.iter().map(|e| encoding_label(e)).collect(),
            0,
            window,
            cx,
        );
        let empty = choice(vec!["NULL".into(), "Empty text".into()], 0, window, cx);
        let mut subs = vec![
            cx.subscribe_in(&file, window, |this, _, e: &InputEvent, window, cx| {
                if matches!(e, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    let text = this.file.read(cx).value().to_string();
                    this.open_path(&text, window, cx);
                }
            }),
            cx.subscribe_in(
                &separator,
                window,
                |this, _, _: &SelectEvent<Vec<SharedString>>, window, cx| {
                    this.read_header(window, cx);
                },
            ),
            cx.subscribe_in(
                &encoding,
                window,
                |this, _, _: &SelectEvent<Vec<SharedString>>, window, cx| {
                    this.read_header(window, cx);
                },
            ),
            cx.subscribe_in(
                &empty,
                window,
                |this, _, _: &SelectEvent<Vec<SharedString>>, _, cx| {
                    this.check(cx);
                },
            ),
        ];
        let picker = table.is_none().then(|| {
            let labels: Vec<SharedString> = tables
                .iter()
                .map(|t| format!("{}.{}", t.database, t.table).into())
                .collect();
            let state = cx.new(|cx| SelectState::new(labels, None, window, cx).searchable(true));
            subs.push(cx.subscribe_in(
                &state,
                window,
                |this, _, e: &SelectEvent<Vec<SharedString>>, window, cx| {
                    let SelectEvent::Confirm(Some(_)) = e else {
                        return;
                    };
                    let picked = this.picker.as_ref().and_then(|(s, refs)| {
                        s.read(cx)
                            .selected_index(cx)
                            .and_then(|i| refs.get(i.row).cloned())
                    });
                    if picked.is_some() {
                        this.table = picked;
                        this.load_info(window, cx);
                    }
                },
            ));
            (state, tables)
        });
        let mut this = Self {
            connection,
            password,
            driver,
            window: window.window_handle(),
            picker,
            table,
            info: None,
            file,
            path: None,
            file_note: None,
            separator,
            encoding,
            empty,
            header: true,
            skip_bad: false,
            names: Vec::new(),
            first: Vec::new(),
            targets: Vec::new(),
            checking: None,
            checked: None,
            running: None,
            done: None,
            _ticker: None,
            _subs: subs,
            _target_subs: Vec::new(),
        };
        this.load_info(window, cx);
        this
    }

    pub fn title(&self) -> String {
        match &self.table {
            Some(t) => format!("Import into {}", t.table),
            None => "Import CSV".into(),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    fn load_info(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let Some(table) = self.table.clone() else {
            return;
        };
        self.info = None;
        let d = Arc::clone(&self.driver);
        let rx = cx
            .global::<Db>()
            .0
            .spawn(async move { d.table_info(&table).await.map_err(|e| e.to_string()) });
        let window = self.window;
        cx.spawn(async move |this, cx| {
            let info = rx
                .await
                .unwrap_or_else(|_| Err("the table task stopped".into()));
            let _ = cx.update_window(window, |_, window, cx| {
                this.update(cx, |this, cx| {
                    this.info = Some(info);
                    this.read_header(window, cx);
                    cx.notify();
                })
            });
        })
        .detach();
        cx.notify();
    }

    /// "Choose file…": the native dialog; without one, the typed path.
    fn choose(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
        let window = self.window;
        cx.spawn(async move |this, cx| {
            let path = match picked.await {
                Ok(Ok(Some(mut paths))) => paths.pop(),
                Ok(Ok(None)) => return,
                // No portal: the field takes a typed path.
                Ok(Err(_)) | Err(_) => None,
            };
            let _ = cx.update_window(window, |_, window, cx| {
                this.update(cx, |this, cx| match path {
                    Some(p) => {
                        let text = p.display().to_string();
                        this.file
                            .update(cx, |f, cx| f.set_value(text.clone(), window, cx));
                        this.open_path(&text, window, cx);
                    }
                    None => {
                        this.file_note = Some((
                            "No file dialog is available on this desktop: type the file's path \
                             and press Enter."
                                .into(),
                            false,
                        ));
                        this.file.update(cx, |f, cx| f.focus(window, cx));
                        cx.notify();
                    }
                })
            });
        })
        .detach();
    }

    /// A typed or picked path: `~` is the home folder, a relative path is under the
    /// home folder too. Detects the separator and encoding, then reads the header.
    fn open_path(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let path = match text.strip_prefix("~/") {
            Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
            None => PathBuf::from(text),
        };
        if self.path.as_ref() == Some(&path) && self.file_note.is_none() {
            return;
        }
        let mut head = Vec::with_capacity(64 * 1024);
        let read =
            std::fs::File::open(&path).and_then(|f| f.take(64 * 1024).read_to_end(&mut head));
        if let Err(e) = read {
            self.path = None;
            self.file_note = Some((format!("{}: {e}", path.display()), true));
            self.names.clear();
            self.checked = None;
            cx.notify();
            return;
        }
        self.file_note = None;
        self.path = Some(path);
        let (sep, enc) = import::detect(&head);
        let sep_at = SEPARATORS.iter().position(|(s, _)| *s == sep).unwrap_or(0);
        let enc_at = ENCODINGS.iter().position(|e| *e == enc).unwrap_or(0);
        self.separator.update(cx, |s, cx| {
            s.set_selected_index(Some(IndexPath::default().row(sep_at)), window, cx);
        });
        self.encoding.update(cx, |s, cx| {
            s.set_selected_index(Some(IndexPath::default().row(enc_at)), window, cx);
        });
        self.read_header(window, cx);
    }

    fn options(&self, cx: &gpui_kit::App) -> Options {
        let at = |c: &Choice| c.read(cx).selected_index(cx).map_or(0, |i| i.row);
        Options {
            separator: SEPARATORS[at(&self.separator).min(SEPARATORS.len() - 1)].0,
            encoding: ENCODINGS[at(&self.encoding).min(ENCODINGS.len() - 1)],
            header: self.header,
            empty_is_null: at(&self.empty) == 0,
        }
    }

    fn table_info(&self) -> Option<&TableInfo> {
        self.info.as_ref().and_then(|i| i.as_ref().ok())
    }

    /// Reads the file's first records, maps its columns again and checks the file.
    fn read_header(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(path), Some(info)) = (self.path.clone(), self.table_info().cloned()) else {
            return;
        };
        let options = self.options(cx);
        let mut first = Vec::new();
        let mut second = Vec::new();
        let read = options.open(&path).and_then(|mut r| {
            let e = |e: std::io::Error| e.to_string();
            r.record(&mut first).map_err(e)?;
            r.record(&mut second).map_err(e)?;
            Ok(())
        });
        if let Err(e) = read {
            self.file_note = Some((e, true));
            cx.notify();
            return;
        }
        let (names, values) = if self.header {
            (first.iter().map(|f| f.text.clone()).collect(), second)
        } else {
            (
                (1..=first.len()).map(|i| format!("Column {i}")).collect(),
                first,
            )
        };
        let map = Mapping::auto(names, &info, self.header);
        let labels: Vec<SharedString> = std::iter::once("Skip".into())
            .chain(
                (0..info.columns.len())
                    .filter(|&i| import::writable(&info, i))
                    .map(|i| SharedString::from(info.columns[i].name.clone())),
            )
            .collect();
        let writable: Vec<usize> = (0..info.columns.len())
            .filter(|&i| import::writable(&info, i))
            .collect();
        self._target_subs.clear();
        self.targets = map
            .targets
            .iter()
            .map(|t| {
                let at = t
                    .and_then(|t| writable.iter().position(|&w| w == t))
                    .map_or(0, |p| p + 1);
                let state = choice(labels.clone(), at, window, cx);
                self._target_subs.push(
                    cx.subscribe(&state, |this, _, _: &SelectEvent<Vec<SharedString>>, cx| {
                        this.check(cx)
                    }),
                );
                state
            })
            .collect();
        self.names = map.names;
        self.first = values;
        self.check(cx);
    }

    /// The mapping as the selects show it.
    fn mapping(&self, cx: &gpui_kit::App) -> Option<Mapping> {
        let info = self.table_info()?;
        let writable: Vec<usize> = (0..info.columns.len())
            .filter(|&i| import::writable(info, i))
            .collect();
        let targets = self
            .targets
            .iter()
            .map(|s| {
                s.read(cx)
                    .selected_index(cx)
                    .and_then(|i| i.row.checked_sub(1))
                    .and_then(|i| writable.get(i).copied())
            })
            .collect();
        Some(Mapping {
            names: self.names.clone(),
            targets,
        })
    }

    /// Checks every row of the file in the background; a check still running stops.
    fn check(&mut self, cx: &mut Context<Self>) {
        if let Some(c) = self.checking.take() {
            c.stop.store(true, Ordering::Relaxed);
        }
        self.checked = None;
        self.done = None;
        let (Some(path), Some(info), Some(map)) = (
            self.path.clone(),
            self.table_info().cloned(),
            self.mapping(cx),
        ) else {
            cx.notify();
            return;
        };
        let options = self.options(cx);
        let (progress, stop) = (
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(false)),
        );
        let (p, s) = (Arc::clone(&progress), Arc::clone(&stop));
        let rx = cx
            .global::<Db>()
            .0
            .spawn_blocking(move || import::check(&path, &options, &info, &map, &p, &s));
        self.checking = Some(Checking {
            progress,
            stop: Arc::clone(&stop),
        });
        self.tick(cx);
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            let _ = this.update(cx, |this, cx| {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                this.checking = None;
                this.checked = result.transpose();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Redraws the progress line while a check or an import runs.
    fn tick(&mut self, cx: &mut Context<Self>) {
        self._ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let busy = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        this.checking.is_some() || this.running.is_some()
                    })
                    .unwrap_or(false);
                if !busy {
                    break;
                }
            }
        }));
    }

    /// Rows the import would write, and why it cannot start yet.
    fn ready(&self, cx: &gpui_kit::App) -> Result<u64, &'static str> {
        if self.connection.read_only {
            return Err("The connection is read-only.");
        }
        let Some(info) = self.table_info() else {
            return Err("Choose a table.");
        };
        if info.view {
            return Err("A view cannot be imported into.");
        }
        if self.running.is_some() {
            return Err("An import runs.");
        }
        if self.checking.is_some() {
            return Err("Wait for the check to finish.");
        }
        if self.mapping(cx).is_none_or(|m| m.columns().is_empty()) {
            return Err("Map at least one column.");
        }
        let Some(Ok(c)) = &self.checked else {
            return Err("Choose a file.");
        };
        if c.bad > 0 && !self.skip_bad {
            return Err("Some rows cannot be imported.");
        }
        match c.rows - if self.skip_bad { c.bad } else { 0 } {
            0 => Err("No row to import."),
            n => Ok(n),
        }
    }

    /// Import: on Production after a confirmation with the row count.
    fn import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(rows) = self.ready(cx) else { return };
        let (Some(path), Some(table), Some(info), Some(map)) = (
            self.path.clone(),
            self.table.clone(),
            self.table_info().cloned(),
            self.mapping(cx),
        ) else {
            return;
        };
        let job = Job {
            path,
            options: self.options(cx),
            table: table.clone(),
            info,
            map,
            skip_bad: self.skip_bad,
        };
        let production = self.connection.environment == Environment::Production;
        let answer = production.then(|| {
            crate::dialog::ask(
                window,
                cx,
                crate::dialog::Ask::new(
                    format!(
                        "Import {} rows into {} on production connection {}?",
                        grouped(usize::try_from(rows).unwrap_or(usize::MAX)),
                        table.table,
                        self.connection.name
                    ),
                    &["Import", "Cancel"],
                )
                .text(
                    "The rows are inserted in one transaction, committed when the last one is in.",
                )
                .production(true),
            )
        });
        let (config, password) = (self.connection.clone(), self.password.clone());
        let rt = Arc::clone(&cx.global::<Db>().0);
        cx.spawn(async move |this, cx| {
            if let Some(a) = answer
                && a.await != Some(0)
            {
                return;
            }
            let progress = Arc::new(AtomicU64::new(0));
            let (tx, rx) = oneshot::channel();
            let started = this.update(cx, |this, cx| {
                this.done = None;
                this.running = Some(Running {
                    progress: Arc::clone(&progress),
                    total: rows,
                    cancel: Some(tx),
                });
                this.tick(cx);
                cx.notify();
            });
            if started.is_err() {
                return;
            }
            let outcome = rt
                .spawn(async move {
                    let fresh = connect(&config, &password)
                        .await
                        .map_err(|e| e.to_string())?;
                    import::run(fresh, job, progress, rx).await
                })
                .await
                .unwrap_or_else(|_| Err("the import task stopped".into()));
            let _ = this.update(cx, |this, cx| {
                this.running = None;
                if outcome.is_ok() {
                    cx.emit(ImportEvent::Imported(table));
                }
                this.done = Some(outcome);
                cx.notify();
            });
        })
        .detach();
    }

    fn cancel_import(&mut self) {
        if let Some(r) = &mut self.running {
            drop(r.cancel.take());
        }
    }
}

/// A label above its field (`.fl`).
fn field(label: &'static str, input: impl IntoElement) -> gpui_kit::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .min_w_0()
        .font_weight(FontWeight::MEDIUM)
        .child(label)
        .child(div().font_weight(FontWeight::NORMAL).child(input))
}

/// The design's 32 px `.b` buttons of this screen.
fn big_button(
    id: &'static str,
    label: SharedString,
    primary: bool,
    enabled: bool,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .h(px(32.))
        .px(px(12.))
        .rounded(RADIUS)
        .whitespace_nowrap()
        .when(primary, |d| {
            d.bg(p.blue)
                .text_color(white())
                .font_weight(FontWeight::MEDIUM)
        })
        .when(!primary, |d| d.text_color(p.tx2))
        .when(enabled, |d| {
            let (bg, fg) = if primary {
                (p.blue_h, white())
            } else {
                (p.hover, p.tx)
            };
            d.hover(move |s| s.bg(bg).text_color(fg))
        })
        .when(!enabled, |d| d.opacity(0.38))
        .child(label)
}

impl Render for ImportTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *cx.global::<Palette>();
        let mono = |d: gpui_kit::Div| d.font_family(MONO_FONT).text_size(data_size());
        let info = self.table_info().cloned();
        let header = div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .text_size(px(20.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .line_height(relative(1.2))
                    .child(self.title()),
            )
            .child(div().text_color(p.tx2).child(
                "Nothing is written until you choose Import. Rows are inserted in one transaction.",
            ));
        let file_field = div()
            .flex()
            .gap(px(8.))
            .child(
                div().flex_1().min_w_0().child(
                    gpui_kit::Styled::h(Input::new(&self.file), px(32.))
                        .font_family(MONO_FONT)
                        .text_size(data_size()),
                ),
            )
            .child(
                div()
                    .id("import-choose")
                    .flex()
                    .flex_none()
                    .items_center()
                    .h(px(32.))
                    .px(px(12.))
                    .border_1()
                    .border_color(p.line2)
                    .rounded(RADIUS)
                    .hover(|s| s.bg(p.hover))
                    .child("Choose…")
                    .on_click(cx.listener(|this, _, _, cx| this.choose(cx))),
            );
        let fields = div()
            .flex()
            .flex_wrap()
            .gap_x(px(12.))
            .gap_y(px(16.))
            .when_some(self.picker.as_ref(), |d, (state, _)| {
                d.child(
                    field(
                        "Table",
                        Select::new(state).h(px(32.)).placeholder("Choose a table"),
                    )
                    .flex_grow(2.)
                    .flex_basis(px(240.)),
                )
            })
            .child(field("File", file_field).flex_grow(3.).flex_basis(px(280.)))
            .child(
                field("Separator", Select::new(&self.separator).h(px(32.)))
                    .flex_grow(1.)
                    .flex_basis(px(120.)),
            )
            .child(
                field("Encoding", Select::new(&self.encoding).h(px(32.)))
                    .flex_grow(1.)
                    .flex_basis(px(120.)),
            )
            .child(
                field("Empty field means", Select::new(&self.empty).h(px(32.)))
                    .flex_grow(1.)
                    .flex_basis(px(150.)),
            );
        let header_box = Checkbox::new("import-header")
            .checked(self.header)
            .label("The first row holds column names")
            .on_click(cx.listener(|this, on: &bool, window, cx| {
                this.header = *on;
                this.read_header(window, cx);
            }));

        // The mapping grid: In the file, Table column, Type, First value.
        let cell = |d: gpui_kit::Div| {
            d.px(px(12.))
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
        };
        let row = |d: gpui_kit::Div, (a, b, c, e): (gpui_kit::Div, gpui_kit::Div, gpui_kit::Div, gpui_kit::Div)| {
            d.flex()
                .items_center()
                .border_b_1()
                .border_color(p.line)
                .child(cell(a).flex_grow(1.).flex_basis(px(120.)))
                .child(cell(b).flex_grow(1.2).flex_basis(px(150.)))
                .child(cell(c).flex_none().w(px(110.)))
                .child(cell(e).flex_grow(1.2).flex_basis(px(140.)))
        };
        let mapping = self.mapping(cx);
        let grid_rows = self.names.iter().enumerate().map(|(i, name)| {
            let target = mapping
                .as_ref()
                .and_then(|m| m.targets.get(i).copied().flatten())
                .and_then(|t| info.as_ref().and_then(|info| info.columns.get(t)));
            let value = self
                .first
                .get(i)
                .map(|f| f.text.clone())
                .unwrap_or_default();
            row(
                div().min_h(px(36.)),
                (
                    mono(div()).child(SharedString::from(name.clone())),
                    div().child(
                        Select::new(&self.targets[i])
                            .h(px(26.))
                            .font_family(MONO_FONT)
                            .text_size(data_size()),
                    ),
                    mono(div())
                        .text_color(p.tx3)
                        .child(target.map_or(String::new(), |c| c.column_type.clone())),
                    mono(div())
                        .when(target.is_none(), |d| d.text_color(p.tx3))
                        .child(SharedString::from(value)),
                ),
            )
        });
        let filled = info.as_ref().map(|info| {
            let used = mapping.as_ref().map(Mapping::columns).unwrap_or_default();
            let rest: Vec<String> = info
                .columns
                .iter()
                .enumerate()
                .filter(|(i, c)| !used.contains(i) && !c.pseudo)
                .map(|(_, c)| {
                    let how = if c.auto_increment {
                        "auto increment"
                    } else if c.generated {
                        "generated"
                    } else if c.nullable {
                        "NULL or its default"
                    } else {
                        "default"
                    };
                    format!("{} ({how})", c.name)
                })
                .collect();
            rest
        });
        let columns = (!self.names.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Columns"))
                .child(
                    div().id("import-grid").overflow_x_scroll().child(
                        div()
                            .min_w(px(620.))
                            .border_t_1()
                            .border_color(p.line)
                            .child(row(
                                div()
                                    .min_h(px(32.))
                                    .bg(p.panel)
                                    .text_color(p.tx2)
                                    .text_size(small_size())
                                    .font_weight(FontWeight::MEDIUM),
                                (
                                    div().child("In the file"),
                                    div().child("Table column"),
                                    div().child("Type"),
                                    div().child("First value"),
                                ),
                            ))
                            .children(grid_rows),
                    ),
                )
                .when_some(filled.filter(|f| !f.is_empty()), |d, f| {
                    d.child(
                        div()
                            .text_size(small_size())
                            .text_color(p.tx3)
                            .child(format!(
                                "Not in the file, filled by the table: {}.",
                                f.join(", ")
                            )),
                    )
                })
        });

        // The status box: the file, the table, the check, the import.
        let status: Option<(String, Vec<String>, bool)> = if let Some(Err(e)) = &self.info {
            Some(("The table cannot be read".into(), vec![e.clone()], true))
        } else if let Some((note, error)) = &self.file_note {
            Some((
                if *error {
                    "The file cannot be read"
                } else {
                    "Choose a file"
                }
                .into(),
                vec![note.clone()],
                *error,
            ))
        } else if let Some(r) = self.running.as_ref().filter(|r| r.cancel.is_none()) {
            let n = r.progress.load(Ordering::Relaxed);
            Some((
                "Cancelling…".into(),
                vec![format!(
                    "Rolling back the {} rows imported so far.",
                    grouped(usize::try_from(n).unwrap_or(0))
                )],
                false,
            ))
        } else if let Some(r) = &self.running {
            let n = r.progress.load(Ordering::Relaxed);
            Some((
                format!(
                    "Importing… {} of {} rows",
                    grouped(usize::try_from(n).unwrap_or(0)),
                    grouped(usize::try_from(r.total).unwrap_or(0))
                ),
                vec!["Cancel rolls back every row imported so far.".into()],
                false,
            ))
        } else if let Some(done) = &self.done {
            Some(match done {
                Ok(n) => (
                    format!(
                        "Imported {} rows into {}",
                        grouped(usize::try_from(*n).unwrap_or(0)),
                        self.table.as_ref().map_or("", |t| t.table.as_str())
                    ),
                    vec!["The transaction is committed.".into()],
                    false,
                ),
                Err(e) if e == "cancelled" => (
                    "The import was cancelled".into(),
                    vec!["Every row was rolled back: the table is as it was.".into()],
                    true,
                ),
                Err(e) => (
                    "Nothing was imported".into(),
                    vec![
                        format!("{}{}", capital(e), if e.ends_with('.') { "" } else { "." }),
                        "Every row was rolled back: the table is as it was.".into(),
                    ],
                    true,
                ),
            })
        } else if let Some(c) = &self.checking {
            let n = c.progress.load(Ordering::Relaxed);
            Some((
                format!(
                    "Checking… {} rows",
                    grouped(usize::try_from(n).unwrap_or(0))
                ),
                Vec::new(),
                false,
            ))
        } else {
            match &self.checked {
                Some(Ok(c)) if c.bad == 0 => Some((
                    format!(
                        "{} rows checked, all can be imported",
                        grouped(usize::try_from(c.rows).unwrap_or(0))
                    ),
                    Vec::new(),
                    false,
                )),
                Some(Ok(c)) => Some((
                    format!(
                        "{} rows checked, {} cannot be imported",
                        grouped(usize::try_from(c.rows).unwrap_or(0)),
                        grouped(usize::try_from(c.bad).unwrap_or(0))
                    ),
                    vec![
                        c.first
                            .iter()
                            .map(|p| format!("Row {}: {}.", p.row, p.text))
                            .collect::<Vec<_>>()
                            .join(" "),
                    ],
                    true,
                )),
                Some(Err(e)) => Some(("The file cannot be read".into(), vec![e.clone()], true)),
                None => None,
            }
        };
        let status = status.map(|(title, lines, warn)| {
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .py(px(12.))
                .px(px(16.))
                .border_1()
                .rounded(RADIUS)
                .when(warn, |d| d.border_color(p.warn_line).bg(p.warn_bg))
                .when(!warn, |d| d.border_color(p.line).bg(p.panel))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                .children(
                    lines
                        .into_iter()
                        .map(|l| div().text_color(p.tx2).child(SharedString::from(l))),
                )
        });

        let ready = self.ready(cx);
        let running = self.running.is_some();
        let import_label: SharedString = match ready {
            Ok(n) => format!("Import {} rows", grouped(usize::try_from(n).unwrap_or(0))).into(),
            Err(_) => "Import".into(),
        };
        let reason = ready.err().unwrap_or_default();
        let footer = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_x(px(12.))
            .gap_y(px(8.))
            .pt(px(20.))
            .border_t_1()
            .border_color(p.line)
            .child(
                Checkbox::new("import-skip")
                    .checked(self.skip_bad)
                    .label("Skip rows that cannot be imported")
                    .on_click(cx.listener(|this, on: &bool, _, cx| {
                        this.skip_bad = *on;
                        cx.notify();
                    })),
            )
            .child(div().flex_grow(1.))
            .when(running, |d| {
                d.child(
                    big_button("import-cancel", "Cancel".into(), false, true, &p).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.cancel_import();
                            cx.notify();
                        }),
                    ),
                )
            })
            .child(
                big_button("import-go", import_label, true, ready.is_ok(), &p)
                    .when(ready.is_ok(), |d| {
                        d.on_click(cx.listener(|this, _, window, cx| this.import(window, cx)))
                    })
                    .when(!reason.is_empty(), |d| {
                        d.tooltip(move |window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(reason).build(window, cx)
                        })
                    }),
            );

        div()
            .id("import-tab")
            .size_full()
            .overflow_y_scroll()
            .bg(p.bg)
            .text_color(p.tx)
            .child(
                div()
                    .pt(px(24.))
                    .px(px(32.))
                    .pb(px(28.))
                    .flex()
                    .flex_col()
                    .gap(px(24.))
                    .line_height(relative(1.4))
                    .child(header)
                    .child(fields)
                    .child(header_box)
                    .children(columns)
                    .children(status)
                    .child(footer),
            )
    }
}

/// An error text as a sentence: capital first letter.
fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}
