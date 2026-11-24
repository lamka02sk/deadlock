//! The Files panel at the window's right: the project's `.sql` files, kept in sync with
//! the disk by a file watcher while the panel is open. New file, New folder, Rename,
//! Delete (to the system trash) and Reveal; its open state and width are kept per
//! project.

use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::component::input::{self, Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, ClickEvent, Context, Entity, FocusHandle, FontWeight, Hsla,
    InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, Subscription, Task, UniformListScrollHandle, Window, div, px, uniform_list,
};
use notify::Watcher as _;

use super::{Front, Shell, type_label};
use crate::app::Db;
use crate::assets::{Icon, icon};
use crate::files::{self, Entry, FileRow};
use crate::query::Kind;
use crate::theme::{Palette, RADIUS, row_height, small_size};
use crate::ui::{Clickable as _, Kind as ButtonKind, button, square_button};

/// The panel's width until the user drags it.
pub(super) const WIDTH: f32 = 264.;
/// Watcher events are gathered this long before the folder is scanned again.
const DEBOUNCE: Duration = Duration::from_millis(250);
/// Without a watcher (it could not start), the folder is scanned this often.
const POLL: Duration = Duration::from_secs(5);

pub(super) struct Files {
    tree: Vec<Entry>,
    rows: Vec<FileRow>,
    found: usize,
    collapsed: HashSet<PathBuf>,
    /// Folders made here that hold no `.sql` file yet, listed anyway.
    made: HashSet<PathBuf>,
    filter: Option<(Entity<InputState>, Subscription)>,
    rename: Option<Rename>,
    /// The row F2 and Delete act on: the last one clicked.
    selected: Option<PathBuf>,
    /// Start renaming this path once a scan lists it (a new file or folder).
    rename_next: Option<PathBuf>,
    scanned: bool,
    /// The last failed file operation, shown at the bottom of the panel.
    error: Option<String>,
    pub(super) focus: FocusHandle,
    list: UniformListScrollHandle,
    /// The watcher's task, while the panel is open; dropping it stops the watcher.
    watch: Option<Task<()>>,
    scan: Option<Task<()>>,
}

struct Rename {
    path: PathBuf,
    folder: bool,
    input: Entity<InputState>,
    /// Focus the field and select the name on the next frame.
    fresh: bool,
    _sub: Subscription,
}

impl Files {
    pub(super) fn new(cx: &mut Context<Shell>) -> Self {
        Self {
            tree: Vec::new(),
            rows: Vec::new(),
            found: 0,
            collapsed: HashSet::new(),
            made: HashSet::new(),
            filter: None,
            rename: None,
            selected: None,
            rename_next: None,
            scanned: false,
            error: None,
            focus: cx.focus_handle(),
            list: UniformListScrollHandle::new(),
            watch: None,
            scan: None,
        }
    }
}

/// Moves `p` from under `from` to under `to`; `None` when it is not under `from`.
fn moved(p: &Path, from: &Path, to: &Path) -> Option<PathBuf> {
    p.strip_prefix(from).ok().map(|rest| {
        if rest.as_os_str().is_empty() {
            to.to_owned()
        } else {
            to.join(rest)
        }
    })
}

impl Shell {
    /// The toolbar button and Ctrl+Alt+B.
    pub(super) fn toggle_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open = !self.project.state.files_open;
        self.project.state.files_open = open;
        if open {
            self.watch_files(cx);
            window.focus(&self.files.focus, cx);
        } else {
            self.files.watch = None;
            self.files.rename = None;
            window.focus(&self.focus, cx);
        }
        self.schedule_save(cx);
        cx.notify();
    }

    /// Ctrl+Alt+F: the panel's own name filter, opening the panel first.
    pub(super) fn filter_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.project.state.files_open {
            self.toggle_files(window, cx);
        }
        if let Some((input, _)) = &self.files.filter {
            input.update(cx, |s, cx| s.focus(window, cx));
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter files by name"));
        let sub = cx.subscribe(&input, |this, _, e: &InputEvent, cx| {
            if matches!(e, InputEvent::Change) {
                this.rebuild_files(cx);
            }
        });
        input.update(cx, |s, cx| s.focus(window, cx));
        self.files.filter = Some((input, sub));
        cx.notify();
    }

    /// Esc and the clear button: every file shows again.
    fn close_files_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.files.filter = None;
        self.rebuild_files(cx);
        window.focus(&self.files.focus, cx);
    }

    /// Scans now, then again whenever the watcher reports a change. A watcher that
    /// cannot start (no inotify watches left, say) falls back to scanning every
    /// [`POLL`].
    pub(super) fn watch_files(&mut self, cx: &mut Context<Self>) {
        self.rescan_files(cx);
        let root = self.project.root().to_owned();
        let rt = std::sync::Arc::clone(&cx.global::<Db>().0);
        self.files.watch = Some(cx.spawn(async move |this, cx| {
            let (tx, rx) = async_channel::unbounded::<()>();
            let start = rt.spawn_blocking(move || {
                let events = root.clone();
                let mut w =
                    notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                        use notify::EventKind as E;
                        use notify::event::ModifyKind as M;
                        let Ok(e) = res else { return };
                        // Opening or writing a file does not change the list.
                        if matches!(
                            e.kind,
                            E::Access(_) | E::Modify(M::Data(_) | M::Metadata(_))
                        ) {
                            return;
                        }
                        if e.paths.iter().any(|p| files::relevant(&events, p)) {
                            let _ = tx.try_send(());
                        }
                    })?;
                w.watch(&root, notify::RecursiveMode::Recursive)?;
                Ok::<_, notify::Error>(w)
            });
            // Held until the task is dropped: the watcher stops with it.
            let watcher = match start.await {
                Ok(Ok(w)) => Some(w),
                Ok(Err(e)) => {
                    eprintln!("deadlock: file watcher: {e}; scanning every {POLL:?}");
                    None
                }
                Err(_) => None,
            };
            loop {
                if watcher.is_some() {
                    if rx.recv().await.is_err() {
                        break;
                    }
                    cx.background_executor().timer(DEBOUNCE).await;
                    while rx.try_recv().is_ok() {}
                } else {
                    cx.background_executor().timer(POLL).await;
                }
                if this.update(cx, |this, cx| this.rescan_files(cx)).is_err() {
                    break;
                }
            }
        }));
    }

    /// Lists the folder again off the UI thread.
    pub(super) fn rescan_files(&mut self, cx: &mut Context<Self>) {
        let root = self.project.root().to_owned();
        let made = self.files.made.clone();
        let rx = cx
            .global::<Db>()
            .0
            .spawn_blocking(move || files::scan(&root, &made));
        let handle = self.window;
        self.files.scan = Some(cx.spawn(async move |this, cx| {
            let Ok(tree) = rx.await else { return };
            let _ = cx.update_window(handle, |_, window, cx| {
                this.update(cx, |this, cx| this.scanned_files(tree, window, cx))
            });
        }));
    }

    fn scanned_files(&mut self, tree: Vec<Entry>, window: &mut Window, cx: &mut Context<Self>) {
        let f = &mut self.files;
        f.tree = tree;
        f.scanned = true;
        f.made.retain(|p| p.is_dir());
        self.rebuild_files(cx);
        if let Some(path) = self.files.rename_next.take() {
            self.begin_rename(&path, window, cx);
        }
        // A renamed row that went away (deleted outside) ends its rename.
        if let Some(r) = &self.files.rename
            && !self.files.rows.iter().any(|row| row.path == r.path)
        {
            self.files.rename = None;
        }
    }

    fn rebuild_files(&mut self, cx: &mut Context<Self>) {
        let filter = self
            .files
            .filter
            .as_ref()
            .map(|(i, _)| i.read(cx).value().to_string())
            .unwrap_or_default();
        let f = &mut self.files;
        (f.rows, f.found) = files::rows(&f.tree, &f.collapsed, &filter);
        cx.notify();
    }

    fn file_tab(&self, path: &Path, cx: &gpui_kit::App) -> Option<usize> {
        self.tabs
            .iter()
            .position(|(t, _)| matches!(&t.read(cx).kind, Kind::File(Some(p)) if p == path))
    }

    /// The front tab's file, when it has one.
    fn front_file(&self, cx: &gpui_kit::App) -> Option<PathBuf> {
        match &self.front_tab()?.read(cx).kind {
            Kind::File(Some(p)) => Some(p.clone()),
            _ => None,
        }
    }

    fn on_click_file(
        &mut self,
        ix: usize,
        e: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.files.rows.get(ix).cloned() else {
            return;
        };
        self.files.selected = Some(row.path.clone());
        window.focus(&self.files.focus, cx);
        if row.folder {
            if !self.files.collapsed.remove(&row.path) {
                self.files.collapsed.insert(row.path);
            }
            self.rebuild_files(cx);
            return;
        }
        self.open_path(row.path, window, cx);
        // One click keeps the keyboard here, as in a file explorer; two go to the editor.
        if e.click_count() < 2 {
            window.focus(&self.files.focus, cx);
        }
    }

    /// The row F2 and Delete act on: the one clicked last, else the front tab's file.
    fn file_target(&self, cx: &gpui_kit::App) -> Option<(PathBuf, bool)> {
        let listed = |p: &Path| self.files.rows.iter().find(|r| r.path == p);
        self.files
            .selected
            .as_deref()
            .and_then(listed)
            .or_else(|| self.front_file(cx).as_deref().and_then(listed))
            .map(|r| (r.path.clone(), r.folder))
    }

    pub(super) fn rename_target(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((path, _)) = self.file_target(cx) {
            self.begin_rename(&path, window, cx);
        }
    }

    pub(super) fn delete_target(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((path, folder)) = self.file_target(cx) {
            self.delete_file(path, folder, window, cx);
        }
    }

    fn begin_rename(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.files.rows.iter().find(|r| r.path == path) else {
            return;
        };
        let (name, folder) = (row.name.clone(), row.folder);
        let input = cx.new(|cx| {
            let mut s = InputState::new(window, cx);
            s.set_value(name, window, cx);
            s
        });
        let sub = cx.subscribe_in(&input, window, |this, _, e: &InputEvent, window, cx| {
            if matches!(e, InputEvent::PressEnter { .. }) {
                this.commit_rename(window, cx);
            }
        });
        self.files.error = None;
        self.files.rename = Some(Rename {
            path: path.to_owned(),
            folder,
            input,
            fresh: true,
            _sub: sub,
        });
        cx.notify();
    }

    /// Focuses a rename field that just opened, with the name selected up to `.sql`.
    pub(super) fn focus_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(r) = self.files.rename.as_mut().filter(|r| r.fresh) else {
            return;
        };
        r.fresh = false;
        let folder = r.folder;
        r.input.update(cx, |s, cx| {
            s.focus(window, cx);
            let name = s.value().to_string();
            let end = if folder {
                name.len()
            } else {
                files::stem_len(&name)
            };
            s.set_selected_range(0..end, cx);
        });
    }

    fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.files.rename = None;
        window.focus(&self.files.focus, cx);
        cx.notify();
    }

    fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(r) = &self.files.rename else { return };
        let typed = r.input.read(cx).value().to_string();
        let (from, folder) = (r.path.clone(), r.folder);
        let name = match files::checked_name(&typed, !folder) {
            Ok(n) => n,
            Err(e) => {
                self.files.error = Some(e.to_owned());
                cx.notify();
                return;
            }
        };
        let Some(dir) = from.parent() else { return };
        let to = dir.join(&name);
        if to == from {
            return self.cancel_rename(window, cx);
        }
        self.files.rename = None;
        window.focus(&self.files.focus, cx);
        let (src, dst) = (from.clone(), to.clone());
        let rx = cx.global::<Db>().0.spawn_blocking(move || {
            // Only a change of case may land on an existing name (the same file).
            let same = |a: &Path, b: &Path| {
                matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
            };
            if dst.exists() && !same(&src, &dst) {
                return Err(format!("{name} already exists."));
            }
            std::fs::rename(&src, &dst).map_err(|e| format!("Cannot rename: {e}"))
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("Rename failed.".into()));
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => this.renamed(&from, &to, cx),
                Err(e) => this.file_error(e, cx),
            });
        })
        .detach();
    }

    /// Tabs, the selection and the panel's own sets follow a rename.
    fn renamed(&mut self, from: &Path, to: &Path, cx: &mut Context<Self>) {
        for (tab, _) in &self.tabs {
            let next = match &tab.read(cx).kind {
                Kind::File(Some(p)) => moved(p, from, to),
                _ => None,
            };
            if let Some(next) = next {
                // Only the path changes: the text and its unsaved mark stay.
                tab.update(cx, |t, cx| {
                    t.kind = Kind::File(Some(next));
                    cx.notify();
                });
            }
        }
        let f = &mut self.files;
        let follow = |set: &mut HashSet<PathBuf>| {
            *set = set
                .drain()
                .map(|p| moved(&p, from, to).unwrap_or(p))
                .collect();
        };
        follow(&mut f.collapsed);
        follow(&mut f.made);
        f.selected = Some(to.to_owned());
        self.schedule_save(cx);
        self.rescan_files(cx);
    }

    fn file_error(&mut self, e: String, cx: &mut Context<Self>) {
        eprintln!("deadlock: files: {e}");
        self.files.error = Some(e);
        self.rescan_files(cx);
    }

    /// New file: an empty `query.sql` (numbered when taken) in `dir`, opened, then
    /// renamed in place.
    pub(super) fn new_file(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.make_entry(dir, true, window, cx);
    }

    fn make_entry(
        &mut self,
        dir: PathBuf,
        file: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.project.state.files_open {
            self.toggle_files(window, cx);
        }
        self.files.error = None;
        // The new row must not hide behind the filter or a closed folder.
        self.files.filter = None;
        self.files.collapsed.retain(|c| !dir.starts_with(c));
        let rx = cx.global::<Db>().0.spawn_blocking(move || {
            let (stem, ext) = if file {
                ("query", ".sql")
            } else {
                ("folder", "")
            };
            let name = files::unique_name(stem, ext, |n| dir.join(n).exists());
            let path = dir.join(name);
            let made = if file {
                std::fs::File::create_new(&path).map(drop)
            } else {
                std::fs::create_dir(&path)
            };
            made.map(|()| path)
                .map_err(|e| format!("Cannot create: {e}"))
        });
        let handle = self.window;
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("Create failed.".into()));
            let _ = cx.update_window(handle, |_, window, cx| {
                this.update(cx, |this, cx| match result {
                    Ok(path) => {
                        if file {
                            this.open_path(path.clone(), window, cx);
                            window.focus(&this.files.focus, cx);
                        } else {
                            this.files.made.insert(path.clone());
                        }
                        this.files.selected = Some(path.clone());
                        this.files.rename_next = Some(path);
                        this.rescan_files(cx);
                    }
                    Err(e) => this.file_error(e, cx),
                })
            });
        })
        .detach();
    }

    /// Asks, closes the tabs of what goes (each with its usual prompts), then moves it
    /// to the system trash.
    fn delete_file(
        &mut self,
        path: PathBuf,
        folder: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = deadlock_core::project::folder_name(&path);
        let place = path
            .parent()
            .map(|d| files::shown_path(self.project.root(), d))
            .filter(|d| !d.is_empty())
            .map_or("the project folder".to_owned(), |d| format!("{d}/"));
        let open = self
            .tabs
            .iter()
            .any(|(t, _)| matches!(&t.read(cx).kind, Kind::File(Some(p)) if p.starts_with(&path)));
        let what = if folder {
            "The folder and everything in it move"
        } else {
            "The file moves"
        };
        let tabs = match (open, folder) {
            (false, _) => "",
            (true, false) => " Its tab closes.",
            (true, true) => " Their tabs close.",
        };
        let answer = crate::dialog::ask(
            window,
            cx,
            crate::dialog::Ask::new(
                format!("Delete {name}?"),
                &[
                    if folder {
                        "Delete folder"
                    } else {
                        "Delete file"
                    },
                    "Cancel",
                ],
            )
            .text(format!("{what} from {place} to the system trash.{tabs}"))
            .danger(true),
        );
        let handle = self.window;
        cx.spawn(async move |this, cx| {
            if answer.await != Some(0) {
                return;
            }
            let _ = cx.update_window(handle, |_, window, cx| {
                this.update(cx, |this, cx| this.trash_when_closed(path, window, cx))
            });
        })
        .detach();
    }

    /// Closes the next tab under `path`, coming back once it is gone; with none left,
    /// trashes `path`. A Cancel in a tab's prompt stops it all.
    fn trash_when_closed(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let open = self.tabs.iter().position(
            |(t, _)| matches!(&t.read(cx).kind, Kind::File(Some(p)) if p.starts_with(&path)),
        );
        if let Some(i) = open {
            let then: super::tabs::AfterClose =
                Box::new(move |this, window, cx| this.trash_when_closed(path, window, cx));
            return self.close_tab_then(i, Some(then), window, cx);
        }
        let target = path.clone();
        let rx = cx.global::<Db>().0.spawn_blocking(move || {
            trash::delete(&target).map_err(|e| format!("Cannot move to the trash: {e}"))
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("Delete failed.".into()));
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.files.made.retain(|m| !m.starts_with(&path));
                    this.files.error = None;
                    this.rescan_files(cx);
                }
                Err(e) => this.file_error(e, cx),
            });
        })
        .detach();
    }

    /// The right-click menu of a row; `None` is the panel's empty space (the root).
    fn file_menu(
        &self,
        row: Option<&FileRow>,
        menu: PopupMenu,
        shell: Entity<Shell>,
        cx: &gpui_kit::App,
    ) -> PopupMenu {
        let p = *cx.global::<Palette>();
        let item = |ic: Icon, label: &'static str, keys: Option<String>, danger: bool| {
            let color = if danger { p.danger } else { p.tx };
            let tint = if danger { p.danger } else { p.tx3 };
            PopupMenuItem::element(move |_, _| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .w_full()
                    .text_color(color)
                    .child(icon(ic, tint))
                    .child(label)
                    .children(keys.clone().map(|k| {
                        div()
                            .ml_auto()
                            .pl(px(16.))
                            .text_size(px(12.))
                            .text_color(p.tx3)
                            .child(k)
                    }))
            })
        };
        let on = |item: PopupMenuItem, f: super::keys::MenuAction| {
            let shell = shell.clone();
            item.on_click(move |_, window, cx| shell.update(cx, |s, cx| f(s, window, cx)))
        };
        let dir = match row {
            Some(r) if r.folder => r.path.clone(),
            Some(r) => r.path.parent().map_or_else(PathBuf::new, Path::to_path_buf),
            None => self.project.root().to_owned(),
        };
        let d2 = dir.clone();
        let menu = menu
            .item(on(
                item(Icon::NewFile, "New file", None, false),
                Box::new(move |s, w, cx| s.new_file(dir.clone(), w, cx)),
            ))
            .item(on(
                item(Icon::NewFolder, "New folder", None, false),
                Box::new(move |s, w, cx| s.make_entry(d2.clone(), false, w, cx)),
            ));
        let Some(row) = row else { return menu };
        let k = |id: &str| Some(crate::keymap::label(id, cx));
        let (p1, p2, p3, folder) = (
            row.path.clone(),
            row.path.clone(),
            row.path.clone(),
            row.folder,
        );
        menu.separator()
            .item(on(
                item(Icon::Rename, "Rename", k("files-rename"), false),
                Box::new(move |s, w, cx| {
                    s.files.selected = Some(p1.clone());
                    s.begin_rename(&p1, w, cx);
                }),
            ))
            .item(on(
                item(Icon::Trash, "Delete", k("files-delete"), true),
                Box::new(move |s, w, cx| s.delete_file(p2.clone(), folder, w, cx)),
            ))
            .separator()
            .item(on(
                item(Icon::Reveal, "Reveal in file manager", None, false),
                Box::new(move |_, _, cx| cx.reveal_path(&p3)),
            ))
    }
}

/// The 6 px unsaved dot, as on the tab.
fn unsaved_dot(color: Hsla) -> gpui_kit::Div {
    div()
        .flex_none()
        .ml_auto()
        .size(px(6.))
        .rounded_full()
        .bg(color)
}

fn name_label(name: &str, hits: &[Range<usize>], p: &Palette) -> AnyElement {
    super::sidebar::label(name, hits, p).into_any_element()
}

fn render_row(shell: &Shell, ix: usize, row: &FileRow, cx: &mut Context<Shell>) -> AnyElement {
    let p = *cx.global::<Palette>();
    // `.f0`, `.f1`, `.f2`: a folder's chevron at 4 + 16 × depth; files line their icon
    // up with the folder icons above them.
    let pad = if row.folder {
        4. + 16. * row.depth as f32
    } else {
        26. + 16. * row.depth as f32
    };
    if let Some(r) = shell.files.rename.as_ref().filter(|r| r.path == row.path) {
        return div()
            .id(("file-row", ix))
            .h(row_height() + px(1.))
            .pb(px(1.))
            .on_action(cx.listener(|this, _: &input::Escape, window, cx| {
                this.cancel_rename(window, cx);
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .size_full()
                    .pl(px(pad - 1.))
                    .pr(px(4.))
                    .border_1()
                    .border_color(p.blue)
                    .rounded(RADIUS)
                    .bg(p.bg)
                    .child(icon(
                        if row.folder { Icon::Folder } else { Icon::File },
                        p.tx3,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&r.input).appearance(false).h(px(24.)).px_0()),
                    ),
            )
            .into_any_element();
    }
    let tab = if row.folder {
        None
    } else {
        shell.file_tab(&row.path, cx)
    };
    let on = tab.is_some_and(|i| shell.front == Front::Query(i));
    let dirty = tab.is_some_and(|i| shell.tabs[i].0.read(cx).dirty);
    let tip: SharedString = files::shown_path(shell.project.root(), &row.path).into();
    let element = div()
        .id(("file-row", ix))
        .h(row_height() + px(1.))
        .pb(px(1.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .size_full()
                .pl(px(pad))
                .pr(px(8.))
                .rounded(RADIUS)
                .whitespace_nowrap()
                .text_color(p.tx)
                .when(tab.is_some(), |d| d.font_weight(FontWeight::MEDIUM))
                .when(on, |d| d.bg(p.sel))
                .when(!on, |d| d.hover(|s| s.bg(p.hover)))
                .when(row.folder, |d| {
                    d.child(icon(
                        if row.expanded {
                            Icon::ChevronDown
                        } else {
                            Icon::ChevronRight
                        },
                        p.tx3,
                    ))
                })
                .child(icon(
                    if row.folder { Icon::Folder } else { Icon::File },
                    p.tx3,
                ))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(name_label(&row.name, &row.hits, &p)),
                )
                .when(row.folder, |d| d.child(type_label(&row.count, &p)))
                .when(dirty, |d| d.child(unsaved_dot(p.tx3))),
        )
        .clickable(cx.listener(move |this, e: &ClickEvent, window, cx| {
            this.on_click_file(ix, e, window, cx);
        }))
        .when(!row.folder, |d| {
            d.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
        });
    let (row, shell_entity) = (row.clone(), cx.entity());
    element
        .context_menu(move |menu, _, cx| {
            shell_entity
                .read(cx)
                .file_menu(Some(&row), menu, shell_entity.clone(), cx)
        })
        .into_any_element()
}

/// The filter field, as the Connections search looks.
fn filter_field(
    input: &Entity<InputState>,
    found: Option<usize>,
    cx: &mut Context<Shell>,
) -> impl IntoElement {
    let p = *cx.global::<Palette>();
    div()
        .flex_none()
        .mx(px(-2.))
        .mt(px(-2.))
        .mb(px(2.))
        .border_2()
        .border_color(p.sel)
        .rounded(px(8.))
        .on_action(cx.listener(|this, _: &input::Escape, window, cx| {
            this.close_files_filter(window, cx);
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
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(input).appearance(false).h(px(24.)).px_0()),
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
                        .id("clear-files-filter")
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .size(px(20.))
                        .rounded(px(4.))
                        .hover(|s| s.bg(p.hover))
                        .child(icon(Icon::Close, p.tx3))
                        .tooltip(|window, cx| Tooltip::new("Clear filter (Esc)").build(window, cx))
                        .clickable(cx.listener(|this, _, window, cx| {
                            this.close_files_filter(window, cx);
                        })),
                ),
        )
}

fn note(text: impl Into<SharedString>, color: Hsla) -> impl IntoElement {
    div()
        .flex_none()
        .mx(px(8.))
        .mt(px(2.))
        .text_size(small_size())
        .text_color(color)
        .line_height(gpui_kit::relative(1.5))
        .child(text.into())
}

pub(super) fn panel(shell: &Shell, cx: &mut Context<Shell>) -> impl IntoElement + use<> {
    let p = *cx.global::<Palette>();
    let f = &shell.files;
    let filtering = f
        .filter
        .as_ref()
        .is_some_and(|(i, _)| !i.read(cx).value().trim().is_empty());
    let empty = f.scanned && f.tree.is_empty();
    let root = shell.project.root().to_owned();
    let shell_entity = cx.entity();
    div()
        .id("files-panel")
        .key_context("Files")
        .track_focus(&f.focus)
        .on_action(cx.listener(|this, _: &super::RenameFile, window, cx| {
            this.rename_target(window, cx);
        }))
        .on_action(cx.listener(|this, _: &super::DeleteFile, window, cx| {
            this.delete_target(window, cx);
        }))
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
                        .child("Files"),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(2.))
                        .child(
                            square_button(
                                "filter-files",
                                Icon::Search,
                                format!(
                                    "Filter files by name ({})",
                                    crate::keymap::label("files-filter", cx)
                                ),
                                cx,
                            )
                            .clickable(cx.listener(
                                |this, _, window, cx| {
                                    this.filter_files(window, cx);
                                },
                            )),
                        )
                        .child({
                            let root = root.clone();
                            square_button("new-file", Icon::Plus, "New SQL file", cx).clickable(
                                cx.listener(move |this, _, window, cx| {
                                    this.new_file(root.clone(), window, cx);
                                }),
                            )
                        }),
                ),
        )
        .when_some(f.filter.as_ref(), |d, (input, _)| {
            d.child(filter_field(input, filtering.then_some(f.found), cx))
        })
        .when(empty, |d| {
            d.child(
                div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap(px(8.))
                    .mt(px(4.))
                    .px(px(8.))
                    .py(px(12.))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("No SQL files yet"),
                    )
                    .child(
                        div()
                            .text_color(p.tx2)
                            .line_height(gpui_kit::relative(1.5))
                            .child(
                                "Every .sql file in this project folder and its subfolders \
                                 shows up here.",
                            ),
                    )
                    .child({
                        let root = root.clone();
                        button("empty-new-file", ButtonKind::Outline, true, "", cx)
                            .child(icon(Icon::NewFile, p.tx))
                            .child("New file")
                            .clickable(cx.listener(move |this, _, window, cx| {
                                this.new_file(root.clone(), window, cx);
                            }))
                    }),
            )
        })
        .when(!empty, |d| {
            d.child(
                div()
                    .id("files-list")
                    .flex_1()
                    .min_h_0()
                    .child(
                        uniform_list(
                            "file-rows",
                            f.rows.len(),
                            cx.processor(|this, range: Range<usize>, _, cx| {
                                range
                                    .filter_map(|ix| {
                                        let row = this.files.rows.get(ix)?.clone();
                                        Some(render_row(this, ix, &row, cx))
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .track_scroll(&f.list)
                        .size_full(),
                    )
                    .context_menu(move |menu, _, cx| {
                        shell_entity
                            .read(cx)
                            .file_menu(None, menu, shell_entity.clone(), cx)
                    }),
            )
        })
        .when(f.filter.is_some(), |d| {
            d.child(note(
                "Matches file names only. Folders without a match are hidden.",
                p.tx3,
            ))
        })
        .when(f.rename.is_some(), |d| {
            d.child(note(
                "Enter renames, Esc cancels, the open tab follows.",
                p.tx3,
            ))
        })
        .when_some(f.error.clone(), |d, e| d.child(note(e, p.danger)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renamed_paths_follow_their_folder() {
        let (from, to) = (Path::new("/p/reports"), Path::new("/p/daily"));
        assert_eq!(moved(from, from, to), Some(to.to_owned()));
        assert_eq!(
            moved(Path::new("/p/reports/a.sql"), from, to),
            Some(PathBuf::from("/p/daily/a.sql"))
        );
        assert_eq!(moved(Path::new("/p/reportsx/a.sql"), from, to), None);
    }
}
