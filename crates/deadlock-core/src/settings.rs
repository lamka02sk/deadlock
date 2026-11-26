//! App settings: one `settings.toml` in the user's config directory, for every project
//! on this computer. Every key has a default; unknown keys are ignored and missing ones
//! take their default, so older and newer files both load. Written atomically.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, io_err, write_atomic};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// Follows the desktop's light or dark mode.
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Indent {
    #[default]
    Two,
    Four,
    Tab,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeywordCase {
    #[default]
    Lower,
    Upper,
    /// Format leaves keywords as typed.
    Keep,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResultsPlace {
    #[default]
    Inline,
    Panel,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TxDefault {
    #[default]
    Auto,
    Manual,
}

/// The interface text sizes offered, in px.
pub const UI_SIZES: [u8; 4] = [12, 13, 14, 15];
/// The editor text sizes offered, in px.
pub const EDITOR_SIZES: [u8; 4] = [12, 13, 14, 16];
/// The page sizes offered: rows fetched at a time.
pub const PAGE_SIZES: [u32; 4] = [200, 500, 1000, 5000];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: Theme,
    pub ui_size: u8,
    pub editor_size: u8,
    /// The interface font's family; empty keeps the shipped one (Inter).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub ui_font: String,
    /// The code font's family (SQL, values); empty keeps the shipped one
    /// (JetBrains Mono).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub mono_font: String,
    pub indent: Indent,
    pub keyword_case: KeywordCase,
    pub wrap: bool,
    /// Suggest names and keywords while typing SQL.
    pub autocomplete: bool,
    /// Where results go in a project that has not chosen.
    pub results: ResultsPlace,
    pub page_size: u32,
    /// How grids show NULL; empty shows `null`.
    pub null_text: String,
    /// How sessions of non-Production connections start; Production starts Manual.
    pub tx_mode: TxDefault,
    /// Ask before UPDATE / DELETE without WHERE, TRUNCATE and DROP on non-Production
    /// connections. Production always asks.
    pub ask_dangerous: bool,
    pub keep_history: bool,
    /// Key bindings that differ from the defaults: action id → keystrokes
    /// (`ctrl-shift-n`).
    pub shortcuts: BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            ui_size: 13,
            editor_size: 13,
            ui_font: String::new(),
            mono_font: String::new(),
            indent: Indent::Two,
            keyword_case: KeywordCase::Lower,
            wrap: false,
            autocomplete: true,
            results: ResultsPlace::Inline,
            page_size: 500,
            null_text: "null".into(),
            tx_mode: TxDefault::Auto,
            ask_dangerous: true,
            keep_history: true,
            shortcuts: BTreeMap::new(),
        }
    }
}

/// Why `settings.toml` could not be read.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{}: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

impl Settings {
    /// `<config dir>/deadlock/settings.toml`, or `None` without a config directory.
    pub fn default_file() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("deadlock").join("settings.toml"))
    }

    /// Reads `path`; a missing file gives the defaults. Values out of range take their
    /// default.
    ///
    /// # Errors
    /// The file cannot be read or is not valid TOML of this shape.
    pub fn load(path: &Path) -> Result<Self, SettingsError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(SettingsError::Io {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let mut s: Self = toml::from_str(&text).map_err(|source| SettingsError::Parse {
            path: path.to_owned(),
            source,
        })?;
        let d = Self::default();
        if !UI_SIZES.contains(&s.ui_size) {
            s.ui_size = d.ui_size;
        }
        if !EDITOR_SIZES.contains(&s.editor_size) {
            s.editor_size = d.editor_size;
        }
        if !PAGE_SIZES.contains(&s.page_size) {
            s.page_size = d.page_size;
        }
        Ok(s)
    }

    /// Writes the settings to `path` through a temporary file, creating its folder.
    ///
    /// # Errors
    /// The folder or file cannot be written.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io_err(dir))?;
        }
        let text = toml::to_string_pretty(self).map_err(|source| Error::TomlWrite {
            path: path.to_owned(),
            source,
        })?;
        write_atomic(path, text.as_bytes())
    }

    /// The text grids show for NULL.
    pub fn null_shown(&self) -> &str {
        if self.null_text.is_empty() {
            "null"
        } else {
            &self.null_text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_and_missing_keys_take_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        assert_eq!(Settings::load(&path).unwrap(), Settings::default());
        std::fs::write(
            &path,
            "theme = \"dark\"\nfuture_key = 3\n[future_table]\nx = 1\n",
        )
        .unwrap();
        let s = Settings::load(&path).unwrap();
        assert_eq!(s.theme, Theme::Dark);
        assert_eq!(
            Settings {
                theme: Theme::Light,
                ..s
            },
            Settings {
                theme: Theme::Light,
                ..Settings::default()
            }
        );
    }

    #[test]
    fn round_trips_and_clamps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("settings.toml");
        let mut s = Settings {
            theme: Theme::Light,
            ui_size: 15,
            editor_size: 16,
            ui_font: "DejaVu Sans".into(),
            mono_font: "Fira Code".into(),
            indent: Indent::Tab,
            keyword_case: KeywordCase::Keep,
            wrap: true,
            autocomplete: false,
            results: ResultsPlace::Panel,
            page_size: 5000,
            null_text: "∅".into(),
            tx_mode: TxDefault::Manual,
            ask_dangerous: false,
            keep_history: false,
            shortcuts: BTreeMap::new(),
        };
        s.shortcuts
            .insert("open-console".into(), "ctrl-alt-c".into());
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), s);
        std::fs::write(&path, "ui_size = 99\npage_size = 7\neditor_size = 15\n").unwrap();
        let s = Settings::load(&path).unwrap();
        assert_eq!((s.ui_size, s.page_size, s.editor_size), (13, 500, 13));
    }

    #[test]
    fn a_corrupt_file_is_an_error_and_stays() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "theme = \"dark\"\nwrap = maybe\n").unwrap();
        let e = Settings::load(&path).unwrap_err();
        assert!(e.to_string().contains("settings.toml"), "{e}");
        assert!(matches!(e, SettingsError::Parse { .. }));
        // Loading never writes.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "theme = \"dark\"\nwrap = maybe\n"
        );
        std::fs::write(&path, "theme = \"purple\"\n").unwrap();
        assert!(Settings::load(&path).is_err());
    }

    #[test]
    fn empty_null_text_shows_null() {
        let s = Settings {
            null_text: String::new(),
            ..Settings::default()
        };
        assert_eq!(s.null_shown(), "null");
        assert_eq!(Settings::default().null_shown(), "null");
    }
}
