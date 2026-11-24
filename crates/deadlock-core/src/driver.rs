//! The driver boundary: one trait the UI talks to, whatever the engine.
//!
//! Every method runs on the core's tokio runtime ([`Runtime`]); the UI awaits the
//! answer over a channel and never blocks.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use chrono::{NaiveDate, NaiveDateTime, TimeDelta};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::classify::Statement;
use crate::connection::{ConnectionConfig, Engine};

/// A boxed future, so [`Driver`] stays usable as `dyn Driver`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Errors from connecting and running statements. The text is shown to the user.
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error(transparent)]
    Mysql(#[from] mysql_async::Error),
    #[error(transparent)]
    Oracle(#[from] oracledb::Error),
    /// From the Thick Oracle driver (Instant Client).
    #[error(transparent)]
    OracleThick(#[from] oracle::Error),
    /// The Thin Oracle driver cannot sign in to this account: it has only an old
    /// password verifier. The Thick driver can.
    #[error(
        "The server accepted the connection, but this account's password uses an old \
         password type (the 10G verifier) that the built-in Thin driver does not support. \
         Switch this connection to the Thick driver, which uses Oracle Instant Client and \
         supports it. (Thin driver: {0})"
    )]
    OldVerifier(String),
    #[error(transparent)]
    Redis(#[from] redis::RedisError),
    #[error("{0}")]
    Unsupported(&'static str),
    #[error("{0}")]
    Invalid(String),
    #[error("the connection's background task stopped")]
    Gone,
    /// The SSH tunnel did not open: the SSH hop failed, or its host key needs a decision.
    #[error(transparent)]
    Tunnel(#[from] crate::ssh::TunnelError),
    /// Refused before sending: the connection is read-only.
    #[error("not run: the connection is read-only, and {0} changes data or schema")]
    ReadOnly(String),
    /// The session died under a statement that is not safe to run twice.
    #[error(
        "The connection to the server was lost while this ran, so it may or may not have \
         taken effect. Deadlock reconnected and did not run it again: check, then run it again \
         if needed."
    )]
    NotRetried,
    /// The session died with a transaction open; the server rolled it back.
    #[error(
        "The connection to the server was lost, so the open transaction was rolled back: its \
         uncommitted changes are gone. Deadlock reconnected; the next statement starts fresh."
    )]
    TransactionLost,
    /// The session died and opening a new one failed too.
    #[error("The connection to the server was lost, and reconnecting failed: {0}")]
    ReconnectFailed(String),
}

impl DriverError {
    /// Whether the server session behind the connection is gone (killed, timed out, the
    /// socket closed), so only a new connection helps. A cancelled statement is not:
    /// MySQL's 1317 (query interrupted) is what `KILL QUERY` sends to a live session.
    pub fn is_connection_lost(&self) -> bool {
        match self {
            Self::Mysql(e) => {
                use mysql_async::{DriverError as D, Error as E};
                match e {
                    E::Io(_) | E::Driver(D::ConnectionClosed | D::PoolDisconnected) => true,
                    // Gone away, lost during query, shutdown, killed, idle timeout.
                    E::Server(s) => matches!(s.code, 2006 | 2013 | 1053 | 1927 | 4031),
                    _ => false,
                }
            }
            Self::Oracle(e) => {
                use oracledb::ErrorKind as K;
                matches!(
                    e.kind(),
                    K::DeadConnection | K::NotConnected | K::StreamOperation
                ) || e.db_error_code().is_some_and(ora_lost)
                    || lost_text(&e.to_string())
            }
            Self::OracleThick(e) => {
                e.db_error()
                    .and_then(|d| usize::try_from(d.code()).ok())
                    .is_some_and(ora_lost)
                    || lost_text(&e.to_string())
            }
            Self::Redis(e) => e.is_io_error() || e.is_connection_dropped(),
            _ => false,
        }
    }
}

/// ORA- codes of a session that is gone: killed (28), not logged on (1012), idle limit
/// (2396), end of file on the channel (3113, 3114, 3135), TNS lost contact (12537, 12547).
fn ora_lost(code: usize) -> bool {
    matches!(code, 28 | 1012 | 2396 | 3113 | 3114 | 3135 | 12537 | 12547)
}

/// The same in an error's text, and ODPI-C's "not connected" / "connection closed".
fn lost_text(text: &str) -> bool {
    [
        "ORA-00028",
        "ORA-01012",
        "ORA-02396",
        "ORA-03113",
        "ORA-03114",
        "ORA-03135",
        "ORA-12537",
        "ORA-12547",
        "DPI-1010",
        "DPI-1080",
    ]
    .iter()
    .any(|c| text.contains(c))
}

/// The state of a self-healing connection ([`Driver::link`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Up,
    /// The session was lost; a new one is being opened.
    Reconnecting,
    /// Reconnecting failed; what failed.
    Lost(String),
}

pub type Result<T, E = DriverError> = std::result::Result<T, E>;

/// What the engine supports. Toolbar buttons and screens enable from this.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub transactions: bool,
    pub explain: bool,
    pub structure: bool,
    pub sessions: bool,
    pub config: bool,
    pub diagram: bool,
}

impl Capabilities {
    /// Everything: the SQL engines.
    pub const SQL: Self = Self {
        transactions: true,
        explain: true,
        structure: true,
        sessions: true,
        config: true,
        diagram: true,
    };
}

/// A cell. Engines map their types onto these; anything without a better fit is text.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    /// Exact numbers (DECIMAL, NUMBER) keep their digits as text.
    Decimal(String),
    Text(String),
    Bytes(Vec<u8>),
    Date(NaiveDate),
    DateTime(NaiveDateTime),
    /// MySQL `TIME` is a signed duration, up to ±838 hours.
    Time(TimeDelta),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("null"),
            Self::Bool(v) => write!(f, "{v}"),
            Self::Int(v) => write!(f, "{v}"),
            Self::UInt(v) => write!(f, "{v}"),
            Self::Float(v) => write!(f, "{v}"),
            Self::Decimal(v) | Self::Text(v) => f.write_str(v),
            Self::Bytes(b) => {
                f.write_str("0x")?;
                b.iter().try_for_each(|x| write!(f, "{x:02X}"))
            }
            Self::Date(d) => write!(f, "{d}"),
            Self::DateTime(d) => write!(f, "{d}"),
            Self::Time(t) => {
                let sign = if *t < TimeDelta::zero() { "-" } else { "" };
                let t = t.abs();
                let s = t.num_seconds();
                write!(f, "{sign}{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)?;
                match t.subsec_nanos() / 1000 {
                    0 => Ok(()),
                    us => write!(f, ".{us:06}"),
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    /// The engine's own type name, e.g. `VARCHAR`, `NUMBER`.
    pub type_name: String,
}

pub type Row = Vec<Value>;

/// Rows per page sent from a driver: the "Rows per page" setting, 500 until set.
static PAGE_SIZE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(500);

/// Rows per page sent from a driver.
pub fn page_size() -> usize {
    PAGE_SIZE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Sets the rows per page for pages read from now on.
pub fn set_page_size(rows: usize) {
    PAGE_SIZE.store(rows.max(1), std::sync::atomic::Ordering::Relaxed);
}

/// The answer to one statement. Rows arrive in pages of up to [`page_size`].
#[derive(Debug)]
pub struct ResultStream {
    /// Empty for statements that return no rows.
    pub columns: Vec<Column>,
    /// Rows changed, for statements that return no rows.
    pub affected_rows: Option<u64>,
    /// What the statement printed, for Messages: Oracle's `DBMS_OUTPUT` lines.
    pub output: Vec<String>,
    pages: mpsc::Receiver<Result<Vec<Row>>>,
    next: Option<oneshot::Receiver<Result<ResultStream>>>,
}

impl ResultStream {
    /// A stream fed by the returned sender. Dropping the stream stops the producer
    /// at its next send.
    pub fn channel(columns: Vec<Column>) -> (mpsc::Sender<Result<Vec<Row>>>, Self) {
        let (tx, pages) = mpsc::channel(2);
        let stream = Self {
            columns,
            affected_rows: None,
            output: Vec::new(),
            pages,
            next: None,
        };
        (tx, stream)
    }

    /// A stream with no rows.
    pub fn affected(rows: u64) -> Self {
        let (_, mut stream) = Self::channel(Vec::new());
        stream.affected_rows = Some(rows);
        stream
    }

    /// A stream with all rows already known.
    pub fn ready(columns: Vec<Column>, rows: Vec<Row>) -> Self {
        let (tx, stream) = Self::channel(columns);
        // Capacity 2: the first send cannot fail or wait.
        let _ = tx.try_send(Ok(rows));
        stream
    }

    /// A stream followed by another result set, which the producer sends on the
    /// returned sender once this one's rows are read or this stream is dropped.
    pub(crate) fn followed(mut self) -> (oneshot::Sender<Result<ResultStream>>, Self) {
        let (tx, rx) = oneshot::channel();
        self.next = Some(rx);
        (tx, self)
    }

    /// The statement's next result set (MySQL procedures and multi-statement text send
    /// several). It arrives once this stream's rows have been read or the stream is
    /// dropped; the receiver fails when there is none.
    pub fn next_result(&mut self) -> Option<oneshot::Receiver<Result<ResultStream>>> {
        self.next.take()
    }

    /// The next page, or `None` when all rows have arrived.
    pub async fn next_page(&mut self) -> Option<Result<Vec<Row>>> {
        self.pages.recv().await
    }

    /// Every remaining row. For tests and small results.
    ///
    /// # Errors
    /// The first error a page carried.
    pub async fn collect(mut self) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        while let Some(page) = self.next_page().await {
            rows.extend(page?);
        }
        Ok(rows)
    }
}

/// A node of the sidebar tree, by its path from the connection down.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectPath(pub Vec<String>);

/// What a tree node is. Decides its icon, indent and whether it opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    Database,
    /// A group under a database (Tables, Views, Procedures, Triggers…; see
    /// [`crate::objects::ObjectKind::group`]), or Triggers under a table.
    Group,
    Table,
    View,
    Routine,
    Column,
    /// Redis: keys sharing a prefix that ends in [`crate::redis::DELIMITER`].
    Namespace,
    /// Redis: one key; `detail` is its type.
    Key,
    /// Redis: a "Load more" row.
    More,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectNode {
    pub name: String,
    pub kind: NodeKind,
    /// The type label on the right: a column's type, a routine's kind.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// A system schema: listed last and dimmed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub system: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub primary_key: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub foreign_key: bool,
    /// Children loaded with this node; `None` means ask [`Driver::children`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<ObjectNode>>,
}

impl ObjectNode {
    pub fn new(name: impl Into<String>, kind: NodeKind) -> Self {
        Self {
            name: name.into(),
            kind,
            detail: String::new(),
            system: false,
            primary_key: false,
            foreign_key: false,
            children: None,
        }
    }

    /// Whether the node shows a chevron. A group opens only when it holds something.
    pub fn has_children(&self) -> bool {
        match self.kind {
            NodeKind::Database | NodeKind::Table | NodeKind::View | NodeKind::Namespace => true,
            NodeKind::Group => self.children.as_ref().is_some_and(|c| !c.is_empty()),
            NodeKind::Routine | NodeKind::Column | NodeKind::Key | NodeKind::More => false,
        }
    }
}

/// One open connection to a server.
pub trait Driver: Send + Sync {
    fn capabilities(&self) -> Capabilities;

    /// Product and version, e.g. `MySQL 8.4.3`, as detected from the server.
    fn server_version(&self) -> &str;

    /// Runs one statement (or, for Redis, one command line).
    fn execute<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<ResultStream>>;

    /// Runs one statement with values for its bind variables, by upper-case name
    /// ([`crate::split::bind_names`]); `None` binds NULL. Engines without binds refuse
    /// any.
    fn execute_bound<'a>(
        &'a self,
        text: &'a str,
        binds: &'a [(String, Option<String>)],
    ) -> BoxFuture<'a, Result<ResultStream>> {
        if binds.is_empty() {
            return self.execute(text);
        }
        Box::pin(async { Err(DriverError::Unsupported("no bind variables on this engine")) })
    }

    /// Stops the statement that is running, if any.
    fn cancel(&self) -> BoxFuture<'_, Result<()>>;

    /// What `text` does. SQL engines read it from its keywords; Redis asks the server.
    fn classify<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Statement>>;

    /// Auto (`true`): each statement commits. Manual: the session holds a transaction
    /// until [`Driver::end_transaction`]. Engines without transactions refuse.
    fn set_auto_commit(&self, _on: bool) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Err(DriverError::Unsupported("no transactions on this engine")) })
    }

    /// Commits (`true`) or rolls back the open transaction.
    fn end_transaction(&self, _commit: bool) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Err(DriverError::Unsupported("no transactions on this engine")) })
    }

    /// Whether the session holds an open transaction, as the server reports it. Waits
    /// for the running statement to finish. Fails when the connection is gone.
    fn transaction_open(&self) -> BoxFuture<'_, Result<bool>> {
        Box::pin(async { Ok(false) })
    }

    /// A table's columns, key and kind, for the data editor. Engines without a data
    /// editor refuse.
    fn table_info<'a>(
        &'a self,
        _table: &'a crate::edit::TableRef,
    ) -> BoxFuture<'a, Result<crate::edit::TableInfo>> {
        Box::pin(async { Err(DriverError::Unsupported("no table editor on this engine")) })
    }

    /// A table's or view's columns, indexes, foreign keys both ways, facts and DDL,
    /// for the Structure view. Engines without one refuse.
    fn structure<'a>(
        &'a self,
        _table: &'a crate::edit::TableRef,
    ) -> BoxFuture<'a, Result<crate::structure::TableStructure>> {
        Box::pin(async {
            Err(DriverError::Unsupported(
                "no table structure on this engine",
            ))
        })
    }

    /// The plan for one statement. `analyze`: run the statement and measure each step,
    /// which executes it, writes included. Engines without [`Capabilities::explain`]
    /// refuse.
    fn explain<'a>(
        &'a self,
        _statement: &'a str,
        _analyze: bool,
    ) -> BoxFuture<'a, Result<crate::plan::Plan>> {
        Box::pin(async { Err(DriverError::Unsupported("no explain plans on this engine")) })
    }

    /// The server's sessions, what they hold and who waits on whom. Engines without
    /// [`Capabilities::sessions`] refuse.
    fn sessions(&self) -> BoxFuture<'_, Result<crate::sessions::Snapshot>> {
        Box::pin(async {
            Err(DriverError::Unsupported(
                "no sessions screen on this engine",
            ))
        })
    }

    /// How many sessions wait on a lock now: a light check for the status bar.
    fn waiting_sessions(&self) -> BoxFuture<'_, Result<usize>> {
        Box::pin(async {
            Err(DriverError::Unsupported(
                "no sessions screen on this engine",
            ))
        })
    }

    /// Stops the statement session `id` runs; the session stays.
    fn cancel_statement(&self, _id: crate::sessions::SessionId) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            Err(DriverError::Unsupported(
                "no sessions screen on this engine",
            ))
        })
    }

    /// Ends session `id`; its open transaction rolls back. This driver's own session
    /// is refused.
    fn end_session(&self, _id: crate::sessions::SessionId) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            Err(DriverError::Unsupported(
                "no sessions screen on this engine",
            ))
        })
    }

    /// Empties the server's slow log ([`crate::sessions::Snapshot::slow_log`]).
    fn reset_slow_log(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Err(DriverError::Unsupported("no slow log on this engine")) })
    }

    /// The Redis driver, for the key browser's own calls; `None` for other engines.
    fn as_redis(&self) -> Option<&crate::redis_driver::Redis> {
        None
    }

    /// The server's report of the last deadlock it detected, as text; `None` when it
    /// has detected none. Parse it with [`crate::sessions::parse_deadlock`].
    fn last_deadlock(&self) -> BoxFuture<'_, Result<Option<String>>> {
        Box::pin(async {
            Err(DriverError::Unsupported(
                "no deadlock report on this engine",
            ))
        })
    }

    /// The server's global variables, with what the server reports about each, and
    /// whether this session may change them. Engines without [`Capabilities::config`]
    /// refuse.
    fn config(&self) -> BoxFuture<'_, Result<crate::config::ServerConfig>> {
        Box::pin(async {
            Err(DriverError::Unsupported(
                "no server configuration screen on this engine",
            ))
        })
    }

    /// Runs `changes` in order with the statements `config` generates for them, and
    /// stops at the first that fails. Variable changes are not transactional: the ones
    /// before it stay applied.
    fn apply_config<'a>(
        &'a self,
        _config: &'a crate::config::ServerConfig,
        _changes: &'a [crate::config::ConfigChange],
    ) -> BoxFuture<'a, Result<crate::config::Applied>> {
        Box::pin(async {
            Err(DriverError::Unsupported(
                "no server configuration screen on this engine",
            ))
        })
    }

    /// Runs `changes` as one unit: each must touch exactly one row, or all of them are
    /// undone. `auto` (the session commits each statement): they run in their own
    /// transaction and commit at the end. Manual: they run inside the open
    /// transaction, behind a savepoint, and stay uncommitted.
    fn apply<'a>(
        &'a self,
        _changes: &'a [crate::edit::Change],
        _auto: bool,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Err(DriverError::Unsupported("no table editor on this engine")) })
    }

    /// A schema's tables with their columns and foreign keys, for the schema diagram,
    /// in one bulk load. Engines without [`Capabilities::diagram`] refuse.
    fn diagram<'a>(&'a self, _schema: &'a str) -> BoxFuture<'a, Result<crate::diagram::Diagram>> {
        Box::pin(async { Err(DriverError::Unsupported("no schema diagram on this engine")) })
    }

    /// Search everywhere: object names, columns and source text containing `text`,
    /// ignoring case, read from the server's metadata with `text` bound, never written
    /// into the SQL. At most [`crate::search::LIMIT`] rows per query; system schemas
    /// are left out, and so are the schemas named in `hidden` (exact names, bound, so
    /// they never count toward the limit; Redis: `db<n>`). `stop` is checked between queries: a newer search sets it.
    fn search_everywhere<'a>(
        &'a self,
        _text: &'a str,
        _hidden: &'a [String],
        _stop: &'a std::sync::atomic::AtomicBool,
    ) -> BoxFuture<'a, Result<Vec<crate::search::Hit>>> {
        Box::pin(async { Err(DriverError::Unsupported("no search on this engine")) })
    }

    /// The `CREATE` statement of routine, trigger or event `name` in `schema`; `kind`
    /// as [`crate::search::Hit::kind`] names it.
    fn definition<'a>(
        &'a self,
        _schema: &'a str,
        _name: &'a str,
        _kind: &'a str,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async { Err(DriverError::Unsupported("no definitions on this engine")) })
    }

    /// The flavor objects are managed in ([`crate::objects`]); `None` on engines
    /// without schema objects (Redis). MySQL and MariaDB are told apart from the
    /// server's version.
    fn object_flavor(&self) -> Option<crate::objects::Flavor> {
        None
    }

    /// One object for the object editor: typed properties, the server's source,
    /// status, compile errors, grants and facts. A body the account may not see
    /// comes back with [`crate::objects::ObjectDefinition::read_only`] set.
    fn object<'a>(
        &'a self,
        _object: &'a crate::objects::ObjectRef,
    ) -> BoxFuture<'a, Result<crate::objects::ObjectDefinition>> {
        Box::pin(async { Err(DriverError::Unsupported("no object editor on this engine")) })
    }

    /// Runs a plan from [`crate::objects::plan`] (or `drop_plan`, `rename_plan`,
    /// `recompile_plan`, `revert_plan`) as its steps say, and reports each step. The
    /// error is only for what stops it before the first statement (read-only
    /// connection, an open transaction DDL would commit, a lost connection); a
    /// failed statement is in the outcome. When the object ends up missing, the
    /// original is written to `backup` when given. Oracle: compile errors are read
    /// after, and the state is `Invalid` when there are any.
    fn apply_object<'a>(
        &'a self,
        _plan: &'a crate::objects::ChangePlan,
        _backup: Option<&'a crate::objects::Backup>,
    ) -> BoxFuture<'a, Result<crate::objects::ApplyOutcome>> {
        Box::pin(async { Err(DriverError::Unsupported("no object editor on this engine")) })
    }

    /// The children of `node` in the sidebar tree. A node may come with its own
    /// children already loaded (MySQL loads a whole database at once).
    fn children<'a>(&'a self, _node: &'a ObjectPath) -> BoxFuture<'a, Result<Vec<ObjectNode>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// The server's id of this connection's session (MySQL `CONNECTION_ID()`, Oracle
    /// SID), where the engine has one.
    fn session_id(&self) -> Option<u64> {
        None
    }

    /// Follows reconnects, for connections that heal themselves after a lost session.
    fn link(&self) -> Option<tokio::sync::watch::Receiver<Link>> {
        None
    }
}

/// Opens a connection, through its SSH tunnel when it has one (with the secret given
/// to [`crate::ssh::remember`]). Call on the core runtime.
///
/// SQL connections heal themselves: a lost session is replaced ([`crate::heal`]).
///
/// # Errors
/// Fails when the SSH server or the database cannot be reached or refuses the
/// sign-in; the text says which hop failed.
pub async fn connect(config: &ConnectionConfig, password: &str) -> Result<Arc<dyn Driver>> {
    let driver = dial(config, password).await?;
    // Redis heals in its connection manager.
    if config.engine == Engine::Redis {
        return Ok(driver);
    }
    Ok(Arc::new(crate::heal::Healing::new(
        config.clone(),
        password.to_owned(),
        driver,
    )))
}

/// One connection, without healing: the first one, and each reconnect.
pub(crate) async fn dial(config: &ConnectionConfig, password: &str) -> Result<Arc<dyn Driver>> {
    if !config.ssh_tunnel {
        return open(config, password, None).await;
    }
    let tunnel = crate::ssh::open(
        &config.ssh,
        &crate::ssh::secret(&config.ssh).unwrap_or_default(),
        &config.host,
        config.port,
        crate::ssh::KnownHosts::default(),
    )
    .await?;
    let local = ConnectionConfig {
        host: "127.0.0.1".into(),
        port: tunnel.local.port(),
        ..config.clone()
    };
    open(&local, password, Some(tunnel)).await.map_err(|e| {
        DriverError::Invalid(format!(
            "database {}:{} through the SSH tunnel: {e}",
            config.host, config.port
        ))
    })
}

async fn open(
    config: &ConnectionConfig,
    password: &str,
    tunnel: Option<crate::ssh::Tunnel>,
) -> Result<Arc<dyn Driver>> {
    Ok(match config.engine {
        Engine::Mysql | Engine::Mariadb => {
            let mut d = crate::mysql::connect(config, password).await?;
            d.tunnel = tunnel;
            Arc::new(d)
        }
        Engine::Oracle => {
            let mut d = crate::oracle::connect(config, password).await?;
            d.tunnel = tunnel;
            Arc::new(d)
        }
        Engine::Redis => {
            let mut d = crate::redis_driver::connect(config, password).await?;
            d.tunnel = tunnel;
            Arc::new(d)
        }
    })
}

/// The core's tokio runtime. The UI hands it futures and awaits the answers on its
/// own executor.
#[derive(Debug)]
pub struct Runtime(tokio::runtime::Runtime);

impl Runtime {
    /// # Errors
    /// Fails when the OS refuses the worker threads.
    pub fn new() -> std::io::Result<Self> {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("deadlock-db")
            .enable_all()
            .build()
            .map(Self)
    }

    /// Runs `future` on the runtime. The receiver works on any executor; it fails
    /// only when the task panicked.
    pub fn spawn<F>(&self, future: F) -> oneshot::Receiver<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send,
    {
        let (tx, rx) = oneshot::channel();
        self.0.spawn(async move {
            let _ = tx.send(future.await);
        });
        rx
    }

    /// Runs blocking `f` (the keychain, say) on the runtime's blocking pool.
    pub fn spawn_blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> oneshot::Receiver<T> {
        let (tx, rx) = oneshot::channel();
        self.0.spawn_blocking(move || {
            let _ = tx.send(f());
        });
        rx
    }

    /// Blocks the calling thread on `future`. For tests and examples, never the UI thread.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.0.block_on(future)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_display_like_the_grid() {
        assert_eq!(Value::Null.to_string(), "null");
        assert_eq!(Value::Bytes(vec![0xDE, 0xAD]).to_string(), "0xDEAD");
        let t = -(TimeDelta::hours(838) + TimeDelta::minutes(59) + TimeDelta::seconds(59));
        assert_eq!(Value::Time(t).to_string(), "-838:59:59");
        let t = TimeDelta::seconds(61) + TimeDelta::microseconds(5);
        assert_eq!(Value::Time(t).to_string(), "00:01:01.000005");
    }

    #[test]
    fn lost_connections_are_told_from_other_errors() {
        use std::io::{Error as Io, ErrorKind as K};
        let my = |e: mysql_async::Error| DriverError::Mysql(e).is_connection_lost();
        let server = |code| {
            mysql_async::Error::Server(mysql_async::ServerError {
                code,
                message: String::new(),
                state: String::new(),
            })
        };
        for kind in [K::BrokenPipe, K::ConnectionReset, K::UnexpectedEof] {
            assert!(my(mysql_async::Error::from(Io::from(kind))), "{kind:?}");
        }
        // EIO, what the user saw as "Input/output error".
        assert!(my(mysql_async::Error::from(Io::from_raw_os_error(5))));
        assert!(my(mysql_async::DriverError::ConnectionClosed.into()));
        assert!(my(mysql_async::DriverError::PoolDisconnected.into()));
        for code in [2006, 2013, 1053, 1927, 4031] {
            assert!(my(server(code)), "{code}");
        }
        // A cancelled statement, a syntax error, a duplicate key: the session lives.
        for code in [1317, 1064, 1062] {
            assert!(!my(server(code)), "{code}");
        }

        let thin = DriverError::Oracle(oracledb::Error::from(Io::from(K::ConnectionReset)));
        assert!(thin.is_connection_lost());
        let thick = |m: &'static str| {
            DriverError::OracleThick(oracle::Error::new(oracle::ErrorKind::OciError, m))
                .is_connection_lost()
        };
        assert!(thick("ORA-00028: your session has been killed"));
        assert!(thick("ORA-03113: end-of-file on communication channel"));
        assert!(thick("ORA-03135: connection lost contact"));
        assert!(thick("ORA-12547: TNS:lost contact"));
        assert!(thick("DPI-1010: not connected"));
        assert!(thick("DPI-1080: connection was closed by ORA-3113"));
        assert!(!thick("ORA-00942: table or view does not exist"));

        let redis = |e: redis::RedisError| DriverError::Redis(e).is_connection_lost();
        assert!(redis(Io::from(K::BrokenPipe).into()));
        assert!(redis(Io::from(K::ConnectionReset).into()));
        assert!(!redis((redis::ErrorKind::Parse, "WRONGTYPE").into()));

        for e in [
            DriverError::Invalid("Input/output error".into()),
            DriverError::Gone,
            DriverError::NotRetried,
            DriverError::ReadOnly("UPDATE".into()),
        ] {
            assert!(!e.is_connection_lost(), "{e}");
        }
    }

    #[test]
    fn ready_stream_yields_its_rows_once() {
        let rt = Runtime::new().unwrap();
        let rows = rt.block_on(ResultStream::ready(vec![], vec![vec![Value::Int(1)]]).collect());
        assert_eq!(rows.unwrap(), vec![vec![Value::Int(1)]]);
        let s = ResultStream::affected(3);
        assert_eq!(s.affected_rows, Some(3));
        assert!(rt.block_on(s.collect()).unwrap().is_empty());
    }
}
