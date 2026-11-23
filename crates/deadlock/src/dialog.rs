//! The confirmation dialog from the design's `Dialog` artboard: a scrim, a card with a
//! title (and the Production badge), wrapping text, an optional highlighted statement,
//! and a right-aligned button row. Every prompt of the app goes through it: GPUI's
//! `window.prompt` is routed here by [`init`], and [`ask`] adds what a plain prompt
//! cannot say (the badge, the statement, a destructive action).
//!
//! Keyboard: Esc cancels, Enter takes the default action unless it is destructive, Tab
//! and Shift+Tab move between the buttons, Space or Enter presses the one with focus.
//! GPUI keeps the focus inside the dialog while it is open and gives it back after.

use std::future::Future;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, BoxShadow, Context, EventEmitter, FocusHandle, Focusable, FontWeight,
    Global, InteractiveElement, IntoElement, ParentElement, PromptButton, PromptHandle,
    PromptLevel, PromptResponse, Render, RenderablePromptHandle, SharedString,
    StatefulInteractiveElement, Styled, Window, actions, div, hsla, point, px, relative, white,
};

use crate::theme::{MONO_FONT, Palette, RADIUS, data_size};

actions!(dialog, [Cancel, Confirm, Press, NextButton, PrevButton]);

const CONTEXT: &str = "Dialog";

/// What a dialog shows. `buttons[0]` is the default action; a button labelled
/// "Cancel" is what Esc picks.
#[derive(Debug, Clone, Default)]
pub struct Ask {
    pub title: String,
    pub text: Option<String>,
    pub statement: Option<String>,
    pub buttons: Vec<String>,
    /// The Production badge next to the title.
    pub production: bool,
    /// The default action destroys something: it is drawn in the danger color and
    /// Enter does not take it.
    pub danger: bool,
}

impl Ask {
    pub fn new(title: impl Into<String>, buttons: &[&str]) -> Self {
        Self {
            title: title.into(),
            buttons: buttons.iter().map(|b| (*b).to_owned()).collect(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    #[must_use]
    pub fn statement(mut self, sql: impl Into<String>) -> Self {
        self.statement = Some(sql.into());
        self
    }

    #[must_use]
    pub fn production(mut self, on: bool) -> Self {
        self.production = on;
        self
    }

    #[must_use]
    pub fn danger(mut self, on: bool) -> Self {
        self.danger = on;
        self
    }
}

/// The dialog [`ask`] is about to open: the prompt builder takes it.
struct Next(Option<Ask>);

impl Global for Next {}

/// Routes every `window.prompt` to the styled dialog; its keys are in [`crate::keymap`].
pub fn init(cx: &mut App) {
    cx.set_global(Next(None));
    cx.set_prompt_builder(build);
}

/// Opens `ask` over `window`. Resolves to the index of the button chosen, `None` when
/// the dialog went away without an answer (another one replaced it, the window
/// closed).
pub fn ask(
    window: &mut Window,
    cx: &mut App,
    ask: Ask,
) -> impl Future<Output = Option<usize>> + use<> {
    let (title, text, buttons) = (ask.title.clone(), ask.text.clone(), ask.buttons.clone());
    cx.set_global(Next(Some(ask)));
    let labels: Vec<&str> = buttons.iter().map(String::as_str).collect();
    let rx = window.prompt(PromptLevel::Warning, &title, text.as_deref(), &labels, cx);
    async move { rx.await.ok() }
}

fn build(
    _: PromptLevel,
    message: &str,
    detail: Option<&str>,
    actions: &[PromptButton],
    handle: PromptHandle,
    window: &mut Window,
    cx: &mut App,
) -> RenderablePromptHandle {
    let ask = cx
        .try_global::<Next>()
        .is_some_and(|n| n.0.is_some())
        .then(|| cx.global_mut::<Next>().0.take())
        .flatten()
        .unwrap_or_else(|| Ask {
            title: message.to_owned(),
            text: detail.map(str::to_owned),
            buttons: actions.iter().map(|a| a.label().to_string()).collect(),
            ..Ask::default()
        });
    let view = cx.new(|cx| Dialog::new(ask, cx));
    handle.with_view(view, window, cx)
}

struct Dialog {
    ask: Ask,
    focus: FocusHandle,
    /// The button with keyboard focus; `None` until Tab when the default is
    /// destructive.
    focused: Option<usize>,
    /// Tab was pressed: the focus ring shows.
    keyboard: bool,
}

impl Dialog {
    fn new(ask: Ask, cx: &mut Context<Self>) -> Self {
        let focused = (!ask.danger && !ask.buttons.is_empty()).then_some(0);
        Self {
            ask,
            focus: cx.focus_handle(),
            focused,
            keyboard: false,
        }
    }

    fn cancel_index(&self) -> usize {
        cancel_index(&self.ask.buttons)
    }

    fn answer(&mut self, i: usize, cx: &mut Context<Self>) {
        cx.emit(PromptResponse(i));
    }

    /// The display order: Cancel first, the default action last (right-most).
    fn order(&self) -> Vec<usize> {
        let cancel = self.cancel_index();
        let mut order: Vec<usize> = (0..self.ask.buttons.len())
            .rev()
            .filter(|&i| i != cancel)
            .collect();
        if cancel < self.ask.buttons.len() {
            order.insert(0, cancel);
        }
        order
    }

    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let order = self.order();
        if order.is_empty() {
            return;
        }
        let at = self
            .focused
            .and_then(|f| order.iter().position(|&i| i == f));
        let next = match (at, forward) {
            (None, true) => 0,
            (None, false) => order.len() - 1,
            (Some(a), true) => (a + 1) % order.len(),
            (Some(a), false) => (a + order.len() - 1) % order.len(),
        };
        self.focused = Some(order[next]);
        self.keyboard = true;
        cx.notify();
    }
}

/// The button Esc picks: the one labelled Cancel, else the last.
fn cancel_index(buttons: &[String]) -> usize {
    buttons
        .iter()
        .position(|b| b.eq_ignore_ascii_case("cancel"))
        .unwrap_or(buttons.len().saturating_sub(1))
}

impl EventEmitter<PromptResponse> for Dialog {}

impl Focusable for Dialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Dialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *cx.global::<Palette>();
        let ask = &self.ask;
        let header = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(ask.title.clone())),
                    )
                    .when(ask.production, |d| d.child(production_badge(&p))),
            )
            .when_some(ask.text.clone(), |d, t| {
                d.child(
                    div()
                        .text_color(p.tx2)
                        .line_height(relative(1.5))
                        .child(SharedString::from(t)),
                )
            });
        let statement = ask.statement.as_ref().map(|sql| {
            div()
                .id("dialog-statement")
                .max_h(px(220.))
                .overflow_scroll()
                .py(px(8.))
                .px(px(12.))
                .border_1()
                .border_color(p.line)
                .rounded(RADIUS)
                .bg(p.panel)
                .font_family(MONO_FONT)
                .text_size(data_size())
                .line_height(px(20.))
                .whitespace_nowrap()
                .child(crate::query::table::highlighted(sql, &p))
        });
        let cancel = self.cancel_index();
        let buttons = self.order().into_iter().map(|i| {
            let label = SharedString::from(ask.buttons[i].clone());
            let primary = i == 0 && i != cancel;
            // The dark theme's danger token is too light under white text, so the fill is fixed.
            let red: gpui_kit::Hsla = gpui_kit::rgb(0xB9_1C1C).into();
            let (bg, fg, border, hover) = match (primary, ask.danger) {
                (true, true) => (red, white(), red, red.opacity(0.9)),
                (true, false) => (p.blue, white(), p.blue, p.blue_h),
                _ => (p.bg, p.tx, p.line2, p.hover),
            };
            let ring = self.keyboard && self.focused == Some(i);
            div()
                .id(("dialog-button", i))
                .flex()
                .items_center()
                .h(px(32.))
                .px(px(12.))
                .border_1()
                .border_color(border)
                .rounded(RADIUS)
                .bg(bg)
                .text_color(fg)
                .whitespace_nowrap()
                .when(primary, |d| d.font_weight(FontWeight::MEDIUM))
                .hover(move |s| s.bg(hover))
                // The design's `:focus-visible`: 2 px blue, 1 px off the button.
                .when(ring, |d| {
                    d.shadow(vec![
                        BoxShadow {
                            color: p.bg,
                            offset: point(px(0.), px(0.)),
                            blur_radius: px(0.),
                            spread_radius: px(1.),
                            inset: false,
                        },
                        BoxShadow {
                            color: p.blue,
                            offset: point(px(0.), px(0.)),
                            blur_radius: px(0.),
                            spread_radius: px(3.),
                            inset: false,
                        },
                    ])
                })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.answer(i, cx)))
        });
        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Cancel, _, cx| {
                let i = this.cancel_index();
                this.answer(i, cx);
            }))
            .on_action(cx.listener(|this, _: &Confirm, _, cx| {
                if let Some(i) = this.focused {
                    this.answer(i, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Press, _, cx| {
                if let Some(i) = this.focused.filter(|_| this.keyboard) {
                    this.answer(i, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NextButton, _, cx| this.step(true, cx)))
            .on_action(cx.listener(|this, _: &PrevButton, _, cx| this.step(false, cx)))
            .id("dialog-scrim")
            .occlude()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(24.))
            .bg(hsla(0., 0., 10. / 255., 0.32))
            .font_family(crate::theme::UI_FONT)
            .text_size(crate::theme::ui_size())
            .text_color(p.tx)
            .child(
                div()
                    .id("dialog")
                    .w_full()
                    .max_w(px(480.))
                    .flex()
                    .flex_col()
                    .gap(px(16.))
                    .p(px(20.))
                    .bg(p.bg)
                    .border_1()
                    .border_color(p.line2)
                    .rounded(px(10.))
                    .shadow(vec![BoxShadow {
                        color: hsla(0., 0., 10. / 255., 0.18),
                        offset: point(px(0.), px(16.)),
                        blur_radius: px(40.),
                        spread_radius: px(0.),
                        inset: false,
                    }])
                    .line_height(relative(1.4))
                    .child(header)
                    .children(statement)
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .justify_end()
                            .gap(px(8.))
                            .children(buttons),
                    ),
            )
    }
}

/// The design's Production badge: `--warn` on `--warn-bg`, 4 px radius.
pub fn production_badge(p: &Palette) -> gpui_kit::Div {
    div()
        .flex_none()
        .px(px(6.))
        .py(px(1.))
        .border_1()
        .border_color(p.warn_line)
        .rounded(px(4.))
        .bg(p.warn_bg)
        .text_color(p.warn)
        .text_size(crate::theme::small_size())
        .font_weight(FontWeight::MEDIUM)
        .child("Production")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esc_picks_cancel_or_the_last_button() {
        let b = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(cancel_index(&b(&["Commit", "Roll Back", "Cancel"])), 2);
        assert_eq!(cancel_index(&b(&["Run", "cancel"])), 1);
        assert_eq!(cancel_index(&b(&["OK"])), 0);
        assert_eq!(cancel_index(&[]), 0);
    }
}
