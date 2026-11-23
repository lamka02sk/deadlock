//! The Settings tab in a project window (Ctrl+,), and the settings each window applies
//! itself: editor wrap and indent, where results go.

use deadlock_core::settings::ResultsPlace;
use gpui_kit::{App, AppContext as _, Context, Window};

use super::{Front, Shell};
use crate::settings_tab::SettingsTab;

impl Shell {
    /// Opens the Settings tab, or brings it forward.
    pub(super) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_tab.is_none() {
            let root = self.project.root().to_owned();
            self.settings_tab = Some(cx.new(|cx| SettingsTab::new(root, window, cx)));
        }
        self.show_tab(Front::Settings, window, cx);
    }

    pub(super) fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_tab = None;
        if self.front == Front::Settings {
            self.front = match self.tabs.len() {
                0 => Front::None,
                n => Front::Query(n - 1),
            };
            self.follow_front(cx);
        }
        cx.notify();
    }

    /// Whether this project's results go inline: its own choice, else the setting.
    pub(super) fn results_inline(&self, cx: &App) -> bool {
        match self.project.state.results_in_panel {
            Some(panel) => !panel,
            None => crate::settings::get(cx).results == ResultsPlace::Inline,
        }
    }

    /// The settings changed: every editor takes the wrap and indent, and tabs of a
    /// project that has not chosen follow the default results place.
    pub(super) fn apply_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let inline = self.results_inline(cx);
        let s = crate::settings::get(cx).clone();
        for (t, _) in &self.tabs {
            t.update(cx, |t, cx| {
                t.apply_settings(&s, window, cx);
                if t.inline != inline {
                    t.set_inline(inline, cx);
                }
            });
        }
        cx.notify();
    }
}
