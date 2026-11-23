//! The History panel at the window's right: opened with the toolbar button or Ctrl+H,
//! its open state and width kept per project.

use gpui_kit::{AppContext as _, Context, Entity, Window};

use super::{Front, Shell};
use crate::history::{HistoryEvent, HistoryPanel};
use crate::query::Kind;

/// The panel's width until the user drags it.
pub(super) const WIDTH: f32 = 420.;

/// A project's history was cleared: every window's panel lists again.
pub(crate) fn history_cleared(cx: &mut gpui_kit::App) {
    crate::app::each_shell(cx, |s, cx| s.history_changed(cx));
}

impl Shell {
    pub(super) fn toggle_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open = !self.project.state.history_open;
        self.project.state.history_open = open;
        if open {
            let panel = self.history_panel(window, cx);
            panel.update(cx, |p, cx| {
                p.reload(cx);
                p.focus(window, cx);
            });
        } else {
            window.focus(&self.focus, cx);
        }
        self.schedule_save(cx);
        cx.notify();
    }

    /// The panel, made on first use.
    pub(super) fn history_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<HistoryPanel> {
        if let Some((panel, _)) = &self.history {
            return panel.clone();
        }
        let root = self.project.root().to_owned();
        let panel = cx.new(|cx| HistoryPanel::new(root, window, cx));
        let sub = cx.subscribe_in(&panel, window, Self::on_history);
        self.history = Some((panel.clone(), sub));
        panel
    }

    fn on_history(
        &mut self,
        _: &Entity<HistoryPanel>,
        e: &HistoryEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match e {
            HistoryEvent::Close => self.toggle_history(window, cx),
            HistoryEvent::Insert(text) => {
                let Some(tab) = self
                    .front_tab()
                    .filter(|t| t.read(cx).table.is_none())
                    .cloned()
                else {
                    return;
                };
                let editor = tab.read(cx).editor.clone();
                editor.update(cx, |e, cx| {
                    e.insert(text.clone(), window, cx);
                    e.focus(window, cx);
                });
            }
            HistoryEvent::Open {
                connection,
                statement,
            } => {
                // A connection removed since: the query opens without one.
                let binding = self.binding(Some(connection));
                let i = self.add_tab(Kind::File(None), binding, statement.clone(), window, cx);
                self.show_tab(Front::Query(i), window, cx);
            }
        }
    }

    /// A statement was recorded.
    pub(super) fn history_changed(&mut self, cx: &mut Context<Self>) {
        if let Some((panel, _)) = &self.history {
            panel.update(cx, |p, cx| p.reload(cx));
        }
    }
}
