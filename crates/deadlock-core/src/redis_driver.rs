//! Redis on the `redis` crate, over RESP3 where the server speaks it. `execute` takes
//! one command line, e.g. `GET foo`.
//!
//! The key browser and the key tabs never send a command whose cost grows with the
//! keyspace or a collection: keys come from `SCAN` in batches, collections from
//! `HSCAN` / `SSCAN` / `ZSCAN` cursors or ranged `LRANGE` / `ZRANGE` / `XREVRANGE`,
//! and sizes from `STRLEN` / `HLEN` / … before any value is read.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use redis::aio::ConnectionManager;
use redis::{ConnectionAddr, ConnectionInfo, ProtocolVersion, RedisConnectionInfo};

use crate::classify::{Danger, Kind, Statement};
use crate::connection::{ConnectionConfig, Encryption};
use crate::driver::{
    BoxFuture, Capabilities, Column, Driver, DriverError, NodeKind, ObjectNode, ObjectPath, Result,
    ResultStream, Row, Value, page_size,
};
use crate::redis::{self as rk, Command, KeyType, Loaded, ScanState};

/// Keys one "Load more" adds to a database's browser.
pub const SCAN_BATCH: usize = 1000;

/// String values longer than this load only their start, read-only.
pub const TEXT_LIMIT: u64 = 1 << 20;

/// JSON documents and module values using more memory than this are not loaded.
pub const VALUE_LIMIT: u64 = 16 << 20;

/// Matches a sidebar search keeps; the scan stops there.
pub const SEARCH_CAP: usize = 1000;

/// `SCAN` steps (of [`SCAN_COUNT`] keys each) a sidebar search takes at most per
/// database: about two million keys.
pub const SEARCH_STEPS: usize = 2000;

/// How a sidebar search ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchEnd {
    Complete,
    Capped,
    TooLong,
    Stopped,
}

/// The `COUNT` hint for every `SCAN`-family call.
const SCAN_COUNT: usize = 1000;

/// What the server's command table says about a command.
#[derive(Debug, Clone, Copy)]
struct Flags {
    write: bool,
    danger: bool,
}

/// One database's key browser: the keys loaded so far and the cursor to go on from.
struct Browse {
    conn: ConnectionManager,
    cursor: u64,
    keys: Loaded,
    total: u64,
    complete: bool,
    /// Keys shown per namespace prefix, beyond the first page.
    limits: HashMap<Vec<u8>, usize>,
}

impl Browse {
    fn state(&self) -> ScanState {
        ScanState {
            loaded: self.keys.len(),
            total: self.total,
            complete: self.complete,
        }
    }

    fn limit(&self, prefix: &[u8]) -> usize {
        self.limits
            .get(prefix)
            .copied()
            .unwrap_or(rk::NAMESPACE_PAGE)
    }
}

pub struct Redis {
    /// The SSH tunnel the connection goes through: closes with the driver.
    pub(crate) tunnel: Option<crate::ssh::Tunnel>,
    info: ConnectionInfo,
    conn: ConnectionManager,
    /// The database the connection selects.
    db: u32,
    /// This connection's `CLIENT ID`.
    id: u64,
    version: String,
    /// Write commands are refused before sending.
    read_only: bool,
    /// By command (`name` or `name|subcommand`), from `COMMAND INFO`.
    flags: Mutex<HashMap<String, Flags>>,
    browse: tokio::sync::Mutex<HashMap<u32, Browse>>,
}

/// What a key tab shows above its values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyFacts {
    pub kind: KeyType,
    /// Elements, or a string's length; `None` where no O(1) count exists.
    pub len: Option<u64>,
    /// `MEMORY USAGE`, when the server allows it.
    pub memory: Option<u64>,
    /// `PTTL`: -1 without expiry, -2 when the key does not exist.
    pub pttl: i64,
    pub encoding: Option<String>,
    /// A module type's `DUMP` length.
    pub dump_len: Option<u64>,
}

/// A string or JSON key's value.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyText {
    /// Text, or bytes that are not UTF-8.
    pub value: Value,
    /// Only the first [`TEXT_LIMIT`] bytes were read.
    pub truncated: bool,
}

/// The connection settings for database `db`.
fn settings(config: &ConnectionConfig, password: &str, db: i64) -> Result<ConnectionInfo> {
    let (host, port) = (config.host.clone(), config.port);
    // shortcut: Prefer connects in plain text; Redis has no TLS negotiation to fall back from.
    let addr = match config.encryption {
        Encryption::Off | Encryption::Prefer => ConnectionAddr::Tcp(host, port),
        Encryption::Require | Encryption::VerifyCertificate => ConnectionAddr::TcpTls {
            host,
            port,
            insecure: config.encryption == Encryption::Require,
            tls_params: None,
        },
    };
    let mut s = RedisConnectionInfo::default()
        .set_db(db)
        .set_protocol(ProtocolVersion::RESP3)
        .set_lib_name("deadlock", env!("CARGO_PKG_VERSION"));
    if !config.user.is_empty() {
        s = s.set_username(&config.user);
    }
    if !password.is_empty() {
        s = s.set_password(password);
    }
    Ok(redis::IntoConnectionInfo::into_connection_info(addr)?.set_redis_settings(s))
}

/// A connection that reconnects by itself after a lost socket: the command that saw
/// it fails, the next one runs on a new connection.
async fn open(info: &ConnectionInfo) -> Result<ConnectionManager> {
    let manager = |info: ConnectionInfo| async move {
        let config = redis::aio::ConnectionManagerConfig::new().set_number_of_retries(2);
        redis::Client::open(info)?
            .get_connection_manager_with_config(config)
            .await
    };
    match manager(info.clone()).await {
        Ok(c) => Ok(c),
        // Servers before 6 have no HELLO: RESP2 then.
        Err(e) if e.to_string().to_ascii_uppercase().contains("HELLO") => {
            let s = info
                .redis_settings()
                .clone()
                .set_protocol(ProtocolVersion::RESP2);
            Ok(manager(info.clone().set_redis_settings(s)).await?)
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn connect(config: &ConnectionConfig, password: &str) -> Result<Redis> {
    let db: u32 = match config.database.trim() {
        "" => 0,
        s => s
            .parse()
            .map_err(|_| DriverError::Invalid(format!("database index {s:?} is not a number")))?,
    };
    let info = settings(config, password, i64::from(db))?;
    let mut conn = open(&info).await?;
    // HELLO answers every user (INFO is @dangerous, so ACL users may not run it); servers
    // before 6.2 need INFO and CLIENT ID instead.
    let hello: Option<redis::Value> = redis::cmd("HELLO").query_async(&mut conn).await.ok();
    let mut fields: HashMap<String, redis::Value> = hello
        .map(|h| {
            let list = items(h);
            list.chunks(2)
                .filter_map(|p| Some((text(p.first()?)?, p.get(1)?.clone())))
                .collect()
        })
        .unwrap_or_default();
    if !fields.contains_key("version") {
        let server: String = redis::cmd("INFO")
            .arg("server")
            .query_async(&mut conn)
            .await
            .unwrap_or_default();
        for name in ["redis_version", "redis_mode"] {
            if let Some(v) = server
                .lines()
                .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
            {
                let key = name.trim_start_matches("redis_");
                fields.insert(key.into(), redis::Value::SimpleString(v.trim().into()));
            }
        }
        if let Ok(id) = redis::cmd("CLIENT")
            .arg("ID")
            .query_async::<i64>(&mut conn)
            .await
        {
            fields.insert("id".into(), redis::Value::Int(id));
        }
    }
    let field = |name: &str| fields.get(name).and_then(text).unwrap_or_default();
    match field("mode").as_str() {
        "cluster" => {
            return Err(DriverError::Invalid(
                "This server is a Redis Cluster node. Deadlock does not support Redis Cluster \
                 yet: its keys live on several nodes, and commands for keys on other nodes \
                 would fail with MOVED. Connect to a standalone server instead."
                    .into(),
            ));
        }
        "sentinel" => {
            return Err(DriverError::Invalid(
                "This is a Redis Sentinel, not a data server. Deadlock does not support \
                 Sentinel yet: connect to the primary it reports instead."
                    .into(),
            ));
        }
        _ => {}
    }
    let id = fields
        .get("id")
        .and_then(int)
        .and_then(|i| u64::try_from(i).ok())
        .unwrap_or(0);
    let version = field("version");
    Ok(Redis {
        tunnel: None,
        info,
        conn,
        db,
        id,
        version: format!("Redis {version}"),
        read_only: config.read_only,
        flags: Mutex::default(),
        browse: tokio::sync::Mutex::default(),
    })
}

/// The flag names in a `COMMAND INFO` flags or ACL categories entry.
fn names(v: Option<&redis::Value>) -> Vec<String> {
    use redis::Value as R;
    let (Some(R::Array(items)) | Some(R::Set(items))) = v else {
        return Vec::new();
    };
    items.iter().filter_map(text).collect()
}

/// A scalar reply as text.
fn text(v: &redis::Value) -> Option<String> {
    use redis::Value as R;
    match v {
        R::SimpleString(s) | R::VerbatimString { text: s, .. } => Some(s.clone()),
        R::BulkString(b) => String::from_utf8(b.clone()).ok(),
        R::Okay => Some("OK".into()),
        R::Int(i) => Some(i.to_string()),
        R::Double(f) => Some(f.to_string()),
        _ => None,
    }
}

fn bytes(v: redis::Value) -> Vec<u8> {
    match v {
        redis::Value::BulkString(b) => b,
        other => text(&other).unwrap_or_default().into_bytes(),
    }
}

/// A bulk string as a cell: text when it is UTF-8.
fn cell_of(b: Vec<u8>) -> Value {
    match String::from_utf8(b) {
        Ok(s) => Value::Text(s),
        Err(e) => Value::Bytes(e.into_bytes()),
    }
}

fn int(v: &redis::Value) -> Option<i64> {
    match v {
        redis::Value::Int(i) => Some(*i),
        other => text(other)?.parse().ok(),
    }
}

/// An array or set reply's items; anything else is empty.
fn items(v: redis::Value) -> Vec<redis::Value> {
    match v {
        redis::Value::Array(a) | redis::Value::Set(a) => a,
        redis::Value::Map(m) => m.into_iter().flat_map(|(k, v)| [k, v]).collect(),
        _ => Vec::new(),
    }
}

/// A `SCAN`-family reply: the next cursor and the items.
fn scan_reply(v: redis::Value) -> Result<(u64, Vec<redis::Value>)> {
    let mut parts = items(v).into_iter();
    let cursor = parts
        .next()
        .and_then(|c| int(&c))
        .and_then(|c| u64::try_from(c).ok())
        .ok_or_else(|| DriverError::Invalid("unexpected SCAN reply".into()))?;
    Ok((cursor, parts.next().map(items).unwrap_or_default()))
}

/// `WRONGTYPE Operation against …`: a server error with its code.
fn server_error(e: &redis::ServerError) -> String {
    match e.details() {
        Some(d) => format!("{} {d}", e.code()),
        None => e.code().to_owned(),
    }
}

fn to_cmd(c: &Command) -> redis::Cmd {
    let mut cmd = redis::cmd(c.name);
    for a in c.args() {
        cmd.arg(a);
    }
    cmd
}

/// Readable text for the errors a cluster node sends.
fn explain(e: redis::RedisError) -> DriverError {
    if matches!(e.code(), Some("MOVED" | "ASK")) {
        return DriverError::Invalid(format!(
            "The key lives on another Redis Cluster node ({e}). Deadlock does not follow \
             cluster redirects: connect to a standalone server."
        ));
    }
    e.into()
}

impl Redis {
    /// The server's flags for the command `args` names: `write`, or `script_runner`
    /// without `readonly` (`EVAL`, `FCALL`, but not `EVAL_RO`), and danger: `admin` or
    /// the `@dangerous` category. A command the server does not know counts as a write.
    async fn command_flags(&self, args: &[Vec<u8>]) -> Result<Flags> {
        let lower = |a: &[u8]| String::from_utf8_lossy(a).to_ascii_lowercase();
        let Some(name) = args.first().map(|a| lower(a)) else {
            return Ok(Flags {
                write: false,
                danger: false,
            });
        };
        let full = match args.get(1) {
            Some(sub) => format!("{name}|{}", lower(sub)),
            None => name.clone(),
        };
        let cached = self
            .flags
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&full)
            .copied();
        if let Some(f) = cached {
            return Ok(f);
        }
        let mut conn = self.conn.clone();
        // One entry per name: the command, then its subcommand (nil when it has none).
        let info: redis::Value = redis::cmd("COMMAND")
            .arg("INFO")
            .arg(&name)
            .arg(&full)
            .query_async(&mut conn)
            .await?;
        let entries = items(info);
        let found = entries
            .get(1)
            .filter(|v| matches!(v, redis::Value::Array(_)))
            .or_else(|| entries.first());
        let f = match found {
            Some(redis::Value::Array(doc)) => {
                let flags = names(doc.get(2));
                let categories = names(doc.get(6));
                let has = |f: &str| flags.iter().any(|x| x == f);
                Flags {
                    write: has("write") || has("script_runner") && !has("readonly"),
                    danger: has("admin") || categories.iter().any(|c| c == "@dangerous"),
                }
            }
            _ => Flags {
                write: true,
                danger: false,
            },
        };
        self.flags
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(full, f);
        Ok(f)
    }

    /// Refuses `c` before sending when the connection is read-only and it writes.
    async fn guard(&self, c: &Command) -> Result<()> {
        if !self.read_only {
            return Ok(());
        }
        let mut args = vec![c.name.as_bytes().to_vec()];
        args.extend(c.args().take(1).map(<[u8]>::to_vec));
        if self.command_flags(&args).await?.write {
            return Err(DriverError::ReadOnly(c.name.to_owned()));
        }
        Ok(())
    }

    /// A connection to database `db`: this one for its own database, else a new one.
    async fn db_conn(&self, db: u32) -> Result<ConnectionManager> {
        if db == self.db {
            return Ok(self.conn.clone());
        }
        let s = self.info.redis_settings().clone().set_db(i64::from(db));
        open(&self.info.clone().set_redis_settings(s)).await
    }

    /// One page of `SCAN cursor [MATCH pattern] COUNT n` with each key's type: the
    /// next cursor (0 at the end) and the keys found.
    async fn scan_types(
        conn: &mut ConnectionManager,
        cursor: u64,
        pattern: Option<&str>,
    ) -> Result<(u64, Vec<(Vec<u8>, KeyType)>)> {
        let mut cmd = redis::cmd("SCAN");
        cmd.arg(cursor);
        if let Some(p) = pattern {
            cmd.arg("MATCH").arg(p);
        }
        cmd.arg("COUNT").arg(SCAN_COUNT);
        let (next, found) = scan_reply(cmd.query_async(conn).await?)?;
        let keys: Vec<Vec<u8>> = found.into_iter().map(bytes).collect();
        if keys.is_empty() {
            return Ok((next, Vec::new()));
        }
        let mut pipe = redis::pipe();
        for k in &keys {
            pipe.cmd("TYPE").arg(k);
        }
        let types: Vec<redis::Value> = pipe.query_async(conn).await?;
        Ok((
            next,
            keys.into_iter()
                .zip(types)
                .filter_map(|(k, t)| {
                    let t = text(&t)?;
                    // Deleted between SCAN and TYPE.
                    (t != "none").then(|| (k, KeyType::from_type(&t)))
                })
                .collect(),
        ))
    }

    /// Scans on until [`SCAN_BATCH`] more keys are loaded or the scan ends.
    async fn scan_batch(b: &mut Browse) -> Result<()> {
        let goal = b.keys.len() + SCAN_BATCH;
        while !b.complete && b.keys.len() < goal {
            let (next, found) = Self::scan_types(&mut b.conn, b.cursor, None).await?;
            b.keys.extend(found);
            b.cursor = next;
            b.complete = next == 0;
        }
        Ok(())
    }

    /// The database nodes: those `INFO keyspace` lists, and the connection's own.
    async fn databases(&self) -> Result<Vec<ObjectNode>> {
        let mut conn = self.conn.clone();
        let info: String = redis::cmd("INFO")
            .arg("keyspace")
            .query_async(&mut conn)
            .await?;
        let mut dbs = rk::keyspace(&info);
        if !dbs.iter().any(|(d, _)| *d == self.db) {
            dbs.push((self.db, 0));
            dbs.sort_unstable();
        }
        Ok(dbs
            .into_iter()
            .map(|(db, keys)| {
                let mut n = ObjectNode::new(format!("db{db}"), NodeKind::Database);
                n.detail = format!(
                    "{} {}",
                    rk::grouped(keys),
                    if keys == 1 { "key" } else { "keys" }
                );
                n
            })
            .collect())
    }

    /// The key browser below a database (`path[0]` is `db<n>`).
    async fn browse(&self, path: &[String]) -> Result<Vec<ObjectNode>> {
        let db: u32 = path[0]
            .strip_prefix("db")
            .and_then(|d| d.parse().ok())
            .ok_or_else(|| DriverError::Invalid(format!("{} is not a database", path[0])))?;
        let mut all = self.browse.lock().await;
        let b = match all.entry(db) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let mut conn = self.db_conn(db).await?;
                let total: u64 = redis::cmd("DBSIZE").query_async(&mut conn).await?;
                let mut b = Browse {
                    conn,
                    cursor: 0,
                    keys: Loaded::new(),
                    total,
                    complete: false,
                    limits: HashMap::new(),
                };
                Self::scan_batch(&mut b).await?;
                e.insert(b)
            }
        };
        let parent = |rest: &[String]| rest.last().map(|l| rk::label_key(l)).unwrap_or_default();
        match path.split_last() {
            // "Load more keys" at the database level: scan further.
            Some((last, _)) if *last == format!("{}keys", rk::MORE) => {
                Self::scan_batch(b).await?;
                Ok(Vec::new())
            }
            // "Load more" under a namespace: show more of its loaded keys.
            Some((last, rest)) if last == rk::MORE => {
                let prefix = parent(&rest[1..]);
                let n = b.limit(&prefix) + rk::NAMESPACE_PAGE;
                b.limits.insert(prefix, n);
                Ok(Vec::new())
            }
            _ => {
                let prefix = parent(&path[1..]);
                Ok(rk::level(&b.keys, &prefix, b.limit(&prefix), b.state()))
            }
        }
    }

    /// A sidebar search in database `db`: `SCAN MATCH pattern` step by step, each
    /// match sent on `found`, until the scan ends, [`SEARCH_CAP`] matches or
    /// [`SEARCH_STEPS`] steps, or `stop` is set.
    ///
    /// # Errors
    /// The server refused the scan.
    pub async fn search(
        &self,
        db: u32,
        pattern: &str,
        stop: &std::sync::atomic::AtomicBool,
        found: &tokio::sync::mpsc::UnboundedSender<(u32, Vec<u8>, KeyType)>,
    ) -> Result<SearchEnd> {
        use std::sync::atomic::Ordering;
        let mut conn = self.db_conn(db).await?;
        let (mut cursor, mut n) = (0, 0);
        for _ in 0..SEARCH_STEPS {
            if stop.load(Ordering::Relaxed) {
                return Ok(SearchEnd::Stopped);
            }
            let (next, keys) = Self::scan_types(&mut conn, cursor, Some(pattern)).await?;
            for (k, t) in keys {
                n += 1;
                if found.send((db, k, t)).is_err() {
                    return Ok(SearchEnd::Stopped);
                }
                if n >= SEARCH_CAP {
                    return Ok(SearchEnd::Capped);
                }
            }
            if next == 0 {
                return Ok(SearchEnd::Complete);
            }
            cursor = next;
        }
        Ok(SearchEnd::TooLong)
    }

    /// Drops deleted keys from database `db`'s browser, so the sidebar stops showing
    /// them: these keys, and with `prefix` every key under it.
    pub async fn forget(&self, db: u32, keys: &[Vec<u8>], prefix: Option<&[u8]>) {
        if let Some(b) = self.browse.lock().await.get_mut(&db) {
            for k in keys {
                b.keys.remove(k);
            }
            if let Some(p) = prefix {
                b.keys.retain(|k, _| !k.starts_with(p));
            }
        }
    }

    /// Adds a key this window created to database `db`'s browser.
    pub async fn remember(&self, db: u32, key: Vec<u8>, kind: KeyType) {
        if let Some(b) = self.browse.lock().await.get_mut(&db) {
            b.keys.insert(key, kind);
        }
    }

    /// The facts header of a key tab, and the start of the tab's `WATCH`: a submit
    /// aborts when the key changes after this.
    ///
    /// # Errors
    /// The server refused a command.
    pub async fn facts(&self, key: &[u8]) -> Result<KeyFacts> {
        let mut conn = self.conn.clone();
        redis::cmd("UNWATCH").query_async::<()>(&mut conn).await?;
        redis::cmd("WATCH")
            .arg(key)
            .query_async::<()>(&mut conn)
            .await?;
        let t: String = redis::cmd("TYPE").arg(key).query_async(&mut conn).await?;
        if t == "none" {
            return Err(DriverError::Invalid(
                "The key does not exist (any more): it may have expired or been deleted.".into(),
            ));
        }
        let kind = KeyType::from_type(&t);
        let pttl: i64 = redis::cmd("PTTL").arg(key).query_async(&mut conn).await?;
        // Both need more than the key's own ACL rights; they are optional facts.
        let memory: Option<u64> = redis::cmd("MEMORY")
            .arg("USAGE")
            .arg(key)
            .query_async(&mut conn)
            .await
            .ok()
            .flatten();
        let encoding: Option<String> = redis::cmd("OBJECT")
            .arg("ENCODING")
            .arg(key)
            .query_async(&mut conn)
            .await
            .ok()
            .flatten();
        let len = match kind.len_command() {
            Some(c) => Some(redis::cmd(c).arg(key).query_async(&mut conn).await?),
            None => None,
        };
        let dump_len = match &kind {
            KeyType::Other(_) if memory.is_some_and(|m| m <= VALUE_LIMIT) => {
                let d: Option<Vec<u8>> = redis::cmd("DUMP").arg(key).query_async(&mut conn).await?;
                d.map(|d| d.len() as u64)
            }
            _ => None,
        };
        Ok(KeyFacts {
            kind,
            len,
            memory,
            pttl,
            encoding,
            dump_len,
        })
    }

    /// A string's or JSON document's text. Long strings load their first
    /// [`TEXT_LIMIT`] bytes only; large JSON documents are refused.
    ///
    /// # Errors
    /// The server refused, or the document is too large.
    pub async fn text(&self, key: &[u8], facts: &KeyFacts) -> Result<KeyText> {
        let mut conn = self.conn.clone();
        match facts.kind {
            KeyType::String => {
                let len = facts.len.unwrap_or(0);
                let truncated = len > TEXT_LIMIT;
                let v: Vec<u8> = if truncated {
                    redis::cmd("GETRANGE")
                        .arg(key)
                        .arg(0)
                        .arg(TEXT_LIMIT - 1)
                        .query_async(&mut conn)
                        .await?
                } else {
                    redis::cmd("GET").arg(key).query_async(&mut conn).await?
                };
                Ok(KeyText {
                    value: cell_of(v),
                    truncated,
                })
            }
            KeyType::Json => {
                if facts.memory.is_some_and(|m| m > VALUE_LIMIT) {
                    return Err(DriverError::Invalid(format!(
                        "The document uses {} of memory: too large to open here.",
                        rk::bytes_size(facts.memory.unwrap_or(0))
                    )));
                }
                let v: Vec<u8> = redis::cmd("JSON.GET")
                    .arg(key)
                    .query_async(&mut conn)
                    .await?;
                Ok(KeyText {
                    value: cell_of(v),
                    truncated: false,
                })
            }
            _ => Err(DriverError::Unsupported("this type has no text value")),
        }
    }

    /// A collection's elements as grid rows, in pages (call on the core runtime): hashes and sets by cursor
    /// (`HSCAN`, `SSCAN`), sorted sets by rank (`ZRANGE`, or `ZSCAN` when filtered),
    /// lists by index (`LRANGE`), streams newest first (`XREVRANGE`). `pattern`
    /// filters fields or members with `MATCH`.
    ///
    /// # Errors
    /// The type has no rows.
    pub fn rows(
        &self,
        key: &[u8],
        kind: &KeyType,
        pattern: Option<String>,
    ) -> Result<ResultStream> {
        let columns = kind
            .columns()
            .iter()
            .map(|n| Column {
                name: (*n).into(),
                type_name: if *n == "Score" || *n == "Index" {
                    "DOUBLE"
                } else {
                    ""
                }
                .into(),
            })
            .collect();
        if kind.columns().is_empty() {
            return Err(DriverError::Unsupported("this type has no rows"));
        }
        let (tx, stream) = ResultStream::channel(columns);
        let (conn, key, kind) = (self.conn.clone(), key.to_vec(), kind.clone());
        tokio::spawn(async move {
            let mut pager = Pager {
                conn,
                key,
                kind,
                pattern,
                cursor: 0,
                offset: 0,
                after: None,
                done: false,
            };
            while !pager.done {
                let page = pager.next().await;
                let failed = page.is_err();
                if tx.send(page).await.is_err() || failed {
                    return;
                }
            }
        });
        Ok(stream)
    }

    /// Runs `commands` one after another, refusing writes on a read-only connection,
    /// and returns their replies. Stops at the first error.
    ///
    /// # Errors
    /// The command the server refused, with its error.
    pub async fn run(&self, commands: &[Command]) -> Result<Vec<redis::Value>> {
        self.run_in(self.db, commands).await
    }

    /// [`Self::run`] in database `db`.
    ///
    /// # Errors
    /// The command the server refused, with its error.
    pub async fn run_in(&self, db: u32, commands: &[Command]) -> Result<Vec<redis::Value>> {
        let mut conn = self.db_conn(db).await?;
        let mut out = Vec::with_capacity(commands.len());
        for c in commands {
            self.guard(c).await?;
            out.push(to_cmd(c).query_async(&mut conn).await.map_err(explain)?);
        }
        Ok(out)
    }

    /// Runs `commands` in `MULTI` / `EXEC` on this connection, under the `WATCH`
    /// [`Self::facts`] set: when the key changed since, nothing runs.
    ///
    /// # Errors
    /// The key changed (nothing ran), a command was refused before running (nothing
    /// ran), or one failed inside the transaction (the others ran: Redis has no
    /// rollback).
    pub async fn transact(&self, commands: &[Command]) -> Result<()> {
        for c in commands {
            self.guard(c).await?;
        }
        let mut pipe = redis::pipe();
        pipe.cmd("MULTI");
        for c in commands {
            pipe.add_command(to_cmd(c));
        }
        pipe.cmd("EXEC");
        pipe.ignore_errors();
        let mut conn = self.conn.clone();
        let replies: Vec<redis::Value> = pipe.query_async(&mut conn).await.map_err(explain)?;
        // Queueing errors come back in place of QUEUED; EXEC then fails as EXECABORT.
        let queued = replies.iter().skip(1).take(commands.len());
        if let Some((c, redis::Value::ServerError(e))) = commands
            .iter()
            .zip(queued)
            .find(|(_, r)| matches!(r, redis::Value::ServerError(_)))
        {
            return Err(DriverError::Invalid(format!(
                "Nothing was written: the server refused {}: {}",
                c.name,
                server_error(e)
            )));
        }
        match replies.last() {
            Some(redis::Value::Nil) => Err(DriverError::Invalid(
                "Nothing was written: the key changed on the server after it was loaded. \
                 Reload it to see the current value, then make the changes again."
                    .into(),
            )),
            Some(redis::Value::Array(results)) => {
                let failed: Vec<String> = commands
                    .iter()
                    .zip(results)
                    .filter_map(|(c, r)| match r {
                        redis::Value::ServerError(e) => {
                            Some(format!("{}: {}", c.text(), server_error(e)))
                        }
                        _ => None,
                    })
                    .collect();
                if failed.is_empty() {
                    Ok(())
                } else {
                    Err(DriverError::Invalid(format!(
                        "{} of {} commands failed; the others ran (Redis has no rollback): {}",
                        failed.len(),
                        commands.len(),
                        failed.join("; ")
                    )))
                }
            }
            Some(redis::Value::ServerError(e)) => Err(DriverError::Invalid(format!(
                "Nothing was written: {}",
                server_error(e)
            ))),
            _ => Err(DriverError::Invalid("unexpected EXEC reply".into())),
        }
    }

    /// Renames `key` to `to`: `RENAME` when `overwrite`, else `RENAMENX`. `false`: the
    /// name is taken and nothing changed.
    ///
    /// # Errors
    /// The server refused (the key is gone, or the connection is read-only).
    pub async fn rename(&self, key: &[u8], to: &[u8], overwrite: bool) -> Result<bool> {
        let c = Command::new(if overwrite { "RENAME" } else { "RENAMENX" })
            .arg(key)
            .arg(to);
        let reply = self.run(&[c]).await?;
        Ok(!matches!(reply.first(), Some(redis::Value::Int(0))))
    }

    /// Whether `key` exists in database `db`, for New key and Rename.
    ///
    /// # Errors
    /// The server refused.
    pub async fn exists_in(&self, db: u32, key: &[u8]) -> Result<bool> {
        let mut conn = self.db_conn(db).await?;
        Ok(redis::cmd("EXISTS").arg(key).query_async(&mut conn).await?)
    }

    /// One step of a namespace count or delete in database `db`: the keys matching
    /// `pattern` from `cursor` on.
    ///
    /// # Errors
    /// The server refused the scan.
    pub async fn scan_keys(
        &self,
        db: u32,
        cursor: u64,
        pattern: &[u8],
    ) -> Result<(u64, Vec<Vec<u8>>)> {
        let mut conn = self.db_conn(db).await?;
        let (next, found) = scan_reply(
            redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(pattern)
                .arg("COUNT")
                .arg(SCAN_COUNT)
                .query_async(&mut conn)
                .await?,
        )?;
        Ok((next, found.into_iter().map(bytes).collect()))
    }

    /// Whether the server has the RedisJSON module.
    ///
    /// # Errors
    /// The server refused `MODULE LIST`.
    pub async fn has_json(&self) -> Result<bool> {
        let mut conn = self.conn.clone();
        let list: redis::Value = redis::cmd("MODULE")
            .arg("LIST")
            .query_async(&mut conn)
            .await?;
        Ok(items(list).into_iter().any(|m| {
            items(m)
                .iter()
                .filter_map(text)
                .any(|t| t.eq_ignore_ascii_case("rejson") || t.eq_ignore_ascii_case("json"))
        }))
    }

    /// The database this connection selects.
    pub fn db(&self) -> u32 {
        self.db
    }
}

/// Reads a collection page by page for [`Redis::rows`].
struct Pager {
    conn: ConnectionManager,
    key: Vec<u8>,
    kind: KeyType,
    pattern: Option<String>,
    cursor: u64,
    offset: i64,
    /// The last stream ID sent: the next page starts before it.
    after: Option<String>,
    done: bool,
}

impl Pager {
    async fn next(&mut self) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        let page = page_size() as i64;
        match (&self.kind, &self.pattern) {
            (KeyType::Hash | KeyType::Set, _) | (KeyType::ZSet, Some(_)) => {
                let name = match self.kind {
                    KeyType::Hash => "HSCAN",
                    KeyType::Set => "SSCAN",
                    _ => "ZSCAN",
                };
                // A cursor step may return few or no items: go on to a full page.
                while rows.len() < page_size() && !self.done {
                    let mut cmd = redis::cmd(name);
                    cmd.arg(&self.key).arg(self.cursor);
                    if let Some(p) = &self.pattern {
                        cmd.arg("MATCH").arg(p);
                    }
                    cmd.arg("COUNT").arg(page_size());
                    let (next, found) = scan_reply(cmd.query_async(&mut self.conn).await?)?;
                    self.cursor = next;
                    self.done = next == 0;
                    let mut it = found.into_iter();
                    while let Some(a) = it.next() {
                        let a = cell_of(bytes(a));
                        rows.push(match self.kind {
                            KeyType::Set => vec![a],
                            KeyType::Hash => {
                                vec![a, cell_of(it.next().map(bytes).unwrap_or_default())]
                            }
                            _ => vec![a, score(it.next())],
                        });
                    }
                }
            }
            (KeyType::ZSet, None) => {
                let reply: redis::Value = redis::cmd("ZRANGE")
                    .arg(&self.key)
                    .arg(self.offset)
                    .arg(self.offset + page - 1)
                    .arg("WITHSCORES")
                    .query_async(&mut self.conn)
                    .await?;
                let list = items(reply);
                // RESP3: [member, score] pairs; RESP2: member, score, member, score.
                if list.iter().all(|v| matches!(v, redis::Value::Array(_))) {
                    for pair in list {
                        let mut p = items(pair).into_iter();
                        let m = cell_of(p.next().map(bytes).unwrap_or_default());
                        rows.push(vec![m, score(p.next())]);
                    }
                } else {
                    let mut it = list.into_iter();
                    while let Some(m) = it.next() {
                        rows.push(vec![cell_of(bytes(m)), score(it.next())]);
                    }
                }
                self.offset += page;
                self.done = rows.len() < page_size();
            }
            (KeyType::List, _) => {
                let values: Vec<Vec<u8>> = redis::cmd("LRANGE")
                    .arg(&self.key)
                    .arg(self.offset)
                    .arg(self.offset + page - 1)
                    .query_async(&mut self.conn)
                    .await?;
                self.done = values.len() < page_size();
                for (i, v) in values.into_iter().enumerate() {
                    rows.push(vec![Value::Int(self.offset + i as i64), cell_of(v)]);
                }
                self.offset += page;
            }
            (KeyType::Stream, _) => {
                let end = self
                    .after
                    .as_ref()
                    .map_or_else(|| "+".to_owned(), |id| format!("({id}"));
                let reply: redis::Value = redis::cmd("XREVRANGE")
                    .arg(&self.key)
                    .arg(end)
                    .arg("-")
                    .arg("COUNT")
                    .arg(page_size())
                    .query_async(&mut self.conn)
                    .await?;
                for entry in items(reply) {
                    let mut e = items(entry).into_iter();
                    let id = e.next().and_then(|v| text(&v)).unwrap_or_default();
                    let mut pairs = Vec::new();
                    let mut f = e.next().map(items).unwrap_or_default().into_iter();
                    while let Some(k) = f.next() {
                        pairs.push((bytes(k), f.next().map(bytes).unwrap_or_default()));
                    }
                    rows.push(vec![
                        Value::Text(id.clone()),
                        Value::Text(rk::stream_text(&pairs)),
                    ]);
                    self.after = Some(id);
                }
                self.done = rows.len() < page_size();
            }
            _ => self.done = true,
        }
        Ok(rows)
    }
}

/// A score reply: a double (RESP3) or its text (RESP2).
fn score(v: Option<redis::Value>) -> Value {
    match v {
        Some(redis::Value::Double(f)) => Value::Float(f),
        Some(other) => text(&other)
            .and_then(|t| match t.as_str() {
                "inf" | "+inf" => Some(f64::INFINITY),
                "-inf" => Some(f64::NEG_INFINITY),
                t => t.parse().ok(),
            })
            .map_or(Value::Null, Value::Float),
        None => Value::Null,
    }
}

impl Driver for Redis {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            sessions: true,
            config: true,
            ..Capabilities::default()
        }
    }

    fn server_version(&self) -> &str {
        &self.version
    }

    fn as_redis(&self) -> Option<&Redis> {
        Some(self)
    }

    fn execute<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<ResultStream>> {
        Box::pin(async move {
            let args = rk::split_args(text).map_err(DriverError::Invalid)?;
            let Some((name, rest)) = args.split_first() else {
                return Err(DriverError::Invalid("empty command".into()));
            };
            if self.read_only && self.command_flags(&args).await?.write {
                return Err(DriverError::ReadOnly(
                    String::from_utf8_lossy(name).to_ascii_uppercase(),
                ));
            }
            let mut conn = self.conn.clone();
            let mut cmd = redis::cmd(&String::from_utf8_lossy(name));
            for a in rest {
                cmd.arg(a);
            }
            let reply: redis::Value = match cmd.query_async(&mut conn).await {
                // The manager reconnects; a read runs again once on the new connection.
                Err(e)
                    if (e.is_io_error() || e.is_connection_dropped())
                        && self.command_flags(&args).await.is_ok_and(|f| !f.write) =>
                {
                    cmd.query_async(&mut conn).await.map_err(explain)?
                }
                r => r.map_err(explain)?,
            };
            Ok(to_result(reply))
        })
    }

    fn classify<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Statement>> {
        Box::pin(async move {
            let args = rk::split_args(text).map_err(DriverError::Invalid)?;
            let f = self.command_flags(&args).await?;
            Ok(Statement {
                kind: if f.write { Kind::Write } else { Kind::Read },
                verb: args
                    .first()
                    .map(|a| String::from_utf8_lossy(a).to_ascii_uppercase())
                    .unwrap_or_default(),
                danger: f.danger.then_some(Danger::ServerFlagged),
                implicit_commit: false,
            })
        })
    }

    fn cancel(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            Err(DriverError::Unsupported(
                "Redis cannot cancel a running command",
            ))
        })
    }

    /// Key names only, Redis has no code: `SCAN MATCH` over each database with keys,
    /// as the sidebar search does, until [`crate::search::LIMIT`] keys. `MATCH` is
    /// case-sensitive.
    fn search_everywhere<'a>(
        &'a self,
        text: &'a str,
        hidden: &'a [String],
        stop: &'a std::sync::atomic::AtomicBool,
    ) -> BoxFuture<'a, Result<Vec<crate::search::Hit>>> {
        Box::pin(async move {
            let pattern = rk::search_pattern(text);
            let mut out = Vec::new();
            for db in self.databases().await? {
                if hidden.contains(&db.name) {
                    continue;
                }
                let Some(n) = db.name.strip_prefix("db").and_then(|n| n.parse().ok()) else {
                    continue;
                };
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                self.search(n, &pattern, stop, &tx).await?;
                drop(tx);
                while let Some((_, key, kind)) = rx.recv().await {
                    out.push(crate::search::Hit::object(
                        db.name.clone(),
                        rk::key_label(&key),
                        kind.label().to_owned(),
                    ));
                }
                if out.len() >= crate::search::LIMIT {
                    out.truncate(crate::search::LIMIT);
                    break;
                }
            }
            Ok(out)
        })
    }

    fn children<'a>(&'a self, node: &'a ObjectPath) -> BoxFuture<'a, Result<Vec<ObjectNode>>> {
        Box::pin(async move {
            if node.0.is_empty() {
                // A reload of the connection starts every browser again.
                self.browse.lock().await.clear();
                return self.databases().await;
            }
            self.browse(&node.0).await
        })
    }

    fn sessions(&self) -> BoxFuture<'_, Result<crate::sessions::Snapshot>> {
        Box::pin(crate::redis_driver_admin::sessions(self))
    }

    fn waiting_sessions(&self) -> BoxFuture<'_, Result<usize>> {
        // Redis has no lock waits.
        Box::pin(async { Ok(0) })
    }

    fn cancel_statement(&self, id: crate::sessions::SessionId) -> BoxFuture<'_, Result<()>> {
        Box::pin(crate::redis_driver_admin::client_command(
            self, id, "UNBLOCK",
        ))
    }

    fn end_session(&self, id: crate::sessions::SessionId) -> BoxFuture<'_, Result<()>> {
        Box::pin(crate::redis_driver_admin::client_command(self, id, "KILL"))
    }

    fn reset_slow_log(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            self.run(&[Command::new("SLOWLOG").arg("RESET")]).await?;
            Ok(())
        })
    }

    fn config(&self) -> BoxFuture<'_, Result<crate::config::ServerConfig>> {
        Box::pin(crate::redis_driver_admin::config(self))
    }

    fn apply_config<'a>(
        &'a self,
        config: &'a crate::config::ServerConfig,
        changes: &'a [crate::config::ConfigChange],
    ) -> BoxFuture<'a, Result<crate::config::Applied>> {
        Box::pin(crate::redis_driver_admin::apply_config(
            self, config, changes,
        ))
    }
}

/// Access for the admin queries next door.
impl Redis {
    pub(crate) fn conn(&self) -> ConnectionManager {
        self.conn.clone()
    }

    pub(crate) fn client_id(&self) -> u64 {
        self.id
    }

    pub(crate) fn is_read_only(&self) -> bool {
        self.read_only
    }
}

fn column(name: &str) -> Column {
    Column {
        name: name.into(),
        type_name: String::new(),
    }
}

/// Whether a reply holds nothing nested.
fn scalar(v: &redis::Value) -> bool {
    !matches!(
        v,
        redis::Value::Array(_)
            | redis::Value::Set(_)
            | redis::Value::Map(_)
            | redis::Value::Push { .. }
            | redis::Value::Attribute { .. }
    )
}

/// Flat maps: Field / Value rows. Flat arrays and sets: one row per element.
/// Anything nested: `redis-cli`'s indented text, one line per row. Scalars: one
/// `reply` row.
fn to_result(reply: redis::Value) -> ResultStream {
    use redis::Value as R;
    match reply {
        R::Map(entries) if entries.iter().all(|(k, v)| scalar(k) && scalar(v)) => {
            ResultStream::ready(
                vec![column("Field"), column("Value")],
                entries
                    .into_iter()
                    .map(|(k, v)| vec![cell(k), cell(v)])
                    .collect(),
            )
        }
        R::Array(items) | R::Set(items) if items.iter().all(scalar) => ResultStream::ready(
            vec![column("value")],
            items
                .into_iter()
                .map(|v| vec![cell(v)])
                .collect::<Vec<Row>>(),
        ),
        other if scalar(&other) => {
            ResultStream::ready(vec![column("reply")], vec![vec![cell(other)]])
        }
        nested => {
            let mut lines = Vec::new();
            cli_lines(&nested, "", &mut lines);
            ResultStream::ready(
                vec![column("reply")],
                lines.into_iter().map(|l| vec![Value::Text(l)]).collect(),
            )
        }
    }
}

/// `redis-cli`'s rendering of a reply: `1) "a"`, nested items indented under their
/// number, map entries as `1# "key" => value`.
fn cli_lines(v: &redis::Value, lead: &str, out: &mut Vec<String>) {
    use redis::Value as R;
    let pad = " ".repeat(lead.chars().count());
    let list = |items: Vec<(String, &redis::Value)>, out: &mut Vec<String>| {
        if items.is_empty() {
            out.push(format!("{lead}(empty)"));
        }
        let width = items.len().to_string().len();
        for (i, (head, item)) in items.into_iter().enumerate() {
            let first = if i == 0 { lead.to_owned() } else { pad.clone() };
            cli_lines(
                item,
                &format!("{first}{head:>width$}", head = head, width = width + 2),
                out,
            );
        }
    };
    match v {
        R::Array(items) | R::Set(items) | R::Push { data: items, .. } => list(
            items
                .iter()
                .enumerate()
                .map(|(i, x)| (format!("{}) ", i + 1), x))
                .collect(),
            out,
        ),
        R::Map(entries)
        | R::Attribute {
            attributes: entries,
            ..
        } => {
            for (i, (k, val)) in entries.iter().enumerate() {
                let first = if i == 0 { lead.to_owned() } else { pad.clone() };
                let head = format!("{first}{}# {} => ", i + 1, scalar_text(k));
                cli_lines(val, &head, out);
            }
            if entries.is_empty() {
                out.push(format!("{lead}(empty)"));
            }
        }
        scalar => out.push(format!("{lead}{}", scalar_text(scalar))),
    }
}

fn scalar_text(v: &redis::Value) -> String {
    use redis::Value as R;
    match v {
        R::Nil => "(nil)".into(),
        R::Int(i) => format!("(integer) {i}"),
        R::Double(f) => format!("(double) {f}"),
        R::Boolean(b) => format!("({b})"),
        R::BulkString(b) => rk::quoted(b),
        R::SimpleString(s) => s.clone(),
        R::VerbatimString { text, .. } => rk::quoted(text.as_bytes()),
        R::Okay => "OK".into(),
        R::ServerError(e) => format!("(error) {}", server_error(e)),
        other => format!("{other:?}"),
    }
}

fn cell(v: redis::Value) -> Value {
    use redis::Value as R;
    match v {
        R::Nil => Value::Null,
        R::Int(i) => Value::Int(i),
        R::Double(f) => Value::Float(f),
        R::Boolean(b) => Value::Bool(b),
        R::BulkString(b) => cell_of(b),
        R::SimpleString(s) | R::VerbatimString { text: s, .. } => Value::Text(s),
        R::Okay => Value::Text("OK".into()),
        other => {
            let mut lines = Vec::new();
            cli_lines(&other, "", &mut lines);
            Value::Text(lines.join(" "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_replies_render_like_redis_cli() {
        use redis::Value as R;
        let v = R::Array(vec![
            R::BulkString(b"a".to_vec()),
            R::Array(vec![R::Int(1), R::Nil]),
            R::Map(vec![(R::SimpleString("k".into()), R::Double(1.5))]),
        ]);
        let mut out = Vec::new();
        cli_lines(&v, "", &mut out);
        assert_eq!(
            out,
            [
                r#"1) "a""#,
                "2) 1) (integer) 1",
                "   2) (nil)",
                "3) 1# k => (double) 1.5",
            ]
        );
        let rows = tokio_rows(to_result(R::Map(vec![(
            R::BulkString(b"f".to_vec()),
            R::BulkString(b"v".to_vec()),
        )])));
        assert_eq!(
            rows,
            [vec![Value::Text("f".into()), Value::Text("v".into())]]
        );
    }

    fn tokio_rows(s: ResultStream) -> Vec<Row> {
        crate::driver::Runtime::new()
            .unwrap()
            .block_on(s.collect())
            .unwrap()
    }
}
