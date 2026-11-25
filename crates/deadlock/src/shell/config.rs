//! Opening and closing the Server configuration tabs.

use gpui_kit::{AppContext as _, Context, Window};

use super::{Front, Shell};
use crate::config::ConfigTab;

impl Shell {
    /// Opens connection `id`'s Server configuration tab, or brings it forward. It opens
    /// its own server session, with the password this window already has.
    pub(super) fn open_config(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(i) = self
            .configs
            .iter()
            .position(|(t, _)| t.read(cx).connection.id == id)
        {
            self.show_tab(Front::Config(i), window, cx);
            return;
        }
        let (Some(config), Some(password)) = (self.config(id).cloned(), self.passwords.get(id))
        else {
            return;
        };
        let password = password.clone();
        let tab = cx.new(|cx| ConfigTab::new(config, password, window, cx));
        // The tab strip shows its pending dot.
        let sub = cx.observe(&tab, |_, _, cx| cx.notify());
        self.configs.push((tab, sub));
        self.show_tab(Front::Config(self.configs.len() - 1), window, cx);
    }

    /// Closes tab `i`; with pending changes it asks first: Apply (closing once they all
    /// went through), Discard, or Cancel.
    pub(super) fn close_config(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.configs.get(i).map(|(t, _)| t.clone()) else {
            return;
        };
        let n = tab.read(cx).pending_count();
        if n == 0 {
            self.remove_config(i, cx);
            return;
        }
        let title = format!(
            "Apply the pending changes before closing the configuration of {}?",
            tab.read(cx).connection.name
        );
        let answer = crate::dialog::ask(
            window,
            cx,
            crate::dialog::Ask::new(title, &["Apply", "Discard", "Cancel"]).text(format!(
                "{n} {} not been applied.",
                if n == 1 { "change has" } else { "changes have" }
            )),
        );
        let handle = self.window;
        cx.spawn(async move |this, cx| {
            match answer.await {
                Some(0) => {
                    let Ok(applied) = cx.update_window(handle, |_, window, cx| {
                        tab.update(cx, |t, cx| t.apply(window, cx))
                    }) else {
                        return;
                    };
                    applied.await;
                    // A failed change stays pending, and the tab with it.
                    if tab.read_with(cx, |t, _| t.pending_count()) > 0 {
                        return;
                    }
                }
                Some(1) => {}
                _ => return,
            }
            let _ = this.update(cx, |this, cx| {
                if let Some(i) = this.configs.iter().position(|(t, _)| *t == tab) {
                    this.remove_config(i, cx);
                }
            });
        })
        .detach();
    }

    pub(super) fn remove_config(&mut self, i: usize, cx: &mut Context<Self>) {
        drop(self.configs.remove(i));
        self.front = match self.front {
            Front::Config(f) if f == i => {
                match (self.configs.len(), self.sessions.len(), self.tabs.len()) {
                    (0, 0, 0) => Front::None,
                    (0, 0, n) => Front::Query(n - 1),
                    (0, n, _) => Front::Sessions(n - 1),
                    (n, ..) => Front::Config(i.min(n - 1)),
                }
            }
            Front::Config(f) if f > i => Front::Config(f - 1),
            other => other,
        };
        self.follow_front(cx);
    }
}
