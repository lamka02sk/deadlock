//! The schema cache: nodes the servers sent, kept in SQLite in the user's app data
//! directory so the sidebar paints before a server answers. Keyed by project folder,
//! connection id and node path; each row holds one node's children as JSON.

use std::path::{Path, PathBuf};

use crate::driver::ObjectNode;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("schema cache: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("schema cache: {0}")]
    Json(#[from] serde_json::Error),
    #[error("schema cache: {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Cached paths below a connection, each with its children.
pub type Cached = Vec<(Vec<String>, Vec<ObjectNode>)>;

pub struct SchemaCache(rusqlite::Connection);

impl SchemaCache {
    /// `<app data>/deadlock/schema-cache.sqlite3`, or `None` without an app data directory.
    pub fn default_file() -> Option<PathBuf> {
        dirs::data_dir().map(|d| d.join("deadlock").join("schema-cache.sqlite3"))
    }

    /// Opens or creates the cache, and its parent directory.
    ///
    /// # Errors
    /// Fails when the file cannot be created or is not a SQLite database.
    pub fn open(file: &Path) -> Result<Self, CacheError> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|source| CacheError::Io {
                path: dir.to_owned(),
                source,
            })?;
        }
        let db = rusqlite::Connection::open(file)?;
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS nodes (
                 project TEXT NOT NULL,
                 connection TEXT NOT NULL,
                 path TEXT NOT NULL,
                 children TEXT NOT NULL,
                 PRIMARY KEY (project, connection, path)
             );",
        )?;
        Ok(Self(db))
    }

    /// Stores the children of `path` (empty: the connection's databases). Saving a
    /// connection's databases drops cached databases that are no longer among them.
    ///
    /// # Errors
    /// Fails when SQLite refuses the write.
    pub fn save(
        &mut self,
        project: &Path,
        connection: &str,
        path: &[String],
        children: &[ObjectNode],
    ) -> Result<(), CacheError> {
        let project = project.to_string_lossy();
        let tx = self.0.transaction()?;
        if path.is_empty() {
            let mut stale = Vec::new();
            {
                let mut q =
                    tx.prepare("SELECT path FROM nodes WHERE project = ?1 AND connection = ?2")?;
                let rows = q.query_map((&project, connection), |r| r.get::<_, String>(0))?;
                for p in rows {
                    let p = p?;
                    let keep = match serde_json::from_str::<Vec<String>>(&p)?.first() {
                        Some(db) => children.iter().any(|c| &c.name == db),
                        None => true,
                    };
                    if !keep {
                        stale.push(p);
                    }
                }
            }
            for p in stale {
                tx.execute(
                    "DELETE FROM nodes WHERE project = ?1 AND connection = ?2 AND path = ?3",
                    (&project, connection, p),
                )?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO nodes (project, connection, path, children) VALUES (?1, ?2, ?3, ?4)",
            (
                &project,
                connection,
                serde_json::to_string(path)?,
                serde_json::to_string(children)?,
            ),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Every cached path of the connection with its children, shortest path first, so
    /// parents can be applied before their children.
    ///
    /// # Errors
    /// Fails when SQLite cannot read the rows or a row is not valid JSON.
    pub fn load(&self, project: &Path, connection: &str) -> Result<Cached, CacheError> {
        let mut q = self
            .0
            .prepare("SELECT path, children FROM nodes WHERE project = ?1 AND connection = ?2")?;
        let rows = q.query_map((project.to_string_lossy(), connection), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (path, children) = row?;
            out.push((
                serde_json::from_str(&path)?,
                serde_json::from_str(&children)?,
            ));
        }
        out.sort_by_key(|(p, _): &(Vec<String>, _)| p.len());
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::NodeKind;

    fn db(name: &str) -> ObjectNode {
        ObjectNode::new(name, NodeKind::Database)
    }

    #[test]
    fn saves_and_loads_per_project_and_connection() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub").join("cache.sqlite3");
        let (a, b) = (Path::new("/p/a"), Path::new("/p/b"));
        let mut cache = SchemaCache::open(&file).unwrap();
        let col = ObjectNode {
            detail: "bigint".into(),
            primary_key: true,
            ..ObjectNode::new("id", NodeKind::Column)
        };
        let table = ObjectNode {
            children: Some(vec![col]),
            ..ObjectNode::new("orders", NodeKind::Table)
        };
        let groups = vec![ObjectNode {
            children: Some(vec![table]),
            ..ObjectNode::new("Tables", NodeKind::Group)
        }];
        cache.save(a, "c1", &[], &[db("shop"), db("old")]).unwrap();
        cache.save(a, "c1", &["shop".into()], &groups).unwrap();
        cache.save(a, "c1", &["old".into()], &groups).unwrap();
        cache.save(a, "c2", &[], &[db("other")]).unwrap();
        cache.save(b, "c1", &[], &[db("elsewhere")]).unwrap();
        drop(cache);

        let mut cache = SchemaCache::open(&file).unwrap();
        let got = cache.load(a, "c1").unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], (vec![], vec![db("shop"), db("old")]));
        assert!(got[1..].contains(&(vec!["shop".into()], groups.clone())));

        // `old` disappeared from the server: its cached children go too.
        cache.save(a, "c1", &[], &[db("shop")]).unwrap();
        let got = cache.load(a, "c1").unwrap();
        assert_eq!(
            got,
            vec![(vec![], vec![db("shop")]), (vec!["shop".into()], groups)]
        );
        assert_eq!(cache.load(a, "c2").unwrap()[0].1, vec![db("other")]);
        assert_eq!(cache.load(b, "c1").unwrap()[0].1, vec![db("elsewhere")]);
        assert!(cache.load(b, "none").unwrap().is_empty());
    }
}
