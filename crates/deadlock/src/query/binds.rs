//! The bind-variable dialog: one field per `:name` of the statements about to run,
//! filled with the values the tab last used.

use gpui_kit::component::input::{self, Input, InputEvent, InputState};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FontWeight, InteractiveElement, IntoElement,
    ParentElement, StatefulInteractiveElement as _, Styled, Subscription, Window, div, hsla, px,
};
use tokio::sync::oneshot;

use super::QueryTab;
use crate::theme::{Palette, RADIUS, mono_font, small_size};
use crate::ui::Clickable as _;
use crate::ui::{Kind, button};

/// Values by bind name; `None` binds NULL.
pub(super) type Values = Vec<(String, Option<String>)>;

pub(super) struct Prompt {
    fields: Vec<(String, Entity<InputState>)>,
    answer: Option<oneshot::Sender<Option<Values>>>,
    /// Focus the first field on the next frame.
    fresh: bool,
    _subs: Vec<Subscription>,
}

impl QueryTab {
    /// Opens the dialog for `names`; `answer` gets the values, or `None` on Cancel.
    pub(super) fn ask_binds(
        &mut self,
        names: Vec<String>,
        answer: oneshot::Sender<Option<Values>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut subs = Vec::new();
        let fields = names
            .into_iter()
            .map(|name| {
                let last = self.bind_values.get(&name).cloned().unwrap_or_default();
                let state = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder("NULL")
                        .default_value(last)
                });
                subs.push(cx.subscribe(&state, |this, _, e: &InputEvent, cx| {
                    if matches!(e, InputEvent::PressEnter { .. }) {
                        this.answer_binds(true, cx);
                    }
                }));
                (name, state)
            })
            .collect();
        self.binds = Some(Prompt {
            fields,
            answer: Some(answer),
            fresh: true,
            _subs: subs,
        });
        cx.notify();
    }

    /// Run (`true`) or Cancel. The values given are kept for the next run.
    pub(super) fn answer_binds(&mut self, run: bool, cx: &mut Context<Self>) {
        let Some(mut prompt) = self.binds.take() else {
            return;
        };
        let values: Values = prompt
            .fields
            .iter()
            .map(|(name, state)| {
                let v = state.read(cx).value().to_string();
                (name.clone(), (!v.is_empty()).then_some(v))
            })
            .collect();
        if run {
            for (name, v) in &values {
                self.bind_values
                    .insert(name.clone(), v.clone().unwrap_or_default());
            }
        }
        if let Some(tx) = prompt.answer.take() {
            let _ = tx.send(run.then_some(values));
        }
        cx.notify();
    }
}

/// The dialog over the tab.
pub(super) fn render(
    tab: &mut QueryTab,
    window: &mut Window,
    cx: &mut Context<QueryTab>,
) -> Option<AnyElement> {
    let p = *cx.global::<Palette>();
    let prompt = tab.binds.as_mut()?;
    if std::mem::take(&mut prompt.fresh)
        && let Some((_, first)) = prompt.fields.first()
    {
        first.update(cx, |s, cx| s.focus(window, cx));
    }
    let rows = prompt.fields.iter().map(|(name, state)| {
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .w(px(120.))
                    .flex_none()
                    .font_family(mono_font())
                    .text_size(small_size())
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(format!(":{name}")),
            )
            .child(div().flex_1().child(Styled::h(Input::new(state), px(32.))))
    });
    Some(
        div()
            .id("binds-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(hsla(0., 0., 0., 0.3))
            .on_action(cx.listener(|this, _: &input::Escape, _, cx| this.answer_binds(false, cx)))
            .child(
                div()
                    .w(px(400.))
                    .p(px(16.))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .bg(p.bg)
                    .border_1()
                    .border_color(p.line)
                    .rounded(RADIUS)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Bind variables"),
                            )
                            .child(
                                div().text_size(small_size()).text_color(p.tx3).child(
                                    "Values are sent as text; leave a field empty for NULL.",
                                ),
                            ),
                    )
                    .child(
                        div()
                            .id("bind-fields")
                            .max_h(px(320.))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .children(rows),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.))
                            .child(
                                button("binds-cancel", Kind::Outline, true, "Cancel (Esc)", cx)
                                    .child("Cancel")
                                    .clickable(
                                        cx.listener(|this, _, _, cx| this.answer_binds(false, cx)),
                                    ),
                            )
                            .child(
                                button("binds-run", Kind::Primary, true, "Run (Enter)", cx)
                                    .child("Run")
                                    .clickable(
                                        cx.listener(|this, _, _, cx| this.answer_binds(true, cx)),
                                    ),
                            ),
                    ),
            )
            .into_any_element(),
    )
}
