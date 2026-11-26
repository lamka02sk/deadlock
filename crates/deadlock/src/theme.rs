//! Design tokens from the Deadlock design, light and dark, following the OS appearance.

use std::sync::RwLock;
use std::sync::atomic::{AtomicU32, Ordering};

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Global, Hsla, Pixels, SharedString, Window, WindowAppearance, px, rgb, white};

/// The fonts the app ships, used until the settings name others.
pub const DEFAULT_UI_FONT: &str = "Inter";
pub const DEFAULT_MONO_FONT: &str = "JetBrains Mono";

/// The interface and code fonts in use, from the settings.
static FONTS: RwLock<(SharedString, SharedString)> = RwLock::new((
    SharedString::new_static(DEFAULT_UI_FONT),
    SharedString::new_static(DEFAULT_MONO_FONT),
));

/// Sets the fonts; a blank name keeps the shipped one.
pub fn set_fonts(ui: &str, mono: &str) {
    let pick = |name: &str, default: &'static str| {
        let name = name.trim();
        if name.is_empty() {
            SharedString::new_static(default)
        } else {
            SharedString::from(name.to_owned())
        }
    };
    let fonts = (pick(ui, DEFAULT_UI_FONT), pick(mono, DEFAULT_MONO_FONT));
    *FONTS
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = fonts;
}

/// The interface font.
pub fn ui_font() -> SharedString {
    FONTS
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .0
        .clone()
}

/// The font of SQL, values and other code.
pub fn mono_font() -> SharedString {
    FONTS
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .1
        .clone()
}
pub const RADIUS: Pixels = px(6.);
pub const SIDEBAR_WIDTH: Pixels = px(264.);

/// The interface and editor text sizes in px, from the settings. The heights below
/// follow the interface size, so rows, grids and bars stay aligned at every size; at
/// 13 px they are the design's 44 / 36 / 28.
static UI_PX: AtomicU32 = AtomicU32::new(13);
static EDITOR_PX: AtomicU32 = AtomicU32::new(13);

pub fn set_sizes(ui: u8, editor: u8) {
    UI_PX.store(u32::from(ui), Ordering::Relaxed);
    EDITOR_PX.store(u32::from(editor), Ordering::Relaxed);
}

fn ui() -> f32 {
    UI_PX.load(Ordering::Relaxed) as f32
}

/// UI text.
pub fn ui_size() -> Pixels {
    px(ui())
}

/// Table headers and the status bar.
pub fn small_size() -> Pixels {
    px(ui() - 1.)
}

/// Data cells.
pub fn data_size() -> Pixels {
    px(ui() - 0.5)
}

pub fn editor_size() -> Pixels {
    px(EDITOR_PX.load(Ordering::Relaxed) as f32)
}

pub fn toolbar_height() -> Pixels {
    px(2. * ui() + 18.)
}

pub fn tab_height() -> Pixels {
    px(2. * ui() + 10.)
}

/// Tree rows and buttons.
pub fn row_height() -> Pixels {
    px(2. * ui() + 2.)
}

pub fn status_bar_height() -> Pixels {
    row_height()
}

/// The design's color tokens. Read with `cx.global::<Palette>()`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub bg: Hsla,
    pub panel: Hsla,
    pub line: Hsla,
    pub line2: Hsla,
    pub tx: Hsla,
    pub tx2: Hsla,
    pub tx3: Hsla,
    pub hover: Hsla,
    pub sel: Hsla,
    pub blue: Hsla,
    pub blue_h: Hsla,
    pub link: Hsla,
    pub warn: Hsla,
    pub warn_bg: Hsla,
    pub warn_line: Hsla,
    pub ok: Hsla,
    pub danger: Hsla,
}

impl Global for Palette {}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

impl Palette {
    pub fn light() -> Self {
        Self {
            bg: c(0xFFFFFF),
            panel: c(0xFAFAFA),
            line: c(0xE8E8E8),
            line2: c(0xD4D4D4),
            tx: c(0x0A0A0A),
            tx2: c(0x525252),
            tx3: c(0x6B6B6B),
            hover: c(0xF0F0F0),
            sel: c(0xE8F0FE),
            blue: c(0x2563EB),
            blue_h: c(0x1D4ED8),
            link: c(0x1D4ED8),
            warn: c(0xB45309),
            warn_bg: c(0xFFF7ED),
            warn_line: c(0xFED7AA),
            ok: c(0x15803D),
            danger: c(0xB91C1C),
        }
    }

    pub fn dark() -> Self {
        Self {
            bg: c(0x0A0A0A),
            panel: c(0x111111),
            line: c(0x262626),
            line2: c(0x3D3D3D),
            tx: c(0xE5E5E5),
            tx2: c(0xA3A3A3),
            tx3: c(0x999999),
            hover: c(0x1F1F1F),
            sel: c(0x14264D),
            blue: c(0x2563EB),
            blue_h: c(0x1D4ED8),
            link: c(0x7AB0FF),
            warn: c(0xFB923C),
            warn_bg: c(0x1F1408),
            warn_line: c(0x5C3511),
            ok: c(0x4ADE80),
            danger: c(0xF87171),
        }
    }
}

fn is_dark(appearance: WindowAppearance) -> bool {
    matches!(
        appearance,
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    )
}

/// Applies the palette matching the window's appearance and keeps it in step with the OS.
pub fn follow_system(window: &mut Window, cx: &mut App) {
    apply(window.appearance(), cx);
    window
        .observe_window_appearance(|window, cx| apply(window.appearance(), cx))
        .detach();
}

/// The desktop's appearance as last reported, for re-applying after a settings change.
struct Os(WindowAppearance);

impl Global for Os {}

/// Applies the palette again: the theme setting or the text size changed.
pub fn reapply(cx: &mut App) {
    let os = cx
        .try_global::<Os>()
        .map_or(WindowAppearance::Light, |o| o.0);
    apply(os, cx);
}

/// Sets the palette global and maps it onto gpui-component's theme. `appearance` is
/// the desktop's; the Theme setting overrides it.
pub fn apply(appearance: WindowAppearance, cx: &mut App) {
    cx.set_global(Os(appearance));
    let dark = match cx
        .try_global::<crate::settings::SettingsState>()
        .map(|s| s.settings.theme)
    {
        Some(deadlock_core::settings::Theme::Light) => false,
        Some(deadlock_core::settings::Theme::Dark) => true,
        _ => is_dark(appearance),
    };
    let p = if dark {
        Palette::dark()
    } else {
        Palette::light()
    };
    cx.set_global(p);
    // `change` reloads the built-in theme colors, so ours go on in a second update.
    Theme::change(
        if dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        },
        None,
        cx,
    );
    Theme::update(cx, |t| {
        t.font_family = ui_font();
        t.font_size = ui_size();
        t.mono_font_family = mono_font();
        t.mono_font_size = data_size();
        t.radius = RADIUS;
        t.radius_lg = RADIUS;

        let k = &mut t.colors;
        k.background = p.bg;
        k.foreground = p.tx;
        k.muted = p.panel;
        k.muted_foreground = p.tx3;
        k.border = p.line;
        k.input = p.line2;
        k.ring = p.blue;
        k.caret = p.tx;
        k.selection = p.sel;
        k.accent = p.hover;
        k.accent_foreground = p.tx;
        k.primary = p.blue;
        k.primary_hover = p.blue_h;
        k.primary_active = p.blue_h;
        k.primary_foreground = white();
        k.secondary = p.panel;
        k.secondary_hover = p.hover;
        k.secondary_active = p.hover;
        k.secondary_foreground = p.tx;
        k.link = p.link;
        k.link_hover = p.link;
        k.link_active = p.link;
        k.danger = p.danger;
        k.warning = p.warn;
        k.success = p.ok;
        k.popover = p.bg;
        k.popover_foreground = p.tx;
        k.sidebar = p.panel;
        k.sidebar_border = p.line;
        k.sidebar_foreground = p.tx;
        k.title_bar = p.panel;
        k.title_bar_border = p.line;
        k.status_bar = p.panel;
        k.status_bar_border = p.line;
        k.tab_bar = p.panel;
        k.tab = p.panel;
        k.tab_active = p.bg;
        k.tab_foreground = p.tx2;
        k.tab_active_foreground = p.tx;
        k.list = p.bg;
        k.list_even = p.bg;
        k.list_head = p.panel;
        k.list_hover = p.hover;
        k.list_active = p.sel;
        k.list_active_border = p.blue;
        k.table = p.bg;
        k.table_even = p.bg;
        k.table_head = p.panel;
        k.table_head_foreground = p.tx2;
        k.table_hover = p.hover;
        // Drawn over the cell, so it must let the value show through.
        k.table_active = p.blue.opacity(0.18);
        k.table_active_border = p.blue;
        k.table_row_border = p.line;
        k.window_border = p.line;
        // `.gr.row:hover` is the panel color, not the general hover.
        k.table_hover = p.panel;
        match editor_theme(&p, dark) {
            Ok(h) => t.highlight_theme = std::sync::Arc::new(h),
            Err(e) => eprintln!("deadlock: editor theme: {e}"),
        }
    });
}

/// The editor's colors from the design: keywords `--link` medium, strings `--ok`,
/// comments `--tx3` italic, numbers `--warn`, the caret line in `--hover`.
fn editor_theme(
    p: &Palette,
    dark: bool,
) -> serde_json::Result<gpui_kit::component::highlighter::HighlightTheme> {
    let c = |h: Hsla| serde_json::to_value(h).unwrap_or_default();
    let comment = serde_json::json!({ "color": c(p.tx3), "font_style": "italic" });
    serde_json::from_value(serde_json::json!({
        "name": "Deadlock",
        "appearance": if dark { "dark" } else { "light" },
        "style": {
            "editor.background": c(p.bg),
            "editor.foreground": c(p.tx),
            "editor.active_line.background": c(p.hover),
            "editor.line_number": c(p.tx3),
            "editor.active_line_number": c(p.tx),
            "syntax": {
                "keyword": { "color": c(p.link), "font_weight": 500 },
                "string": { "color": c(p.ok) },
                "comment": comment,
                "comment.doc": comment,
                "number": { "color": c(p.warn) },
                // Ctrl/Cmd-hovered object names, underlined.
                "link_text": { "color": c(p.link) },
            },
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palettes_differ_where_the_design_says() {
        let (l, d) = (Palette::light(), Palette::dark());
        assert_ne!(l.bg, d.bg);
        assert_eq!(l.blue, d.blue);
        assert_eq!(l.link, c(0x1D4ED8));
        assert_eq!(d.link, c(0x7AB0FF));
    }

    #[test]
    fn editor_theme_parses_for_both_modes() {
        let light = editor_theme(&Palette::light(), false).unwrap();
        let keyword = light.style.syntax.style("keyword").unwrap();
        assert_eq!(keyword.color, Some(Palette::light().link));
        assert_eq!(keyword.font_weight, Some(gpui_kit::FontWeight::MEDIUM));
        let comment = light.style.syntax.style("comment").unwrap();
        assert_eq!(comment.font_style, Some(gpui_kit::FontStyle::Italic));
        assert!(editor_theme(&Palette::dark(), true).is_ok());
    }
}
