//! Small building blocks shared by the windows, styled from the design's CSS classes.

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Div, Hsla, InteractiveElement, IntoElement, ParentElement, SharedString, Stateful,
    StatefulInteractiveElement, Styled, div, px, transparent_black, white,
};

use crate::assets::{Icon, icon};
use crate::theme::{Palette, RADIUS, row_height};

/// The design's button variants: `.b`, `.b.bo` and `.b.bp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Ghost,
    Outline,
    Primary,
}

/// A toolbar-sized button (`.b`): 28 px high, 8 px side padding, 6 px gap.
/// Disabled buttons fade to 38 % and lose their hover. An empty `tooltip` shows none:
/// a menu's trigger gets its tooltip from [`crate::shell::Trigger`], which drops it
/// while the menu is open.
pub fn button(
    id: &'static str,
    kind: Kind,
    enabled: bool,
    tooltip: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    let tooltip: SharedString = tooltip.into();
    let p = cx.global::<Palette>();
    let (bg, bg_hover, fg, fg_hover, border) = match kind {
        Kind::Ghost => (
            transparent_black(),
            p.hover,
            p.tx2,
            p.tx,
            transparent_black(),
        ),
        Kind::Outline => (transparent_black(), p.hover, p.tx, p.tx, p.line2),
        Kind::Primary => (p.blue, p.blue_h, white(), white(), transparent_black()),
    };
    div()
        .id(id)
        .group(id)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .h(row_height())
        .px(px(8.))
        .border_1()
        .border_color(border)
        .rounded(RADIUS)
        .bg(bg)
        .text_color(fg)
        .when(kind == Kind::Primary, |d| {
            d.font_weight(gpui_kit::FontWeight::MEDIUM)
        })
        .when(enabled, |d| {
            d.hover(move |s| s.bg(bg_hover).text_color(fg_hover))
        })
        .when(!enabled, |d| d.opacity(0.38))
        .when(!tooltip.is_empty(), |d| {
            d.tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        })
}

/// An icon inside a [`button`] that follows the button's hover color.
pub fn button_icon(
    group: &'static str,
    i: Icon,
    kind: Kind,
    enabled: bool,
    cx: &App,
) -> impl IntoElement {
    let p = cx.global::<Palette>();
    let (fg, fg_hover) = match kind {
        Kind::Ghost => (p.tx2, p.tx),
        Kind::Outline => (p.tx, p.tx),
        Kind::Primary => (white(), white()),
    };
    icon(i, fg).when(enabled, |s| {
        s.group_hover(group, move |s| s.text_color(fg_hover))
    })
}

/// A toolbar button with an icon and an optional label.
pub fn icon_button(
    id: &'static str,
    i: Icon,
    label: Option<&'static str>,
    kind: Kind,
    enabled: bool,
    tooltip: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    button(id, kind, enabled, tooltip, cx)
        .child(button_icon(id, i, kind, enabled, cx))
        .when_some(label, |d, l| d.child(l))
}

/// A square 28 px icon-only button, as in the sidebar header.
pub fn square_button(
    id: &'static str,
    i: Icon,
    tooltip: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    icon_button(id, i, None, Kind::Ghost, true, tooltip, cx)
        .w(row_height())
        .px_0()
        .justify_center()
}

/// A dialog over the window, as the bind-variable dialog looks: dimmed backdrop, a
/// panel with a title, an optional line under it, then `body` (fields, then a button
/// row). The caller handles Escape on the returned element.
pub fn modal(
    id: &'static str,
    width: f32,
    title: String,
    sub: Option<String>,
    body: Vec<gpui_kit::AnyElement>,
    p: &Palette,
) -> Stateful<Div> {
    div()
        .id(id)
        .absolute()
        .inset_0()
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui_kit::hsla(0., 0., 0., 0.3))
        .child(
            div()
                .w(px(width))
                .p(px(16.))
                .flex()
                .flex_col()
                .gap(px(12.))
                .bg(p.bg)
                .text_color(p.tx)
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
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .child(title),
                        )
                        .when_some(sub, |d, s| {
                            d.child(
                                div()
                                    .text_size(crate::theme::small_size())
                                    .text_color(p.tx3)
                                    .line_height(gpui_kit::relative(1.5))
                                    .child(s),
                            )
                        }),
                )
                .children(body),
        )
}

/// A field label above its input, for [`modal`] bodies.
pub fn labelled(label: &'static str, input: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .child(label)
        .child(div().font_weight(gpui_kit::FontWeight::NORMAL).child(input))
}

/// The 8 px status dot.
pub fn dot(color: Hsla) -> Div {
    div().size(px(8.)).flex_none().rounded_full().bg(color)
}
