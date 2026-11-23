//! The app's settings while it runs: loaded once from `settings.toml`, changed through
//! [`update`], which saves the file and applies the change to every window at once.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use deadlock_core::settings::Settings;
use gpui_kit::{App, Global};

/// How long the status bar says "Settings saved".
const SAVED_NOTE: Duration = Duration::from_secs(3);

pub struct SettingsState {
    pub settings: Settings,
    path: Option<PathBuf>,
    /// The file could not be read: defaults are in use, and the file is kept until a
    /// setting changes (it is then moved aside, never silently overwritten).
    pub error: Option<String>,
    saved: Option<Instant>,
}

impl Global for SettingsState {}

/// Loads the settings and applies those that live outside views: text sizes, the page
/// size. Call before opening windows.
pub fn init(cx: &mut App) {
    let path = Settings::default_file();
    let (settings, error) = match path.as_deref().map(Settings::load) {
        Some(Ok(s)) => (s, None),
        Some(Err(e)) => {
            eprintln!("deadlock: settings: {e}");
            (Settings::default(), Some(e.to_string()))
        }
        None => (Settings::default(), None),
    };
    cx.set_global(SettingsState {
        settings,
        path,
        error,
        saved: None,
    });
    apply_globals(cx);
}

pub fn get(cx: &App) -> &Settings {
    &cx.global::<SettingsState>().settings
}

fn apply_globals(cx: &App) {
    let s = get(cx);
    crate::theme::set_sizes(s.ui_size, s.editor_size);
    deadlock_core::driver::set_page_size(usize::try_from(s.page_size).unwrap_or(500));
}

/// Changes the settings, saves them and applies them in every window.
pub fn update(cx: &mut App, change: impl FnOnce(&mut Settings)) {
    let before = get(cx).clone();
    let state = cx.global_mut::<SettingsState>();
    change(&mut state.settings);
    if state.settings == before {
        return;
    }
    if let Some(path) = state.path.clone() {
        // An unreadable file is kept next to the new one.
        if state.error.take().is_some() {
            let mut aside = path.as_os_str().to_owned();
            aside.push(".unreadable");
            if let Err(e) = std::fs::rename(&path, &aside) {
                eprintln!("deadlock: settings: {}: {e}", path.display());
            }
        }
        match state.settings.save(&path) {
            Ok(()) => state.saved = Some(Instant::now()),
            Err(e) => {
                eprintln!("deadlock: settings: {e}");
                state.error = Some(e.to_string());
            }
        }
    }
    let shortcuts = before.shortcuts != get(cx).shortcuts;
    let theme = before.theme != get(cx).theme;
    apply_globals(cx);
    if shortcuts {
        crate::keymap::apply(cx);
    }
    if theme || before.ui_size != get(cx).ui_size {
        crate::theme::reapply(cx);
    }
    cx.refresh_windows();
    // The note goes away by itself.
    cx.spawn(async move |cx| {
        cx.background_executor().timer(SAVED_NOTE).await;
        cx.update(|cx| cx.refresh_windows());
    })
    .detach();
}

/// The status bar's note: a recent save, or why the file could not be read.
pub fn note(cx: &App) -> Option<(String, bool)> {
    let state = cx.try_global::<SettingsState>()?;
    if state.error.is_some() {
        // The parser's message is several lines: the Settings tab shows it whole.
        return Some((
            "Settings file unreadable, defaults in use (see Settings)".to_owned(),
            true,
        ));
    }
    state
        .saved
        .filter(|t| t.elapsed() < SAVED_NOTE)
        .map(|_| ("Settings saved".to_owned(), false))
}
