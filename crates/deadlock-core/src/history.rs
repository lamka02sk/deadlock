//! Query history: every statement run from a query tab or a console, per project, in
//! SQLite next to the schema cache. The newest [`KEEP`] statements of a project are
//! kept. Statements that set a password are stored with the secret replaced by `***`.

use std::borrow::Cow;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::cache::CacheError;
use crate::connection::Engine;
use crate::split::tokens;

/// Statements kept per project.
pub const KEEP: usize = 10_000;

/// How a statement ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Rows returned; `more` when the result had more than were read.
    Rows { count: u64, more: bool },
    /// Rows changed.
    Affected(u64),
    /// The server's error code (`Error 1142`, `ORA-00942`) when there is one, and its
    /// message.
    Failed {
        code: Option<String>,
        message: String,
    },
}

/// One statement as run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: i64,
    /// Unix time in seconds.
    pub at: i64,
    pub connection_id: String,
    pub connection_name: String,
    pub statement: String,
    pub duration_ms: u64,
    pub outcome: Outcome,
}

/// What the History panel lists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    /// Part of the statement text, any case.
    pub text: String,
    /// Only this connection's statements.
    pub connection: Option<String>,
    /// Only statements that failed.
    pub failed: bool,
}

pub struct History(rusqlite::Connection);

impl History {
    /// `<app data>/deadlock/history.sqlite3`, or `None` without an app data directory.
    pub fn default_file() -> Option<PathBuf> {
        dirs::data_dir().map(|d| d.join("deadlock").join("history.sqlite3"))
    }

    /// Opens or creates the history, and its parent directory.
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
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS history (
                 id INTEGER PRIMARY KEY,
                 project TEXT NOT NULL,
                 at INTEGER NOT NULL,
                 connection_id TEXT NOT NULL,
                 connection_name TEXT NOT NULL,
                 statement TEXT NOT NULL,
                 duration_ms INTEGER NOT NULL,
                 rows INTEGER,
                 more INTEGER NOT NULL DEFAULT 0,
                 affected INTEGER,
                 error_code TEXT,
                 error TEXT
             );
             CREATE INDEX IF NOT EXISTS history_project ON history (project, id);",
        )?;
        Ok(Self(db))
    }

    /// Records `entry` (its `id` is ignored) and drops the project's statements past
    /// the newest [`KEEP`]. The statement is masked by [`mask`] first.
    ///
    /// # Errors
    /// Fails when SQLite refuses the write.
    pub fn record(
        &mut self,
        project: &Path,
        engine: Engine,
        entry: &Entry,
    ) -> Result<(), CacheError> {
        let project = project.to_string_lossy();
        let (rows, more, affected, code, error) = match &entry.outcome {
            Outcome::Rows { count, more } => (Some(*count), *more, None, None, None),
            Outcome::Affected(n) => (None, false, Some(*n), None, None),
            Outcome::Failed { code, message } => {
                (None, false, None, code.as_deref(), Some(message.as_str()))
            }
        };
        let tx = self.0.transaction()?;
        tx.execute(
            "INSERT INTO history (project, at, connection_id, connection_name, statement, \
             duration_ms, rows, more, affected, error_code, error) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                project,
                entry.at,
                entry.connection_id,
                entry.connection_name,
                mask(&entry.statement, engine),
                entry.duration_ms,
                rows,
                more,
                affected,
                code,
                error
            ],
        )?;
        tx.execute(
            "DELETE FROM history WHERE project = ?1 AND id <= (SELECT id FROM history \
             WHERE project = ?1 ORDER BY id DESC LIMIT 1 OFFSET ?2)",
            rusqlite::params![project, KEEP],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Deletes every statement of `project`.
    ///
    /// # Errors
    /// Fails when SQLite refuses the write.
    pub fn clear(&mut self, project: &Path) -> Result<(), CacheError> {
        self.0.execute(
            "DELETE FROM history WHERE project = ?1",
            [project.to_string_lossy()],
        )?;
        Ok(())
    }

    /// Up to `limit` statements matching `filter`, newest first, older than `before`
    /// (an [`Entry::id`]) when given: the next page of the list.
    ///
    /// # Errors
    /// Fails when SQLite cannot read the rows.
    pub fn page(
        &self,
        project: &Path,
        filter: &Filter,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Entry>, CacheError> {
        // shortcut: LIKE scans the project's rows (10,000 at most); FTS if that gets slow.
        let pattern = format!(
            "%{}%",
            filter
                .text
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let mut q = self.0.prepare(
            "SELECT id, at, connection_id, connection_name, statement, duration_ms, rows, \
             more, affected, error_code, error FROM history \
             WHERE project = ?1 AND id < ?2 AND statement LIKE ?3 ESCAPE '\\' \
             AND (?4 IS NULL OR connection_id = ?4) AND (?5 = 0 OR error IS NOT NULL) \
             ORDER BY id DESC LIMIT ?6",
        )?;
        let rows = q.query_map(
            rusqlite::params![
                project.to_string_lossy(),
                before.unwrap_or(i64::MAX),
                pattern,
                filter.connection,
                filter.failed,
                limit
            ],
            |r| {
                let outcome = match (r.get::<_, Option<String>>(10)?, r.get::<_, Option<u64>>(8)?) {
                    (Some(message), _) => Outcome::Failed {
                        code: r.get(9)?,
                        message,
                    },
                    (None, Some(n)) => Outcome::Affected(n),
                    (None, None) => Outcome::Rows {
                        count: r.get::<_, Option<u64>>(6)?.unwrap_or(0),
                        more: r.get(7)?,
                    },
                };
                Ok(Entry {
                    id: r.get(0)?,
                    at: r.get(1)?,
                    connection_id: r.get(2)?,
                    connection_name: r.get(3)?,
                    statement: r.get(4)?,
                    duration_ms: r.get(5)?,
                    outcome,
                })
            },
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

/// The error code at the start of a server error: MySQL / MariaDB `ERROR 1142 (42000)`
/// as `Error 1142`, Oracle `ORA-00942`, Redis' error kind (`WRONGTYPE`).
pub fn error_code(message: &str) -> Option<String> {
    if let Some(at) = message.find("ERROR ") {
        let digits: String = message[at + 6..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if !digits.is_empty() {
            return Some(format!("Error {digits}"));
        }
    }
    if let Some(at) = message.find("ORA-") {
        let code = &message[at..];
        if code.len() >= 9 && code.as_bytes()[4..9].iter().all(u8::is_ascii_digit) {
            return Some(code[..9].to_owned());
        }
    }
    // Redis: `WRONGTYPE: Operation against a key…`, `NOPERM: …`.
    let head = message.split([':', ' ']).next().unwrap_or_default();
    (head.len() >= 3 && head.bytes().all(|b| b.is_ascii_uppercase())).then(|| head.to_owned())
}

/// `text` with the secrets of password statements replaced by `***`: what follows
/// `IDENTIFIED BY` (also `BY PASSWORD`), `PASSWORD(…)`, `REPLACE`, `AS` / `USING` with
/// a quoted hash, `SET PASSWORD … =`; Redis `AUTH`, `HELLO … AUTH`, `ACL SETUSER`
/// password rules and `CONFIG SET requirepass / masterauth`.
pub fn mask(text: &str, engine: Engine) -> Cow<'_, str> {
    let toks = tokens(text, engine);
    let word = |i: usize| toks.get(i).map(|r| &text[r.clone()]);
    let is = |i: usize, w: &str| word(i).is_some_and(|t| t.eq_ignore_ascii_case(w));
    let quoted = |i: usize| word(i).is_some_and(|t| t.starts_with(['\'', '"']));
    let mut secret: Vec<Range<usize>> = Vec::new();
    if engine == Engine::Redis {
        let n = toks.len();
        if is(0, "auth") && n > 1 {
            secret.push(toks[n - 1].clone());
        } else if is(0, "hello") {
            if let Some(a) = (1..n).find(|&i| is(i, "auth"))
                && a + 2 < n
            {
                secret.push(toks[a + 2].clone());
            }
        } else if is(0, "acl") && is(1, "setuser") {
            for r in toks.iter().skip(3) {
                let t = &text[r.clone()];
                if t.starts_with(['>', '<', '#', '!']) && t.len() > 1 {
                    secret.push(r.start + 1..r.end);
                }
            }
        } else if is(0, "config")
            && is(1, "set")
            && (is(2, "requirepass") || is(2, "masterauth"))
            && n > 3
        {
            secret.push(toks[3].clone());
        }
    } else {
        let user = (is(0, "create") || is(0, "alter")) && is(1, "user");
        let set_password = is(0, "set") && is(1, "password");
        let identified = (0..toks.len()).any(|i| is(i, "identified"));
        if !(user || set_password || identified || is(0, "grant")) {
            return Cow::Borrowed(text);
        }
        for i in 0..toks.len() {
            let target = if is(i, "by") {
                let j = if is(i + 1, "password") { i + 2 } else { i + 1 };
                (!is(j, "random")).then_some(j)
            } else if is(i, "password") && word(i + 1) == Some("(") {
                Some(i + 2)
            } else if is(i, "replace")
                || (is(i, "as") || is(i, "using")) && quoted(i + 1)
                || set_password && word(i) == Some("=") && !is(i + 1, "password")
            {
                Some(i + 1)
            } else {
                None
            };
            if let Some(r) = target.and_then(|j| toks.get(j)) {
                secret.push(r.clone());
            }
        }
    }
    if secret.is_empty() {
        return Cow::Borrowed(text);
    }
    secret.sort_by_key(|r| r.start);
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for r in secret {
        if r.start < at {
            continue;
        }
        out.push_str(&text[at..r.start]);
        let t = &text[r.clone()];
        match t.chars().next() {
            Some(q @ ('\'' | '"' | '`')) => {
                out.push(q);
                out.push_str("***");
                out.push(q);
            }
            _ => out.push_str("***"),
        }
        at = r.end;
    }
    out.push_str(&text[at..]);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_statements_are_masked() {
        let my = |t| mask(t, Engine::Mysql).into_owned();
        assert_eq!(
            my("CREATE USER 'bob'@'%' IDENTIFIED BY 's3;cret'"),
            "CREATE USER 'bob'@'%' IDENTIFIED BY '***'"
        );
        assert_eq!(
            my("alter user bob identified with caching_sha2_password by 'x' replace 'old'"),
            "alter user bob identified with caching_sha2_password by '***' replace '***'"
        );
        assert_eq!(
            my("CREATE USER a IDENTIFIED BY RANDOM PASSWORD"),
            "CREATE USER a IDENTIFIED BY RANDOM PASSWORD"
        );
        assert_eq!(
            my("SET PASSWORD FOR 'a'@'%' = 'n'"),
            "SET PASSWORD FOR 'a'@'%' = '***'"
        );
        assert_eq!(
            my("set password = PASSWORD('n')"),
            "set password = PASSWORD('***')"
        );
        assert_eq!(
            my("GRANT ALL ON *.* TO a IDENTIFIED BY PASSWORD '*94BD'"),
            "GRANT ALL ON *.* TO a IDENTIFIED BY PASSWORD '***'"
        );
        assert_eq!(
            my("CREATE USER a IDENTIFIED VIA ed25519 USING PASSWORD('pw')"),
            "CREATE USER a IDENTIFIED VIA ed25519 USING PASSWORD('***')"
        );
        // Text that only looks like it stays.
        assert_eq!(
            my("SELECT 'IDENTIFIED BY x' FROM t"),
            "SELECT 'IDENTIFIED BY x' FROM t"
        );
        assert_eq!(
            my("SELECT * FROM t ORDER BY a"),
            "SELECT * FROM t ORDER BY a"
        );
        let ora = |t| mask(t, Engine::Oracle).into_owned();
        assert_eq!(
            ora("ALTER USER app IDENTIFIED BY \"N3w pw\" REPLACE old1"),
            "ALTER USER app IDENTIFIED BY \"***\" REPLACE ***"
        );
        assert_eq!(
            ora("create user x identified by secret default tablespace users"),
            "create user x identified by *** default tablespace users"
        );
    }

    #[test]
    fn redis_secrets_are_masked() {
        let r = |t| mask(t, Engine::Redis).into_owned();
        assert_eq!(r("AUTH pw"), "AUTH ***");
        assert_eq!(r("auth bob \"p w\""), "auth bob \"***\"");
        assert_eq!(r("HELLO 3 AUTH bob pw"), "HELLO 3 AUTH bob ***");
        assert_eq!(
            r("ACL SETUSER bob on >pw1 <old ~keys:* +get #ab12"),
            "ACL SETUSER bob on >*** <*** ~keys:* +get #***"
        );
        assert_eq!(
            r("CONFIG SET requirepass hunter2"),
            "CONFIG SET requirepass ***"
        );
        assert_eq!(r("GET auth"), "GET auth");
    }

    #[test]
    fn error_codes_come_from_the_message() {
        assert_eq!(
            error_code("Server error: `ERROR 1142 (42000): SELECT command denied'").as_deref(),
            Some("Error 1142")
        );
        assert_eq!(
            error_code("ORA-00942: table or view does not exist").as_deref(),
            Some("ORA-00942")
        );
        assert_eq!(
            error_code("WRONGTYPE: Operation against a key").as_deref(),
            Some("WRONGTYPE")
        );
        assert_eq!(error_code("the query task stopped"), None);
    }

    fn entry(conn: &str, statement: &str, outcome: Outcome) -> Entry {
        Entry {
            id: 0,
            at: 1_700_000_000,
            connection_id: conn.into(),
            connection_name: conn.into(),
            statement: statement.into(),
            duration_ms: 12,
            outcome,
        }
    }

    #[test]
    fn records_filters_pages_and_masks() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = History::open(&dir.path().join("h.sqlite3")).unwrap();
        let (a, b) = (Path::new("/p/a"), Path::new("/p/b"));
        let rows = Outcome::Rows {
            count: 5,
            more: false,
        };
        h.record(a, Engine::Mysql, &entry("c1", "select 1", rows.clone()))
            .unwrap();
        h.record(
            a,
            Engine::Mysql,
            &entry(
                "c2",
                "update t set a = 50_% where id = 1",
                Outcome::Failed {
                    code: Some("Error 1142".into()),
                    message: "denied".into(),
                },
            ),
        )
        .unwrap();
        h.record(
            a,
            Engine::Mysql,
            &entry(
                "c1",
                "create user u identified by 'pw'",
                Outcome::Affected(0),
            ),
        )
        .unwrap();
        h.record(b, Engine::Mysql, &entry("c1", "select 2", rows.clone()))
            .unwrap();

        let all = h.page(a, &Filter::default(), None, 10).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].statement, "create user u identified by '***'");
        assert_eq!(all[0].outcome, Outcome::Affected(0));
        assert_eq!(all[2].outcome, rows);
        let f = |text: &str, connection: Option<&str>, failed| Filter {
            text: text.into(),
            connection: connection.map(Into::into),
            failed,
        };
        assert_eq!(
            h.page(a, &f("SELECT", None, false), None, 10)
                .unwrap()
                .len(),
            1
        );
        // `%` and `_` match themselves.
        assert_eq!(
            h.page(a, &f("50_%", None, false), None, 10).unwrap().len(),
            1
        );
        assert_eq!(
            h.page(a, &f("5_0", None, false), None, 10).unwrap().len(),
            0
        );
        assert_eq!(
            h.page(a, &f("", Some("c1"), false), None, 10)
                .unwrap()
                .len(),
            2
        );
        let failed = h.page(a, &f("", None, true), None, 10).unwrap();
        assert_eq!(failed.len(), 1);
        assert!(
            matches!(&failed[0].outcome, Outcome::Failed { code: Some(c), .. } if c == "Error 1142")
        );
        // Paging: older than the newest.
        let older = h.page(a, &Filter::default(), Some(all[0].id), 1).unwrap();
        assert_eq!(older, vec![all[1].clone()]);
        // Clearing one project leaves the other.
        h.clear(a).unwrap();
        assert!(h.page(a, &Filter::default(), None, 10).unwrap().is_empty());
        assert_eq!(h.page(b, &Filter::default(), None, 10).unwrap().len(), 1);
    }

    #[test]
    fn keeps_the_newest_per_project() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = History::open(&dir.path().join("h.sqlite3")).unwrap();
        let a = Path::new("/p/a");
        for i in 0..KEEP + 3 {
            h.record(
                a,
                Engine::Redis,
                &entry(
                    "c",
                    &format!("GET k{i}"),
                    Outcome::Rows {
                        count: 1,
                        more: false,
                    },
                ),
            )
            .unwrap();
        }
        let all = h.page(a, &Filter::default(), None, KEEP + 10).unwrap();
        assert_eq!(all.len(), KEEP);
        assert_eq!(all[0].statement, format!("GET k{}", KEEP + 2));
        assert_eq!(all[KEEP - 1].statement, "GET k3");
    }
}
