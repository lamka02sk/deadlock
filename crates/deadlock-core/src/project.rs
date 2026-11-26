//! A project is a folder. Its settings live in `.deadlock/` inside it.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cache::SchemaCache;
use crate::connection::{ConnectionConfig, keychain};
use crate::history::History;
use crate::{Error, Result, io_err, read_json, write_atomic, write_json};

const DIR: &str = ".deadlock";
const CONFIG: &str = "project.toml";
const STATE: &str = "state.json";
const CONSOLES: &str = "consoles";

/// `.deadlock/project.toml`: shared project settings. Never holds secrets.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// Shown instead of the folder name. `None` or blank: the folder name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `#rrggbb`, one of the app's palette colors. `None`: no title bar gradient.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// The pattern laid over `color` in the title bar.
    #[serde(default, skip_serializing_if = "Pattern::is_none")]
    pub pattern: Pattern,
    /// Stays last: TOML writes plain values before an array of tables.
    #[serde(default)]
    pub connections: Vec<ConnectionConfig>,
}

/// The pattern laid over the project color in the title bar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pattern {
    #[default]
    None,
    Stripes,
    Checks,
    Dots,
}

impl Pattern {
    pub const ALL: [Self; 4] = [Self::None, Self::Stripes, Self::Checks, Self::Dots];

    pub fn is_none(&self) -> bool {
        *self == Self::None
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Stripes => "Stripes",
            Self::Checks => "Checks",
            Self::Dots => "Dots",
        }
    }
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
    /// The sidebar's width, once the user changed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidebar_width: Option<f32>,
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
    /// Column widths set by hand in tables' Data views.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub column_widths: Vec<TableWidths>,
    /// The sidebar's open nodes (connection id, then names down), sorted.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub expanded: Vec<Vec<String>>,
}

/// One table's Data view column widths, by column name, in logical pixels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableWidths {
    pub connection: String,
    pub database: String,
    pub table: String,
    pub widths: BTreeMap<String, f32>,
}

impl ProjectState {
    /// The widths saved for a table, if any.
    #[must_use]
    pub fn widths(
        &self,
        connection: &str,
        database: &str,
        table: &str,
    ) -> Option<&BTreeMap<String, f32>> {
        self.column_widths
            .iter()
            .find(|w| w.connection == connection && w.database == database && w.table == table)
            .map(|w| &w.widths)
    }

    /// Saves a table's widths, replacing what it had.
    pub fn set_widths(
        &mut self,
        connection: &str,
        database: &str,
        table: &str,
        widths: BTreeMap<String, f32>,
    ) {
        self.column_widths.retain(|w| {
            !(w.connection == connection && w.database == database && w.table == table)
        });
        self.column_widths.push(TableWidths {
            connection: connection.to_owned(),
            database: database.to_owned(),
            table: table.to_owned(),
            widths,
        });
    }
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
        /// The schema picked or typed (`USE`) on the tab, when not the connection's
        /// default: set again when the tab's session opens.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    /// The connection's console; its buffer lives in `.deadlock/consoles/`.
    Console {
        connection: String,
        /// As a file tab's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    /// A table's or view's data.
    Table {
        connection: String,
        database: String,
        table: String,
    },
    /// An object editor tab; `object` is the kind's label (`trigger`, `view`…).
    /// Object tabs come after the others.
    Object {
        connection: String,
        schema: String,
        name: String,
        object: String,
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

impl ProjectConfig {
    /// Moves the connection at `from` to index `to`, shifting the ones between: the
    /// order `project.toml` keeps and the sidebar shows. Out of range or in place:
    /// nothing changes and `false` comes back.
    pub fn move_connection(&mut self, from: usize, to: usize) -> bool {
        let n = self.connections.len();
        if from == to || from >= n || to >= n {
            return false;
        }
        let c = self.connections.remove(from);
        self.connections.insert(to, c);
        true
    }
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

    /// The project's display name, or the folder's name when it has none; shown in the
    /// window title, the title bar and the recent list.
    pub fn name(&self) -> String {
        display_name(self.config.name.as_deref(), &self.root)
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

/// Removes everything Deadlock stored for the project at `root`, never the folder itself:
/// keychain entries of `connections` (password and SSH secret), history rows, schema
/// cache rows, then `.deadlock/`. Secrets go first and `.deadlock/` last, so a failure
/// part way leaves a project that still opens and can be deleted again. A machine with
/// no keychain service at all skips the secrets: nothing could have been saved there.
///
/// # Errors
/// The first step that fails; nothing after it runs. A locked keychain is a failure.
pub fn delete(
    root: &Path,
    connections: &[String],
    history: &mut History,
    cache: &SchemaCache,
) -> Result<()> {
    delete_with(root, connections, history, cache, keychain::delete)
}

/// [`delete`] with the keychain step passed in, so tests run without a keychain.
fn delete_with(
    root: &Path,
    connections: &[String],
    history: &mut History,
    cache: &SchemaCache,
    mut forget_secret: impl FnMut(&Path, &str) -> keyring::Result<()>,
) -> Result<()> {
    'secrets: for id in connections {
        for entry in [id.clone(), keychain::ssh_entry(id)] {
            match forget_secret(root, &entry) {
                Ok(()) => {}
                Err(keyring::Error::NoDefaultStore | keyring::Error::PlatformFailure(_)) => {
                    break 'secrets;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    history.clear(root)?;
    cache.forget(root, None)?;
    let dir = root.join(DIR);
    match fs::remove_dir_all(&dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io_err(&dir)(e)),
        _ => Ok(()),
    }
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

/// `name` trimmed, or the folder's name when `name` is missing or blank.
pub fn display_name(name: Option<&str>, root: &Path) -> String {
    match name.map(str::trim) {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => folder_name(root),
    }
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
    fn widths_replace_per_table() {
        let mut s = ProjectState::default();
        let w = |n: f32| BTreeMap::from([("id".to_owned(), n)]);
        s.set_widths("c", "db", "t", w(80.));
        s.set_widths("c", "db", "u", w(90.));
        s.set_widths("c", "db", "t", w(120.));
        assert_eq!(s.column_widths.len(), 2);
        assert_eq!(s.widths("c", "db", "t"), Some(&w(120.)));
        assert_eq!(s.widths("c", "other", "t"), None);
        let back: ProjectState = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn connections_move_and_keep_the_rest_in_order() {
        let mut config = ProjectConfig::default();
        for id in ["a", "b", "c", "d"] {
            config.connections.push(ConnectionConfig {
                id: id.into(),
                ..ConnectionConfig::new(crate::connection::Engine::Mysql)
            });
        }
        let ids = |c: &ProjectConfig| {
            c.connections
                .iter()
                .map(|c| c.id.clone())
                .collect::<Vec<_>>()
        };
        assert!(config.move_connection(0, 2));
        assert_eq!(ids(&config), ["b", "c", "a", "d"]);
        assert!(config.move_connection(3, 0));
        assert_eq!(ids(&config), ["d", "b", "c", "a"]);
        assert!(!config.move_connection(1, 1));
        assert!(!config.move_connection(4, 0));
        assert!(!config.move_connection(0, 4));
        assert_eq!(ids(&config), ["d", "b", "c", "a"]);
    }

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
        // A file from before names, colors and patterns.
        assert_eq!(project.config.name, None);
        assert_eq!(project.config.color, None);
        assert_eq!(project.config.pattern, Pattern::None);
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
    fn name_color_and_pattern_round_trip() {
        use crate::connection::{ConnectionConfig, Engine};
        let dir = tempfile::tempdir().unwrap();
        let mut project = Project::open(dir.path()).unwrap();
        assert_eq!(project.name(), folder_name(project.root()));
        project.config.name = Some("Billing".into());
        project.config.color = Some("#2f6fed".into());
        project.config.pattern = Pattern::Dots;
        project.config.connections.push(ConnectionConfig {
            id: "local".into(),
            name: "local".into(),
            ..ConnectionConfig::new(Engine::Mysql)
        });
        project.save_config().unwrap();
        let text = fs::read_to_string(dir.path().join(DIR).join(CONFIG)).unwrap();
        assert!(
            text.starts_with("name = \"Billing\"\ncolor = \"#2f6fed\"\npattern = \"dots\"\n"),
            "{text}"
        );
        let again = Project::open(dir.path()).unwrap();
        assert_eq!(again.config, project.config);
        assert_eq!(again.name(), "Billing");
    }

    #[test]
    fn blank_name_falls_back_to_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let mut project = Project::open(dir.path()).unwrap();
        project.config.name = Some("  ".into());
        assert_eq!(project.name(), folder_name(project.root()));
        project.config.name = Some(" Shop ".into());
        assert_eq!(project.name(), "Shop");
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
                schema: Some("shop_test".into()),
            },
            TabState::File {
                path: None,
                connection: None,
                text: Some("SELECT 1".into()),
                schema: None,
            },
            TabState::Console {
                connection: "local".into(),
                schema: Some("HR".into()),
            },
            TabState::Object {
                connection: "local".into(),
                schema: "app".into(),
                name: "orders_bu".into(),
                object: "trigger".into(),
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
        assert!(json.contains(r#""object": "trigger""#), "{json}");
        assert!(json.contains(r#""schema": "shop_test""#), "{json}");
    }

    #[test]
    fn tabs_saved_before_schemas_load() {
        let old = r#"[{"kind":"console","connection":"local"},{"kind":"file","text":"x"}]"#;
        let tabs: Vec<TabState> = serde_json::from_str(old).unwrap();
        assert_eq!(
            tabs,
            [
                TabState::Console {
                    connection: "local".into(),
                    schema: None
                },
                TabState::File {
                    path: None,
                    connection: None,
                    text: Some("x".into()),
                    schema: None
                }
            ]
        );
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
    fn delete_removes_deadlock_data_and_keeps_the_folder_files() {
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let project = Project::open(dir.path()).unwrap();
        let root = project.root().to_owned();
        save_text(&console_path(&root, "a").unwrap(), "select 1;").unwrap();
        fs::write(root.join("query.sql"), "select 2;").unwrap();
        let mut history = History::open(&data.path().join("history.sqlite3")).unwrap();
        let mut cache = SchemaCache::open(&data.path().join("cache.sqlite3")).unwrap();
        let other = Path::new("/elsewhere");
        cache.save(&root, "a", &[], &[]).unwrap();
        cache.save(other, "a", &[], &[]).unwrap();

        let mut forgotten = Vec::new();
        let ids = ["a".to_owned(), "b".to_owned()];
        delete_with(&root, &ids, &mut history, &cache, |r, id| {
            // Secrets go before `.deadlock/`.
            assert!(r.join(DIR).is_dir());
            forgotten.push(id.to_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(forgotten, ["a", "a#ssh", "b", "b#ssh"]);
        assert!(!root.join(DIR).exists());
        assert_eq!(
            fs::read_to_string(root.join("query.sql")).unwrap(),
            "select 2;"
        );
        assert!(cache.load(&root, "a").unwrap().is_empty());
        assert_eq!(cache.load(other, "a").unwrap().len(), 1);
        // Deleting again finds nothing to remove.
        delete_with(&root, &ids, &mut history, &cache, |_, _| Ok(())).unwrap();
    }

    #[test]
    fn delete_stops_at_the_first_failure() {
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let project = Project::open(dir.path()).unwrap();
        let mut history = History::open(&data.path().join("history.sqlite3")).unwrap();
        let cache = SchemaCache::open(&data.path().join("cache.sqlite3")).unwrap();
        let err = delete_with(
            project.root(),
            &["a".to_owned()],
            &mut history,
            &cache,
            |_, _| Err(keyring::Error::NoStorageAccess("locked".into())),
        );
        assert!(matches!(err, Err(Error::Keychain(_))));
        assert!(project.root().join(DIR).join(CONFIG).is_file());
    }

    #[test]
    fn delete_without_a_keychain_service_still_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let project = Project::open(dir.path()).unwrap();
        let mut history = History::open(&data.path().join("history.sqlite3")).unwrap();
        let cache = SchemaCache::open(&data.path().join("cache.sqlite3")).unwrap();
        let mut calls = 0;
        delete_with(
            project.root(),
            &["a".to_owned(), "b".to_owned()],
            &mut history,
            &cache,
            |_, _| {
                calls += 1;
                Err(keyring::Error::PlatformFailure("no secret service".into()))
            },
        )
        .unwrap();
        assert_eq!(calls, 1);
        assert!(!project.root().join(DIR).exists());
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
