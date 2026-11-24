//! The Files panel's model: the project's `.sql` files as a tree, its rows with the
//! name filter, and the checks behind New file and Rename. No UI here.

use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};

/// Folders deeper than this are not listed.
const MAX_DEPTH: usize = 16;
/// The scan stops after this many folder entries, so a huge folder cannot stall it.
const MAX_ENTRIES: usize = 50_000;

/// A `.sql` file, or a folder that holds one somewhere below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    /// `None` for a file; a folder's entries, folders first, then by name.
    pub children: Option<Vec<Entry>>,
}

impl Entry {
    fn files(&self) -> usize {
        match &self.children {
            None => 1,
            Some(c) => c.iter().map(Entry::files).sum(),
        }
    }
}

/// Lists `root`'s `.sql` files and the folders holding them. Hidden entries (and so
/// `.deadlock`) are skipped; symlinked folders are not followed, so loops cannot occur.
/// Folders in `keep` (made in the panel, still empty) are listed too.
pub fn scan(root: &Path, keep: &HashSet<PathBuf>) -> Vec<Entry> {
    let mut budget = MAX_ENTRIES;
    scan_dir(root, keep, 0, &mut budget)
}

fn scan_dir(dir: &Path, keep: &HashSet<PathBuf>, depth: usize, budget: &mut usize) -> Vec<Entry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in read.flatten() {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        let name = item.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(kind) = item.file_type() else { continue };
        let path = item.path();
        if kind.is_dir() {
            if depth + 1 < MAX_DEPTH {
                let children = scan_dir(&path, keep, depth + 1, budget);
                if !children.is_empty() || keep.contains(&path) {
                    out.push(Entry {
                        path,
                        name,
                        children: Some(children),
                    });
                }
            }
        } else if is_sql(&name) && path.is_file() {
            out.push(Entry {
                path,
                name,
                children: None,
            });
        }
    }
    sort(&mut out);
    out
}

/// Folders first, then by name, case-insensitively.
fn sort(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.children
            .is_some()
            .cmp(&a.children.is_some())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
}

pub fn is_sql(name: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("sql"))
}

/// A line of the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    pub path: PathBuf,
    pub name: String,
    /// Folders above it.
    pub depth: usize,
    pub folder: bool,
    pub expanded: bool,
    /// A folder's file count: `3`, or `1 of 3` while filtering.
    pub count: String,
    /// The filter's match in `name`.
    pub hits: Vec<Range<usize>>,
}

/// The rows to draw and, while `filter` is not blank, how many files match. Filtering
/// matches file names only, opens every folder and hides folders without a match.
pub fn rows(tree: &[Entry], collapsed: &HashSet<PathBuf>, filter: &str) -> (Vec<FileRow>, usize) {
    let needle = filter.trim().to_lowercase();
    let mut out = Vec::new();
    let found = push_rows(tree, 0, collapsed, &needle, &mut out);
    (out, found)
}

/// Appends `entries`' rows; returns the files that match.
fn push_rows(
    entries: &[Entry],
    depth: usize,
    collapsed: &HashSet<PathBuf>,
    needle: &str,
    out: &mut Vec<FileRow>,
) -> usize {
    let mut found = 0;
    for e in entries {
        match &e.children {
            None => {
                let hits = if needle.is_empty() {
                    Some(Vec::new())
                } else {
                    match_range(&e.name, needle)
                };
                if let Some(hits) = hits {
                    found += 1;
                    out.push(FileRow {
                        path: e.path.clone(),
                        name: e.name.clone(),
                        depth,
                        folder: false,
                        expanded: false,
                        count: String::new(),
                        hits,
                    });
                }
            }
            Some(children) => {
                let at = out.len();
                let expanded = !needle.is_empty() || !collapsed.contains(&e.path);
                out.push(FileRow {
                    path: e.path.clone(),
                    name: e.name.clone(),
                    depth,
                    folder: true,
                    expanded,
                    count: String::new(),
                    hits: Vec::new(),
                });
                let total = e.files();
                let matched = if expanded {
                    push_rows(children, depth + 1, collapsed, needle, out)
                } else {
                    total
                };
                if needle.is_empty() {
                    out[at].count = total.to_string();
                } else if matched == 0 {
                    out.truncate(at);
                } else {
                    out[at].count = format!("{matched} of {total}");
                }
                found += matched;
            }
        }
    }
    found
}

/// Where `needle` (lowercase) occurs in `name`, ignoring case; `None` without a match.
fn match_range(name: &str, needle: &str) -> Option<Vec<Range<usize>>> {
    let lower = name.to_lowercase();
    let start = lower.find(needle)?;
    // Lowercasing can change byte lengths; highlight only when it did not.
    Some(if lower.len() == name.len() {
        std::iter::once(start..start + needle.len()).collect()
    } else {
        Vec::new()
    })
}

/// The name Rename or New folder would give, from what was typed. A file keeps or
/// gains `.sql`.
///
/// # Errors
/// Says why the name cannot be used.
pub fn checked_name(typed: &str, file: bool) -> Result<String, &'static str> {
    let name = typed.trim();
    if name.is_empty() {
        return Err("A name is required.");
    }
    if name.contains(['/', '\\']) || name.contains('\0') {
        return Err("A name cannot contain / or \\.");
    }
    if name == "." || name == ".." || name.starts_with('.') {
        return Err("A name cannot start with a dot.");
    }
    if file && !is_sql(name) {
        return Ok(format!("{name}.sql"));
    }
    Ok(name.to_owned())
}

/// The part of a file name to select for Rename: up to `.sql`.
pub fn stem_len(name: &str) -> usize {
    if is_sql(name) {
        name.len() - 4
    } else {
        name.len()
    }
}

/// `query.sql`, else `query-2.sql`, `query-3.sql`… (`ext` empty for a folder).
pub fn unique_name(stem: &str, ext: &str, taken: impl Fn(&str) -> bool) -> String {
    let make = |n: usize| match n {
        1 => format!("{stem}{ext}"),
        n => format!("{stem}-{n}{ext}"),
    };
    (1..)
        .map(make)
        .find(|name| !taken(name))
        .unwrap_or_default()
}

/// `path` relative to `root`, as the status bar shows it; elsewhere, the whole path.
pub fn shown_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Whether a watcher event at `path` can change the list: hidden entries (like
/// `.deadlock/state.json`, written all the time) cannot.
pub fn relevant(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|rel| {
        !rel.components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn names(rows: &[FileRow]) -> Vec<String> {
        rows.iter()
            .map(|r| format!("{}{}", "  ".repeat(r.depth), r.name))
            .collect()
    }

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for f in [
            "seed.sql",
            "Alpha.SQL",
            "notes.txt",
            "reports/daily.sql",
            "reports/recent_orders.sql",
            "reports/deep/x.sql",
            "migrations/2026_order_status.sql",
            "empty/readme.md",
            ".deadlock/consoles/local.sql",
            ".git/x.sql",
        ] {
            let path = root.join(f);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        dir
    }

    #[test]
    fn scan_lists_sql_only_folders_first_and_sorted() {
        let dir = project();
        let tree = scan(dir.path(), &HashSet::new());
        let (rows, found) = rows(&tree, &HashSet::new(), "");
        assert_eq!(
            names(&rows),
            [
                "migrations",
                "  2026_order_status.sql",
                "reports",
                "  deep",
                "    x.sql",
                "  daily.sql",
                "  recent_orders.sql",
                "Alpha.SQL",
                "seed.sql",
            ]
        );
        assert_eq!(found, 6);
        assert_eq!(rows[2].count, "3");
        assert!(
            rows.iter()
                .all(|r| !r.path.starts_with(dir.path().join(".deadlock")))
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_folders_are_not_followed() {
        let dir = project();
        std::os::unix::fs::symlink(dir.path(), dir.path().join("reports/loop")).unwrap();
        let (rows, _) = rows(&scan(dir.path(), &HashSet::new()), &HashSet::new(), "");
        assert!(rows.iter().all(|r| r.name != "loop"));
    }

    #[test]
    fn kept_empty_folders_are_listed() {
        let dir = project();
        let made = dir.path().join("reports/new");
        fs::create_dir(&made).unwrap();
        let (rows, _) = rows(
            &scan(dir.path(), &HashSet::from([made])),
            &HashSet::new(),
            "",
        );
        let new = rows.iter().find(|r| r.name == "new").unwrap();
        assert_eq!((new.folder, new.depth, new.count.as_str()), (true, 1, "0"));
    }

    #[test]
    fn collapsed_folders_hide_their_files() {
        let dir = project();
        let tree = scan(dir.path(), &HashSet::new());
        let collapsed = HashSet::from([dir.path().join("reports")]);
        let (rows, _) = rows(&tree, &collapsed, "");
        let reports = rows.iter().find(|r| r.name == "reports").unwrap();
        assert!(!reports.expanded);
        assert!(rows.iter().all(|r| r.name != "daily.sql"));
    }

    #[test]
    fn filter_matches_names_and_hides_empty_folders() {
        let dir = project();
        let tree = scan(dir.path(), &HashSet::new());
        // Collapsed folders open while filtering.
        let collapsed = HashSet::from([dir.path().join("reports")]);
        let (rows, found) = rows(&tree, &collapsed, " ORD ");
        assert_eq!(
            names(&rows),
            [
                "migrations",
                "  2026_order_status.sql",
                "reports",
                "  recent_orders.sql"
            ]
        );
        assert_eq!(found, 2);
        assert_eq!(rows[2].count, "1 of 3");
        assert_eq!(rows[3].hits.first(), Some(&(7..10)));
        let (rows, found) = rows_for(&tree, "nothing");
        assert!(rows.is_empty());
        assert_eq!(found, 0);
    }

    fn rows_for(tree: &[Entry], filter: &str) -> (Vec<FileRow>, usize) {
        rows(tree, &HashSet::new(), filter)
    }

    #[test]
    fn names_are_checked_and_files_keep_sql() {
        assert_eq!(checked_name(" top ", true), Ok("top.sql".into()));
        assert_eq!(checked_name("top.SQL", true), Ok("top.SQL".into()));
        assert_eq!(checked_name("v1.2", false), Ok("v1.2".into()));
        for bad in ["", "  ", "a/b", "a\\b", ".", "..", ".hidden"] {
            assert!(checked_name(bad, true).is_err(), "{bad:?}");
        }
        assert_eq!(stem_len("top_customers.sql"), 13);
        assert_eq!(stem_len("folder"), 6);
    }

    #[test]
    fn unique_names_count_up() {
        assert_eq!(unique_name("query", ".sql", |_| false), "query.sql");
        let taken = ["query.sql", "query-2.sql"];
        assert_eq!(
            unique_name("query", ".sql", |n| taken.contains(&n)),
            "query-3.sql"
        );
        assert_eq!(unique_name("folder", "", |n| n == "folder"), "folder-2");
    }

    #[test]
    fn hidden_paths_are_not_relevant() {
        let root = Path::new("/p");
        assert!(relevant(root, Path::new("/p/reports/a.sql")));
        assert!(!relevant(root, Path::new("/p/.deadlock/state.json")));
        assert!(!relevant(root, Path::new("/elsewhere/a.sql")));
        assert_eq!(
            shown_path(root, Path::new("/p/reports/a.sql")),
            "reports/a.sql"
        );
    }
}
