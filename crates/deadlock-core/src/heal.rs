//! Connections that heal themselves. When the server session behind one is lost
//! (killed, timed out, its socket closed), the call that saw it opens a new session the
//! way the first was opened (SSH tunnel, sign-in, session setup), restores the
//! transaction mode and current schema, and runs the call again once when that is safe:
//! reads and metadata loads, outside a transaction. Writes, DDL and anything inside an
//! open transaction are never run again; they fail with a plain error, and the next
//! call uses the new session.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::classify::{Kind, Statement, classify};
use crate::connection::ConnectionConfig;
use crate::driver::{
    BoxFuture, Capabilities, Driver, DriverError, Link, ObjectNode, ObjectPath, Result,
    ResultStream,
};
use crate::sessions::Mark;

/// The wait before each reconnect attempt.
const ATTEMPTS: [Duration; 3] = [
    Duration::ZERO,
    Duration::from_millis(500),
    Duration::from_millis(1500),
];

/// After reconnecting failed, calls fail at once for this long, so polls do not spin.
const COOLDOWN: Duration = Duration::from_secs(30);

type Dial = Box<dyn Fn() -> BoxFuture<'static, Result<Arc<dyn Driver>>> + Send + Sync>;

struct Current {
    driver: Arc<dyn Driver>,
    generation: u64,
    /// Marks the session as this app's on the sessions screen.
    mark: Option<Mark>,
}

pub struct Healing {
    dial: Dial,
    config: ConnectionConfig,
    version: String,
    capabilities: Capabilities,
    current: RwLock<Current>,
    /// One reconnect at a time.
    reconnecting: tokio::sync::Mutex<()>,
    /// Auto-commit is off.
    manual: AtomicBool,
    /// The session may hold a transaction: a lost one was rolled back.
    in_tx: AtomicBool,
    /// The last `USE` / `ALTER SESSION SET CURRENT_SCHEMA`, run again on a new session.
    schema: Mutex<Option<String>>,
    /// The last reconnect failed: when, and why.
    failed: Mutex<Option<(Instant, String)>>,
    link: watch::Sender<Link>,
}

/// `run!(healing, retry, |d| d.call(args))`: the call on the session; on a lost one,
/// reconnects and makes the call again once when `retry` and no transaction was open.
/// A macro, not a closure-taking fn: async closures over `&dyn Driver` lose `Send`.
macro_rules! run {
    ($h:expr, $retry:expr, |$d:ident| $call:expr) => {
        async {
            let h = $h;
            let (session, generation) = h.session()?;
            let first = {
                let $d = &*session;
                $call.await
            };
            match first {
                Err(e) if e.is_connection_lost() => {
                    drop(session);
                    h.recover(generation, &e, $retry).await?;
                    let session = h.driver();
                    let $d = &*session;
                    $call.await
                }
                r => r,
            }
        }
    };
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn server(config: &ConnectionConfig) -> String {
    format!("{}:{}", config.host, config.port)
}

impl Healing {
    pub(crate) fn new(config: ConnectionConfig, password: String, driver: Arc<dyn Driver>) -> Self {
        let c = config.clone();
        let dial: Dial = Box::new(move || {
            let (c, p) = (c.clone(), password.clone());
            Box::pin(async move { crate::driver::dial(&c, &p).await })
        });
        Self::with_dial(config, driver, dial)
    }

    fn with_dial(config: ConnectionConfig, driver: Arc<dyn Driver>, dial: Dial) -> Self {
        let mark = driver.session_id().map(|id| Mark::new(server(&config), id));
        Self {
            dial,
            version: driver.server_version().to_owned(),
            capabilities: driver.capabilities(),
            current: RwLock::new(Current {
                driver,
                generation: 0,
                mark,
            }),
            config,
            reconnecting: tokio::sync::Mutex::new(()),
            manual: AtomicBool::new(false),
            in_tx: AtomicBool::new(false),
            schema: Mutex::new(None),
            failed: Mutex::new(None),
            link: watch::channel(Link::Up).0,
        }
    }

    fn driver(&self) -> Arc<dyn Driver> {
        let c = self.current.read().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(&c.driver)
    }

    /// The reconnect failed a moment ago: its error.
    fn cooling(&self) -> Option<DriverError> {
        match &*lock(&self.failed) {
            Some((at, e)) if at.elapsed() < COOLDOWN => {
                Some(DriverError::ReconnectFailed(e.clone()))
            }
            _ => None,
        }
    }

    /// The session to run on, and its generation; fails at once while cooling down.
    fn session(&self) -> Result<(Arc<dyn Driver>, u64)> {
        if let Some(e) = self.cooling() {
            return Err(e);
        }
        let c = self.current.read().unwrap_or_else(PoisonError::into_inner);
        Ok((Arc::clone(&c.driver), c.generation))
    }

    /// After `lost` on session `generation`: reconnects; `Ok` means run the call again.
    async fn recover(&self, generation: u64, lost: &DriverError, retry: bool) -> Result<()> {
        let in_tx = self.in_tx.swap(false, Ordering::Relaxed);
        self.heal(generation, lost).await?;
        if in_tx {
            return Err(DriverError::TransactionLost);
        }
        if !retry {
            return Err(DriverError::NotRetried);
        }
        Ok(())
    }

    /// Replaces the session of `generation`, unless another call already did.
    async fn heal(&self, generation: u64, cause: &DriverError) -> Result<()> {
        let _one = self.reconnecting.lock().await;
        if let Some(e) = self.cooling() {
            return Err(e);
        }
        let now = self
            .current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .generation;
        if now != generation {
            return Ok(());
        }
        eprintln!(
            "deadlock: {}: connection lost ({cause}); reconnecting",
            self.config.name
        );
        self.link.send_replace(Link::Reconnecting);
        let mut last = String::new();
        for wait in ATTEMPTS {
            tokio::time::sleep(wait).await;
            match self.reopen().await {
                Ok(d) => {
                    let old = {
                        let mut c = self.current.write().unwrap_or_else(PoisonError::into_inner);
                        // Unmarked first: Oracle may give the new session the old SID.
                        c.mark = None;
                        c.mark = d.session_id().map(|id| Mark::new(server(&self.config), id));
                        c.generation += 1;
                        std::mem::replace(&mut c.driver, d)
                    };
                    drop(old);
                    *lock(&self.failed) = None;
                    self.link.send_replace(Link::Up);
                    return Ok(());
                }
                // mysql_async says it twice: `Input/output error: Input/output error: …`.
                Err(e) => last = e.to_string().replace("Input/output error: ", ""),
            }
        }
        eprintln!(
            "deadlock: {}: reconnecting failed: {last}",
            self.config.name
        );
        *lock(&self.failed) = Some((Instant::now(), last.clone()));
        self.link.send_replace(Link::Lost(last.clone()));
        Err(DriverError::ReconnectFailed(last))
    }

    /// A new session, set up like the lost one.
    async fn reopen(&self) -> Result<Arc<dyn Driver>> {
        let d = (self.dial)().await?;
        if self.manual.load(Ordering::Relaxed) {
            d.set_auto_commit(false).await?;
        }
        let schema = lock(&self.schema).clone();
        if let Some(sql) = schema
            && let Err(e) = d.execute(&sql).await
        {
            eprintln!("deadlock: {}: {sql}: {e}", self.config.name);
        }
        Ok(d)
    }

    /// Follows what a statement that ran did to the session's transaction and schema.
    fn ran(&self, text: &str, s: &Statement) {
        let head: String = text
            .split_whitespace()
            .take(5)
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_uppercase();
        let ends = s.kind == Kind::Transaction
            && matches!(s.verb.as_str(), "COMMIT" | "ROLLBACK")
            && !head.contains("SAVEPOINT");
        if ends || s.implicit_commit {
            self.in_tx.store(false, Ordering::Relaxed);
        } else if self.manual.load(Ordering::Relaxed) || s.kind == Kind::Transaction {
            self.in_tx.store(true, Ordering::Relaxed);
        }
        if s.verb == "USE" || head.starts_with("ALTER SESSION SET CURRENT_SCHEMA") {
            *lock(&self.schema) = Some(text.trim().to_owned());
        }
    }
}

impl Driver for Healing {
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn server_version(&self) -> &str {
        &self.version
    }

    fn execute<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<ResultStream>> {
        self.execute_bound(text, &[])
    }

    fn execute_bound<'a>(
        &'a self,
        text: &'a str,
        binds: &'a [(String, Option<String>)],
    ) -> BoxFuture<'a, Result<ResultStream>> {
        Box::pin(async move {
            let s = classify(text, self.config.engine);
            let r = run!(self, s.kind == Kind::Read, |d| d.execute_bound(text, binds)).await;
            if r.is_ok() {
                self.ran(text, &s);
            }
            r
        })
    }

    /// From a side session: never reconnects this one.
    fn cancel(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { self.driver().cancel().await })
    }

    fn classify<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Statement>> {
        Box::pin(async move { self.driver().classify(text).await })
    }

    fn set_auto_commit(&self, on: bool) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            run!(self, true, |d| d.set_auto_commit(on)).await?;
            self.manual.store(!on, Ordering::Relaxed);
            if on {
                self.in_tx.store(false, Ordering::Relaxed);
            }
            Ok(())
        })
    }

    fn end_transaction(&self, commit: bool) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            // Whatever a lost session held is gone: reported as a rolled-back transaction.
            self.in_tx.store(true, Ordering::Relaxed);
            run!(self, false, |d| d.end_transaction(commit)).await?;
            self.in_tx.store(false, Ordering::Relaxed);
            Ok(())
        })
    }

    fn transaction_open(&self) -> BoxFuture<'_, Result<bool>> {
        Box::pin(async move { run!(self, true, |d| d.transaction_open()).await })
    }

    fn table_info<'a>(
        &'a self,
        table: &'a crate::edit::TableRef,
    ) -> BoxFuture<'a, Result<crate::edit::TableInfo>> {
        Box::pin(async move { run!(self, true, |d| d.table_info(table)).await })
    }

    fn structure<'a>(
        &'a self,
        table: &'a crate::edit::TableRef,
    ) -> BoxFuture<'a, Result<crate::structure::TableStructure>> {
        Box::pin(async move { run!(self, true, |d| d.structure(table)).await })
    }

    fn collations(&self) -> BoxFuture<'_, Result<Vec<(String, String)>>> {
        Box::pin(async move { run!(self, true, |d| d.collations()).await })
    }

    fn explain<'a>(
        &'a self,
        statement: &'a str,
        analyze: bool,
    ) -> BoxFuture<'a, Result<crate::plan::Plan>> {
        Box::pin(async move {
            // Measuring runs the statement.
            let safe = !analyze || classify(statement, self.config.engine).kind == Kind::Read;
            run!(self, safe, |d| d.explain(statement, analyze)).await
        })
    }

    fn sessions(&self) -> BoxFuture<'_, Result<crate::sessions::Snapshot>> {
        Box::pin(async move {
            let mut snap = run!(self, true, |d| d.sessions()).await?;
            crate::sessions::mark_ours(&mut snap, &server(&self.config));
            Ok(snap)
        })
    }

    fn waiting_sessions(&self) -> BoxFuture<'_, Result<usize>> {
        Box::pin(async move { run!(self, true, |d| d.waiting_sessions()).await })
    }

    fn cancel_statement(&self, id: crate::sessions::SessionId) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { run!(self, false, |d| d.cancel_statement(id)).await })
    }

    fn end_session(&self, id: crate::sessions::SessionId) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { run!(self, false, |d| d.end_session(id)).await })
    }

    fn reset_slow_log(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { run!(self, false, |d| d.reset_slow_log()).await })
    }

    fn last_deadlock(&self) -> BoxFuture<'_, Result<Option<String>>> {
        Box::pin(async move { run!(self, true, |d| d.last_deadlock()).await })
    }

    fn config(&self) -> BoxFuture<'_, Result<crate::config::ServerConfig>> {
        Box::pin(async move { run!(self, true, |d| d.config()).await })
    }

    fn apply_config<'a>(
        &'a self,
        config: &'a crate::config::ServerConfig,
        changes: &'a [crate::config::ConfigChange],
    ) -> BoxFuture<'a, Result<crate::config::Applied>> {
        Box::pin(async move { run!(self, false, |d| d.apply_config(config, changes)).await })
    }

    fn apply<'a>(
        &'a self,
        changes: &'a [crate::edit::Change],
        auto: bool,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            run!(self, false, |d| d.apply(changes, auto)).await?;
            if !auto {
                self.in_tx.store(true, Ordering::Relaxed);
            }
            Ok(())
        })
    }

    fn diagram<'a>(&'a self, schema: &'a str) -> BoxFuture<'a, Result<crate::diagram::Diagram>> {
        Box::pin(async move { run!(self, true, |d| d.diagram(schema)).await })
    }

    fn search_everywhere<'a>(
        &'a self,
        text: &'a str,
        hidden: &'a [String],
        stop: &'a std::sync::atomic::AtomicBool,
    ) -> BoxFuture<'a, Result<Vec<crate::search::Hit>>> {
        Box::pin(async move { run!(self, true, |d| d.search_everywhere(text, hidden, stop)).await })
    }

    fn definition<'a>(
        &'a self,
        schema: &'a str,
        name: &'a str,
        kind: &'a str,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move { run!(self, true, |d| d.definition(schema, name, kind)).await })
    }

    fn object_flavor(&self) -> Option<crate::objects::Flavor> {
        self.driver().object_flavor()
    }

    fn object<'a>(
        &'a self,
        object: &'a crate::objects::ObjectRef,
    ) -> BoxFuture<'a, Result<crate::objects::ObjectDefinition>> {
        Box::pin(async move { run!(self, true, |d| d.object(object)).await })
    }

    fn apply_object<'a>(
        &'a self,
        plan: &'a crate::objects::ChangePlan,
        backup: Option<&'a crate::objects::Backup>,
    ) -> BoxFuture<'a, Result<crate::objects::ApplyOutcome>> {
        Box::pin(async move { run!(self, false, |d| d.apply_object(plan, backup)).await })
    }

    fn children<'a>(&'a self, node: &'a ObjectPath) -> BoxFuture<'a, Result<Vec<ObjectNode>>> {
        Box::pin(async move { run!(self, true, |d| d.children(node)).await })
    }

    fn session_id(&self) -> Option<u64> {
        self.driver().session_id()
    }

    fn link(&self) -> Option<watch::Receiver<Link>> {
        Some(self.link.subscribe())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::connection::Engine;

    /// A session that answers until `dead`, then fails as a killed one does.
    #[derive(Default)]
    struct Fake {
        dead: AtomicBool,
        runs: AtomicUsize,
        texts: Mutex<Vec<String>>,
    }

    impl Driver for Fake {
        fn capabilities(&self) -> Capabilities {
            Capabilities::SQL
        }
        fn server_version(&self) -> &str {
            "MySQL 8.4.0"
        }
        fn execute<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<ResultStream>> {
            self.runs.fetch_add(1, Ordering::Relaxed);
            lock(&self.texts).push(text.to_owned());
            let dead = self.dead.load(Ordering::Relaxed);
            Box::pin(async move {
                if dead {
                    let io = std::io::Error::from(std::io::ErrorKind::BrokenPipe);
                    return Err(mysql_async::Error::from(io).into());
                }
                Ok(ResultStream::affected(0))
            })
        }
        fn cancel(&self) -> BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn classify<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Statement>> {
            Box::pin(async move { Ok(classify(text, Engine::Mysql)) })
        }
        fn set_auto_commit(&self, _on: bool) -> BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    type Opened = Arc<Mutex<Vec<Arc<Fake>>>>;

    /// A healing driver over a session that is already dead, and the sessions its
    /// reconnects opened (`reconnects` false: reconnecting fails).
    fn healing(reconnects: bool) -> (Healing, Arc<Fake>, Opened) {
        let first = Arc::new(Fake::default());
        first.dead.store(true, Ordering::Relaxed);
        let opened: Opened = Arc::default();
        let o = Arc::clone(&opened);
        let dial: Dial = Box::new(move || {
            let o = Arc::clone(&o);
            Box::pin(async move {
                if !reconnects {
                    return Err(DriverError::Invalid("connection refused".into()));
                }
                let f = Arc::new(Fake::default());
                lock(&o).push(Arc::clone(&f));
                Ok(f as Arc<dyn Driver>)
            })
        });
        let h = Healing::with_dial(
            ConnectionConfig::new(Engine::Mysql),
            Arc::clone(&first) as Arc<dyn Driver>,
            dial,
        );
        (h, first, opened)
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    #[test]
    fn a_read_runs_again_on_a_new_session() {
        let (h, first, opened) = healing(true);
        rt().block_on(h.execute("SELECT 1")).unwrap();
        assert_eq!(first.runs.load(Ordering::Relaxed), 1);
        assert_eq!(lock(&opened)[0].runs.load(Ordering::Relaxed), 1);
        assert_eq!(*h.link.borrow(), Link::Up);
    }

    #[test]
    fn a_write_is_not_run_again_but_the_next_call_works() {
        let (h, _, opened) = healing(true);
        let rt = rt();
        let e = rt.block_on(h.execute("UPDATE t SET a = 1")).unwrap_err();
        assert!(matches!(e, DriverError::NotRetried), "{e}");
        assert_eq!(lock(&opened)[0].runs.load(Ordering::Relaxed), 0);
        rt.block_on(h.execute("UPDATE t SET a = 1")).unwrap();
    }

    #[test]
    fn a_read_in_an_open_transaction_is_not_run_again() {
        let (h, first, opened) = healing(true);
        let rt = rt();
        rt.block_on(h.set_auto_commit(false)).unwrap();
        first.dead.store(false, Ordering::Relaxed);
        rt.block_on(h.execute("UPDATE t SET a = 1")).unwrap();
        first.dead.store(true, Ordering::Relaxed);
        let e = rt.block_on(h.execute("SELECT 1")).unwrap_err();
        assert!(matches!(e, DriverError::TransactionLost), "{e}");
        assert_eq!(lock(&opened)[0].runs.load(Ordering::Relaxed), 0);
        rt.block_on(h.execute("SELECT 1")).unwrap();
    }

    #[test]
    fn a_failed_reconnect_backs_off() {
        let (h, first, _) = healing(false);
        let rt = rt();
        let e = rt.block_on(h.execute("SELECT 1")).unwrap_err();
        assert!(matches!(e, DriverError::ReconnectFailed(_)), "{e}");
        assert!(matches!(*h.link.borrow(), Link::Lost(_)));
        // Within the cooldown: no new attempt, the session is not even tried.
        let e = rt.block_on(h.execute("SELECT 1")).unwrap_err();
        assert!(matches!(e, DriverError::ReconnectFailed(_)), "{e}");
        assert_eq!(first.runs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_picked_schema_is_set_again_on_a_new_session() {
        let (h, first, opened) = healing(true);
        let rt = rt();
        first.dead.store(false, Ordering::Relaxed);
        let pick = crate::session_schema::switch_sql(Engine::Mysql, "shop").unwrap();
        rt.block_on(h.execute(&pick)).unwrap();
        first.dead.store(true, Ordering::Relaxed);
        rt.block_on(h.execute("SELECT 1")).unwrap();
        assert_eq!(*lock(&lock(&opened)[0].texts), ["USE `shop`", "SELECT 1"]);
    }
}
