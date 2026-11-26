//! Serves Deadlock's own SVG icons next to gpui-component's.

use std::borrow::Cow;

use gpui_kit::{AssetSource, Hsla, ParentElement as _, SharedString, Styled, Svg, svg};

/// The design's icons, served as `deadlock/icons/<name>.svg` so they never shadow
/// gpui-component's `icons/` set.
#[derive(rust_embed::RustEmbed)]
#[folder = "assets/icons"]
#[prefix = "deadlock/icons/"]
struct Icons;

/// Deadlock's icons first, then gpui-component's.
#[derive(Debug, Clone, Copy, Default)]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        match Icons::get(path) {
            Some(file) => Ok(Some(file.data)),
            None => gpui_kit::assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut out: Vec<SharedString> = Icons::iter()
            .filter(|p| p.starts_with(path))
            .map(|p| SharedString::from(p.into_owned()))
            .collect();
        out.extend(gpui_kit::assets::Assets.list(path)?);
        Ok(out)
    }
}

/// Icons from `assets/icons`. Every one of them is drawn in `currentColor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    ChevronDown,
    ChevronRight,
    Close,
    Console,
    Commit,
    Database,
    Spinner,
    Explain,
    Export,
    Folder,
    ForeignKey,
    Format,
    History,
    Import,
    Minus,
    NewQuery,
    Play,
    Plus,
    PrimaryKey,
    Refresh,
    Rollback,
    Routine,
    Save,
    Search,
    SortAsc,
    SortDesc,
    Stop,
    Table,
    Sessions,
    Lock,
    Blocks,
    Config,
    ArrowRight,
    Diagram,
    File,
    NewFile,
    NewFolder,
    Rename,
    Trash,
    Reveal,
    PanelRight,
    Calendar,
    Copy,
    Duplicate,
    Paste,
    SetNull,
    SetDefault,
    Json,
    Eye,
    EyeOff,
    Open,
    SelectAll,
    Color,
    Warning,
    Layers,
    Sequence,
    Procedure,
    Package,
    Trigger,
    Link,
    ObjectType,
    Transpose,
}

impl Icon {
    pub fn path(self) -> &'static str {
        match self {
            Self::ChevronDown => "deadlock/icons/chevron-down.svg",
            Self::ChevronRight => "deadlock/icons/chevron-right.svg",
            Self::Close => "deadlock/icons/close.svg",
            Self::Console => "deadlock/icons/console.svg",
            Self::Commit => "deadlock/icons/check.svg",
            Self::Database => "deadlock/icons/database.svg",
            Self::Spinner => "deadlock/icons/spinner.svg",
            Self::Explain => "deadlock/icons/explain.svg",
            Self::Export => "deadlock/icons/export.svg",
            Self::Folder => "deadlock/icons/folder.svg",
            Self::ForeignKey => "deadlock/icons/foreign-key.svg",
            Self::Format => "deadlock/icons/format.svg",
            Self::History => "deadlock/icons/history.svg",
            Self::Import => "deadlock/icons/import.svg",
            Self::Minus => "deadlock/icons/minus.svg",
            Self::NewQuery => "deadlock/icons/new-query.svg",
            Self::Play => "deadlock/icons/play.svg",
            Self::Plus => "deadlock/icons/plus.svg",
            Self::PrimaryKey => "deadlock/icons/primary-key.svg",
            Self::Refresh => "deadlock/icons/refresh.svg",
            Self::Rollback => "deadlock/icons/rollback.svg",
            Self::Routine => "deadlock/icons/routine.svg",
            Self::Save => "deadlock/icons/save.svg",
            Self::Search => "deadlock/icons/search.svg",
            Self::SortAsc => "deadlock/icons/sort-asc.svg",
            Self::SortDesc => "deadlock/icons/sort-desc.svg",
            Self::Stop => "deadlock/icons/stop.svg",
            Self::Table => "deadlock/icons/table.svg",
            Self::Sessions => "deadlock/icons/sessions.svg",
            Self::Lock => "deadlock/icons/lock.svg",
            Self::Blocks => "deadlock/icons/blocks.svg",
            Self::Config => "deadlock/icons/config.svg",
            Self::ArrowRight => "deadlock/icons/arrow-right.svg",
            Self::Diagram => "deadlock/icons/diagram.svg",
            Self::File => "deadlock/icons/file.svg",
            Self::NewFile => "deadlock/icons/new-file.svg",
            Self::NewFolder => "deadlock/icons/new-folder.svg",
            Self::Rename => "deadlock/icons/rename.svg",
            Self::Trash => "deadlock/icons/trash.svg",
            Self::Reveal => "deadlock/icons/reveal.svg",
            Self::PanelRight => "deadlock/icons/panel-right.svg",
            Self::Calendar => "deadlock/icons/calendar.svg",
            Self::Copy => "deadlock/icons/copy.svg",
            Self::Duplicate => "deadlock/icons/duplicate.svg",
            Self::Paste => "deadlock/icons/paste.svg",
            Self::SetNull => "deadlock/icons/null.svg",
            Self::SetDefault => "deadlock/icons/default.svg",
            Self::Json => "deadlock/icons/json.svg",
            Self::Eye => "deadlock/icons/eye.svg",
            Self::EyeOff => "deadlock/icons/eye-off.svg",
            Self::Open => "deadlock/icons/open.svg",
            Self::SelectAll => "deadlock/icons/select-all.svg",
            Self::Color => "deadlock/icons/color.svg",
            Self::Warning => "deadlock/icons/warning.svg",
            Self::Layers => "deadlock/icons/layers.svg",
            Self::Sequence => "deadlock/icons/sequence.svg",
            Self::Procedure => "deadlock/icons/procedure.svg",
            Self::Package => "deadlock/icons/package.svg",
            Self::Trigger => "deadlock/icons/trigger.svg",
            Self::Link => "deadlock/icons/link.svg",
            Self::ObjectType => "deadlock/icons/type.svg",
            Self::Transpose => "deadlock/icons/transpose.svg",
        }
    }
}

/// The multicolor app logo, drawn with `img` rather than tinted.
pub const APP_LOGO: &str = "deadlock/icons/app-logo.svg";

/// A 16 px icon tinted with `color`. GPUI paints an SVG as a mask in its own text color,
/// so the color is set here rather than inherited.
pub fn icon(icon: Icon, color: Hsla) -> Svg {
    svg()
        .path(icon.path())
        .size_4()
        .flex_none()
        .text_color(color)
}

/// A context-menu item with a 16 px icon in the text color (so it follows the hover and
/// disabled states). Items without an icon still get the column: see `popup_menu.rs`.
pub fn menu_item(
    label: impl Into<SharedString>,
    i: Icon,
) -> gpui_kit::component::menu::PopupMenuItem {
    gpui_kit::component::menu::PopupMenuItem::new(label).icon(menu_icon(i))
}

pub fn menu_icon(i: Icon) -> gpui_kit::component::Icon {
    gpui_kit::component::Icon::empty().path(i.path())
}

/// A context-menu item with a hint at its right end, as the design's `.mi .kb`
/// ("Double-click", "Enter"): what else does the same.
pub fn menu_item_hint(
    label: impl Into<SharedString>,
    hint: &'static str,
    i: Icon,
) -> gpui_kit::component::menu::PopupMenuItem {
    use gpui_kit::{ParentElement as _, px};
    let label = label.into();
    gpui_kit::component::menu::PopupMenuItem::element(move |_, cx| {
        let p = *cx.global::<crate::theme::Palette>();
        gpui_kit::div()
            .flex()
            .flex_1()
            .items_center()
            .justify_between()
            .gap(px(16.))
            .child(label.clone())
            .child(
                gpui_kit::div()
                    .text_size(px(11.5))
                    .text_color(p.tx3)
                    .child(hint),
            )
    })
    .icon(menu_icon(i))
}

/// A destructive context-menu item: label and icon in the danger color.
pub fn menu_item_danger(
    label: impl Into<SharedString>,
    i: Icon,
    cx: &gpui_kit::App,
) -> gpui_kit::component::menu::PopupMenuItem {
    let (label, danger) = (label.into(), cx.global::<crate::theme::Palette>().danger);
    gpui_kit::component::menu::PopupMenuItem::element(move |_, _| {
        gpui_kit::div().text_color(danger).child(label.clone())
    })
    .icon(menu_icon(i).text_color(danger))
}

/// A disabled item with the reason under it, as the design's `.mi.x`.
pub fn menu_item_disabled(
    label: &'static str,
    reason: &'static str,
    i: Icon,
) -> gpui_kit::component::menu::PopupMenuItem {
    use gpui_kit::{ParentElement as _, px};
    gpui_kit::component::menu::PopupMenuItem::element(move |_, cx| {
        let p = *cx.global::<crate::theme::Palette>();
        gpui_kit::div()
            .flex()
            .flex_col()
            .max_w(px(300.))
            .py(px(2.))
            .child(label)
            .child(
                gpui_kit::div()
                    .text_size(crate::theme::small_size())
                    .text_color(p.tx3)
                    .whitespace_normal()
                    .child(reason),
            )
    })
    .icon(menu_icon(i))
    .disabled(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_and_the_logo_load() {
        let all = [
            Icon::ChevronDown,
            Icon::ChevronRight,
            Icon::Close,
            Icon::Console,
            Icon::Commit,
            Icon::Database,
            Icon::Spinner,
            Icon::Explain,
            Icon::Export,
            Icon::Folder,
            Icon::ForeignKey,
            Icon::Format,
            Icon::History,
            Icon::Import,
            Icon::Minus,
            Icon::NewQuery,
            Icon::Play,
            Icon::Plus,
            Icon::PrimaryKey,
            Icon::Refresh,
            Icon::Rollback,
            Icon::Routine,
            Icon::Save,
            Icon::Search,
            Icon::SortAsc,
            Icon::SortDesc,
            Icon::Stop,
            Icon::Table,
            Icon::Sessions,
            Icon::Lock,
            Icon::Blocks,
            Icon::Config,
            Icon::ArrowRight,
            Icon::Diagram,
            Icon::File,
            Icon::NewFile,
            Icon::NewFolder,
            Icon::Rename,
            Icon::Trash,
            Icon::Reveal,
            Icon::PanelRight,
            Icon::Calendar,
            Icon::Copy,
            Icon::Duplicate,
            Icon::Paste,
            Icon::SetNull,
            Icon::SetDefault,
            Icon::Json,
            Icon::Eye,
            Icon::EyeOff,
            Icon::Open,
            Icon::SelectAll,
            Icon::Color,
            Icon::Warning,
            Icon::Layers,
            Icon::Sequence,
            Icon::Procedure,
            Icon::Package,
            Icon::Trigger,
            Icon::Link,
            Icon::ObjectType,
            Icon::Transpose,
        ];
        for path in all.iter().map(|i| i.path()).chain([APP_LOGO]) {
            assert!(Assets.load(path).unwrap().is_some(), "{path}");
        }
        // gpui-component's own icons still resolve.
        assert!(Assets.load("icons/check.svg").unwrap().is_some());
    }
}
