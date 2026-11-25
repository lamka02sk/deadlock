//! The Settings tab in a project window (Ctrl+,), and the settings each window applies
//! itself: editor wrap and indent, where results go.

use deadlock_core::project::{Pattern, Project};
use deadlock_core::settings::ResultsPlace;
use gpui_kit::{App, AppContext as _, Context, Window};

use super::{Front, Shell};
use crate::settings_tab::{Section, SettingsTab};

impl Shell {
    /// Opens the Settings tab, or brings it forward.
    pub(super) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_tab.is_none() {
            let root = self.project.root().to_owned();
            let shell = cx.entity().downgrade();
            let name = self.project.config.name.clone();
            self.settings_tab = Some(cx.new(|cx| SettingsTab::new(root, shell, name, window, cx)));
        }
        self.show_tab(Front::Settings, window, cx);
    }

    /// Opens the Settings tab on its Project section.
    pub(crate) fn open_project_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.switcher_open = false;
        self.open_settings(window, cx);
        if let Some(tab) = &self.settings_tab {
            tab.update(cx, |t, cx| t.show(Section::Project, cx));
        }
    }

    pub(crate) fn project(&self) -> &Project {
        &self.project
    }

    /// Saves the project's name, color and pattern to `project.toml` and shows them in
    /// the window title, the title bar and the recent list. A blank name is no name.
    ///
    /// # Errors
    /// The write failed: the project keeps its old look, in memory and in the file.
    pub(crate) fn set_look(
        &mut self,
        name: Option<String>,
        color: Option<String>,
        pattern: Pattern,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> deadlock_core::Result<()> {
        let name = name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty());
        let old = self.project.config.clone();
        let config = &mut self.project.config;
        (config.name, config.color, config.pattern) = (name, color, pattern);
        if let Err(e) = self.project.save_config() {
            self.project.config = old;
            return Err(e);
        }
        window.set_window_title(&format!("{} — Deadlock", self.project.name()));
        let config = &self.project.config;
        crate::app::set_recent_look(
            self.project.root(),
            config.name.clone(),
            config.color.clone(),
            cx,
        );
        cx.notify();
        Ok(())
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
