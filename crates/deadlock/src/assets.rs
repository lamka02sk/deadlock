//! Serves Deadlock's own SVG icons next to gpui-component's.

use std::borrow::Cow;

use gpui_kit::{AssetSource, Hsla, SharedString, Styled, Svg, svg};

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
        ];
        for path in all.iter().map(|i| i.path()).chain([APP_LOGO]) {
            assert!(Assets.load(path).unwrap().is_some(), "{path}");
        }
        // gpui-component's own icons still resolve.
        assert!(Assets.load("icons/check.svg").unwrap().is_some());
    }
}
