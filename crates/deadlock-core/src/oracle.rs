//! Oracle, on one of two drivers: Thin (`oracledb`, Oracle's pure-Rust driver, the
//! default) or Thick (`oracle` on ODPI-C, which loads Oracle Instant Client and signs
//! in where Thin cannot, such as accounts with only the old 10G password verifier).
//! Both are blocking, so every call runs on tokio's blocking pool, and both sit
//! behind [`conn::Conn`]: everything else here is shared.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::task::spawn_blocking;

use crate::classify::{Statement, classify};
use crate::connection::{ConnectionConfig, Encryption, Engine};
use crate::driver::{
    BoxFuture, Capabilities, Column, Driver, DriverError, ObjectNode, ObjectPath, Result,
    ResultStream, Value, page_size,
};

mod config;
pub(crate) mod conn;
mod plan;
mod schema;
mod sessions;
mod structure;
mod table;
pub mod thick;

use conn::Bind;

/// An open session, on either driver.
pub(crate) type Connection = dyn conn::Conn;

pub struct Oracle {
    /// The SSH tunnel the connection goes through: closes with the driver.
    pub(crate) tunnel: Option<crate::ssh::Tunnel>,
    conn: Arc<Connection>,
    /// The Thick driver: cancels with a break instead of a side session.
    thick: bool,
    /// Kept for the side connection that cancels.
    user: String,
    password: String,
    connect_string: String,
    session: (usize, usize),
    version: String,
    /// Held while a statement runs, so transaction calls wait for it.
    busy: Arc<Mutex<()>>,
    /// The driver never commits on its own: in Auto mode each statement commits here.
    auto: Arc<AtomicBool>,
    /// Writes are refused before sending; each transaction is also `READ ONLY`.
    read_only: bool,
}

/// Part of the Thin driver's error when the account has only an old password
/// verifier (10G, `0x939`); its own text is `not implemented: password verifier type`.
const THIN_VERIFIER: &str = "password verifier type";

fn open_thin(user: &str, password: &str, connect_string: &str) -> Result<oracledb::Connection> {
    let config = oracledb::Config::default()
        .set_credentials(user, password)
        .set_connect_string(connect_string)?;
    oracledb::connect(config).map_err(|e| {
        let text = e.to_string();
        if text.contains(THIN_VERIFIER) {
            DriverError::OldVerifier(text)
        } else {
            e.into()
        }
    })
}

/// A session on the chosen driver, with its session id and serial number.
fn open(
    thick: bool,
    user: &str,
    password: &str,
    connect_string: &str,
) -> Result<(Arc<Connection>, usize, usize, String)> {
    if thick {
        let c = thick::open(user, password, connect_string)?;
        // Sent with the next call: marks the session as Deadlock's in V$SESSION.
        c.set_module(sessions::MODULE)?;
        let (version, _) = c.server_version()?;
        let c: Arc<Connection> = Arc::new(c);
        let sid = texts(&*c, "SELECT SYS_CONTEXT('USERENV', 'SID') FROM dual", &[])?
            .first()
            .and_then(|r| num(r, 0))
            .unwrap_or(0);
        return Ok((c, sid, 0, version.to_string()));
    }
    let c = open_thin(user, password, connect_string)?;
    c.set_module(sessions::MODULE)?;
    let (sid, serial, version) = (c.session_id()?, c.serial_num()?, c.version()?.to_string());
    Ok((Arc::new(c), sid, serial, version))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    spawn_blocking(f).await.map_err(|_| DriverError::Gone)?
}

/// Session settings the app relies on: dates, timestamps and numbers read and write in
/// one fixed form whatever the server's defaults (string to date conversions, the
/// data editor's time-zone literals, `DBMS_OUTPUT` text), and `DBMS_OUTPUT` on.
const SESSION_SETUP: [&str; 2] = [
    "ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY-MM-DD HH24:MI:SS' \
     NLS_TIMESTAMP_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF' \
     NLS_TIMESTAMP_TZ_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM' \
     NLS_NUMERIC_CHARACTERS = '.,'",
    "BEGIN DBMS_OUTPUT.ENABLE(NULL); END;",
];

/// `host:port/service` (Easy Connect) or, for a SID, the descriptor that names it.
pub fn connect_string(config: &ConnectionConfig) -> String {
    // shortcut: Prefer is plain TCP; Require and Verify both use TCPS with the
    // driver's default certificate checks.
    let tcps = matches!(
        config.encryption,
        Encryption::Require | Encryption::VerifyCertificate
    );
    if config.sid {
        format!(
            "(DESCRIPTION=(ADDRESS=(PROTOCOL={})(HOST={})(PORT={}))(CONNECT_DATA=(SID={})))",
            if tcps { "TCPS" } else { "TCP" },
            config.host,
            config.port,
            config.database
        )
    } else {
        format!(
            "{}{}:{}/{}",
            if tcps { "tcps://" } else { "" },
            config.host,
            config.port,
            config.database
        )
    }
}

pub async fn connect(config: &ConnectionConfig, password: &str) -> Result<Oracle> {
    let connect_string = connect_string(config);
    let (user, password) = (config.user.clone(), password.to_owned());
    let (read_only, thick) = (config.read_only, config.thick);
    blocking(move || {
        let (conn, sid, serial, version) = open(thick, &user, &password, &connect_string)?;
        for sql in SESSION_SETUP {
            conn.execute(sql, &[])?;
        }
        Ok(Oracle {
            tunnel: None,
            session: (sid, serial),
            version: format!("Oracle {version}"),
            conn,
            thick,
            user,
            password,
            connect_string,
            busy: Arc::default(),
            auto: Arc::new(AtomicBool::new(true)),
            read_only,
        })
    })
    .await
}

/// Whether the session holds a transaction: Oracle's own transaction id, which is set
/// from the first change (or `SET TRANSACTION`) until commit or rollback.
fn transaction_open(conn: &Connection) -> Result<bool> {
    let rows = texts(
        conn,
        "SELECT DBMS_TRANSACTION.LOCAL_TRANSACTION_ID FROM dual",
        &[],
    )?;
    Ok(rows
        .first()
        .is_some_and(|r| r.first().is_some_and(Option::is_some)))
}

/// Every row of `sql`, each column read as text. Dictionary queries convert dates and
/// numbers in the SQL, so this is all they need.
pub(crate) fn texts(
    conn: &Connection,
    sql: &str,
    params: &[&dyn Bind],
) -> Result<Vec<Vec<Option<String>>>> {
    conn.texts(sql, params)
}

/// Column `i` of a [`texts`] row, empty when NULL or missing.
pub(crate) fn col(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

/// Column `i` of a [`texts`] row as a number.
pub(crate) fn num<T: std::str::FromStr>(row: &[Option<String>], i: usize) -> Option<T> {
    row.get(i)?.as_deref()?.parse().ok()
}

/// `'text'` with quotes doubled: a value written into SQL. Dictionary queries that
/// would return a LOB use it, and fetch the LOB in VARCHAR2 pieces: for a LOB column
/// the beta driver adds a define round trip without the binds, which fails
/// (ORA-01008, ORA-01002) when the server has to reparse the statement.
pub(crate) fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The ORA- number of a server error.
pub(crate) fn ora_code(e: &DriverError) -> Option<usize> {
    match e {
        DriverError::Oracle(e) => e.db_error_code(),
        DriverError::OracleThick(e) => e.db_error().and_then(|d| usize::try_from(d.code()).ok()),
        _ => None,
    }
}

impl Oracle {
    /// Runs `f` on the session, after any statement that is running.
    fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
    ) -> BoxFuture<'static, Result<T>> {
        let (conn, busy) = (Arc::clone(&self.conn), Arc::clone(&self.busy));
        Box::pin(blocking(move || {
            let _busy = busy.lock().unwrap_or_else(PoisonError::into_inner);
            f(&*conn)
        }))
    }

    fn run_statement(
        &self,
        text: &str,
        binds: Vec<(String, Option<String>)>,
    ) -> BoxFuture<'static, Result<ResultStream>> {
        let conn = Arc::clone(&self.conn);
        let (busy, auto, read_only) = (
            Arc::clone(&self.busy),
            Arc::clone(&self.auto),
            self.read_only,
        );
        let refused = read_only.then(|| classify(text, Engine::Oracle));
        let text = text.to_owned();
        Box::pin(async move {
            if let Some(s) = refused.filter(Statement::changes) {
                return Err(DriverError::ReadOnly(s.verb));
            }
            let (head_tx, head) = tokio::sync::oneshot::channel();
            spawn_blocking(move || {
                let _busy = busy.lock().unwrap_or_else(PoisonError::into_inner);
                if read_only
                    && let Err(e) = transaction_open(&*conn).and_then(|open| {
                        if !open {
                            conn.execute("SET TRANSACTION READ ONLY", &[])?;
                        }
                        Ok(())
                    })
                {
                    let _ = head_tx.send(Err(e));
                    return;
                }
                run(&*conn, &text, &binds, auto.load(Ordering::Relaxed), head_tx);
            });
            head.await.map_err(|_| DriverError::Gone)?
        })
    }
}

impl Driver for Oracle {
    fn capabilities(&self) -> Capabilities {
        Capabilities::SQL
    }

    fn server_version(&self) -> &str {
        &self.version
    }

    fn execute<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<ResultStream>> {
        self.run_statement(text, Vec::new())
    }

    fn execute_bound<'a>(
        &'a self,
        text: &'a str,
        binds: &'a [(String, Option<String>)],
    ) -> BoxFuture<'a, Result<ResultStream>> {
        self.run_statement(text, binds.to_vec())
    }

    fn classify<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Statement>> {
        Box::pin(async move { Ok(classify(text, Engine::Oracle)) })
    }

    fn set_auto_commit(&self, on: bool) -> BoxFuture<'_, Result<()>> {
        self.auto.store(on, Ordering::Relaxed);
        Box::pin(async { Ok(()) })
    }

    fn end_transaction(&self, commit: bool) -> BoxFuture<'_, Result<()>> {
        self.call(move |conn| {
            if commit {
                conn.commit()?;
            } else {
                conn.rollback()?;
            }
            Ok(())
        })
    }

    fn transaction_open(&self) -> BoxFuture<'_, Result<bool>> {
        self.call(transaction_open)
    }

    /// Thick: a break on the session itself. Thin: `ALTER SYSTEM CANCEL SQL` from a
    /// side session, which needs the `ALTER SYSTEM` privilege; without it the error
    /// says so.
    fn cancel(&self) -> BoxFuture<'_, Result<()>> {
        if self.thick {
            let conn = Arc::clone(&self.conn);
            return Box::pin(blocking(move || conn.break_execution().unwrap_or(Ok(()))));
        }
        let (user, password, cs) = (
            self.user.clone(),
            self.password.clone(),
            self.connect_string.clone(),
        );
        let (sid, serial) = self.session;
        Box::pin(blocking(move || {
            let side = open_thin(&user, &password, &cs)?;
            match side.execute(&format!("ALTER SYSTEM CANCEL SQL '{sid}, {serial}'"), &[]) {
                Ok(_) => Ok(()),
                Err(e) if e.db_error_code() == Some(1031) => Err(DriverError::Invalid(
                    "Oracle stops a running statement with ALTER SYSTEM CANCEL SQL, which needs \
                     the ALTER SYSTEM privilege this user does not have"
                        .into(),
                )),
                Err(e) => Err(e.into()),
            }
        }))
    }

    fn table_info<'a>(
        &'a self,
        t: &'a crate::edit::TableRef,
    ) -> BoxFuture<'a, Result<crate::edit::TableInfo>> {
        let t = t.clone();
        self.call(move |conn| table::table_info(conn, &t))
    }

    /// Estimated: `EXPLAIN PLAN`. Measured: the statement runs (writes included) and
    /// its cursor's statistics are read, which needs dictionary privileges.
    fn explain<'a>(
        &'a self,
        statement: &'a str,
        analyze: bool,
    ) -> BoxFuture<'a, Result<crate::plan::Plan>> {
        if analyze && self.read_only {
            let s = classify(statement, Engine::Oracle);
            if s.changes() {
                return Box::pin(async move { Err(DriverError::ReadOnly(s.verb)) });
            }
        }
        let statement = statement.to_owned();
        let auto = self.auto.load(Ordering::Relaxed);
        self.call(move |conn| {
            let mut p = if analyze {
                plan::measured(conn, &statement, auto)?
            } else {
                plan::estimated(conn, &statement)?
            };
            p.database = texts(
                conn,
                "SELECT SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') FROM dual",
                &[],
            )?
            .first()
            .map(|r| col(r, 0));
            Ok(p)
        })
    }

    fn config(&self) -> BoxFuture<'_, Result<crate::config::ServerConfig>> {
        self.call(config::read)
    }

    fn apply_config<'a>(
        &'a self,
        cfg: &'a crate::config::ServerConfig,
        changes: &'a [crate::config::ConfigChange],
    ) -> BoxFuture<'a, Result<crate::config::Applied>> {
        if self.read_only {
            return Box::pin(async {
                Err(DriverError::Invalid(
                    "not applied: the connection is read-only".into(),
                ))
            });
        }
        let (cfg, changes) = (cfg.clone(), changes.to_vec());
        self.call(move |conn| Ok(config::apply(conn, &cfg, &changes)))
    }

    fn sessions(&self) -> BoxFuture<'_, Result<crate::sessions::Snapshot>> {
        let own = u64::try_from(self.session.0).unwrap_or(0);
        self.call(move |conn| sessions::snapshot(conn, own))
    }

    fn waiting_sessions(&self) -> BoxFuture<'_, Result<usize>> {
        self.call(sessions::waiting)
    }

    /// `ALTER SYSTEM CANCEL SQL`: needs the ALTER SYSTEM privilege.
    fn cancel_statement(&self, id: crate::sessions::SessionId) -> BoxFuture<'_, Result<()>> {
        self.call(move |conn| {
            let a = sessions::address(conn, id)?;
            conn.execute(&format!("ALTER SYSTEM CANCEL SQL {a}"), &[])?;
            Ok(())
        })
    }

    /// `ALTER SYSTEM KILL SESSION … IMMEDIATE`: its transaction rolls back.
    fn end_session(&self, id: crate::sessions::SessionId) -> BoxFuture<'_, Result<()>> {
        if u64::try_from(self.session.0) == Ok(id) {
            return Box::pin(async {
                Err(DriverError::Invalid(
                    "not ended: this is the session the screen itself uses".into(),
                ))
            });
        }
        self.call(move |conn| {
            let a = sessions::address(conn, id)?;
            conn.execute(&format!("ALTER SYSTEM KILL SESSION {a} IMMEDIATE"), &[])?;
            Ok(())
        })
    }

    fn last_deadlock(&self) -> BoxFuture<'_, Result<Option<String>>> {
        Box::pin(async { Err(DriverError::Unsupported(sessions::NO_DEADLOCK_REPORT)) })
    }

    fn structure<'a>(
        &'a self,
        t: &'a crate::edit::TableRef,
    ) -> BoxFuture<'a, Result<crate::structure::TableStructure>> {
        let t = t.clone();
        self.call(move |conn| structure::structure(conn, &t))
    }

    fn apply<'a>(
        &'a self,
        changes: &'a [crate::edit::Change],
        auto: bool,
    ) -> BoxFuture<'a, Result<()>> {
        if self.read_only {
            return Box::pin(async { Err(DriverError::ReadOnly("a submit".into())) });
        }
        let changes = changes.to_vec();
        self.call(move |conn| table::apply(conn, &changes, auto))
    }

    /// `[]`: the schemas. `[schema, …]`: the schema's groups, each loaded down to the
    /// columns in four dictionary queries, then walked down the rest of the path.
    fn diagram<'a>(&'a self, schema: &'a str) -> BoxFuture<'a, Result<crate::diagram::Diagram>> {
        let schema = schema.to_owned();
        self.call(move |conn| schema::diagram(conn, &schema))
    }

    fn children<'a>(&'a self, node: &'a ObjectPath) -> BoxFuture<'a, Result<Vec<ObjectNode>>> {
        let path = node.0.clone();
        self.call(move |conn| schema::children(conn, &path))
    }
}

/// Runs `text` and sends its result on `head`. `auto`: commits before the answer goes
/// out (after the last row, for a query), so other sessions see the change by then.
fn run(
    conn: &Connection,
    text: &str,
    binds: &[(String, Option<String>)],
    auto: bool,
    head: tokio::sync::oneshot::Sender<Result<ResultStream>>,
) {
    // shortcut: a failed auto-commit only shows as the open-transaction mark.
    let commit = || {
        if auto && let Err(e) = conn.commit() {
            eprintln!("deadlock: oracle: commit after statement: {e}");
        }
    };
    if let Some(described) = describe(conn, text) {
        commit();
        let _ = head.send(described);
        return;
    }
    let mut head = Some(head);
    let tx = std::cell::RefCell::new(None);
    let size = page_size();
    let mut page = Vec::with_capacity(size);
    let result = conn.stream(
        text,
        binds,
        &mut |columns| {
            let (sender, stream) = ResultStream::channel(columns);
            *tx.borrow_mut() = Some(sender);
            head.take().is_some_and(|h| h.send(Ok(stream)).is_ok())
        },
        &mut |row| {
            page.push(row);
            page.len() < size
                || tx
                    .borrow()
                    .as_ref()
                    .is_some_and(|t| t.blocking_send(Ok(std::mem::take(&mut page))).is_ok())
        },
    );
    let tx = tx.into_inner();
    match (result, head) {
        // Not a query.
        (Ok(Some(n)), Some(head)) => {
            let r = compile_errors(conn, text).and_then(|()| {
                let mut s = ResultStream::affected(n);
                s.output = conn.output()?;
                Ok(s)
            });
            commit();
            let _ = head.send(r);
        }
        (Err(e), Some(head)) => {
            commit();
            let _ = head.send(Err(e));
        }
        // A query: its rows went out, or the fetch failed after its columns did.
        (r, _) => {
            if let Some(t) = &tx {
                let _ = match r {
                    Err(e) => t.blocking_send(Err(e)),
                    Ok(_) if !page.is_empty() => t.blocking_send(Ok(page)),
                    Ok(_) => Ok(()),
                };
            }
            // shortcut: a query's DBMS_OUTPUT (from functions it calls) is read and
            // dropped, so it does not show under the next statement.
            let _ = conn.output();
            commit();
        }
    }
}

/// `DESC[RIBE] [schema.]name`, a SQL*Plus command rather than SQL: the columns of a
/// table or view (through a synonym too), as SQL*Plus lists them. `None` when `text`
/// is not a `DESC`.
fn describe(conn: &Connection, text: &str) -> Option<Result<ResultStream>> {
    let words = name_tokens(text);
    let verb = words.first()?.0.to_ascii_uppercase();
    if verb != "DESC" && verb != "DESCRIBE" {
        return None;
    }
    let name = |i: usize| {
        words.get(i).map(|(w, quoted)| {
            if *quoted {
                w.clone()
            } else {
                w.to_ascii_uppercase()
            }
        })
    };
    // Everything after the name must be gone: `name_tokens` stops at other characters.
    let rest = text.trim().trim_end_matches(';').split_whitespace().count();
    let (owner, object) = match (name(1), name(2).as_deref(), name(3), words.len(), rest) {
        (Some(o), Some("."), Some(n), 4, 2..=4) => (Some(o), n),
        (Some(n), None, None, 2, 2) => (None, n),
        _ => {
            return Some(Err(DriverError::Invalid(
                "DESC takes one table or view name, like DESC orders or DESC hr.employees".into(),
            )));
        }
    };
    Some(describe_object(conn, owner, &object))
}

fn describe_object(conn: &Connection, owner: Option<String>, name: &str) -> Result<ResultStream> {
    let columns = |owner: &Option<String>, name: &str| {
        texts(
            conn,
            "SELECT COLUMN_NAME, DECODE(NULLABLE, 'N', 'NOT NULL'), DATA_TYPE,              TO_CHAR(DATA_LENGTH), TO_CHAR(DATA_PRECISION), TO_CHAR(DATA_SCALE),              TO_CHAR(CHAR_LENGTH), CHAR_USED FROM ALL_TAB_COLUMNS              WHERE OWNER = NVL(:1, SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA'))              AND TABLE_NAME = :2 ORDER BY COLUMN_ID",
            &[owner, &name],
        )
    };
    let mut rows = columns(&owner, name)?;
    if rows.is_empty() {
        // A synonym: the schema's own, else a public one.
        let target = texts(
            conn,
            "SELECT TABLE_OWNER, TABLE_NAME FROM ALL_SYNONYMS WHERE SYNONYM_NAME = :2              AND OWNER IN (NVL(:1, SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')), 'PUBLIC')              ORDER BY DECODE(OWNER, 'PUBLIC', 1, 0)",
            &[&owner, &name],
        )?;
        if let Some(t) = target.first() {
            rows = columns(&Some(col(t, 0)), &col(t, 1))?;
        }
    }
    if rows.is_empty() {
        let kind = texts(
            conn,
            "SELECT LOWER(OBJECT_TYPE) FROM ALL_OBJECTS WHERE OBJECT_NAME = :2              AND OWNER = NVL(:1, SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA'))",
            &[&owner, &name],
        )?;
        return Err(DriverError::Invalid(match kind.first() {
            Some(k) => format!(
                "DESC lists the columns of tables and views; {name} is a {}. Open it \
                 from the sidebar, or query ALL_ARGUMENTS for its parameters.",
                col(k, 0)
            ),
            None => format!("DESC: there is no table or view {name} that you can see"),
        }));
    }
    let header = ["Name", "Null?", "Type"].map(|n| Column {
        name: n.into(),
        type_name: "VARCHAR2".into(),
    });
    let text = |s: String| {
        if s.is_empty() {
            Value::Null
        } else {
            Value::Text(s)
        }
    };
    let rows = rows
        .iter()
        .map(|r| {
            vec![
                Value::Text(col(r, 0)),
                text(col(r, 1)),
                Value::Text(schema::type_label(r, 2).to_ascii_uppercase()),
            ]
        })
        .collect();
    Ok(ResultStream::ready(header.to_vec(), rows))
}

/// After a `CREATE` of PL/SQL that compiled with errors, the errors with their line and
/// column, as an error: the object exists but cannot run.
fn compile_errors(conn: &Connection, text: &str) -> Result<()> {
    let warned = conn
        .last_warning()?
        .is_some_and(|w| w.contains("compilation error") || w.contains("ORA-24344"));
    let Some((owner, name, kind)) = warned.then(|| created_object(text)).flatten() else {
        return Ok(());
    };
    let rows = texts(
        conn,
        "SELECT TO_CHAR(LINE), TO_CHAR(POSITION), TEXT FROM ALL_ERRORS \
         WHERE OWNER = NVL(:1, SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')) AND NAME = :2 \
         AND TYPE = :3 ORDER BY SEQUENCE",
        &[&owner, &name, &kind],
    )?;
    let lines: Vec<String> = rows
        .iter()
        .map(|r| {
            format!(
                "Line {}, column {}: {}",
                col(r, 0),
                col(r, 1),
                col(r, 2).trim_end()
            )
        })
        .collect();
    Err(DriverError::Invalid(format!(
        "{kind} {name} was created with compilation errors:\n{}",
        if lines.is_empty() {
            "the server lists none you can see".to_owned()
        } else {
            lines.join("\n")
        }
    )))
}

/// The owner, name and `ALL_ERRORS` type of the PL/SQL object `text` creates.
fn created_object(text: &str) -> Option<(Option<String>, String, String)> {
    let words = name_tokens(text);
    let mut w = words.iter().map(|(t, quoted)| {
        if *quoted {
            t.clone()
        } else {
            t.to_ascii_uppercase()
        }
    });
    let mut next = w.next();
    if next.as_deref() != Some("CREATE") {
        return None;
    }
    next = w.next();
    if next.as_deref() == Some("OR") {
        w.next();
        next = w.next();
    }
    while matches!(
        next.as_deref(),
        Some("EDITIONABLE" | "NONEDITIONABLE" | "EDITIONING" | "FORCE" | "NOFORCE")
    ) {
        next = w.next();
    }
    let mut kind = next?;
    if !matches!(
        kind.as_str(),
        "PROCEDURE" | "FUNCTION" | "PACKAGE" | "TRIGGER" | "TYPE" | "VIEW"
    ) {
        return None;
    }
    let mut name = w.next()?;
    if name == "BODY" {
        kind.push_str(" BODY");
        name = w.next()?;
    }
    if w.next().as_deref() == Some(".") {
        return Some((Some(name), w.next()?, kind));
    }
    Some((None, name, kind))
}

/// Words, quoted names (without quotes, flagged) and dots, up to the first other
/// character.
fn name_tokens(text: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some(&(i, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '"' {
            chars.next();
            let rest = &text[i + 1..];
            let end = rest.find('"').unwrap_or(rest.len());
            out.push((rest[..end].to_owned(), true));
            while chars.peek().is_some_and(|&(j, _)| j <= i + 1 + end) {
                chars.next();
            }
        } else if c == '.' {
            out.push((".".into(), false));
            chars.next();
        } else if c.is_alphanumeric() || matches!(c, '_' | '$' | '#') {
            let mut w = String::new();
            while let Some(&(_, c)) = chars.peek() {
                if !(c.is_alphanumeric() || matches!(c, '_' | '$' | '#')) {
                    break;
                }
                w.push(c);
                chars.next();
            }
            out.push((w, false));
        } else {
            break;
        }
        if out.len() > 12 {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_objects_are_named_for_all_errors() {
        let o = |t: &str| created_object(t);
        assert_eq!(
            o("create or replace procedure p_x as begin null; end;"),
            Some((None, "P_X".into(), "PROCEDURE".into()))
        );
        assert_eq!(
            o("CREATE OR REPLACE EDITIONABLE PACKAGE BODY app.\"Pk\" AS END;"),
            Some((Some("APP".into()), "Pk".into(), "PACKAGE BODY".into()))
        );
        assert_eq!(
            o("create trigger t1 before insert on x begin null; end;"),
            Some((None, "T1".into(), "TRIGGER".into()))
        );
        assert_eq!(o("create table t (a number)"), None);
        assert_eq!(o("select 1 from dual"), None);
    }
}
