//! Recently opened projects, stored in the user's app data directory.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Result, io_err, read_json, write_json};

const MAX: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentProject {
    pub path: PathBuf,
    /// Unix time in seconds.
    pub opened_at: u64,
    /// The project's display name when it has one, copied from its `project.toml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The project's color (`#rrggbb`), copied from its `project.toml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

impl RecentProject {
    /// The display name, or the folder's name when it has none.
    pub fn label(&self) -> String {
        crate::project::display_name(self.name.as_deref(), &self.path)
    }
}

/// Most recent first, unique by canonical path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentProjects {
    entries: Vec<RecentProject>,
}

impl RecentProjects {
    /// `<app data>/deadlock/recent.json`, or `None` when the OS has no app data directory.
    pub fn default_file() -> Option<PathBuf> {
        dirs::data_dir().map(|d| d.join("deadlock").join("recent.json"))
    }

    /// Reads the list, dropping folders that no longer exist and duplicates.
    /// A missing file is an empty list.
    ///
    /// # Errors
    /// Fails when the file exists but cannot be read or parsed.
    pub fn load(file: &Path) -> Result<Self> {
        if !file.exists() {
            return Ok(Self::default());
        }
        let stored: Self = read_json(file)?;
        let mut list = Self::default();
        for entry in stored.entries {
            if let Ok(path) = fs::canonicalize(&entry.path)
                && path.is_dir()
                && !list.entries.iter().any(|e| e.path == path)
            {
                list.entries.push(RecentProject { path, ..entry });
            }
        }
        Ok(list)
    }

    /// Writes the list, creating the parent directory if needed.
    ///
    /// # Errors
    /// Fails when the file cannot be written.
    pub fn save(&self, file: &Path) -> Result<()> {
        if let Some(dir) = file.parent() {
            fs::create_dir_all(dir).map_err(io_err(dir))?;
        }
        write_json(file, self)
    }

    /// Moves `path` to the top, adding it if new. A known entry keeps its name and color.
    pub fn touch(&mut self, path: &Path, opened_at: u64) {
        let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        let entry = match self.entries.iter().position(|e| e.path == path) {
            Some(i) => RecentProject {
                opened_at,
                ..self.entries.remove(i)
            },
            None => RecentProject {
                path,
                opened_at,
                name: None,
                color: None,
            },
        };
        self.entries.insert(0, entry);
        self.entries.truncate(MAX);
    }

    /// Sets the name and color shown for `path`; nothing when it is not in the list.
    pub fn set_look(&mut self, path: &Path, name: Option<String>, color: Option<String>) {
        let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        if let Some(entry) = self.entries.iter_mut().find(|e| e.path == path) {
            entry.name = name;
            entry.color = color;
        }
    }

    /// Drops `path` from the list; nothing when it is not in it.
    pub fn remove(&mut self, path: &Path) {
        let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        self.entries.retain(|e| e.path != path);
    }

    pub fn entries(&self) -> &[RecentProject] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::folder_name;

    #[test]
    fn touch_orders_most_recent_first_and_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();

        let mut list = RecentProjects::default();
        list.touch(&a, 1);
        list.touch(&b, 2);
        // The same folder through a different spelling.
        list.touch(&b.join("..").join("a"), 3);

        let paths: Vec<_> = list.entries().iter().map(|e| e.path.clone()).collect();
        let canon = |p: &Path| fs::canonicalize(p).unwrap();
        assert_eq!(paths, [canon(&a), canon(&b)]);
        assert_eq!(list.entries()[0].opened_at, 3);
    }

    #[test]
    fn touch_keeps_at_most_max() {
        let mut list = RecentProjects::default();
        for i in 0..30 {
            list.touch(Path::new(&format!("/nonexistent/{i}")), i);
        }
        assert_eq!(list.entries().len(), MAX);
        assert_eq!(list.entries()[0].path, Path::new("/nonexistent/29"));
    }

    #[test]
    fn save_load_round_trip_drops_missing_folders_and_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data").join("recent.json");
        assert_eq!(
            RecentProjects::load(&file).unwrap(),
            RecentProjects::default()
        );

        let (kept, gone) = (dir.path().join("kept"), dir.path().join("gone"));
        fs::create_dir(&kept).unwrap();
        fs::create_dir(&gone).unwrap();
        let mut list = RecentProjects::default();
        list.touch(&kept, 1);
        list.touch(&gone, 2);
        // A hand-edited duplicate, spelled differently.
        list.entries.push(RecentProject {
            path: kept.join("."),
            opened_at: 0,
            name: None,
            color: None,
        });
        list.save(&file).unwrap();
        fs::remove_dir(&gone).unwrap();

        let loaded = RecentProjects::load(&file).unwrap();
        assert_eq!(
            loaded.entries(),
            [RecentProject {
                path: fs::canonicalize(&kept).unwrap(),
                opened_at: 1,
                name: None,
                color: None,
            }]
        );
    }

    #[test]
    fn look_survives_touch_and_old_files_load() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("recent.json");
        let canon = fs::canonicalize(dir.path()).unwrap();
        // A file from before names and colors.
        let old = serde_json::json!({ "entries": [{ "path": canon, "opened_at": 5 }] });
        fs::write(&file, old.to_string()).unwrap();
        let mut list = RecentProjects::load(&file).unwrap();
        assert_eq!(list.entries()[0].name, None);
        assert_eq!(list.entries()[0].label(), folder_name(&canon));
        assert_eq!(list.entries()[0].opened_at, 5);

        list.set_look(dir.path(), Some("Shop".into()), Some("#e5484d".into()));
        list.touch(dir.path(), 6);
        list.save(&file).unwrap();
        let entry = &RecentProjects::load(&file).unwrap().entries[0];
        assert_eq!(entry.name.as_deref(), Some("Shop"));
        assert_eq!(entry.color.as_deref(), Some("#e5484d"));
        assert_eq!(entry.opened_at, 6);
        assert_eq!(entry.label(), "Shop");
    }

    #[test]
    fn remove_drops_only_that_folder() {
        let mut list = RecentProjects::default();
        for i in 0..3 {
            list.touch(Path::new(&format!("/nonexistent/{i}")), i);
        }
        list.remove(Path::new("/nonexistent/1"));
        list.remove(Path::new("/nonexistent/9"));
        let paths: Vec<_> = list.entries().iter().map(|e| e.path.clone()).collect();
        assert_eq!(
            paths,
            [Path::new("/nonexistent/2"), Path::new("/nonexistent/0")]
        );
    }

    #[test]
    fn corrupt_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("recent.json");
        fs::write(&file, "[").unwrap();
        assert!(RecentProjects::load(&file).is_err());
    }
}
