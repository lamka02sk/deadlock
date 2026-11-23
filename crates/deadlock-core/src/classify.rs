//! What a statement does, read from its keywords: whether it reads, writes or changes
//! the schema, whether it commits an open transaction on its own, and whether it is
//! dangerous enough to confirm first.
//!
//! Works on one statement as [`crate::split::split`] returns it. Comments and the
//! contents of strings and quoted names are ignored; only clauses outside parentheses
//! count, so a `WHERE` inside a subquery or a CTE is not the statement's own. Anything
//! not recognised is a [`Kind::Write`].

use std::fmt;

use crate::connection::Engine;
use crate::split::{block_comment_end, line_end, q_quote_end, quoted_end};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Reads data or session settings: `SELECT` (also `FOR UPDATE`), `SHOW`, `SET @x`.
    Read,
    /// Changes data, or may: DML, procedure calls, PL/SQL blocks, anything unknown.
    Write,
    /// Changes the schema, users or server settings: `CREATE`, `ALTER`, `GRANT`.
    Ddl,
    /// Transaction and lock control: `COMMIT`, `ROLLBACK`, `START TRANSACTION`.
    Transaction,
}

/// Statements asked about in every environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Danger {
    UpdateWithoutWhere,
    DeleteWithoutWhere,
    Truncate,
    Drop,
    /// Redis: the server flags the command `admin` or `@dangerous`.
    ServerFlagged,
}

impl fmt::Display for Danger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UpdateWithoutWhere => "UPDATE without WHERE changes every row of the table",
            Self::DeleteWithoutWhere => "DELETE without WHERE deletes every row of the table",
            Self::Truncate => "TRUNCATE deletes every row of the table",
            Self::Drop => "DROP deletes the object and everything in it",
            Self::ServerFlagged => {
                "The server flags this command as dangerous or administrative: it can delete \
                 data, change settings or stop the server"
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub kind: Kind,
    /// The statement's first keyword, upper case: `UPDATE`, `CREATE`.
    pub verb: String,
    pub danger: Option<Danger>,
    /// Commits an open transaction before it runs: MySQL's implicit-commit statements,
    /// Oracle DDL.
    pub implicit_commit: bool,
}

impl Statement {
    /// Whether it changes data or schema: refused on read-only connections, confirmed
    /// on production ones.
    pub fn changes(&self) -> bool {
        matches!(self.kind, Kind::Write | Kind::Ddl)
    }
}

/// Classifies one SQL statement for `engine` (MySQL, MariaDB or Oracle).
pub fn classify(text: &str, engine: Engine) -> Statement {
    let mysql = matches!(engine, Engine::Mysql | Engine::Mariadb);
    let words = words(text, mysql);
    let Some((first, depth)) = words.first() else {
        return statement(Kind::Read, "", None, false);
    };
    // `(SELECT …) UNION (SELECT …)`: a query wrapped in parentheses.
    if *depth > 0 {
        let kind = if matches!(first.as_str(), "SELECT" | "WITH" | "VALUES" | "TABLE") {
            Kind::Read
        } else {
            Kind::Write
        };
        return statement(kind, first, None, false);
    }
    let top: Vec<&str> = words
        .iter()
        .filter(|(_, d)| *d == 0)
        .map(|(w, _)| w.as_str())
        .collect();
    of(&top, mysql)
}

fn statement(kind: Kind, verb: &str, danger: Option<Danger>, implicit_commit: bool) -> Statement {
    Statement {
        kind,
        verb: verb.to_owned(),
        danger,
        implicit_commit,
    }
}

/// Words at the statement's own level, starting with its verb.
fn of(w: &[&str], mysql: bool) -> Statement {
    use Kind::{Ddl, Read, Transaction, Write};
    let verb = w.first().copied().unwrap_or_default();
    let second = w.get(1).copied().unwrap_or_default();
    let has = |k: &str| w.contains(&k);
    // The statement inside `WITH …` or `EXPLAIN ANALYZE …`.
    let inner = || {
        w.iter().skip(1).position(|x| {
            matches!(
                *x,
                "SELECT"
                    | "INSERT"
                    | "UPDATE"
                    | "DELETE"
                    | "REPLACE"
                    | "MERGE"
                    | "TABLE"
                    | "VALUES"
            )
        })
    };
    let st = |kind, implicit| statement(kind, verb, None, implicit);
    match verb {
        "SELECT" | "VALUES" | "TABLE" => {
            let into_file = w
                .windows(2)
                .any(|p| p[0] == "INTO" && matches!(p[1], "OUTFILE" | "DUMPFILE"));
            st(if into_file { Write } else { Read }, false)
        }
        "WITH" => match inner() {
            Some(i) => of(&w[i + 1..], mysql),
            None => st(Write, false),
        },
        // MySQL's EXPLAIN ANALYZE runs the statement.
        "EXPLAIN" | "DESCRIBE" | "DESC" if has("ANALYZE") => match inner() {
            Some(i) => of(&w[i + 1..], mysql),
            None => st(Read, false),
        },
        "SHOW" | "EXPLAIN" | "DESCRIBE" | "DESC" | "HELP" | "USE" | "DO" | "PREPARE"
        | "DEALLOCATE" | "CHECKSUM" => st(Read, false),
        "ANALYZE" if mysql => match second {
            "TABLE" | "TABLES" | "NO_WRITE_TO_BINLOG" | "LOCAL" => st(Read, true),
            // MariaDB's ANALYZE runs the statement.
            _ => of(&w[1..], mysql),
        },
        "CHECK" if mysql => st(Read, true),
        "UPDATE" => statement(
            Write,
            verb,
            (!has("WHERE")).then_some(Danger::UpdateWithoutWhere),
            false,
        ),
        "DELETE" => statement(
            Write,
            verb,
            (!has("WHERE")).then_some(Danger::DeleteWithoutWhere),
            false,
        ),
        "TRUNCATE" => statement(Ddl, verb, Some(Danger::Truncate), true),
        "DROP" => statement(
            Ddl,
            verb,
            Some(Danger::Drop),
            !(mysql && second == "TEMPORARY"),
        ),
        "CREATE" => st(Ddl, !(mysql && second == "TEMPORARY")),
        "ALTER" if !mysql && second == "SESSION" => st(Read, false),
        "ALTER" if !mysql && second == "SYSTEM" => st(Ddl, false),
        "LOAD" if second == "DATA" || second == "XML" => st(Write, false),
        "ALTER" | "RENAME" | "GRANT" | "REVOKE" | "COMMENT" | "AUDIT" | "NOAUDIT" | "FLASHBACK"
        | "PURGE" | "ANALYZE" | "OPTIMIZE" | "REPAIR" | "FLUSH" | "RESET" | "INSTALL"
        | "UNINSTALL" | "CACHE" | "LOAD" | "CHANGE" | "STOP" | "IMPORT" => st(Ddl, true),
        "SET" if mysql => set(w),
        "SET" if second == "TRANSACTION" => {
            let read_write = w.windows(2).any(|p| p == ["READ", "WRITE"]);
            st(if read_write { Write } else { Transaction }, false)
        }
        "SET" => st(Read, false),
        // MariaDB's `BEGIN NOT ATOMIC` opens a compound statement.
        "BEGIN" if mysql && second != "NOT" => st(Transaction, true),
        "START" if mysql && second == "TRANSACTION" => st(Transaction, true),
        "START" if mysql => st(Ddl, true),
        "LOCK" | "UNLOCK" => st(Transaction, mysql),
        "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" | "XA" => st(Transaction, false),
        // INSERT, REPLACE, MERGE, CALL, PL/SQL blocks, and anything unknown.
        _ => st(Write, false),
    }
}

/// MySQL `SET`: session settings are reads, except the ones that lift read-only mode,
/// end transactions or change the server.
fn set(w: &[&str]) -> Statement {
    let names: Vec<&str> = w.iter().map(|x| x.trim_start_matches('@')).collect();
    let has = |k: &str| names.contains(&k);
    let verb = "SET";
    if has("AUTOCOMMIT") {
        return statement(Kind::Transaction, verb, None, true);
    }
    if names.get(1) == Some(&"PASSWORD") {
        return statement(Kind::Ddl, verb, None, true);
    }
    if has("TRANSACTION_READ_ONLY")
        || has("TX_READ_ONLY")
        || names.windows(2).any(|p| p == ["READ", "WRITE"])
    {
        return statement(Kind::Write, verb, None, false);
    }
    if has("GLOBAL") || has("PERSIST") || has("PERSIST_ONLY") {
        return statement(Kind::Ddl, verb, None, false);
    }
    if names.get(1) == Some(&"TRANSACTION") {
        return statement(Kind::Transaction, verb, None, false);
    }
    statement(Kind::Read, verb, None, false)
}

/// The words outside strings, quoted names and comments, upper case, each with its
/// parenthesis depth. MySQL `/*! … */` comments are code; `/*+ … */` hints are not.
fn words(text: &str, mysql: bool) -> Vec<(String, u32)> {
    let b = text.as_bytes();
    let ident = |c: u8| {
        c.is_ascii_alphanumeric()
            || matches!(c, b'_' | b'$' | b'@')
            || c >= 0x80
            || !mysql && c == b'#'
    };
    let mut out = Vec::new();
    let mut depth = 0u32;
    let mut executable = false;
    let mut i = 0;
    while i < b.len() {
        let next = b.get(i + 1).copied();
        i = match b[i] {
            b'(' => {
                depth += 1;
                i + 1
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i + 1
            }
            b'\'' | b'"' => quoted_end(b, i, mysql),
            b'`' if mysql => quoted_end(b, i, false),
            b'#' if mysql => line_end(b, i),
            b'-' if next == Some(b'-')
                && (!mysql
                    || b.get(i + 2)
                        .is_none_or(|c| c.is_ascii_whitespace() || c.is_ascii_control())) =>
            {
                line_end(b, i)
            }
            b'/' if next == Some(b'*') => {
                if mysql && b.get(i + 2) == Some(&b'!') {
                    executable = true;
                    let digits = b[i + 3..].iter().take_while(|c| c.is_ascii_digit()).count();
                    i + 3 + digits
                } else {
                    block_comment_end(b, i)
                }
            }
            b'*' if executable && next == Some(b'/') => {
                executable = false;
                i + 2
            }
            c if ident(c) => {
                let end = b[i..]
                    .iter()
                    .position(|&c| !ident(c))
                    .map_or(b.len(), |n| i + n);
                let word = text[i..end].to_ascii_uppercase();
                // Oracle `q'[…]'` and `nq'[…]'` strings.
                if !mysql && b.get(end) == Some(&b'\'') && matches!(word.as_str(), "Q" | "NQ") {
                    q_quote_end(text, end - 1)
                } else {
                    out.push((word, depth));
                    end
                }
            }
            _ => i + 1,
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn my(text: &str) -> Statement {
        classify(text, Engine::Mysql)
    }

    fn ora(text: &str) -> Statement {
        classify(text, Engine::Oracle)
    }

    fn kind(text: &str) -> Kind {
        my(text).kind
    }

    #[test]
    fn reads() {
        for sql in [
            "SELECT * FROM t",
            "select 1",
            "SELECT * FROM t WHERE id = 1 FOR UPDATE",
            "SELECT * FROM t LOCK IN SHARE MODE",
            "SELECT a INTO @x FROM t",
            "SHOW TABLES",
            "DESCRIBE t",
            "EXPLAIN DELETE FROM t",
            "EXPLAIN FORMAT=JSON UPDATE t SET a = 1",
            "USE shop",
            "SET @x = 1",
            "SET NAMES utf8mb4",
            "SET SESSION sql_mode = ''",
            "VALUES ROW(1, 2)",
            "TABLE t",
            "(SELECT 1) UNION (SELECT 2)",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 9) SELECT * FROM n",
            "/*!40101 SELECT 1 */",
            "",
            "-- only a comment",
        ] {
            assert_eq!(kind(sql), Kind::Read, "{sql}");
        }
        assert_eq!(
            ora("SELECT * FROM t WHERE id = 1 FOR UPDATE").kind,
            Kind::Read
        );
        assert_eq!(
            ora("ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY'").kind,
            Kind::Read
        );
    }

    #[test]
    fn writes() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "INSERT INTO t SELECT * FROM u",
            "REPLACE INTO t VALUES (1)",
            "REPLACE INTO t SELECT * FROM u WHERE 1",
            "CALL refund(1)",
            "UPDATE t SET a = 1 WHERE id = 2",
            "DELETE FROM t WHERE id = 2",
            "LOAD DATA INFILE 'x' INTO TABLE t",
            "SELECT * FROM t INTO OUTFILE '/tmp/x'",
            "WITH x AS (SELECT id FROM u) DELETE FROM t WHERE id IN (SELECT id FROM x)",
            "WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x",
            "EXPLAIN ANALYZE DELETE FROM t WHERE id = 1",
            "ANALYZE DELETE FROM t WHERE id = 1",
            "BEGIN NOT ATOMIC SELECT 1; END",
            "SET SESSION TRANSACTION READ WRITE",
            "SET transaction_read_only = OFF",
            "SET @@SESSION.transaction_read_only = 0",
            "HANDLER t OPEN",
            "FROBNICATE everything",
            "(UPDATE t SET a = 1)",
        ] {
            assert_eq!(kind(sql), Kind::Write, "{sql}");
        }
        for sql in [
            "BEGIN UPDATE t SET a = 1; END;",
            "DECLARE x NUMBER; BEGIN NULL; END;",
            "MERGE INTO t USING u ON (t.id = u.id) WHEN MATCHED THEN UPDATE SET t.a = u.a",
            "SET TRANSACTION READ WRITE",
            "EXEC p",
        ] {
            assert_eq!(ora(sql).kind, Kind::Write, "{sql}");
        }
    }

    #[test]
    fn ddl_and_transaction_control() {
        for sql in [
            "CREATE TABLE t (a INT)",
            "ALTER TABLE t ADD b INT",
            "DROP TABLE t",
            "TRUNCATE TABLE t",
            "RENAME TABLE a TO b",
            "GRANT SELECT ON *.* TO u",
            "SET GLOBAL max_connections = 10",
            "SET PERSIST max_connections = 10",
            "SET @@global.max_connections = 10",
            "SET PASSWORD = 'x'",
            "OPTIMIZE TABLE t",
            "FLUSH PRIVILEGES",
        ] {
            assert_eq!(kind(sql), Kind::Ddl, "{sql}");
        }
        for sql in [
            "COMMIT",
            "ROLLBACK",
            "ROLLBACK TO SAVEPOINT a",
            "SAVEPOINT a",
            "START TRANSACTION",
            "BEGIN",
            "BEGIN WORK",
            "LOCK TABLES t WRITE",
            "UNLOCK TABLES",
            "SET autocommit = 0",
            "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        ] {
            assert_eq!(kind(sql), Kind::Transaction, "{sql}");
        }
        assert_eq!(ora("SET TRANSACTION READ ONLY").kind, Kind::Transaction);
        assert_eq!(
            ora("LOCK TABLE t IN EXCLUSIVE MODE").kind,
            Kind::Transaction
        );
        assert_eq!(ora("ALTER SYSTEM SET processes = 10").kind, Kind::Ddl);
    }

    #[test]
    fn update_and_delete_without_where_are_dangerous() {
        let d = |sql: &str| my(sql).danger;
        assert_eq!(d("UPDATE t SET a = 1"), Some(Danger::UpdateWithoutWhere));
        assert_eq!(d("delete from t"), Some(Danger::DeleteWithoutWhere));
        assert_eq!(
            d("DELETE FROM t LIMIT 10"),
            Some(Danger::DeleteWithoutWhere)
        );
        assert_eq!(d("UPDATE t SET a = 1 WHERE id = 1"), None);
        // Multi-table forms.
        assert_eq!(
            d("UPDATE a JOIN b ON a.id = b.id SET a.x = b.x"),
            Some(Danger::UpdateWithoutWhere)
        );
        assert_eq!(
            d("UPDATE a JOIN b ON a.id = b.id SET a.x = b.x WHERE b.y = 2"),
            None
        );
        assert_eq!(
            d("DELETE a FROM a JOIN b ON a.id = b.id"),
            Some(Danger::DeleteWithoutWhere)
        );
        assert_eq!(
            d("DELETE a FROM a JOIN b ON a.id = b.id WHERE b.y = 2"),
            None
        );
        // A WHERE in a subquery, CTE, string or comment is not the statement's own.
        assert_eq!(
            d("UPDATE t SET a = (SELECT max(b) FROM u WHERE u.id = 1)"),
            Some(Danger::UpdateWithoutWhere)
        );
        assert_eq!(
            d("WITH x AS (SELECT id FROM u WHERE y = 1) DELETE FROM t"),
            Some(Danger::DeleteWithoutWhere)
        );
        assert_eq!(
            d("UPDATE t SET note = 'where id = 1'"),
            Some(Danger::UpdateWithoutWhere)
        );
        assert_eq!(
            d("UPDATE t SET a = 1 -- WHERE id = 1"),
            Some(Danger::UpdateWithoutWhere)
        );
        assert_eq!(
            d("UPDATE t SET a = 1 # WHERE id = 1"),
            Some(Danger::UpdateWithoutWhere)
        );
        assert_eq!(
            d("UPDATE t SET a = 1 /* WHERE id = 1 */"),
            Some(Danger::UpdateWithoutWhere)
        );
        assert_eq!(
            d("UPDATE t SET `where` = 1"),
            Some(Danger::UpdateWithoutWhere)
        );
        assert_eq!(
            d("UPDATE t SET @where = 1"),
            Some(Danger::UpdateWithoutWhere)
        );
        // Executable comments are code.
        assert_eq!(d("UPDATE t SET a = 1 /*!50000 WHERE id = 1 */"), None);
        assert_eq!(
            ora("UPDATE t SET a = q'[ where ]'").danger,
            Some(Danger::UpdateWithoutWhere)
        );
        assert_eq!(ora("DELETE t").danger, Some(Danger::DeleteWithoutWhere));
        assert_eq!(ora("DELETE FROM t WHERE ROWNUM < 2").danger, None);
    }

    #[test]
    fn truncate_and_drop_are_dangerous() {
        assert_eq!(my("TRUNCATE t").danger, Some(Danger::Truncate));
        assert_eq!(my("DROP TABLE IF EXISTS t").danger, Some(Danger::Drop));
        assert_eq!(my("drop database shop").danger, Some(Danger::Drop));
        assert_eq!(ora("DROP USER app CASCADE").danger, Some(Danger::Drop));
        assert_eq!(my("CREATE TABLE t (a INT)").danger, None);
        assert_eq!(my("SELECT 'DROP TABLE t'").danger, None);
        assert_eq!(my("INSERT INTO t SELECT * FROM u").danger, None);
    }

    #[test]
    fn implicit_commits_follow_mysqls_list() {
        let ic = |sql: &str| my(sql).implicit_commit;
        for sql in [
            "CREATE TABLE t (a INT)",
            "CREATE INDEX i ON t (a)",
            "ALTER TABLE t ADD b INT",
            "DROP TABLE t",
            "TRUNCATE t",
            "RENAME TABLE a TO b",
            "GRANT SELECT ON *.* TO u",
            "REVOKE SELECT ON *.* FROM u",
            "CREATE USER u",
            "SET PASSWORD = 'x'",
            "LOCK TABLES t WRITE",
            "UNLOCK TABLES",
            "START TRANSACTION",
            "BEGIN",
            "SET autocommit = 1",
            "ANALYZE TABLE t",
            "CHECK TABLE t",
            "OPTIMIZE TABLE t",
            "REPAIR TABLE t",
            "FLUSH TABLES",
            "LOAD INDEX INTO CACHE t",
            "CACHE INDEX t IN hot",
            "RESET MASTER",
            "START REPLICA",
            "STOP REPLICA",
            "CHANGE REPLICATION SOURCE TO SOURCE_HOST = 'h'",
            "INSTALL PLUGIN p SONAME 'p.so'",
            "/*!40101 CREATE TABLE t (a INT) */",
        ] {
            assert!(ic(sql), "{sql}");
        }
        for sql in [
            "CREATE TEMPORARY TABLE t (a INT)",
            "DROP TEMPORARY TABLE t",
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1",
            "SELECT 1",
            "COMMIT",
            "ROLLBACK",
            "SAVEPOINT a",
            "LOAD DATA INFILE 'x' INTO TABLE t",
            "SET @x = 1",
            "-- CREATE TABLE t (a INT)\nSELECT 1",
        ] {
            assert!(!ic(sql), "{sql}");
        }
        assert!(ora("CREATE TABLE t (a NUMBER)").implicit_commit);
        assert!(ora("TRUNCATE TABLE t").implicit_commit);
        assert!(!ora("ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY'").implicit_commit);
        assert!(!ora("BEGIN NULL; END;").implicit_commit);
        assert_eq!(my("create table t (a int)").verb, "CREATE");
    }

    #[test]
    fn writes_change_and_reads_do_not() {
        assert!(my("INSERT INTO t VALUES (1)").changes());
        assert!(my("DROP TABLE t").changes());
        assert!(my("CALL p()").changes());
        assert!(!my("SELECT 1").changes());
        assert!(!my("COMMIT").changes());
        assert!(!my("LOCK TABLES t READ").changes());
    }
}
