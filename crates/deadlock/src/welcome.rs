//! The open-project window: recent projects and Open folder.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, NaiveDate};
use deadlock_core::recent::RecentProject;
use gpui_kit::{
    Context, Div, FocusHandle, Font, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Render, Stateful, StatefulInteractiveElement, Styled, StyledText, TextRun, Window, div, img,
    px, relative,
};

use crate::app::{OpenFolder, Registry, open_from_welcome, pick_folder};
use crate::assets::{APP_LOGO, Icon, icon};
use crate::theme::{Palette, RADIUS, data_size, mono_font, small_size};
use crate::ui::Clickable as _;
use crate::ui::{Kind, dot, icon_button};

/// Key context for the window's own bindings (Ctrl+O).
pub const CONTEXT: &str = "Welcome";

pub struct Welcome {
    focus: FocusHandle,
}

impl Welcome {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        // Window open markers follow the registry.
        cx.observe_global::<Registry>(|_, cx| cx.notify()).detach();
        Self { focus }
    }
}

impl Render for Welcome {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *cx.global::<Palette>();
        let registry = cx.global::<Registry>();
        let today = Local::now().date_naive();
        let rows: Vec<_> = registry
            .recent
            .entries()
            .iter()
            .enumerate()
            .map(|(ix, e)| {
                let open = registry.projects.contains_key(&e.path);
                recent_row(ix, e, open, day_label(e.opened_at, today), &p)
            })
            .collect();

        let header = div()
            .flex()
            .items_center()
            .gap(px(16.))
            .child(img(APP_LOGO).size(px(40.)).flex_none())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(20.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .line_height(relative(1.2))
                            .child("Open a project"),
                    )
                    .child(
                        div()
                            .text_color(p.tx2)
                            .child("A project is a folder. Each project opens in its own window."),
                    ),
            )
            .child(
                icon_button(
                    "open-folder",
                    Icon::Folder,
                    Some("Open folder"),
                    Kind::Primary,
                    true,
                    format!("Open folder ({})", crate::keymap::label("open-folder", cx)),
                    cx,
                )
                .h(px(32.))
                .px(px(12.))
                .clickable(|_, _, cx| pick_folder(cx)),
            );

        let recent = if rows.is_empty() {
            empty_state(window, &p).into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(
                    div()
                        .px(px(10.))
                        .text_size(small_size())
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.tx2)
                        .child("Recent projects"),
                )
                .child(div().flex().flex_col().gap(px(1.)).children(rows))
                .into_any_element()
        };

        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(|_: &OpenFolder, _, cx| pick_folder(cx))
            .size_full()
            .bg(p.bg)
            .text_color(p.tx)
            .line_height(relative(1.4))
            .child(
                div().id("welcome").size_full().overflow_y_scroll().child(
                    div()
                        .w_full()
                        .max_w(px(640.))
                        .mx_auto()
                        .px(px(24.))
                        .pt(px(48.))
                        .pb(px(32.))
                        .flex()
                        .flex_col()
                        .gap(px(32.))
                        .child(header)
                        .child(recent),
                ),
            )
    }
}

/// A recent project: its folder icon in the project color, its name and its path.
fn recent_row(ix: usize, e: &RecentProject, open: bool, day: String, p: &Palette) -> Stateful<Div> {
    let path = &e.path;
    let target = path.to_owned();
    let tint = e.color.as_deref().and_then(crate::color::parse_hex);
    let trailing = if open {
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.))
            .text_color(p.ok)
            .font_weight(FontWeight::MEDIUM)
            .child(dot(p.ok))
            .child("Window open")
    } else {
        div().flex_none().text_color(p.tx3).child(day)
    };
    let row = div()
        .id(("recent", ix))
        .flex()
        .items_center()
        .gap(px(10.))
        .w_full()
        .min_h(px(36.))
        .px(px(10.))
        .rounded(RADIUS)
        .whitespace_nowrap()
        .hover(|s| s.bg(p.hover))
        .clickable(move |_, _, cx| open_from_welcome(&target, cx))
        .child(icon(Icon::Folder, tint.unwrap_or(p.tx3)))
        .child(
            div()
                .flex_none()
                .font_weight(FontWeight::MEDIUM)
                .child(e.label()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .font_family(mono_font())
                .text_size(data_size())
                .text_color(p.tx3)
                .child(tilde(path)),
        )
        .child(trailing);
    if open {
        row.tooltip(|window, cx| {
            gpui_kit::component::tooltip::Tooltip::new("Switch to the open window")
                .build(window, cx)
        })
    } else {
        row
    }
}

fn empty_state(window: &Window, p: &Palette) -> impl IntoElement {
    let (before, code, after) = (
        "Open any folder to start. Deadlock keeps the project's connections and consoles in a ",
        ".deadlock",
        " folder inside it.",
    );
    let style = window.text_style();
    let run = |len, font: Font| TextRun {
        len,
        font,
        color: p.tx2,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let mono = Font {
        family: mono_font(),
        ..style.font()
    };
    let text = StyledText::new(format!("{before}{code}{after}")).with_runs(vec![
        run(before.len(), style.font()),
        run(code.len(), mono),
        run(after.len(), style.font()),
    ]);
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .p(px(20.))
        .border_1()
        .border_color(p.line)
        .rounded(RADIUS)
        .bg(p.panel)
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child("No recent projects"),
        )
        .child(div().line_height(relative(1.5)).child(text))
}

/// `~/Code/x` for paths under the home folder.
pub(crate) fn tilde(path: &Path) -> String {
    match dirs::home_dir().and_then(|h| path.strip_prefix(&h).ok().map(PathBuf::from)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// "Today", "Yesterday", "14 Nov", or "14 Nov 2025" for another year.
pub(crate) fn day_label(opened_at: u64, today: NaiveDate) -> String {
    let Some(at) = i64::try_from(opened_at)
        .ok()
        .and_then(|s| DateTime::from_timestamp(s, 0))
    else {
        return String::new();
    };
    let day = at.with_timezone(&Local).date_naive();
    if day == today {
        "Today".into()
    } else if today.pred_opt() == Some(day) {
        "Yesterday".into()
    } else if day.format("%Y").to_string() == today.format("%Y").to_string() {
        day.format("%-d %b").to_string()
    } else {
        day.format("%-d %b %Y").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn secs(d: NaiveDate) -> u64 {
        let t = Local
            .from_local_datetime(&d.and_hms_opt(12, 0, 0).unwrap())
            .unwrap();
        u64::try_from(t.timestamp()).unwrap()
    }

    #[test]
    fn day_labels() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 22).unwrap();
        assert_eq!(day_label(secs(today), today), "Today");
        assert_eq!(
            day_label(secs(today - Duration::days(1)), today),
            "Yesterday"
        );
        assert_eq!(day_label(secs(today - Duration::days(8)), today), "14 Nov");
        assert_eq!(
            day_label(secs(NaiveDate::from_ymd_opt(2025, 3, 2).unwrap()), today),
            "2 Mar 2025"
        );
    }

    #[test]
    fn home_paths_use_a_tilde() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(tilde(&home.join("Code").join("x")), "~/Code/x");
        assert_eq!(tilde(Path::new("/opt/x")), "/opt/x");
    }
}
