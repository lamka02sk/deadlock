//! Deadlock's GPUI app: theme, fonts, assets and windows, shared by the binary and the spikes.

mod app;
pub mod assets;
mod config;
mod connection_form;
pub mod diagram;
mod dialog;
mod export;
mod history;
mod import;
mod keymap;
mod query;
mod results;
mod sessions;
mod settings;
mod settings_tab;
mod shell;
pub mod theme;
mod ui;
mod welcome;

use std::borrow::Cow;

use gpui_kit::App;

pub use app::run;

pub(crate) const FONTS: [&[u8]; 5] = [
    include_bytes!("../assets/fonts/inter/Inter-Regular.ttf"),
    include_bytes!("../assets/fonts/inter/Inter-Medium.ttf"),
    include_bytes!("../assets/fonts/inter/Inter-SemiBold.ttf"),
    include_bytes!("../assets/fonts/jetbrains-mono/JetBrainsMono-Regular.ttf"),
    include_bytes!("../assets/fonts/jetbrains-mono/JetBrainsMono-Medium.ttf"),
];

/// Loads the bundled fonts and gpui-component. Call once, before opening windows.
///
/// # Errors
/// Fails when a bundled font cannot be loaded.
pub fn init(cx: &mut App) -> anyhow::Result<()> {
    cx.text_system()
        .add_fonts(FONTS.iter().map(|f| Cow::Borrowed(*f)).collect())?;
    gpui_kit::init(cx);
    dialog::init(cx);
    register_sql();
    Ok(())
}

/// The SQL grammar with its highlight query fixed for tree-sitter: the bundled query
/// matches numbers with a Lua pattern (`%d`), so every number drew as a string, and
/// the plain-literal rule must come after the number rules (the first capture wins).
/// Decimals draw as numbers too.
fn register_sql() {
    use gpui_kit::component::highlighter::{LanguageConfig, LanguageRegistry};
    let query = tree_sitter_sequel::HIGHLIGHTS_QUERY
        .replace("(literal) @string\n", "")
        .replace("%d", "[0-9]")
        .replace("@float", "@number");
    let query = format!("{query}\n(literal) @string\n");
    let config = LanguageConfig::new(
        "sql",
        tree_sitter_sequel::LANGUAGE.into(),
        Vec::new(),
        &query,
        "",
        "",
    );
    LanguageRegistry::singleton().register("sql", &config);
}
