//! A project is a folder. Its settings live in `.deadlock/` inside it.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::connection::ConnectionConfig;
use crate::{Error, Result, io_err, read_json, write_atomic, write_json};

const DIR: &str = ".deadlock";
const CONFIG: &str = "project.toml";
const STATE: &str = "state.json";
const CONSOLES: &str = "consoles";

/// `.deadlock/project.toml`: shared project settings. Never holds secrets.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectConfig {
    #[serde(default)]
    pub connections: Vec<ConnectionConfig>,
}

/// `.deadlock/state.json`: per-machine UI state, rewritten freely.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowState>,
    /// Open editor tabs, left to right.
    pub tabs: Vec<TabState>,
    /// Index into `tabs` of the tab in front.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_tab: Option<usize>,
    /// Results go to the bottom panel (`true`) instead of under their statements;
    /// `None` until the project chooses: the app's setting decides.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub results_in_panel: Option<bool>,
    /// The History panel is open.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub history_open: bool,
    /// The History panel's width, once the user changed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_width: Option<f32>,
    /// The Files panel is open.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub files_open: bool,
    /// The Files panel's width, once the user changed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_width: Option<f32>,
}

/// An editor tab as saved in `state.json`. Every tab is bound to a connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TabState {
    /// A `.sql` file, or an untitled buffer whose text is kept here until it is saved.
    File {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        connection: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// The connection's console; its buffer lives in `.deadlock/consoles/`.
    Console { connection: String },
    /// A table's or view's data.
    Table {
        connection: String,
        database: String,
        table: String,
    },
}

/// Window position and size in logical pixels. For a maximized or fullscreen window,
/// the bounds are the ones it restores to.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub mode: WindowMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowMode {
    #[default]
    Windowed,
    Maximized,
    Fullscreen,
}

/// An open project folder.
#[derive(Debug, Clone)]
pub struct Project {
    root: PathBuf,
    pub config: ProjectConfig,
    pub state: ProjectState,
}

impl Project {
    /// Opens the folder at `path`, creating `.deadlock/` and its files on first open.
    ///
    /// # Errors
    /// Fails when `path` is not an existing folder, when `.deadlock/` cannot be written,
    /// or when `project.toml` does not parse. An unreadable `state.json` is replaced
    /// by the default state, since it holds nothing that cannot be rebuilt.
    pub fn open(path: &Path) -> Result<Self> {
        let root = fs::canonicalize(path).map_err(io_err(path))?;
        if !root.is_dir() {
            return Err(Error::NotADirectory(root));
        }
        let dir = root.join(DIR);
        fs::create_dir_all(&dir).map_err(io_err(&dir))?;

        let config_path = dir.join(CONFIG);
        let config = if config_path.exists() {
            let text = fs::read_to_string(&config_path).map_err(io_err(&config_path))?;
            toml::from_str(&text).map_err(|source| Error::Toml {
                path: config_path,
                source,
            })?
        } else {
            write_atomic(&config_path, b"connections = []\n")?;
            ProjectConfig::default()
        };

        let project = Self {
            state: read_json(&dir.join(STATE)).unwrap_or_default(),
            root,
            config,
        };
        if !project.state_path().exists() {
            project.save_state()?;
        }
        Ok(project)
    }

    /// The canonical project folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The folder's name, shown in the window title and the recent list.
    pub fn name(&self) -> String {
        folder_name(&self.root)
    }

    /// Writes `config` to `.deadlock/project.toml`.
    ///
    /// # Errors
    /// Fails when the file cannot be written.
    pub fn save_config(&self) -> Result<()> {
        let path = self.root.join(DIR).join(CONFIG);
        let text = toml::to_string(&self.config).map_err(|source| Error::TomlWrite {
            path: path.clone(),
            source,
        })?;
        write_atomic(&path, text.as_bytes())
    }

    /// Writes `state` to `.deadlock/state.json`.
    ///
    /// # Errors
    /// Fails when the file cannot be written.
    pub fn save_state(&self) -> Result<()> {
        write_json(&self.state_path(), &self.state)
    }

    fn state_path(&self) -> PathBuf {
        self.root.join(DIR).join(STATE)
    }
}

/// `<project>/.deadlock/consoles/<connection id>.sql`.
///
/// # Errors
/// Fails for an id that is not a plain file name (`project.toml` is shared and may
/// hold anything).
pub fn console_path(root: &Path, connection: &str) -> Result<PathBuf> {
    let plain = !connection.is_empty()
        && connection
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'));
    if !plain {
        return Err(Error::BadName(connection.to_owned()));
    }
    Ok(root
        .join(DIR)
        .join(CONSOLES)
        .join(format!("{connection}.sql")))
}

/// Reads a text file; a missing file reads as empty (a console never saved).
///
/// # Errors
/// Fails when the file exists but cannot be read as UTF-8 text.
pub fn load_text(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(io_err(path)(e)),
    }
}

/// Writes a text file through a temporary file, creating its folder.
///
/// # Errors
/// Fails when the folder or the file cannot be written.
pub fn save_text(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(io_err(dir))?;
    }
    write_atomic(path, text.as_bytes())
}

/// The last component of `path`, or the whole path for a root like `/`.
pub fn folder_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_open_creates_files_and_state_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut project = Project::open(dir.path()).unwrap();
        assert!(dir.path().join(".deadlock/project.toml").is_file());
        assert!(dir.path().join(".deadlock/state.json").is_file());
        assert!(project.config.connections.is_empty());
        assert_eq!(project.state, ProjectState::default());

        project.state.window = Some(WindowState {
            x: 10.,
            y: 20.,
            width: 1200.,
            height: 800.,
            mode: WindowMode::Maximized,
        });
        project.save_state().unwrap();

        let again = Project::open(dir.path()).unwrap();
        assert_eq!(again.state, project.state);
        assert_eq!(again.root(), fs::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn existing_config_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(DIR)).unwrap();
        let text = "[[connections]]\nid = \"local\"\nname = \"local\"\nengine = \"mysql\"\nhost = \"localhost\"\nport = 3306\n";
        fs::write(dir.path().join(DIR).join(CONFIG), text).unwrap();
        let project = Project::open(dir.path()).unwrap();
        assert_eq!(project.config.connections.len(), 1);
        assert_eq!(
            fs::read_to_string(dir.path().join(DIR).join(CONFIG)).unwrap(),
            text
        );
    }

    #[test]
    fn saved_connections_round_trip() {
        use crate::connection::{ConnectionConfig, Engine};
        let dir = tempfile::tempdir().unwrap();
        let mut project = Project::open(dir.path()).unwrap();
        project.config.connections.push(ConnectionConfig {
            id: "local".into(),
            name: "local".into(),
            ..ConnectionConfig::new(Engine::Oracle)
        });
        project.save_config().unwrap();
        let again = Project::open(dir.path()).unwrap();
        assert_eq!(again.config, project.config);
    }

    #[test]
    fn tabs_round_trip_through_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut project = Project::open(dir.path()).unwrap();
        project.state.tabs = vec![
            TabState::File {
                path: Some(dir.path().join("a.sql")),
                connection: Some("local".into()),
                text: None,
            },
            TabState::File {
                path: None,
                connection: None,
                text: Some("SELECT 1".into()),
            },
            TabState::Console {
                connection: "local".into(),
            },
        ];
        project.state.active_tab = Some(2);
        project.state.files_open = true;
        project.state.files_width = Some(300.);
        project.save_state().unwrap();
        let again = Project::open(dir.path()).unwrap();
        assert_eq!(again.state, project.state);
        let json = fs::read_to_string(dir.path().join(DIR).join(STATE)).unwrap();
        assert!(json.contains(r#""kind": "console""#), "{json}");
    }

    #[test]
    fn consoles_save_and_load_by_connection() {
        let dir = tempfile::tempdir().unwrap();
        let path = console_path(dir.path(), "local").unwrap();
        assert_eq!(path, dir.path().join(".deadlock/consoles/local.sql"));
        assert_eq!(load_text(&path).unwrap(), "");
        save_text(&path, "SELECT 1;\n").unwrap();
        assert_eq!(load_text(&path).unwrap(), "SELECT 1;\n");
        for bad in ["", "../x", "a/b", "a.b"] {
            assert!(
                matches!(console_path(dir.path(), bad), Err(Error::BadName(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn corrupt_state_falls_back_to_default() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(DIR)).unwrap();
        fs::write(dir.path().join(DIR).join(STATE), "{not json").unwrap();
        let project = Project::open(dir.path()).unwrap();
        assert_eq!(project.state, ProjectState::default());
    }

    #[test]
    fn bad_config_and_missing_folder_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Project::open(&dir.path().join("missing")).is_err());
        let file = dir.path().join("file.sql");
        fs::write(&file, "").unwrap();
        assert!(matches!(Project::open(&file), Err(Error::NotADirectory(_))));
        fs::create_dir(dir.path().join(DIR)).unwrap();
        fs::write(dir.path().join(DIR).join(CONFIG), "connections = 3").unwrap();
        assert!(matches!(Project::open(dir.path()), Err(Error::Toml { .. })));
    }
}
