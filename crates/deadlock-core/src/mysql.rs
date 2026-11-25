//! MySQL and MariaDB on `mysql_async`. One driver; the flavor comes from `VERSION()`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{NaiveDate, NaiveDateTime, TimeDelta};
use mysql_async::consts::{ColumnFlags, ColumnType, StatusFlags};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, OptsBuilder, SslOpts};
use tokio::sync::{Mutex, oneshot};

use crate::classify::{Kind, Statement, classify};
use crate::connection::{ConnectionConfig, Encryption, Engine};
use crate::diagram::Diagram;
use crate::edit::{Change, ChangeKind, ColumnInfo, TableInfo, TableRef, pick_key, quote_ident};
use crate::plan::{self, Plan};
use crate::structure::{
    Check, ForeignKey, Generated, Index, IndexPart, StructureColumn, TableFacts, TableStructure,
};
use std::collections::{HashMap, HashSet};

use crate::driver::{
    BoxFuture, Capabilities, Column, Driver, DriverError, NodeKind, ObjectNode, ObjectPath, Result,
    ResultStream, Value, page_size,
};

mod config;
mod objects;
mod search;
mod sessions;

/// The `program_name` connection attribute: marks Deadlock's sessions to the server.
/// mysql_async drops it on TLS connections, so [`crate::sessions::Mark`] marks them in
/// this process.
const PROGRAM_NAME: &str = "Deadlock";

/// MySQL's binary collation: byte strings, not text.
const BINARY_CHARSET: u16 = 63;

pub struct Mysql {
    /// The SSH tunnel the connection goes through: closes with the driver.
    pub(crate) tunnel: Option<crate::ssh::Tunnel>,
    conn: Arc<Mutex<Conn>>,
    /// Kept for the side connection that cancels.
    opts: Opts,
    id: u32,
    version: String,
    /// Writes are refused before sending; the session is also read-only on the server.
    read_only: bool,
    mariadb: bool,
    /// The open transaction ran more than reads. With autocommit off the server opens
    /// one on any read too, and that one has nothing to commit.
    wrote: Arc<AtomicBool>,
}

fn opts(config: &ConnectionConfig, password: &str, ssl: Option<SslOpts>) -> Opts {
    let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_owned());
    OptsBuilder::default()
        .ip_or_hostname(config.host.clone())
        .tcp_port(config.port)
        .user(non_empty(&config.user))
        .pass(non_empty(password))
        .db_name(non_empty(&config.database))
        .ssl_opts(ssl)
        .connect_attribute("program_name", PROGRAM_NAME)
        .into()
}

pub async fn connect(config: &ConnectionConfig, password: &str) -> Result<Mysql> {
    let lax = SslOpts::default().with_danger_accept_invalid_certs(true);
    let (conn, opts) = match config.encryption {
        Encryption::Off => open(opts(config, password, None)).await?,
        Encryption::Require => open(opts(config, password, Some(lax))).await?,
        Encryption::VerifyCertificate => {
            open(opts(config, password, Some(SslOpts::default()))).await?
        }
        // shortcut: a TLS failure and a sign-in failure look alike here, so any error
        // retries in plain text once.
        Encryption::Prefer => match open(opts(config, password, Some(lax))).await {
            Ok(c) => c,
            Err(_) => open(opts(config, password, None)).await?,
        },
    };
    let mut conn = conn;
    let raw: Option<String> = conn.query_first("SELECT VERSION()").await?;
    let raw = raw.unwrap_or_default();
    if config.read_only {
        conn.query_drop("SET SESSION TRANSACTION READ ONLY").await?;
    }
    Ok(Mysql {
        tunnel: None,
        read_only: config.read_only,
        mariadb: raw.contains("MariaDB"),
        wrote: Arc::default(),
        id: conn.id(),
        conn: Arc::new(Mutex::new(conn)),
        opts,
        version: version_label(&raw),
    })
}

async fn open(opts: Opts) -> Result<(Conn, Opts)> {
    Ok((Conn::new(opts.clone()).await?, opts))
}

/// `8.4.3` → `MySQL 8.4.3`; `11.8.2-MariaDB-ubu2404` → `MariaDB 11.8.2`.
fn version_label(raw: &str) -> String {
    let number = raw.split('-').next().unwrap_or(raw);
    let flavor = if raw.contains("MariaDB") {
        "MariaDB"
    } else {
        "MySQL"
    };
    format!("{flavor} {number}")
}

impl Driver for Mysql {
    fn capabilities(&self) -> Capabilities {
        Capabilities::SQL
    }

    fn session_id(&self) -> Option<u64> {
        Some(u64::from(self.id))
    }

    fn server_version(&self) -> &str {
        &self.version
    }

    fn execute<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<ResultStream>> {
        let conn = Arc::clone(&self.conn);
        let statement = classify(text, Engine::Mysql);
        let wrote = Arc::clone(&self.wrote);
        let text = text.to_owned();
        Box::pin(async move {
            if self.read_only && statement.changes() {
                return Err(DriverError::ReadOnly(statement.verb));
            }
            let writes = statement.kind != Kind::Read;
            // DDL only: START TRANSACTION also commits, but opens one.
            let commits = statement.implicit_commit && statement.kind == Kind::Ddl;
            let (head_tx, head) = oneshot::channel();
            tokio::spawn(run(conn, text, writes, commits, wrote, head_tx));
            head.await.map_err(|_| DriverError::Gone)?
        })
    }

    fn cancel(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let mut side = Conn::new(self.opts.clone()).await?;
            side.query_drop(format!("KILL QUERY {}", self.id)).await?;
            side.disconnect().await?;
            Ok(())
        })
    }

    fn classify<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Statement>> {
        Box::pin(async move { Ok(classify(text, Engine::Mysql)) })
    }

    fn set_auto_commit(&self, on: bool) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let sql = if on {
                "SET autocommit = 1"
            } else {
                "SET autocommit = 0"
            };
            Ok(self.conn.lock().await.query_drop(sql).await?)
        })
    }

    fn end_transaction(&self, commit: bool) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let sql = if commit { "COMMIT" } else { "ROLLBACK" };
            Ok(self.conn.lock().await.query_drop(sql).await?)
        })
    }

    /// From the status flags of a ping's OK packet: fresh even after an error, and the
    /// ping fails when the connection is gone. A transaction that only read (autocommit
    /// off opens one on the first SELECT) does not count.
    fn transaction_open(&self) -> BoxFuture<'_, Result<bool>> {
        Box::pin(async move {
            let mut conn = self.conn.lock().await;
            conn.ping().await?;
            Ok(track(&conn, &self.wrote, false))
        })
    }

    fn table_info<'a>(&'a self, t: &'a TableRef) -> BoxFuture<'a, Result<TableInfo>> {
        Box::pin(async move {
            let mut conn = self.conn.lock().await;
            let args = (&t.database, &t.table);
            let kind: Option<(String, Option<u64>)> = conn
                .exec_first(
                    "SELECT TABLE_TYPE, TABLE_ROWS FROM information_schema.TABLES \
                     WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
                    args,
                )
                .await?;
            let Some((table_type, estimated_rows)) = kind else {
                return Err(DriverError::Invalid(format!(
                    "{}.{} does not exist",
                    t.database, t.table
                )));
            };
            let rows: Vec<(String, String, String, String, String)> = conn
                .exec(
                    "SELECT COLUMN_NAME, DATA_TYPE, COLUMN_TYPE, IS_NULLABLE, EXTRA \
                     FROM information_schema.COLUMNS \
                     WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
                    args,
                )
                .await?;
            let indexes: Vec<(String, i64, Option<String>)> = conn
                .exec(
                    "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME FROM information_schema.STATISTICS \
                     WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY INDEX_NAME, SEQ_IN_INDEX",
                    args,
                )
                .await?;
            let mode: Option<String> = conn.query_first("SELECT @@SESSION.sql_mode").await?;
            drop(conn);
            let columns: Vec<ColumnInfo> = rows
                .into_iter()
                .map(|(name, data_type, column_type, nullable, extra)| {
                    let extra = extra.to_ascii_uppercase();
                    ColumnInfo {
                        name,
                        data_type: data_type.to_ascii_lowercase(),
                        column_type: column_type.to_ascii_lowercase(),
                        nullable: nullable == "YES",
                        auto_increment: extra.contains("AUTO_INCREMENT"),
                        // `DEFAULT_GENERATED` is an expression default, not a generated column.
                        generated: ["VIRTUAL", "STORED", "PERSISTENT"]
                            .iter()
                            .any(|g| extra.contains(g)),
                        pseudo: false,
                    }
                })
                .collect();
            let indexes: Vec<_> = indexes
                .into_iter()
                .map(|(name, non_unique, col)| (name, non_unique != 0, col))
                .collect();
            let view = table_type.ends_with("VIEW");
            Ok(TableInfo {
                key: if view {
                    None
                } else {
                    pick_key(&columns, &indexes)
                },
                columns,
                view,
                estimated_rows,
                backslash_escapes: crate::edit::backslash_escapes(&mode.unwrap_or_default()),
                oracle: false,
                read_only: None,
            })
        })
    }

    fn structure<'a>(&'a self, t: &'a TableRef) -> BoxFuture<'a, Result<TableStructure>> {
        Box::pin(async move {
            let mut conn = self.conn.lock().await;
            let args = (&t.database, &t.table);
            let table: Option<TableRow> = conn
                .exec_first(
                    "SELECT TABLE_TYPE, ENGINE, TABLE_COLLATION, TABLE_ROWS, DATA_LENGTH, \
                     INDEX_LENGTH, TABLE_COMMENT FROM information_schema.TABLES \
                     WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
                    args,
                )
                .await?;
            let Some(table) = table else {
                return Err(DriverError::Invalid(format!(
                    "{}.{} does not exist",
                    t.database, t.table
                )));
            };
            // MariaDB has no SRS_ID.
            let srid = if self.mariadb { "NULL" } else { "SRS_ID" };
            let columns: Vec<ColumnRow> = conn
                .exec(
                    format!(
                        "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, EXTRA, \
                         GENERATION_EXPRESSION, DATA_TYPE, COLUMN_COMMENT, CHARACTER_SET_NAME, \
                         COLLATION_NAME, {srid} FROM information_schema.COLUMNS \
                         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION"
                    ),
                    args,
                )
                .await?;
            // MariaDB has no functional key parts, and no EXPRESSION column.
            let expression = if self.mariadb { "NULL" } else { "EXPRESSION" };
            let indexes: Vec<IndexRow> = conn
                .exec(
                    format!(
                        "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME, {expression}, COLLATION, \
                         SUB_PART, INDEX_TYPE FROM information_schema.STATISTICS \
                         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                         ORDER BY INDEX_NAME <> 'PRIMARY', INDEX_NAME, SEQ_IN_INDEX"
                    ),
                    args,
                )
                .await?;
            let keys: Vec<KeyRow> = conn
                .exec(
                    "SELECT k.CONSTRAINT_NAME, k.TABLE_SCHEMA, k.TABLE_NAME, k.COLUMN_NAME, \
                     k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME, k.REFERENCED_COLUMN_NAME, \
                     r.UPDATE_RULE, r.DELETE_RULE \
                     FROM information_schema.KEY_COLUMN_USAGE k \
                     JOIN information_schema.REFERENTIAL_CONSTRAINTS r \
                       ON r.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA \
                      AND r.CONSTRAINT_NAME = k.CONSTRAINT_NAME AND r.TABLE_NAME = k.TABLE_NAME \
                     WHERE k.REFERENCED_TABLE_NAME IS NOT NULL \
                       AND (k.TABLE_SCHEMA = ? AND k.TABLE_NAME = ? \
                         OR k.REFERENCED_TABLE_SCHEMA = ? AND k.REFERENCED_TABLE_NAME = ?) \
                     ORDER BY k.TABLE_SCHEMA, k.TABLE_NAME, k.CONSTRAINT_NAME, k.ORDINAL_POSITION",
                    (&t.database, &t.table, &t.database, &t.table),
                )
                .await?;
            let view = table.0.ends_with("VIEW");
            let what = if view { "VIEW" } else { "TABLE" };
            let name = format!("{}.{}", quote_ident(&t.database), quote_ident(&t.table));
            let row: Option<mysql_async::Row> = conn
                .query_first(format!("SHOW CREATE {what} {name}"))
                .await?;
            let ddl = row
                .and_then(|mut r| r.take_opt::<String, _>(1))
                .and_then(std::result::Result::ok)
                .unwrap_or_default();
            let mode: Option<String> = conn.query_first("SELECT @@SESSION.sql_mode").await?;
            // MariaDB's view has the table and no enforced flag; MySQL's has neither
            // table nor flag, which TABLE_CONSTRAINTS has. Servers before MySQL 8.0.16 /
            // MariaDB 10.2.22 have no such view: `None`.
            let checks: Option<Vec<(String, String, Option<String>)>> = if view {
                Some(Vec::new())
            } else if self.mariadb {
                // LEVEL is 10.5.10+; older servers get it from the DDL.
                let level = "SELECT CONSTRAINT_NAME, CHECK_CLAUSE, LEVEL \
                     FROM information_schema.CHECK_CONSTRAINTS \
                     WHERE CONSTRAINT_SCHEMA = ? AND TABLE_NAME = ? ORDER BY CONSTRAINT_NAME";
                match conn.exec(level, args).await {
                    Ok(rows) => Some(rows),
                    Err(_) => conn.exec(level.replace("LEVEL", "NULL"), args).await.ok(),
                }
            } else {
                conn.exec(
                    "SELECT tc.CONSTRAINT_NAME, cc.CHECK_CLAUSE, tc.ENFORCED \
                     FROM information_schema.TABLE_CONSTRAINTS tc \
                     JOIN information_schema.CHECK_CONSTRAINTS cc \
                       ON cc.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA \
                      AND cc.CONSTRAINT_NAME = tc.CONSTRAINT_NAME \
                     WHERE tc.TABLE_SCHEMA = ? AND tc.TABLE_NAME = ? \
                       AND tc.CONSTRAINT_TYPE = 'CHECK' ORDER BY tc.CONSTRAINT_NAME",
                    args,
                )
                .await
                .ok()
            };
            // An InnoDB estimate can be 0 on a table with rows.
            let rows_exist = if table.3 == Some(0) && !view {
                conn.query_first::<u8, _>(format!("SELECT 1 FROM {name} LIMIT 1"))
                    .await
                    .map(|r| r.is_some())
                    .ok()
            } else {
                None
            };
            drop(conn);
            let backslash = crate::edit::backslash_escapes(&mode.unwrap_or_default());
            let flavor = (self.mariadb, backslash);
            let mut s = structure_from_rows(t, flavor, table, columns, indexes, keys);
            if rows_exist != Some(false) && s.facts.rows == Some(0) {
                // Some rows, or no answer: the count is not known.
                s.facts.rows = None;
            }
            s.ddl = ddl;
            s.checks_unsupported = checks.is_none();
            for (name, condition, level) in checks.unwrap_or_default() {
                let column = if self.mariadb {
                    column_check(&s.ddl, &s.columns, &name, &condition, level.as_deref())
                } else {
                    None
                };
                match column {
                    Some(i) => s.columns[i].check = Some(condition),
                    None => s.checks.push(Check {
                        name,
                        condition,
                        // MariaDB enforces every check it lists.
                        enabled: self.mariadb || level.as_deref() == Some("YES"),
                    }),
                }
            }
            Ok(s)
        })
    }

    /// Estimated: `EXPLAIN FORMAT=JSON` on both flavors. Measured: `EXPLAIN ANALYZE`
    /// (the tree) on MySQL, `ANALYZE FORMAT=JSON` on MariaDB; both run the statement.
    fn explain<'a>(&'a self, statement: &'a str, analyze: bool) -> BoxFuture<'a, Result<Plan>> {
        Box::pin(async move {
            if analyze && self.read_only {
                let s = classify(statement, Engine::Mysql);
                if s.changes() {
                    return Err(DriverError::ReadOnly(s.verb));
                }
            }
            let sql = match (analyze, self.mariadb) {
                (false, _) => format!("EXPLAIN FORMAT=JSON {statement}"),
                (true, false) => format!("EXPLAIN ANALYZE {statement}"),
                (true, true) => format!("ANALYZE FORMAT=JSON {statement}"),
            };
            let mut conn = self.conn.lock().await;
            let database: Option<Option<String>> = conn.query_first("SELECT DATABASE()").await?;
            let raw: Option<String> = conn.query_first(sql).await?;
            drop(conn);
            let raw = raw.ok_or_else(|| DriverError::Invalid("the server sent no plan".into()))?;
            let plan = if analyze && !self.mariadb {
                plan::mysql_tree(&raw)
            } else {
                plan::json(&raw, analyze)
            };
            let mut plan = plan.map_err(DriverError::Invalid)?;
            plan.database = database.flatten();
            Ok(plan)
        })
    }

    fn apply<'a>(&'a self, changes: &'a [Change], auto: bool) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if self.read_only {
                return Err(DriverError::ReadOnly("a submit".into()));
            }
            let (begin, done, undo) = if auto {
                ("START TRANSACTION", "COMMIT", "ROLLBACK")
            } else {
                (
                    "SAVEPOINT deadlock_submit",
                    "RELEASE SAVEPOINT deadlock_submit",
                    "ROLLBACK TO SAVEPOINT deadlock_submit",
                )
            };
            let mut conn = self.conn.lock().await;
            conn.query_drop(begin).await?;
            // Manual mode: the changes stay in the session's transaction.
            self.wrote.store(!auto, Ordering::Relaxed);
            for (i, c) in changes.iter().enumerate() {
                let outcome = match conn.query_drop(c.sql.as_str()).await {
                    Ok(()) => {
                        let rows = touched(c.kind, &conn.info(), conn.affected_rows());
                        if rows == 1 {
                            Ok(())
                        } else {
                            Err(DriverError::Invalid(format!(
                                "change {} of {} touched {rows} rows instead of 1: someone else \
                                 may have changed or removed the row. Nothing was submitted.",
                                i + 1,
                                changes.len()
                            )))
                        }
                    }
                    Err(e) => Err(DriverError::Invalid(format!(
                        "change {} of {} failed: {e}. Nothing was submitted.",
                        i + 1,
                        changes.len()
                    ))),
                };
                if let Err(e) = outcome {
                    if let Err(u) = conn.query_drop(undo).await {
                        return Err(DriverError::Invalid(format!(
                            "{e} Undoing it failed too: {u}"
                        )));
                    }
                    return Err(e);
                }
            }
            if let Err(e) = conn.query_drop(done).await {
                let _ = conn.query_drop(undo).await;
                return Err(e.into());
            }
            Ok(())
        })
    }

    fn config(&self) -> BoxFuture<'_, Result<crate::config::ServerConfig>> {
        Box::pin(async move { config::read(&mut *self.conn.lock().await, self.mariadb).await })
    }

    fn apply_config<'a>(
        &'a self,
        cfg: &'a crate::config::ServerConfig,
        changes: &'a [crate::config::ConfigChange],
    ) -> BoxFuture<'a, Result<crate::config::Applied>> {
        Box::pin(async move {
            if self.read_only {
                return Err(DriverError::Invalid(
                    "not applied: the connection is read-only".into(),
                ));
            }
            Ok(config::apply(&mut *self.conn.lock().await, cfg, changes).await)
        })
    }

    fn sessions(&self) -> BoxFuture<'_, Result<crate::sessions::Snapshot>> {
        Box::pin(async move {
            let mut conn = self.conn.lock().await;
            sessions::snapshot(&mut conn, self.mariadb, self.id).await
        })
    }

    fn waiting_sessions(&self) -> BoxFuture<'_, Result<usize>> {
        Box::pin(async move {
            let mut conn = self.conn.lock().await;
            sessions::waiting(&mut conn, self.mariadb).await
        })
    }

    fn cancel_statement(&self, id: crate::sessions::SessionId) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            Ok(self
                .conn
                .lock()
                .await
                .query_drop(format!("KILL QUERY {id}"))
                .await?)
        })
    }

    fn end_session(&self, id: crate::sessions::SessionId) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if id == u64::from(self.id) {
                return Err(DriverError::Invalid(
                    "not ended: this is the session the screen itself uses".into(),
                ));
            }
            Ok(self
                .conn
                .lock()
                .await
                .query_drop(format!("KILL {id}"))
                .await?)
        })
    }

    fn last_deadlock(&self) -> BoxFuture<'_, Result<Option<String>>> {
        Box::pin(async move {
            let row: Option<(String, String, String)> = self
                .conn
                .lock()
                .await
                .query_first("SHOW ENGINE INNODB STATUS")
                .await?;
            Ok(row.and_then(|(_, _, status)| {
                crate::sessions::deadlock_section(&status).map(str::to_owned)
            }))
        })
    }

    /// The tree's bulk load of the database, plus its foreign keys column by column.
    fn collations(&self) -> BoxFuture<'_, Result<Vec<(String, String)>>> {
        Box::pin(async move {
            let mut conn = self.conn.lock().await;
            // MariaDB 10.10+ names its UCA 14.0 collations per character set only here
            // (`utf8mb4_uca1400_ai_ci`); COLLATIONS has them without one.
            let full = if self.mariadb {
                conn.query(
                    "SELECT FULL_COLLATION_NAME, CHARACTER_SET_NAME \
                     FROM information_schema.COLLATION_CHARACTER_SET_APPLICABILITY",
                )
                .await
                .ok()
            } else {
                None
            };
            let mut list: Vec<(String, String)> = match full {
                Some(list) => list,
                None => {
                    conn.query(
                        "SELECT COLLATION_NAME, CHARACTER_SET_NAME \
                         FROM information_schema.COLLATIONS WHERE CHARACTER_SET_NAME IS NOT NULL",
                    )
                    .await?
                }
            };
            // By name as Rust orders it: the server sorts in its own collation.
            list.sort();
            list.dedup();
            Ok(list)
        })
    }

    fn diagram<'a>(&'a self, schema: &'a str) -> BoxFuture<'a, Result<Diagram>> {
        Box::pin(async move {
            let groups = self.children(&ObjectPath(vec![schema.to_owned()])).await?;
            let keys: Vec<crate::diagram::KeyRow> = self
                .conn
                .lock()
                .await
                .exec(
                    "SELECT CONSTRAINT_NAME, TABLE_NAME, COLUMN_NAME, REFERENCED_TABLE_NAME, \
                     REFERENCED_COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE \
                     WHERE TABLE_SCHEMA = ? AND REFERENCED_TABLE_SCHEMA = ? \
                     ORDER BY TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION",
                    (schema, schema),
                )
                .await?;
            Ok(Diagram::from_tree(groups, keys))
        })
    }

    fn search_everywhere<'a>(
        &'a self,
        text: &'a str,
        hidden: &'a [String],
        stop: &'a AtomicBool,
    ) -> BoxFuture<'a, Result<Vec<crate::search::Hit>>> {
        Box::pin(
            async move { search::search(&mut *self.conn.lock().await, text, hidden, stop).await },
        )
    }

    fn definition<'a>(
        &'a self,
        schema: &'a str,
        name: &'a str,
        kind: &'a str,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            search::definition(&mut *self.conn.lock().await, schema, name, kind).await
        })
    }

    fn object_flavor(&self) -> Option<crate::objects::Flavor> {
        Some(if self.mariadb {
            crate::objects::Flavor::Mariadb
        } else {
            crate::objects::Flavor::Mysql
        })
    }

    fn object<'a>(
        &'a self,
        o: &'a crate::objects::ObjectRef,
    ) -> BoxFuture<'a, Result<crate::objects::ObjectDefinition>> {
        Box::pin(async move {
            objects::load(
                &mut *self.conn.lock().await,
                self.mariadb,
                self.read_only,
                o,
            )
            .await
        })
    }

    fn apply_object<'a>(
        &'a self,
        plan: &'a crate::objects::ChangePlan,
        backup: Option<&'a crate::objects::Backup>,
    ) -> BoxFuture<'a, Result<crate::objects::ApplyOutcome>> {
        Box::pin(async move {
            if self.read_only {
                return Err(DriverError::ReadOnly(plan.verb().into()));
            }
            let mut conn = self.conn.lock().await;
            if track(&conn, &self.wrote, false) {
                return Err(DriverError::Invalid(
                    "not applied: the session has an open transaction, and MySQL would commit \
                     it before the DDL. Commit or roll back first."
                        .into(),
                ));
            }
            objects::apply(&mut conn, plan, backup).await
        })
    }

    /// `[]`: the databases. `[db, …]`: the database's groups, each loaded down to the
    /// columns in six `information_schema` queries, then walked down the rest of the path.
    fn children<'a>(&'a self, node: &'a ObjectPath) -> BoxFuture<'a, Result<Vec<ObjectNode>>> {
        Box::pin(async move {
            let mut conn = self.conn.lock().await;
            let Some((db, rest)) = node.0.split_first() else {
                let names: Vec<String> = conn
                    .query(
                        "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA ORDER BY SCHEMA_NAME",
                    )
                    .await?;
                return Ok(databases(names));
            };
            let tables: Vec<(String, String)> = conn
                .exec(
                    "SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES \
                     WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME",
                    (db,),
                )
                .await?;
            let routines: Vec<(String, String)> = conn
                .exec(
                    "SELECT ROUTINE_NAME, ROUTINE_TYPE FROM information_schema.ROUTINES \
                     WHERE ROUTINE_SCHEMA = ? ORDER BY ROUTINE_NAME",
                    (db,),
                )
                .await?;
            let columns: Vec<(String, String, String, String)> = conn
                .exec(
                    "SELECT TABLE_NAME, COLUMN_NAME, COLUMN_TYPE, COLUMN_KEY \
                     FROM information_schema.COLUMNS \
                     WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME, ORDINAL_POSITION",
                    (db,),
                )
                .await?;
            let foreign: Vec<(String, String)> = conn
                .exec(
                    "SELECT DISTINCT TABLE_NAME, COLUMN_NAME \
                     FROM information_schema.KEY_COLUMN_USAGE \
                     WHERE TABLE_SCHEMA = ? AND REFERENCED_TABLE_NAME IS NOT NULL",
                    (db,),
                )
                .await?;
            let triggers: Vec<(String, String, String, String)> = conn
                .exec(
                    "SELECT TRIGGER_NAME, EVENT_OBJECT_TABLE, ACTION_TIMING, EVENT_MANIPULATION \
                     FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = ? ORDER BY TRIGGER_NAME",
                    (db,),
                )
                .await?;
            let events: Vec<(String, String, Option<String>, Option<String>)> = conn
                .exec(
                    "SELECT EVENT_NAME, EVENT_TYPE, INTERVAL_VALUE, INTERVAL_FIELD \
                     FROM information_schema.EVENTS WHERE EVENT_SCHEMA = ? ORDER BY EVENT_NAME",
                    (db,),
                )
                .await?;
            drop(conn);
            let mut nodes = database_children(
                tables,
                routines,
                columns,
                foreign,
                (triggers, events),
                self.mariadb,
            );
            for name in rest {
                nodes = nodes
                    .into_iter()
                    .find(|n| &n.name == name)
                    .and_then(|n| n.children)
                    .ok_or_else(|| DriverError::Invalid(format!("no object named {name}")))?;
            }
            Ok(nodes)
        })
    }
}

/// `(type, engine, collation, rows, data bytes, index bytes, comment)`.
type TableRow = (
    String,
    Option<String>,
    Option<String>,
    Option<u64>,
    Option<u64>,
    Option<u64>,
    Option<String>,
);
/// `(name, type, nullable, default, extra, generation expression, data type, comment,
/// character set, collation, SRID)`.
type ColumnRow = (
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<u32>,
);
/// `(index, non-unique, column, expression, collation, prefix, method)`.
type IndexRow = (
    String,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u64>,
    String,
);
/// `(name, schema, table, column, referenced schema, table, column, on update, on delete)`.
type KeyRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
);

/// The `information_schema` rows of one table as a [`TableStructure`], without the DDL.
/// MySQL and MariaDB write defaults and generation expressions differently; both come
/// out as SQL.
/// `flavor`: MariaDB, and whether the session treats `\` as an escape.
fn structure_from_rows(
    t: &TableRef,
    (mariadb, backslash): (bool, bool),
    table: TableRow,
    columns: Vec<ColumnRow>,
    indexes: Vec<IndexRow>,
    keys: Vec<KeyRow>,
) -> TableStructure {
    let (table_type, engine, collation, rows, data_bytes, index_bytes, comment) = table;
    let view = table_type.ends_with("VIEW");
    let mut out_indexes: Vec<Index> = Vec::new();
    for (name, non_unique, column, expression, collation, prefix, method) in indexes {
        if out_indexes.last().is_none_or(|i| i.name != name) {
            out_indexes.push(Index {
                primary: name == "PRIMARY",
                unique: non_unique == 0,
                kind: method,
                parts: Vec::new(),
                name,
                constraint: None,
            });
        }
        let (column, expression) = match (column, expression) {
            (Some(c), _) => (c, false),
            (None, e) => (e.unwrap_or_default(), true),
        };
        if let Some(last) = out_indexes.last_mut() {
            last.parts.push(IndexPart {
                column,
                expression,
                descending: collation.as_deref() == Some("D"),
                prefix: prefix.and_then(|n| u32::try_from(n).ok()),
            });
        }
    }
    let (mut foreign_keys, mut referenced_by): (Vec<ForeignKey>, Vec<ForeignKey>) =
        (Vec::new(), Vec::new());
    let mut all: Vec<ForeignKey> = Vec::new();
    for (name, db, tbl, col, rdb, rtbl, rcol, on_update, on_delete) in keys {
        let same = all
            .last()
            .is_some_and(|k| k.name == name && k.from.database == db && k.from.table == tbl);
        if !same {
            all.push(ForeignKey {
                name,
                from: TableRef {
                    database: db,
                    table: tbl,
                },
                from_columns: Vec::new(),
                to: TableRef {
                    database: rdb,
                    table: rtbl,
                },
                to_columns: Vec::new(),
                on_delete: on_delete.to_ascii_lowercase(),
                on_update: on_update.to_ascii_lowercase(),
            });
        }
        if let Some(k) = all.last_mut() {
            k.from_columns.push(col);
            k.to_columns.push(rcol);
        }
    }
    for k in all {
        // A key from the table to itself shows in both lists.
        if k.to == *t {
            referenced_by.push(k.clone());
        }
        if k.from == *t {
            foreign_keys.push(k);
        }
    }
    let primary: HashSet<&str> = out_indexes
        .iter()
        .filter(|i| i.primary)
        .flat_map(|i| i.parts.iter().map(|p| p.column.as_str()))
        .collect();
    let foreign: HashSet<&str> = foreign_keys
        .iter()
        .flat_map(|k| k.from_columns.iter().map(String::as_str))
        .collect();
    let columns = columns
        .into_iter()
        .map(
            |(
                name,
                column_type,
                nullable,
                default,
                extra,
                generation,
                data_type,
                comment,
                charset,
                collation,
                srid,
            )| {
                column_from_row(
                    (mariadb, backslash),
                    StructureColumn {
                        primary_key: primary.contains(name.as_str()),
                        foreign_key: foreign.contains(name.as_str()),
                        name,
                        column_type,
                        nullable: nullable == "YES",
                        comment,
                        charset,
                        collation,
                        srid,
                        ..StructureColumn::default()
                    },
                    default,
                    &extra,
                    generation,
                    &data_type,
                )
            },
        )
        .collect();
    TableStructure {
        view,
        columns,
        indexes: out_indexes,
        foreign_keys,
        referenced_by,
        facts: TableFacts {
            engine,
            collation,
            rows,
            data_bytes,
            index_bytes,
            // MySQL writes `VIEW` as a view's comment.
            comment: if view {
                String::new()
            } else {
                comment.unwrap_or_default()
            },
            tablespace: None,
        },
        checks: Vec::new(),
        checks_unsupported: false,
        ddl: String::new(),
        backslash_escapes: backslash,
        oracle: false,
        mariadb,
    }
}

/// MariaDB: the column a check belongs to, when it is the column's own (`LEVEL =
/// 'Column'`). Before 10.5.10 there is no `LEVEL`: a column's own check is named after
/// the column, and the DDL writes it at the end of that column's line.
fn column_check(
    ddl: &str,
    columns: &[StructureColumn],
    name: &str,
    clause: &str,
    level: Option<&str>,
) -> Option<usize> {
    let i = columns.iter().position(|c| c.name == name)?;
    let own = match level {
        Some(level) => level == "Column",
        None => {
            let (start, end) = (
                format!("{} ", quote_ident(name)),
                format!(" CHECK ({clause})"),
            );
            ddl.lines().any(|l| {
                let l = l.trim();
                l.starts_with(&start) && l.trim_end_matches(',').ends_with(&end)
            })
        }
    };
    own.then_some(i)
}

/// Fills in a column's default, generation expression, `on update` and extra.
fn column_from_row(
    (mariadb, backslash): (bool, bool),
    mut c: StructureColumn,
    default: Option<String>,
    extra: &str,
    generation: Option<String>,
    data_type: &str,
) -> StructureColumn {
    let upper = extra.to_ascii_uppercase();
    let generated = [
        "VIRTUAL GENERATED",
        "STORED GENERATED",
        "PERSISTENT GENERATED",
    ]
    .iter()
    .find(|g| upper.contains(*g));
    let mut rest = extra.to_owned();
    for token in [
        "DEFAULT_GENERATED",
        "VIRTUAL GENERATED",
        "STORED GENERATED",
        "PERSISTENT GENERATED",
    ] {
        if let Some(i) = rest.to_ascii_uppercase().find(token) {
            rest.replace_range(i..i + token.len(), "");
        }
    }
    // `on update CURRENT_TIMESTAMP(3)`: the token after it is the expression.
    if let Some(i) = rest.to_ascii_lowercase().find("on update ") {
        let after = &rest[i + "on update ".len()..];
        let expr = after
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned();
        let end = i + "on update ".len() + after.find(&expr).unwrap_or(0) + expr.len();
        rest.replace_range(i..end, "");
        c.on_update = (!expr.is_empty()).then_some(expr);
    }
    c.extra = rest.split_whitespace().collect::<Vec<_>>().join(" ");
    if let (Some(g), Some(expr)) = (generated, generation.filter(|e| !e.is_empty())) {
        // MySQL wraps the expression in parentheses already; MariaDB does not.
        let expression = if mariadb { format!("({expr})") } else { expr };
        c.generated = Some(Generated {
            expression,
            stored: !g.starts_with("VIRTUAL"),
        });
        return c;
    }
    c.default = match default {
        None => None,
        // MariaDB writes the default as SQL, and `NULL` for a NULL default.
        Some(d) if mariadb => (d != "NULL").then(|| expression_default(d)),
        // MySQL writes literals bare and marks expressions.
        Some(d) if upper.contains("DEFAULT_GENERATED") => Some(expression_default(d)),
        Some(d) => {
            let ty = data_type.to_ascii_lowercase();
            let bare = ty == "bit"
                || [
                    "tinyint",
                    "smallint",
                    "mediumint",
                    "int",
                    "bigint",
                    "decimal",
                    "float",
                    "double",
                ]
                .contains(&ty.as_str());
            Some(if bare {
                d
            } else {
                crate::edit::quote_str(&d, backslash)
            })
        }
    };
    c
}

/// A default as SQL: `uuid()` comes back bare and gets its parentheses, else it would
/// be written as a string. Literals, numbers and current-time functions stay as they
/// are (MySQL would turn `(CURRENT_TIMESTAMP)` into an expression default).
fn expression_default(d: String) -> String {
    let t = d.trim();
    let literal = t.eq_ignore_ascii_case("null")
        || t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'')
        || t.len() >= 3
            && t.get(..2).is_some_and(|b| b.eq_ignore_ascii_case("b'"))
            && t.ends_with('\'')
        || t.parse::<f64>().is_ok()
        || t.starts_with('(') && t.ends_with(')')
        || crate::structure::is_time_function(t);
    if literal { d } else { format!("({t})") }
}

const SYSTEM_SCHEMAS: [&str; 4] = ["information_schema", "mysql", "performance_schema", "sys"];

/// User databases first, then the system schemas, each group in the server's order.
fn databases(names: Vec<String>) -> Vec<ObjectNode> {
    let (system, user): (Vec<_>, Vec<_>) = names
        .into_iter()
        .partition(|n| SYSTEM_SCHEMAS.contains(&n.to_ascii_lowercase().as_str()));
    user.into_iter()
        .map(|n| ObjectNode::new(n, NodeKind::Database))
        .chain(system.into_iter().map(|n| ObjectNode {
            system: true,
            ..ObjectNode::new(n, NodeKind::Database)
        }))
        .collect()
}

/// `(trigger, table, timing, event)` and `(event, type, interval value, interval field)`.
type ObjectRows = (
    Vec<(String, String, String, String)>,
    Vec<(String, String, Option<String>, Option<String>)>,
);

/// `daily`, `every 5 minute`, `once`: an event's label in the tree.
fn event_label(ty: &str, value: Option<&str>, field: Option<&str>) -> String {
    if ty.eq_ignore_ascii_case("ONE TIME") {
        return "once".into();
    }
    let field = field.unwrap_or_default().to_ascii_lowercase();
    let value = value.unwrap_or_default().trim_matches('\'');
    match (value, field.as_str()) {
        ("1", "day") => "daily".into(),
        ("1", "hour") => "hourly".into(),
        ("1", "week") => "weekly".into(),
        ("1", "month") => "monthly".into(),
        ("1", "year") => "yearly".into(),
        ("1", f) => format!("every {f}"),
        (v, f) => format!("every {v} {f}"),
    }
}

/// The groups of one database, with every column inside: Tables, Views, Procedures,
/// Functions, Triggers, Events and on MariaDB Sequences, empty ones included. A
/// table with triggers also lists them in a Triggers group after its columns.
/// Rows are `(table, type)`, `(routine, type)`, `(table, column, type, key)`,
/// `(table, column)` for columns that reference another table, and [`ObjectRows`].
fn database_children(
    tables: Vec<(String, String)>,
    routines: Vec<(String, String)>,
    columns: Vec<(String, String, String, String)>,
    foreign: Vec<(String, String)>,
    (triggers, events): ObjectRows,
    mariadb: bool,
) -> Vec<ObjectNode> {
    let leaf = |name: String, detail: String| ObjectNode {
        detail,
        ..ObjectNode::new(name, NodeKind::Routine)
    };
    let mut on_table: HashMap<String, Vec<ObjectNode>> = HashMap::new();
    let mut trigger_nodes = Vec::new();
    for (name, table, timing, event) in triggers {
        let when = format!("{timing} {event}").to_ascii_lowercase();
        on_table
            .entry(table.clone())
            .or_default()
            .push(leaf(name.clone(), when));
        trigger_nodes.push(leaf(name, table));
    }
    let event_nodes = events
        .into_iter()
        .map(|(name, ty, value, field)| {
            let label = event_label(&ty, value.as_deref(), field.as_deref());
            leaf(name, label)
        })
        .collect();
    let foreign: HashSet<(String, String)> = foreign.into_iter().collect();
    let mut by_table: HashMap<String, Vec<ObjectNode>> = HashMap::new();
    for (table, name, ty, key) in columns {
        let foreign_key = foreign.contains(&(table.clone(), name.clone()));
        by_table.entry(table).or_default().push(ObjectNode {
            detail: ty,
            primary_key: key == "PRI",
            foreign_key,
            ..ObjectNode::new(name, NodeKind::Column)
        });
    }
    let (mut table_nodes, mut view_nodes, mut sequence_nodes) =
        (Vec::new(), Vec::new(), Vec::new());
    for (name, ty) in tables {
        if ty == "SEQUENCE" {
            sequence_nodes.push(leaf(name, String::new()));
            continue;
        }
        let is_view = ty.ends_with("VIEW");
        let mut cols = by_table.remove(&name).unwrap_or_default();
        if let Some(t) = on_table.remove(&name) {
            cols.push(ObjectNode {
                children: Some(t),
                ..ObjectNode::new("Triggers", NodeKind::Group)
            });
        }
        let children = Some(cols);
        let kind = if is_view {
            NodeKind::View
        } else {
            NodeKind::Table
        };
        let node = ObjectNode {
            children,
            ..ObjectNode::new(name, kind)
        };
        if is_view {
            view_nodes.push(node);
        } else {
            table_nodes.push(node);
        }
    }
    let (procedures, functions): (Vec<_>, Vec<_>) = routines
        .into_iter()
        .partition(|(_, ty)| ty.eq_ignore_ascii_case("PROCEDURE"));
    let routine_nodes = |rows: Vec<(String, String)>| -> Vec<ObjectNode> {
        rows.into_iter()
            .map(|(name, _)| leaf(name, String::new()))
            .collect()
    };
    let mut groups = vec![
        ("Tables", table_nodes),
        ("Views", view_nodes),
        ("Procedures", routine_nodes(procedures)),
        ("Functions", routine_nodes(functions)),
        ("Triggers", trigger_nodes),
        ("Events", event_nodes),
    ];
    if mariadb {
        groups.push(("Sequences", sequence_nodes));
    }
    groups
        .into_iter()
        .map(|(name, children)| ObjectNode {
            children: Some(children),
            ..ObjectNode::new(name, NodeKind::Group)
        })
        .collect()
}

/// Whether the session's transaction is open and ran more than reads, from the status of
/// the session's last reply; `writes` marks the statement about to run.
fn track(conn: &Conn, wrote: &AtomicBool, writes: bool) -> bool {
    let in_trans = conn.last_ok_packet().is_some_and(|ok| {
        ok.status_flags()
            .contains(StatusFlags::SERVER_STATUS_IN_TRANS)
    });
    let open = writes || in_trans && wrote.load(Ordering::Relaxed);
    wrote.store(open, Ordering::Relaxed);
    in_trans && open
}

/// Runs `text` and streams its result sets one after another: each stream carries the
/// sender for the next. A set whose stream was dropped is read to its end and skipped.
/// `writes`: `text` is more than a read, so the transaction it runs in counts as open.
/// `commits`: it commits on its own (DDL), so once it ran nothing is left to commit,
/// and a read after it does not count as open.
async fn run(
    conn: Arc<Mutex<Conn>>,
    text: String,
    writes: bool,
    commits: bool,
    wrote: Arc<AtomicBool>,
    head: oneshot::Sender<Result<ResultStream>>,
) {
    let mut conn = conn.lock_owned().await;
    track(&conn, &wrote, writes);
    let mut result = match conn.query_iter(text).await {
        Ok(r) => r,
        Err(e) => {
            let _ = head.send(Err(e.into()));
            return;
        }
    };
    if commits {
        wrote.store(false, Ordering::Relaxed);
    }
    let mut head = head;
    loop {
        let cols: Vec<mysql_async::Column> = result.columns_ref().to_vec();
        let columns = cols
            .iter()
            .map(|c| Column {
                name: c.name_str().into_owned(),
                type_name: type_name(c.column_type()),
            })
            .collect();
        let (tx, stream) = if cols.is_empty() {
            let (tx, mut s) = ResultStream::channel(Vec::new());
            s.affected_rows = Some(result.affected_rows());
            (tx, s)
        } else {
            ResultStream::channel(columns)
        };
        let (next, stream) = stream.followed();
        // Nobody waits for this set or any after it; dropping the result skips the rest.
        let mut tx = head.send(Ok(stream)).is_ok().then_some(tx);
        let size = page_size();
        let mut page = Vec::with_capacity(size);
        loop {
            match result.next().await {
                Ok(Some(row)) => {
                    let Some(sender) = &tx else { continue };
                    page.push(
                        cols.iter()
                            .zip(row.unwrap_raw())
                            .map(|(c, v)| v.map_or(Value::Null, |v| cell(c, v)))
                            .collect(),
                    );
                    if page.len() == size
                        && sender.send(Ok(std::mem::take(&mut page))).await.is_err()
                    {
                        tx = None;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    if let Some(sender) = &tx {
                        let _ = sender.send(Err(e.into())).await;
                    } else {
                        let _ = next.send(Err(e.into()));
                    }
                    return;
                }
            }
        }
        if let Some(sender) = tx
            && !page.is_empty()
        {
            let _ = sender.send(Ok(page)).await;
        }
        if result.is_empty() {
            // A later statement's error waits as the pending result; `next` hands it over
            // here instead of to the session's next query.
            if let Err(e) = result.next().await {
                let _ = next.send(Err(e.into()));
            }
            return;
        }
        head = next;
    }
}

/// Rows a change touched: for an UPDATE the rows it matched (`Rows matched: 1  Changed: 0`),
/// since setting a value it already has changes nothing; otherwise the affected rows.
fn touched(kind: ChangeKind, info: &str, affected: u64) -> u64 {
    if kind != ChangeKind::Update {
        return affected;
    }
    info.split("Rows matched:")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(affected)
}

/// `MYSQL_TYPE_VAR_STRING` → `VAR_STRING`.
fn type_name(t: ColumnType) -> String {
    let name = format!("{t:?}");
    name.strip_prefix("MYSQL_TYPE_").unwrap_or(&name).to_owned()
}

/// Maps a text-protocol value onto [`Value`] using the column's type.
fn cell(col: &mysql_async::Column, v: mysql_async::Value) -> Value {
    use mysql_async::Value as M;
    let bytes = match v {
        M::NULL => return Value::Null,
        M::Bytes(b) => b,
        M::Int(i) => return Value::Int(i),
        M::UInt(u) => return Value::UInt(u),
        M::Float(f) => return Value::Float(f.into()),
        M::Double(f) => return Value::Float(f),
        // The binary protocol's forms; the text protocol sends bytes.
        other => return Value::Text(other.as_sql(true)),
    };
    let t = col.column_type();
    if t == ColumnType::MYSQL_TYPE_BIT
        || col.character_set() == BINARY_CHARSET
            && !t.is_numeric_type()
            && !is_temporal(t)
            // MySQL sends JSON with the binary charset; it is UTF-8 text.
            && t != ColumnType::MYSQL_TYPE_JSON
    {
        return Value::Bytes(bytes);
    }
    let text = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => return Value::Bytes(e.into_bytes()),
    };
    let unsigned = col.flags().contains(ColumnFlags::UNSIGNED_FLAG);
    let parsed = match t {
        ColumnType::MYSQL_TYPE_TINY
        | ColumnType::MYSQL_TYPE_SHORT
        | ColumnType::MYSQL_TYPE_INT24
        | ColumnType::MYSQL_TYPE_LONG
        | ColumnType::MYSQL_TYPE_LONGLONG
        | ColumnType::MYSQL_TYPE_YEAR => {
            if unsigned {
                text.parse().ok().map(Value::UInt)
            } else {
                text.parse().ok().map(Value::Int)
            }
        }
        ColumnType::MYSQL_TYPE_FLOAT | ColumnType::MYSQL_TYPE_DOUBLE => {
            text.parse().ok().map(Value::Float)
        }
        ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL => {
            return Value::Decimal(text);
        }
        ColumnType::MYSQL_TYPE_DATE => NaiveDate::parse_from_str(&text, "%Y-%m-%d")
            .ok()
            .map(Value::Date),
        ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_TIMESTAMP => {
            NaiveDateTime::parse_from_str(&text, "%Y-%m-%d %H:%M:%S%.f")
                .ok()
                .map(Value::DateTime)
        }
        ColumnType::MYSQL_TYPE_TIME => parse_time(&text).map(Value::Time),
        _ => None,
    };
    // Zero dates and other values outside the typed range stay as the server wrote them.
    parsed.unwrap_or(Value::Text(text))
}

fn is_temporal(t: ColumnType) -> bool {
    matches!(
        t,
        ColumnType::MYSQL_TYPE_DATE
            | ColumnType::MYSQL_TYPE_DATETIME
            | ColumnType::MYSQL_TYPE_TIMESTAMP
            | ColumnType::MYSQL_TYPE_TIME
            | ColumnType::MYSQL_TYPE_YEAR
    )
}

/// `-838:59:59.5` → a signed duration.
fn parse_time(text: &str) -> Option<TimeDelta> {
    let (negative, rest) = match text.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, text),
    };
    let (hms, frac) = rest.split_once('.').unwrap_or((rest, ""));
    let mut parts = hms.split(':').map(str::parse::<i64>);
    let (h, m, s) = (
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    );
    let micros = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<6}").get(..6)?.parse::<i64>().ok()?
    };
    let d = TimeDelta::hours(h)
        + TimeDelta::minutes(m)
        + TimeDelta::seconds(s)
        + TimeDelta::microseconds(micros);
    Some(if negative { -d } else { d })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_count_matched_rows() {
        let info = "Rows matched: 1  Changed: 0  Warnings: 0";
        assert_eq!(touched(ChangeKind::Update, info, 0), 1);
        assert_eq!(touched(ChangeKind::Update, "", 0), 0);
        assert_eq!(touched(ChangeKind::Delete, info, 0), 0);
    }

    #[test]
    fn version_labels_detect_the_flavor() {
        assert_eq!(version_label("8.4.3"), "MySQL 8.4.3");
        assert_eq!(version_label("11.8.2-MariaDB-ubu2404"), "MariaDB 11.8.2");
    }

    fn s(v: &str) -> String {
        v.to_owned()
    }

    #[test]
    fn system_schemas_come_last_and_dimmed() {
        let got = databases(vec![
            s("information_schema"),
            s("app"),
            s("mysql"),
            s("zoo"),
        ]);
        let names: Vec<_> = got.iter().map(|n| (n.name.as_str(), n.system)).collect();
        assert_eq!(
            names,
            [
                ("app", false),
                ("zoo", false),
                ("information_schema", true),
                ("mysql", true)
            ]
        );
        assert!(
            got.iter()
                .all(|n| n.kind == NodeKind::Database && n.children.is_none())
        );
    }

    #[test]
    fn database_rows_become_groups_with_columns() {
        let groups = database_children(
            vec![
                (s("orders"), s("BASE TABLE")),
                (s("totals"), s("VIEW")),
                (s("empty"), s("BASE TABLE")),
            ],
            vec![(s("refund"), s("PROCEDURE"))],
            vec![
                (s("orders"), s("id"), s("bigint unsigned"), s("PRI")),
                (s("orders"), s("customer_id"), s("bigint"), s("MUL")),
                (s("orders"), s("note"), s("varchar(20)"), s("")),
                (s("totals"), s("sum"), s("decimal(12,2)"), s("")),
            ],
            vec![(s("orders"), s("customer_id"))],
            (
                vec![(s("orders_bu"), s("orders"), s("BEFORE"), s("UPDATE"))],
                vec![(s("nightly"), s("RECURRING"), Some(s("1")), Some(s("DAY")))],
            ),
            false,
        );
        let names: Vec<_> = groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Tables",
                "Views",
                "Procedures",
                "Functions",
                "Triggers",
                "Events"
            ]
        );
        assert_eq!(groups[3].children, Some(vec![]));
        let trigger = &groups[4].children.as_ref().unwrap()[0];
        assert_eq!(
            (trigger.name.as_str(), trigger.detail.as_str()),
            ("orders_bu", "orders")
        );
        assert_eq!(groups[5].children.as_ref().unwrap()[0].detail, "daily");
        let tables = groups[0].children.as_ref().unwrap();
        assert_eq!(tables.len(), 2);
        let orders = &tables[0];
        assert_eq!(orders.kind, NodeKind::Table);
        let cols = orders.children.as_ref().unwrap();
        assert_eq!(cols.len(), 4);
        assert_eq!(cols[3].name, "Triggers");
        assert_eq!(
            cols[3].children.as_ref().unwrap()[0].detail,
            "before update"
        );
        assert!(cols[0].primary_key && !cols[0].foreign_key);
        assert_eq!(cols[0].detail, "bigint unsigned");
        assert!(cols[1].foreign_key && !cols[1].primary_key);
        assert!(!cols[2].primary_key && !cols[2].foreign_key);
        assert_eq!(tables[1].children, Some(vec![]));
        let views = groups[1].children.as_ref().unwrap();
        assert_eq!(views[0].kind, NodeKind::View);
        assert_eq!(views[0].children.as_ref().unwrap()[0].name, "sum");
        let routines = groups[2].children.as_ref().unwrap();
        assert_eq!(routines[0].name, "refund");
        assert!(!routines[0].has_children());
        let maria = database_children(
            vec![(s("seq"), s("SEQUENCE"))],
            vec![],
            vec![],
            vec![],
            (vec![], vec![]),
            true,
        );
        assert_eq!(maria.last().unwrap().name, "Sequences");
        assert_eq!(maria[0].children, Some(vec![]));
        assert_eq!(maria[6].children.as_ref().unwrap()[0].name, "seq");
        assert_eq!(
            event_label("RECURRING", Some("5"), Some("MINUTE")),
            "every 5 minute"
        );
        assert_eq!(event_label("ONE TIME", None, None), "once");
    }

    fn probe(mariadb: bool) -> TableStructure {
        let t = TableRef {
            database: s("p"),
            table: s("child"),
        };
        let o = |v: &str| Some(s(v));
        let col = |name: &str,
                   ty: &str,
                   null: &str,
                   def: Option<String>,
                   extra: &str,
                   generation: Option<String>,
                   dt: &str| {
            (
                s(name),
                s(ty),
                s(null),
                def,
                s(extra),
                generation,
                s(dt),
                String::new(),
                (name == "title").then(|| s("utf8mb4")),
                (name == "title").then(|| s("utf8mb4_bin")),
                None,
            )
        };
        let columns = if mariadb {
            vec![
                col(
                    "id",
                    "bigint(20) unsigned",
                    "NO",
                    None,
                    "auto_increment",
                    None,
                    "bigint",
                ),
                col("pa", "int(11)", "YES", o("NULL"), "", None, "int"),
                col(
                    "title",
                    "varchar(200)",
                    "NO",
                    o("'it''s'"),
                    "",
                    None,
                    "varchar",
                ),
                col("qty", "int(11)", "NO", o("0"), "", None, "int"),
                col(
                    "created",
                    "datetime",
                    "YES",
                    o("current_timestamp()"),
                    "on update current_timestamp()",
                    None,
                    "datetime",
                ),
                col(
                    "total",
                    "decimal(10,2)",
                    "YES",
                    None,
                    "STORED GENERATED",
                    o("`qty` * 2"),
                    "decimal",
                ),
                col(
                    "v",
                    "int(11)",
                    "YES",
                    None,
                    "VIRTUAL GENERATED",
                    o("`qty` + 1"),
                    "int",
                ),
                col("u", "char(36)", "YES", o("uuid()"), "", None, "char"),
            ]
        } else {
            vec![
                col(
                    "id",
                    "bigint unsigned",
                    "NO",
                    None,
                    "auto_increment",
                    o(""),
                    "bigint",
                ),
                col("pa", "int", "YES", None, "", o(""), "int"),
                col(
                    "title",
                    "varchar(200)",
                    "NO",
                    o("it's"),
                    "",
                    o(""),
                    "varchar",
                ),
                col("qty", "int", "NO", o("0"), "", o(""), "int"),
                col(
                    "created",
                    "datetime",
                    "YES",
                    o("CURRENT_TIMESTAMP"),
                    "DEFAULT_GENERATED on update CURRENT_TIMESTAMP",
                    o(""),
                    "datetime",
                ),
                col(
                    "total",
                    "decimal(10,2)",
                    "YES",
                    None,
                    "STORED GENERATED",
                    o("(`qty` * 2)"),
                    "decimal",
                ),
                col(
                    "v",
                    "int",
                    "YES",
                    None,
                    "VIRTUAL GENERATED",
                    o("(`qty` + 1)"),
                    "int",
                ),
                col(
                    "u",
                    "char(36)",
                    "YES",
                    o("uuid()"),
                    "DEFAULT_GENERATED",
                    o(""),
                    "char",
                ),
            ]
        };
        let ix =
            |name: &str,
             nu: i64,
             c: Option<&str>,
             e: Option<&str>,
             coll: Option<&str>,
             sub: Option<u64>,
             m: &str| { (s(name), nu, c.map(s), e.map(s), coll.map(s), sub, s(m)) };
        let indexes = vec![
            ix("PRIMARY", 0, Some("id"), None, Some("A"), None, "BTREE"),
            ix("ft_body", 1, Some("body"), None, None, None, "FULLTEXT"),
            ix("ix_desc", 1, Some("qty"), None, Some("D"), None, "BTREE"),
            ix(
                "ix_desc",
                1,
                Some("created"),
                None,
                Some("A"),
                None,
                "BTREE",
            ),
            ix(
                "ix_fn",
                1,
                None,
                Some("lower(`title`)"),
                Some("A"),
                None,
                "BTREE",
            ),
            ix(
                "uq_title",
                0,
                Some("title"),
                None,
                Some("A"),
                Some(50),
                "BTREE",
            ),
        ];
        let key = |name: &str, tbl: &str, col: &str, rtbl: &str, rcol: &str| {
            (
                s(name),
                s("p"),
                s(tbl),
                s(col),
                s("p"),
                s(rtbl),
                s(rcol),
                s("SET NULL"),
                s("CASCADE"),
            )
        };
        let keys = vec![
            key("fk_back", "grandchild", "child_id", "child", "id"),
            key("fk_parent", "child", "pa", "parent", "a"),
            key("fk_parent", "child", "pb", "parent", "b"),
            key("fk_self", "child", "pa", "child", "id"),
        ];
        let table = (
            s("BASE TABLE"),
            o("InnoDB"),
            o("utf8mb4_general_ci"),
            Some(3),
            Some(16384),
            Some(81920),
            o("kids"),
        );
        structure_from_rows(&t, (mariadb, true), table, columns, indexes, keys)
    }

    #[test]
    fn structure_rows_map_alike_on_both_flavors() {
        for mariadb in [false, true] {
            let st = probe(mariadb);
            let c = &st.columns;
            assert_eq!(c[0].key_label(), "PK", "{mariadb}");
            assert_eq!(c[0].extra, "auto_increment");
            assert_eq!(c[0].default, None);
            assert_eq!(c[1].key_label(), "FK");
            assert_eq!(c[1].default, None, "{mariadb}");
            assert_eq!(c[2].default.as_deref(), Some("'it''s'"), "{mariadb}");
            assert!(!c[2].nullable && c[1].nullable);
            assert_eq!(c[3].default.as_deref(), Some("0"));
            assert!(
                c[4].on_update
                    .as_deref()
                    .is_some_and(|u| u.eq_ignore_ascii_case("current_timestamp")
                        || u == "current_timestamp()"),
                "{mariadb}"
            );
            assert_eq!(c[4].extra, "", "{mariadb}");
            assert_eq!(c[2].collation.as_deref(), Some("utf8mb4_bin"));
            assert_eq!(c[3].collation, None);
            assert_eq!(c[7].default.as_deref(), Some("(uuid())"), "{mariadb}");
            assert_eq!(st.mariadb, mariadb);
            assert!(
                c[4].default
                    .as_deref()
                    .is_some_and(|d| d.to_ascii_lowercase().starts_with("current_timestamp"))
            );
            assert_eq!(
                c[5].default_label(),
                "generated always as (`qty` * 2) stored",
                "{mariadb}"
            );
            assert_eq!(
                c[6].default_label(),
                "generated always as (`qty` + 1) virtual"
            );
            let names: Vec<_> = st.indexes.iter().map(|i| i.name.as_str()).collect();
            assert_eq!(
                names,
                ["PRIMARY", "ft_body", "ix_desc", "ix_fn", "uq_title"]
            );
            assert_eq!(st.indexes[0].kind_label(), "primary");
            assert_eq!(st.indexes[1].kind_label(), "fulltext");
            assert_eq!(st.indexes[2].parts_label(), "qty desc, created");
            assert_eq!(st.indexes[2].kind_label(), "btree");
            assert_eq!(st.indexes[3].parts_label(), "(lower(`title`))");
            assert_eq!(st.indexes[4].parts_label(), "title(50)");
            assert_eq!(st.indexes[4].kind_label(), "unique");
            let fk: Vec<_> = st.foreign_keys.iter().map(|k| k.name.as_str()).collect();
            assert_eq!(fk, ["fk_parent", "fk_self"]);
            assert_eq!(st.foreign_keys[0].from_columns, ["pa", "pb"]);
            assert_eq!(st.foreign_keys[0].to_columns, ["a", "b"]);
            assert_eq!(st.foreign_keys[0].to.table, "parent");
            assert_eq!(
                (
                    st.foreign_keys[0].on_delete.as_str(),
                    st.foreign_keys[0].on_update.as_str()
                ),
                ("cascade", "set null")
            );
            let back: Vec<_> = st.referenced_by.iter().map(|k| k.name.as_str()).collect();
            assert_eq!(back, ["fk_back", "fk_self"]);
            assert_eq!(st.referenced_by[0].from.table, "grandchild");
            assert_eq!(st.facts.size(), Some(16384 + 81920));
            assert_eq!(st.facts.comment, "kids");
            assert!(!st.view);
        }
    }

    /// MariaDB before 10.5.10 lists checks without `LEVEL`: a column's own check is
    /// the one named after the column whose DDL line ends with it.
    #[test]
    fn column_checks_without_level_come_from_the_ddl() {
        let ddl = "CREATE TABLE `zz` (\n  `id` int(11) NOT NULL,\n  \
                   `j` longtext CHARACTER SET utf8mb4 COLLATE utf8mb4_bin DEFAULT NULL CHECK (json_valid(`j`)),\n  \
                   `c` int(11) DEFAULT NULL COMMENT 'CHECK (x)' CHECK (`c` > 0),\n  \
                   `n` int(11) DEFAULT NULL,\n  PRIMARY KEY (`id`),\n  \
                   CONSTRAINT `n` CHECK (`n` < 5),\n  CONSTRAINT `ck_t` CHECK (`id` > 0)\n\
                   ) ENGINE=InnoDB";
        let columns: Vec<StructureColumn> = ["id", "j", "c", "n"]
            .iter()
            .map(|n| StructureColumn {
                name: s(n),
                ..StructureColumn::default()
            })
            .collect();
        let own = |name: &str, clause: &str, level: Option<&str>| {
            column_check(ddl, &columns, name, clause, level)
        };
        assert_eq!(own("j", "json_valid(`j`)", None), Some(1));
        assert_eq!(own("c", "`c` > 0", None), Some(2));
        // A table check named after a column, and one named otherwise.
        assert_eq!(own("n", "`n` < 5", None), None);
        assert_eq!(own("ck_t", "`id` > 0", None), None);
        // With LEVEL, the server says.
        assert_eq!(own("j", "json_valid(`j`)", Some("Column")), Some(1));
        assert_eq!(own("n", "`n` < 5", Some("Table")), None);
    }

    #[test]
    fn existing_columns_restate_unchanged() {
        use crate::structure::{NewColumn, column_definition};
        for mariadb in [false, true] {
            let st = probe(mariadb);
            let lines: Vec<String> = st
                .columns
                .iter()
                .map(|c| column_definition(&NewColumn::from_existing(c, &st), None, &st).unwrap())
                .collect();
            let (int, bigint, created, generated_null) = if mariadb {
                (
                    "int(11)",
                    "bigint(20) unsigned",
                    "current_timestamp() on update current_timestamp",
                    "",
                )
            } else {
                (
                    "int",
                    "bigint unsigned",
                    "CURRENT_TIMESTAMP on update current_timestamp",
                    " null",
                )
            };
            assert_eq!(
                lines,
                [
                    format!("`id` {bigint} not null auto_increment"),
                    format!("`pa` {int} null"),
                    "`title` varchar(200) collate utf8mb4_bin not null default 'it''s'".to_owned(),
                    format!("`qty` {int} not null default 0"),
                    format!("`created` datetime null default {created}"),
                    format!(
                        "`total` decimal(10,2) generated always as (`qty` * 2) stored{generated_null}"
                    ),
                    format!("`v` {int} generated always as (`qty` + 1) virtual{generated_null}"),
                    "`u` char(36) null default (uuid())".to_owned(),
                ],
                "{mariadb}"
            );
        }
    }

    #[test]
    fn times_parse_signed_with_fractions() {
        assert_eq!(parse_time("01:02:03"), Some(TimeDelta::seconds(3723)));
        assert_eq!(
            parse_time("-838:59:59.5"),
            Some(-(TimeDelta::seconds(838 * 3600 + 59 * 60 + 59) + TimeDelta::milliseconds(500)))
        );
        assert_eq!(parse_time("nope"), None);
    }
}
