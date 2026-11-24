//! The window registry: one window per project, keyed by canonical path, plus the
//! open-project window. Registry events are logged to stderr.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use deadlock_core::instance::Request;
use deadlock_core::project::{Project, WindowMode, WindowState};
use deadlock_core::recent::RecentProjects;
use gpui_kit::{
    AnyWindowHandle, App, AppContext, Bounds, Global, PathPromptOptions, Pixels, WindowBounds,
    WindowOptions, actions, point, px, size,
};

use crate::{shell, theme, welcome};

actions!(deadlock, [OpenProject, OpenFolder, Quit]);

/// Wayland only sends activation requests for windows with an app id.
const APP_ID: &str = "deadlock";

#[derive(Default)]
pub(crate) struct Registry {
    pub projects: HashMap<PathBuf, AnyWindowHandle>,
    pub welcome: Option<AnyWindowHandle>,
    /// Each project window's shell, for Quit.
    shells: Vec<(AnyWindowHandle, gpui_kit::WeakEntity<shell::Shell>)>,
    pub recent: RecentProjects,
    recent_file: Option<PathBuf>,
}

impl Global for Registry {}

/// Runs `f` on every project window's shell.
pub(crate) fn each_shell(
    cx: &mut App,
    f: impl Fn(&mut shell::Shell, &mut gpui_kit::Context<shell::Shell>),
) {
    let shells: Vec<_> = cx
        .global::<Registry>()
        .shells
        .iter()
        .map(|(_, s)| s.clone())
        .collect();
    for s in shells {
        let _ = s.update(cx, |s, cx| f(s, cx));
    }
}

/// The core's tokio runtime, shared by every window.
pub(crate) struct Db(pub std::sync::Arc<deadlock_core::driver::Runtime>);

impl Global for Db {}

/// The query history, shared by every window; `None` when it could not be opened.
pub(crate) struct HistoryDb(
    pub Option<std::sync::Arc<std::sync::Mutex<deadlock_core::history::History>>>,
);

impl Global for HistoryDb {}

fn log(event: &str, cx: &App) {
    let r = cx.global::<Registry>();
    eprintln!(
        "deadlock: registry: {event} ({} project windows, {} windows)",
        r.projects.len(),
        cx.windows().len()
    );
}

/// Starts the app. `listener` is the single-instance socket; requests on it are
/// handled like `request`.
pub fn run(request: Request, listener: Option<std::os::unix::net::UnixListener>) {
    gpui_kit::application()
        .with_assets(crate::assets::Assets)
        .run(move |cx: &mut App| {
            if let Err(e) = start(request, listener, cx) {
                eprintln!("deadlock: {e:#}");
                cx.quit();
            }
        });
}

fn start(
    request: Request,
    listener: Option<std::os::unix::net::UnixListener>,
    cx: &mut App,
) -> anyhow::Result<()> {
    crate::init(cx)?;
    cx.set_global(Db(std::sync::Arc::new(
        deadlock_core::driver::Runtime::new()?,
    )));
    let cache = match deadlock_core::cache::SchemaCache::default_file()
        .map(|f| deadlock_core::cache::SchemaCache::open(&f))
    {
        Some(Ok(c)) => Some(std::sync::Arc::new(std::sync::Mutex::new(c))),
        Some(Err(e)) => {
            eprintln!("deadlock: {e}");
            None
        }
        None => None,
    };
    cx.set_global(shell::Cache(cache));
    let history = match deadlock_core::history::History::default_file()
        .map(|f| deadlock_core::history::History::open(&f))
    {
        Some(Ok(h)) => Some(std::sync::Arc::new(std::sync::Mutex::new(h))),
        Some(Err(e)) => {
            eprintln!("deadlock: history: {e}");
            None
        }
        None => None,
    };
    cx.set_global(HistoryDb(history));
    crate::settings::init(cx);
    crate::keymap::init(cx);
    cx.on_action(|_: &OpenProject, cx| show_welcome(cx));
    // Deferred: the key's window is busy dispatching it and cannot be updated yet.
    cx.on_action(|_: &Quit, cx| cx.defer(quit));

    let recent_file = RecentProjects::default_file();
    let recent = match recent_file.as_deref().map(RecentProjects::load) {
        Some(Ok(list)) => list,
        Some(Err(e)) => {
            eprintln!("deadlock: recent projects: {e}");
            RecentProjects::default()
        }
        None => RecentProjects::default(),
    };
    cx.set_global(Registry {
        recent,
        recent_file,
        ..Default::default()
    });

    cx.on_window_closed(|cx, id| {
        let r = cx.global_mut::<Registry>();
        let closed: Vec<_> = r
            .projects
            .iter()
            .filter(|(_, h)| h.window_id() == id)
            .map(|(p, _)| p.clone())
            .collect();
        for path in &closed {
            r.projects.remove(path);
        }
        r.shells.retain(|(h, _)| h.window_id() != id);
        if r.welcome.is_some_and(|h| h.window_id() == id) {
            r.welcome = None;
        }
        let what = closed
            .first()
            .map_or("open-project window".into(), |p| p.display().to_string());
        log(&format!("closed {what}"), cx);
    })
    .detach();

    if let Some(listener) = listener {
        serve(listener, cx);
    }
    handle(request, false, cx);
    Ok(())
}

/// Forwards requests from the socket thread to the UI thread.
fn serve(listener: std::os::unix::net::UnixListener, cx: &mut App) {
    let (tx, rx) = async_channel::unbounded();
    let socket = listener
        .local_addr()
        .ok()
        .and_then(|a| a.as_pathname().map(Path::to_owned));
    std::thread::spawn(move || {
        deadlock_core::instance::serve(&listener, |request| {
            // The UI is gone when the send fails; the process is exiting.
            let _ = tx.send_blocking(request);
        });
    });
    cx.spawn(async move |cx| {
        while let Ok(request) = rx.recv().await {
            eprintln!("deadlock: forwarded launch: {request:?}");
            cx.update(|cx| handle(request, true, cx));
        }
    })
    .detach();
    if let Some(socket) = socket {
        cx.on_app_quit(move |_| {
            let _ = std::fs::remove_file(&socket);
            async {}
        })
        .detach();
    }
}

fn handle(request: Request, forwarded: bool, cx: &mut App) {
    match request {
        // A file (a `.sql` opened from the desktop): its folder is the project.
        Request::Open(path) if path.is_file() => {
            let Some(folder) = path.parent() else { return };
            if let Err(e) = open_project(folder, forwarded, cx) {
                eprintln!("deadlock: cannot open {}: {e:#}", folder.display());
                if cx.windows().is_empty() {
                    show_welcome(cx);
                }
                return;
            }
            let r = cx.global::<Registry>();
            let shell = r.projects.get(folder).and_then(|h| {
                r.shells
                    .iter()
                    .find(|(s, _)| s == h)
                    .map(|(h, s)| (*h, s.clone()))
            });
            if let Some((handle, shell)) = shell {
                let _ = handle.update(cx, |_, window, cx| {
                    shell.update(cx, |s, cx| s.open_path(path, window, cx))
                });
            }
        }
        Request::Open(path) => {
            if let Err(e) = open_project(&path, forwarded, cx) {
                eprintln!("deadlock: cannot open {}: {e:#}", path.display());
                if cx.windows().is_empty() {
                    show_welcome(cx);
                }
            }
        }
        Request::Welcome => show_welcome(cx),
    }
}

/// Brings `handle` forward. A forwarded launch on Wayland has no activation token,
/// so the compositor may only mark the window as wanting attention.
fn focus(handle: AnyWindowHandle, forwarded: bool, cx: &mut App) -> bool {
    handle
        .update(cx, |_, window, _| {
            window.activate_window();
            if forwarded {
                window.request_attention();
            }
        })
        .is_ok()
}

/// Shows the open-project window, creating it if needed.
pub(crate) fn show_welcome(cx: &mut App) {
    if let Some(h) = cx.global::<Registry>().welcome
        && focus(h, false, cx)
    {
        log("focused open-project window", cx);
        return;
    }
    let options = WindowOptions {
        app_id: Some(APP_ID.into()),
        window_bounds: Some(WindowBounds::centered(size(px(880.), px(600.)), cx)),
        ..Default::default()
    };
    match gpui_kit::open_window(options, cx, |window, cx| {
        theme::follow_system(window, cx);
        window.set_window_title("Deadlock");
        cx.new(|cx| welcome::Welcome::new(window, cx))
    }) {
        Ok((handle, _)) => {
            cx.global_mut::<Registry>().welcome = Some(handle);
            log("opened open-project window", cx);
        }
        Err(e) => eprintln!("deadlock: cannot open a window: {e:#}"),
    }
}

/// Focuses the project's window, or opens the project in a new one.
///
/// # Errors
/// Fails when the folder cannot be opened as a project or the window cannot be created.
pub(crate) fn open_project(path: &Path, forwarded: bool, cx: &mut App) -> anyhow::Result<()> {
    let root = std::fs::canonicalize(path)?;
    if let Some(&h) = cx.global::<Registry>().projects.get(&root)
        && focus(h, forwarded, cx)
    {
        log(
            &format!("focused existing window for {}", root.display()),
            cx,
        );
        return Ok(());
    }
    let project = Project::open(&root)?;
    let title = format!("{} — Deadlock", project.name());
    let options = WindowOptions {
        app_id: Some(APP_ID.into()),
        window_bounds: Some(match project.state.window {
            Some(w) => to_bounds(w),
            None => WindowBounds::centered(size(px(1280.), px(800.)), cx),
        }),
        ..Default::default()
    };
    let (handle, shell) = gpui_kit::open_window(options, cx, |window, cx| {
        theme::follow_system(window, cx);
        window.set_window_title(&title);
        cx.new(|cx| shell::Shell::new(project, window, cx))
    })?;
    register(root, handle, &shell, cx);
    Ok(())
}

/// Switches window `handle` to the project at `path`; a project open in another window
/// is focused there instead.
///
/// # Errors
/// Fails when the folder cannot be opened as a project or the window is gone.
pub(crate) fn switch_project(
    handle: AnyWindowHandle,
    path: &Path,
    cx: &mut App,
) -> anyhow::Result<()> {
    let root = std::fs::canonicalize(path)?;
    if cx.global::<Registry>().projects.contains_key(&root) {
        return open_project(&root, false, cx);
    }
    let project = Project::open(&root)?;
    let title = format!("{} — Deadlock", project.name());
    let shell = handle.update(cx, |_, window, cx| {
        window.set_window_title(&title);
        let shell = cx.new(|cx| shell::Shell::new(project, window, cx));
        // Dropping the old root releases the old shell, which saves its state.
        window.replace_root(cx, |window, cx| {
            gpui_kit::base::Root::new(shell.clone(), window, cx)
        });
        shell
    })?;
    let r = cx.global_mut::<Registry>();
    r.projects.retain(|_, h| *h != handle);
    r.shells.retain(|(h, _)| *h != handle);
    register(root, handle, &shell, cx);
    Ok(())
}

/// Records a project window and moves the project to the top of the recent list.
fn register(
    root: PathBuf,
    handle: AnyWindowHandle,
    shell: &gpui_kit::Entity<shell::Shell>,
    cx: &mut App,
) {
    let r = cx.global_mut::<Registry>();
    r.projects.insert(root.clone(), handle);
    r.shells.push((handle, shell.downgrade()));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    r.recent.touch(&root, now);
    if let Some(file) = &r.recent_file
        && let Err(e) = r.recent.save(file)
    {
        eprintln!("deadlock: recent projects: {e}");
    }
    log(&format!("opened {}", root.display()), cx);
}

/// Closes every window the way its close button does: each project window asks about
/// open transactions and unsaved files first, and stays open on Cancel. The app exits
/// with its last window.
fn quit(cx: &mut App) {
    let r = cx.global::<Registry>();
    let (shells, welcome) = (r.shells.clone(), r.welcome);
    for (handle, shell) in shells {
        let _ = handle.update(cx, |_, window, cx| {
            let close = shell
                .update(cx, |s, cx| s.close(window, cx))
                .unwrap_or(true);
            if close {
                window.remove_window();
            }
        });
    }
    if let Some(h) = welcome {
        let _ = h.update(cx, |_, window, _| window.remove_window());
    }
}

/// Opens the native folder picker and opens the chosen folder.
pub(crate) fn pick_folder(cx: &mut App) {
    let picked = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Open".into()),
    });
    cx.spawn(async move |cx| {
        let path = match picked.await {
            Ok(Ok(Some(mut paths))) if !paths.is_empty() => paths.swap_remove(0),
            Ok(Ok(_)) | Err(_) => return,
            Ok(Err(e)) => {
                eprintln!("deadlock: folder picker: {e:#}");
                return;
            }
        };
        cx.update(|cx| open_from_welcome(&path, cx));
    })
    .detach();
}

/// Opens or focuses a project chosen in the open-project window, then closes that window.
pub(crate) fn open_from_welcome(path: &Path, cx: &mut App) {
    if let Err(e) = open_project(path, false, cx) {
        eprintln!("deadlock: cannot open {}: {e:#}", path.display());
        return;
    }
    // Deferred: a click on a recent project runs inside the open-project window's own
    // update, where updating that window again fails.
    cx.defer(|cx| {
        if let Some(h) = cx.global::<Registry>().welcome {
            let _ = h.update(cx, |_, window, _| window.remove_window());
        }
    });
}

pub(crate) fn to_state(bounds: WindowBounds) -> WindowState {
    let (mode, b) = match bounds {
        WindowBounds::Windowed(b) => (WindowMode::Windowed, b),
        WindowBounds::Maximized(b) => (WindowMode::Maximized, b),
        WindowBounds::Fullscreen(b) => (WindowMode::Fullscreen, b),
    };
    WindowState {
        x: b.origin.x.into(),
        y: b.origin.y.into(),
        width: b.size.width.into(),
        height: b.size.height.into(),
        mode,
    }
}

fn to_bounds(w: WindowState) -> WindowBounds {
    let b: Bounds<Pixels> = Bounds {
        origin: point(px(w.x), px(w.y)),
        size: size(px(w.width.max(400.)), px(w.height.max(300.))),
    };
    match w.mode {
        WindowMode::Windowed => WindowBounds::Windowed(b),
        WindowMode::Maximized => WindowBounds::Maximized(b),
        WindowMode::Fullscreen => WindowBounds::Fullscreen(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_state_round_trips_through_gpui_bounds() {
        let w = WindowState {
            x: 12.,
            y: 34.,
            width: 1000.,
            height: 700.,
            mode: WindowMode::Maximized,
        };
        assert_eq!(to_state(to_bounds(w)), w);
    }
}
