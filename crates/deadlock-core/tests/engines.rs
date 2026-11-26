//! Connects to the four engines in `docker-compose.yml` through the `Driver` trait.
//!
//! Ignored by default. Run with the stack up:
//! `docker compose up -d --wait && cargo test -p deadlock-core --test engines -- --ignored`
//!
//! The Oracle tests use the Thin driver; with `DEADLOCK_TEST_ORACLE=thick` they run
//! the same tests on the Thick one (needs Oracle Instant Client).

use std::time::{Duration, Instant};

use deadlock_core::connection::{ConnectionConfig, Encryption, Engine};
use deadlock_core::driver::{Driver, Runtime, Value, connect};

const PASSWORD: &str = "deadlock";

fn config(engine: Engine, port: u16, database: &str) -> ConnectionConfig {
    ConnectionConfig {
        id: "test".into(),
        name: "test".into(),
        host: "127.0.0.1".into(),
        port,
        database: database.into(),
        user: if engine == Engine::Redis {
            String::new()
        } else {
            "deadlock".into()
        },
        thick: engine == Engine::Oracle && thick(),
        ..ConnectionConfig::new(engine)
    }
}

/// The Oracle tests run on the Thick driver.
fn thick() -> bool {
    std::env::var("DEADLOCK_TEST_ORACLE").is_ok_and(|d| d == "thick")
}

fn rows(rt: &Runtime, driver: &dyn Driver, sql: &str) -> Vec<Vec<Value>> {
    rt.block_on(async { driver.execute(sql).await.unwrap().collect().await.unwrap() })
}

#[test]
#[ignore = "needs docker compose"]
fn mysql_connects_and_maps_types() {
    let rt = Runtime::new().unwrap();
    let driver = rt
        .block_on(connect(&config(Engine::Mysql, 3306, "deadlock"), PASSWORD))
        .unwrap();
    assert!(
        driver.server_version().starts_with("MySQL 8.4"),
        "{}",
        driver.server_version()
    );
    let got = rows(
        &rt,
        &*driver,
        "SELECT 1, CAST(18446744073709551615 AS UNSIGNED), 1.50, 'héllo', NULL, \
         DATE '2026-11-22', TIMESTAMP '2026-11-22 10:11:12.5', TIME '-01:02:03', x'DEAD'",
    );
    assert_eq!(
        got[0][..5],
        [
            Value::Int(1),
            Value::UInt(u64::MAX),
            Value::Decimal("1.50".into()),
            Value::Text("héllo".into()),
            Value::Null
        ]
    );
    assert_eq!(got[0][5].to_string(), "2026-11-22");
    assert_eq!(got[0][6].to_string(), "2026-11-22 10:11:12.500");
    assert_eq!(got[0][7].to_string(), "-01:02:03");
    assert_eq!(got[0][8], Value::Bytes(vec![0xDE, 0xAD]));
}

#[test]
#[ignore = "needs docker compose"]
fn mysql_pages_and_cancels() {
    let rt = Runtime::new().unwrap();
    let driver = rt
        .block_on(connect(&config(Engine::Mysql, 3306, ""), PASSWORD))
        .unwrap();
    // 1 600 rows arrive as pages of 500, 500, 500 and 100.
    let pages = rt.block_on(async {
        let mut s = driver
            .execute("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 40) SELECT a.i FROM n a, n b")
            .await
            .unwrap();
        let mut sizes = vec![];
        while let Some(p) = s.next_page().await {
            sizes.push(p.unwrap().len());
        }
        sizes
    });
    assert_eq!(pages, [500, 500, 500, 100]);

    let started = Instant::now();
    let d = driver.clone();
    let sleep = rt.spawn(async move { d.execute("SELECT SLEEP(20)").await.map(|_| ()) });
    rt.block_on(async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        driver.cancel().await.unwrap();
        sleep.await.unwrap().unwrap();
    });
    assert!(started.elapsed() < Duration::from_secs(10));
    let affected = rt.block_on(async { driver.execute("DO 1").await.unwrap().affected_rows });
    assert_eq!(affected, Some(0));
}

#[test]
#[ignore = "needs docker compose"]
fn mariadb_connects_and_is_detected() {
    let rt = Runtime::new().unwrap();
    // Chosen as MySQL: the flavor comes from the server, not the form.
    let cfg = ConnectionConfig {
        encryption: Encryption::Off,
        ..config(Engine::Mysql, 3307, "deadlock")
    };
    let driver = rt.block_on(connect(&cfg, PASSWORD)).unwrap();
    assert!(
        driver.server_version().starts_with("MariaDB 11"),
        "{}",
        driver.server_version()
    );
    assert_eq!(rows(&rt, &*driver, "SELECT 1 + 1"), [[Value::Int(2)]]);
}

#[test]
#[ignore = "needs docker compose"]
fn oracle_connects_and_maps_types() {
    let rt = Runtime::new().unwrap();
    let driver = rt
        .block_on(connect(&config(Engine::Oracle, 1521, "FREEPDB1"), PASSWORD))
        .unwrap();
    assert!(
        driver.server_version().starts_with("Oracle 23"),
        "{}",
        driver.server_version()
    );
    let got = rows(
        &rt,
        &*driver,
        "SELECT 42, 3.14, 'héllo', DATE '2026-11-22', TIMESTAMP '2026-11-22 10:11:12.5', \
         TO_CLOB('long text'), CAST(NULL AS VARCHAR2(10)) FROM dual",
    );
    assert_eq!(
        got[0],
        [
            Value::Decimal("42".into()),
            Value::Decimal("3.14".into()),
            Value::Text("héllo".into()),
            got[0][3].clone(),
            got[0][4].clone(),
            Value::Text("long text".into()),
            Value::Null
        ]
    );
    assert_eq!(got[0][3].to_string(), "2026-11-22 00:00:00");
    assert_eq!(got[0][4].to_string(), "2026-11-22 10:11:12.500");
    let affected = rt.block_on(async {
        driver
            .execute("BEGIN NULL; END;")
            .await
            .unwrap()
            .affected_rows
    });
    assert!(affected.is_some());
}

#[test]
#[ignore = "needs docker compose"]
fn redis_connects_and_runs_a_command_line() {
    let rt = Runtime::new().unwrap();
    let driver = rt
        .block_on(connect(&config(Engine::Redis, 6379, "1"), PASSWORD))
        .unwrap();
    assert!(
        driver.server_version().starts_with("Redis 8"),
        "{}",
        driver.server_version()
    );
    assert_eq!(
        rows(&rt, &*driver, "SET deadlock:test \"a b\""),
        [[Value::Text("OK".into())]]
    );
    assert_eq!(
        rows(&rt, &*driver, "GET deadlock:test"),
        [[Value::Text("a b".into())]]
    );
    assert_eq!(rows(&rt, &*driver, "DEL deadlock:test"), [[Value::Int(1)]]);
    assert_eq!(rows(&rt, &*driver, "GET deadlock:test"), [[Value::Null]]);
    assert!(rt.block_on(driver.execute("NOSUCHCOMMAND")).is_err());
}

#[test]
#[ignore = "needs docker compose"]
fn wrong_password_is_an_error_with_text() {
    let rt = Runtime::new().unwrap();
    let err = rt
        .block_on(connect(&config(Engine::Mysql, 3306, ""), "wrong"))
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("Access denied"), "{err}");
    assert!(!err.contains("wrong"));
}

/// Builds a small schema in `deadlock` and reads it back through `children`.
fn browses_schema(engine: Engine, port: u16) {
    use deadlock_core::driver::{NodeKind, ObjectPath};
    let rt = Runtime::new().unwrap();
    let driver = rt
        .block_on(connect(&config(engine, port, "deadlock"), PASSWORD))
        .unwrap();
    for sql in [
        "DROP VIEW IF EXISTS p3_totals",
        "DROP TABLE IF EXISTS p3_orders",
        "DROP TABLE IF EXISTS p3_customers",
        "DROP PROCEDURE IF EXISTS p3_noop",
        "CREATE TABLE p3_customers (id BIGINT PRIMARY KEY, name VARCHAR(40))",
        "CREATE TABLE p3_orders (id BIGINT UNSIGNED PRIMARY KEY, customer_id BIGINT, \
         total DECIMAL(12,2), FOREIGN KEY (customer_id) REFERENCES p3_customers(id))",
        "CREATE VIEW p3_totals AS SELECT customer_id, SUM(total) AS s FROM p3_orders GROUP BY customer_id",
        "CREATE PROCEDURE p3_noop() BEGIN END",
    ] {
        rows(&rt, &*driver, sql);
    }
    let path = |p: &[&str]| ObjectPath(p.iter().map(|s| (*s).to_owned()).collect());
    let dbs = rt.block_on(driver.children(&path(&[]))).unwrap();
    let ours = dbs.iter().position(|d| d.name == "deadlock").unwrap();
    let system = dbs
        .iter()
        .position(|d| d.name == "information_schema")
        .unwrap();
    assert!(ours < system && dbs[system].system && !dbs[ours].system);

    let groups = rt.block_on(driver.children(&path(&["deadlock"]))).unwrap();
    let names: Vec<_> = groups.iter().map(|g| g.name.as_str()).collect();
    let mut want = vec![
        "Tables",
        "Views",
        "Procedures",
        "Functions",
        "Triggers",
        "Events",
    ];
    if engine == Engine::Mariadb {
        want.push("Sequences");
    }
    assert_eq!(names, want);
    let find = |g: usize, name: &str| {
        groups[g]
            .children
            .as_ref()
            .unwrap()
            .iter()
            .find(|n| n.name == name)
            .cloned()
            .unwrap()
    };
    let orders = find(0, "p3_orders");
    let cols = orders.children.unwrap();
    let col = |n: &str| cols.iter().find(|c| c.name == n).unwrap();
    // MariaDB keeps the display width: `bigint(20) unsigned`.
    assert!(col("id").primary_key && col("id").detail.starts_with("bigint"));
    assert!(col("id").detail.ends_with("unsigned"));
    assert!(col("customer_id").foreign_key && !col("customer_id").primary_key);
    assert_eq!(col("total").detail, "decimal(12,2)");
    assert_eq!(find(1, "p3_totals").kind, NodeKind::View);
    assert_eq!(find(2, "p3_noop").kind, NodeKind::Routine);

    let direct = rt
        .block_on(driver.children(&path(&["deadlock", "Tables", "p3_orders"])))
        .unwrap();
    assert_eq!(direct.len(), 3);
    assert!(
        rt.block_on(driver.children(&path(&["deadlock", "Tables", "nope"])))
            .is_err()
    );
}

#[test]
#[ignore = "needs docker compose"]
fn mysql_browses_schema() {
    browses_schema(Engine::Mysql, 3306);
}

#[test]
#[ignore = "needs docker compose"]
fn mariadb_browses_schema() {
    browses_schema(Engine::Mariadb, 3307);
}

/// Timings for a 5 000-table database `big` (created by hand, see the Phase 3 report).
#[test]
#[ignore = "needs docker compose and the `big` database"]
fn mysql_big_schema_timings() {
    use std::collections::HashSet;

    use deadlock_core::driver::ObjectPath;
    use deadlock_core::schema::{ConnectionTree, Query, Tree};
    let rt = Runtime::new().unwrap();
    let mut c = config(Engine::Mysql, 3306, "");
    c.user = "root".into();
    let driver = rt.block_on(connect(&c, PASSWORD)).unwrap();
    let started = Instant::now();
    let dbs = rt.block_on(driver.children(&ObjectPath(vec![]))).unwrap();
    eprintln!("databases: {} in {:?}", dbs.len(), started.elapsed());
    let started = Instant::now();
    let groups = rt
        .block_on(driver.children(&ObjectPath(vec!["big".into()])))
        .unwrap();
    let tables = groups[0].children.as_ref().unwrap().len();
    eprintln!(
        "big: {tables} tables with columns in {:?}",
        started.elapsed()
    );
    assert_eq!(tables, 5000);

    let mut tree = Tree::default();
    tree.connections.push(ConnectionTree {
        id: "c".into(),
        name: "c".into(),
        nodes: Some(dbs),
        hidden: Vec::new(),
    });
    assert!(tree.set_children(&["c".into(), "big".into()], groups));
    for id in [vec!["c"], vec!["c", "big"], vec!["c", "big", "Tables"]] {
        tree.expanded
            .insert(id.into_iter().map(String::from).collect());
    }
    let started = Instant::now();
    let rows = tree.rows();
    eprintln!("rows: {} in {:?}", rows.len(), started.elapsed());
    let mut worst = Duration::ZERO;
    for typed in [
        "t", "t4", "t49", "t499", "t4999", "a", "am", "amo", "amou", "amoun", "amount",
    ] {
        let started = Instant::now();
        let (rows, found) = tree.search(&Query::new(typed).unwrap(), &HashSet::new());
        let took = started.elapsed();
        worst = worst.max(took);
        eprintln!(
            "search {typed:>6}: {found:>5} found, {:>5} rows in {took:?}",
            rows.len()
        );
    }
    eprintln!("worst keystroke: {worst:?}");
}

/// Every result set of multi-statement text and of a procedure, in order, with errors
/// arriving in the set they belong to.
fn multiple_result_sets(engine: Engine, port: u16) {
    let rt = Runtime::new().unwrap();
    let cfg = ConnectionConfig {
        encryption: Encryption::Off,
        ..config(engine, port, "deadlock")
    };
    let driver = rt.block_on(connect(&cfg, PASSWORD)).unwrap();
    let sets = |sql: &'static str| {
        let driver = driver.clone();
        rt.block_on(async move {
            let mut out = Vec::new();
            let mut next = Some(driver.execute(sql).await);
            while let Some(stream) = next.take() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(e) => {
                        out.push(format!("error: {e}"));
                        break;
                    }
                };
                let rx = stream.next_result();
                let affected = stream.affected_rows;
                match stream.collect().await {
                    Ok(_) if affected.is_some() => out.push(format!("affected {affected:?}")),
                    Ok(rows) => out.push(format!("{} rows", rows.len())),
                    Err(e) => out.push(format!("error: {e}")),
                }
                if let Some(rx) = rx {
                    next = rx.await.ok();
                }
            }
            out
        })
    };
    rows(&rt, &*driver, "DROP PROCEDURE IF EXISTS two_sets");
    rows(
        &rt,
        &*driver,
        "CREATE PROCEDURE two_sets() BEGIN SELECT 1 UNION SELECT 2; SELECT 'x'; END",
    );
    // mysql_async folds the procedure's closing OK packet into the last set.
    assert_eq!(sets("CALL two_sets()"), ["2 rows", "1 rows"]);
    assert_eq!(
        sets("SELECT 1; DO 2; SELECT 3 UNION SELECT 4"),
        ["1 rows", "affected Some(0)", "2 rows"]
    );
    let got = sets("SELECT 1; SELECT * FROM no_such_table");
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got[1].contains("no_such_table"), "{got:?}");
    // The session is usable afterwards.
    assert_eq!(rows(&rt, &*driver, "SELECT 5"), [[Value::Int(5)]]);
}

#[test]
#[ignore = "needs docker compose"]
fn mysql_returns_every_result_set() {
    multiple_result_sets(Engine::Mysql, 3306);
}

#[test]
#[ignore = "needs docker compose"]
fn mariadb_returns_every_result_set() {
    multiple_result_sets(Engine::Mariadb, 3307);
}

fn connect_as(rt: &Runtime, cfg: &ConnectionConfig) -> std::sync::Arc<dyn Driver> {
    rt.block_on(connect(cfg, PASSWORD)).unwrap()
}

fn run(rt: &Runtime, driver: &dyn Driver, sql: &str) -> Result<Vec<Vec<Value>>, String> {
    rt.block_on(async {
        let s = driver.execute(sql).await.map_err(|e| e.to_string())?;
        s.collect().await.map_err(|e| e.to_string())
    })
}

fn count(rt: &Runtime, driver: &dyn Driver, table: &str) -> String {
    rows(rt, driver, &format!("SELECT COUNT(*) FROM {table}"))[0][0].to_string()
}

fn open(rt: &Runtime, driver: &dyn Driver) -> bool {
    rt.block_on(driver.transaction_open()).unwrap()
}

/// A manual transaction holds several statements, is visible to a second session only
/// after commit, and rolls back cleanly.
fn manual_transactions(engine: Engine, port: u16, database: &str) {
    let rt = Runtime::new().unwrap();
    let cfg = ConnectionConfig {
        encryption: Encryption::Off,
        ..config(engine, port, database)
    };
    let (a, b) = (connect_as(&rt, &cfg), connect_as(&rt, &cfg));
    for sql in [
        "DROP TABLE IF EXISTS p5_tx",
        "CREATE TABLE p5_tx (id INT PRIMARY KEY)",
    ] {
        run(&rt, &*a, sql).unwrap();
    }
    rt.block_on(a.set_auto_commit(false)).unwrap();
    assert!(!open(&rt, &*a));
    // Reads leave nothing to commit, though MySQL opens a transaction for them.
    assert_eq!(count(&rt, &*a, "p5_tx"), "0");
    assert!(!open(&rt, &*a));
    run(&rt, &*a, "INSERT INTO p5_tx VALUES (1)").unwrap();
    assert!(open(&rt, &*a));
    run(&rt, &*a, "INSERT INTO p5_tx VALUES (2)").unwrap();
    run(&rt, &*a, "UPDATE p5_tx SET id = id + 10").unwrap();
    assert_eq!(count(&rt, &*a, "p5_tx"), "2");
    assert!(open(&rt, &*a));
    assert_eq!(count(&rt, &*b, "p5_tx"), "0");
    rt.block_on(a.end_transaction(false)).unwrap();
    assert!(!open(&rt, &*a));
    assert_eq!(count(&rt, &*a, "p5_tx"), "0");
    assert!(!open(&rt, &*a));

    run(&rt, &*a, "INSERT INTO p5_tx VALUES (3)").unwrap();
    assert_eq!(count(&rt, &*b, "p5_tx"), "0");
    rt.block_on(a.end_transaction(true)).unwrap();
    assert!(!open(&rt, &*a));
    // MySQL's `b` reads a fresh snapshot: in Auto mode each SELECT is its own transaction.
    assert_eq!(count(&rt, &*b, "p5_tx"), "1");
    // DDL commits what ran before it, and the reads after it leave nothing to commit.
    run(&rt, &*a, "INSERT INTO p5_tx VALUES (5)").unwrap();
    run(&rt, &*a, "ALTER TABLE p5_tx ADD c INT").unwrap();
    assert_eq!(count(&rt, &*a, "p5_tx"), "2");
    assert!(!open(&rt, &*a));
    assert_eq!(count(&rt, &*b, "p5_tx"), "2");

    // Auto mode: every statement commits on its own.
    rt.block_on(a.set_auto_commit(true)).unwrap();
    run(&rt, &*a, "INSERT INTO p5_tx VALUES (4, NULL)").unwrap();
    assert!(!open(&rt, &*a));
    assert_eq!(count(&rt, &*b, "p5_tx"), "3");
    // A transaction started by hand counts as open before any write.
    if engine != Engine::Oracle {
        run(&rt, &*a, "START TRANSACTION").unwrap();
        assert_eq!(count(&rt, &*a, "p5_tx"), "3");
        assert!(open(&rt, &*a));
        rt.block_on(a.end_transaction(false)).unwrap();
        assert!(!open(&rt, &*a));
    }
    run(&rt, &*a, "DROP TABLE p5_tx").unwrap();
}

#[test]
#[ignore = "needs docker compose"]
fn mysql_manual_transactions() {
    manual_transactions(Engine::Mysql, 3306, "deadlock");
}

#[test]
#[ignore = "needs docker compose"]
fn mariadb_manual_transactions() {
    manual_transactions(Engine::Mariadb, 3307, "deadlock");
}

#[test]
#[ignore = "needs docker compose"]
fn oracle_manual_transactions() {
    manual_transactions(Engine::Oracle, 1521, "FREEPDB1");
}

/// The classifier's implicit-commit flag matches what the server does to an open
/// transaction, and a dropped connection says so instead of reporting no transaction.
fn implicit_commits(engine: Engine, port: u16) {
    use deadlock_core::classify::classify;
    let rt = Runtime::new().unwrap();
    let cfg = ConnectionConfig {
        encryption: Encryption::Off,
        ..config(engine, port, "deadlock")
    };
    let (a, b) = (connect_as(&rt, &cfg), connect_as(&rt, &cfg));
    for sql in [
        "DROP TABLE IF EXISTS p5_ic",
        "DROP TABLE IF EXISTS p5_other",
        "CREATE TABLE p5_ic (id INT PRIMARY KEY)",
    ] {
        run(&rt, &*a, sql).unwrap();
    }
    rt.block_on(a.set_auto_commit(false)).unwrap();
    // Each statement runs after an uncommitted insert; the insert survives the
    // rollback that follows only when the statement committed it.
    let mut kept = 0;
    for (id, (setup, sql)) in [
        (None, "CREATE TEMPORARY TABLE p5_tmp (a INT)"),
        (None, "DROP TEMPORARY TABLE p5_tmp"),
        (None, "SAVEPOINT s1"),
        (None, "SELECT * FROM p5_ic"),
        (None, "CREATE TABLE p5_other (a INT)"),
        (None, "ALTER TABLE p5_other ADD b INT"),
        (None, "LOCK TABLES p5_other WRITE"),
        (
            Some("LOCK TABLES p5_ic WRITE, p5_other WRITE"),
            "UNLOCK TABLES",
        ),
        (None, "TRUNCATE TABLE p5_other"),
        (None, "DROP TABLE p5_other"),
        (None, "START TRANSACTION"),
        (None, "SET autocommit = 1"),
    ]
    .into_iter()
    .enumerate()
    {
        if let Some(setup) = setup {
            run(&rt, &*a, setup).unwrap();
        }
        run(&rt, &*a, &format!("INSERT INTO p5_ic VALUES ({id})")).unwrap();
        assert!(open(&rt, &*a), "{sql}");
        run(&rt, &*a, sql).unwrap();
        rt.block_on(a.end_transaction(false)).unwrap();
        let committed = count(&rt, &*b, &format!("p5_ic WHERE id = {id}")) == "1";
        kept += usize::from(committed);
        assert_eq!(
            classify(sql, engine).implicit_commit,
            committed,
            "{sql}: the server {}",
            if committed {
                "committed"
            } else {
                "did not commit"
            }
        );
    }
    let committed = count(&rt, &*b, "p5_ic");
    assert_eq!(committed, kept.to_string());
    rt.block_on(a.set_auto_commit(false)).unwrap();

    // A dropped connection: the server rolled the transaction back.
    let conn_id = rows(&rt, &*a, "SELECT CONNECTION_ID()")[0][0].to_string();
    run(&rt, &*a, "INSERT INTO p5_ic VALUES (100)").unwrap();
    assert!(open(&rt, &*a));
    run(&rt, &*b, &format!("KILL {conn_id}")).unwrap();
    // Said so, and not run again on the new session; that one holds nothing.
    let e = run(&rt, &*a, "SELECT 1").unwrap_err();
    assert!(e.contains("transaction was rolled back"), "{e}");
    assert!(!open(&rt, &*a));
    assert_eq!(count(&rt, &*b, "p5_ic"), committed);
    run(&rt, &*b, "DROP TABLE p5_ic").unwrap();
}

#[test]
#[ignore = "needs docker compose"]
fn mysql_implicit_commits() {
    implicit_commits(Engine::Mysql, 3306);
}

#[test]
#[ignore = "needs docker compose"]
fn mariadb_implicit_commits() {
    implicit_commits(Engine::Mariadb, 3307);
}

/// Read-only sessions refuse writes before sending them, and the server refuses the
/// writes the client cannot see: a function that inserts, called from a SELECT.
fn read_only_sessions(engine: Engine, port: u16) {
    let rt = Runtime::new().unwrap();
    let cfg = ConnectionConfig {
        encryption: Encryption::Off,
        ..config(engine, port, "deadlock")
    };
    // Root, so MySQL lets a function that writes be created with binary logging on.
    let rw = connect_as(
        &rt,
        &ConnectionConfig {
            user: "root".into(),
            ..cfg.clone()
        },
    );
    for sql in [
        "DROP FUNCTION IF EXISTS p5_bump",
        "DROP TABLE IF EXISTS p5_ro",
        "CREATE TABLE p5_ro (id INT AUTO_INCREMENT PRIMARY KEY)",
        "CREATE FUNCTION p5_bump() RETURNS INT DETERMINISTIC MODIFIES SQL DATA \
         BEGIN INSERT INTO p5_ro VALUES (NULL); RETURN 1; END",
    ] {
        run(&rt, &*rw, sql).unwrap();
    }
    let ro = connect_as(
        &rt,
        &ConnectionConfig {
            read_only: true,
            ..cfg
        },
    );
    let err = run(&rt, &*ro, "UPDATE p5_ro SET id = id").unwrap_err();
    assert!(err.contains("read-only") && err.contains("UPDATE"), "{err}");
    assert!(run(&rt, &*ro, "DROP TABLE p5_ro").is_err());
    let err = run(&rt, &*ro, "SELECT p5_bump()").unwrap_err();
    assert!(err.contains("READ ONLY"), "{err}");
    assert_eq!(count(&rt, &*ro, "p5_ro"), "0");
    for sql in ["DROP FUNCTION p5_bump", "DROP TABLE p5_ro"] {
        run(&rt, &*rw, sql).unwrap();
    }
}

#[test]
#[ignore = "needs docker compose"]
fn mysql_read_only_sessions() {
    read_only_sessions(Engine::Mysql, 3306);
}

#[test]
#[ignore = "needs docker compose"]
fn mariadb_read_only_sessions() {
    read_only_sessions(Engine::Mariadb, 3307);
}

/// Thick without Instant Client says so. Runs only with the loader pointed at an empty
/// folder, in a process of its own: `DEADLOCK_ORACLE_CLIENT_DIR=$(mktemp -d) cargo test
/// -p deadlock-core --test engines -- --ignored thick_without_instant_client`.
#[test]
#[ignore = "needs an empty DEADLOCK_ORACLE_CLIENT_DIR"]
fn thick_without_instant_client() {
    let Some(dir) = std::env::var_os("DEADLOCK_ORACLE_CLIENT_DIR") else {
        return;
    };
    let rt = Runtime::new().unwrap();
    let cfg = ConnectionConfig {
        thick: true,
        ..config(Engine::Oracle, 1521, "FREEPDB1")
    };
    let e = rt
        .block_on(connect(&cfg, PASSWORD))
        .err()
        .unwrap()
        .to_string();
    let dir = std::path::Path::new(&dir).display().to_string();
    assert!(
        e.contains(&format!("libclntsh.so was not found in {dir}")),
        "{e}"
    );
    assert!(e.contains("Install Instant Client"), "{e}");
}

/// JSON comes back as text and a LOB selected with a bind reads, on either driver.
#[test]
#[ignore = "needs docker compose"]
fn oracle_json_and_lob_binds() {
    let rt = Runtime::new().unwrap();
    let d = rt
        .block_on(connect(&config(Engine::Oracle, 1521, "FREEPDB1"), PASSWORD))
        .unwrap();
    let json = r#"SELECT JSON('{"b":1,"a":[true,null]}') FROM dual"#;
    if thick() {
        // The `oracle` crate cannot fetch the native JSON type: the error says what to do.
        let e = run(&rt, &*d, json).unwrap_err();
        assert!(e.contains("JSON_SERIALIZE"), "{e}");
    } else {
        let text = rows(&rt, &*d, json)[0][0].to_string();
        assert!(
            text.contains("\"a\":[true,null]") && text.contains("\"b\":1"),
            "{text}"
        );
    }
    let binds = [("A".to_owned(), Some("x".repeat(5000)))];
    let got = rt
        .block_on(async {
            d.execute_bound("SELECT TO_CLOB(:a) || 'y' FROM dual", &binds)
                .await?
                .collect()
                .await
        })
        .unwrap();
    assert_eq!(got[0][0].to_string().len(), 5001);
}

#[test]
#[ignore = "needs docker compose"]
fn oracle_read_only_refuses_writes() {
    let rt = Runtime::new().unwrap();
    let cfg = ConnectionConfig {
        read_only: true,
        ..config(Engine::Oracle, 1521, "FREEPDB1")
    };
    let ro = connect_as(&rt, &cfg);
    let err = run(&rt, &*ro, "DELETE FROM dual").unwrap_err();
    assert!(err.contains("read-only"), "{err}");
    assert_eq!(
        rows(&rt, &*ro, "SELECT 1 FROM dual"),
        [[Value::Decimal("1".into())]]
    );
    // Each statement ran in a READ ONLY transaction that Auto mode committed.
    assert!(!open(&rt, &*ro));
}

#[test]
#[ignore = "needs docker compose"]
fn redis_read_only_uses_command_flags() {
    let rt = Runtime::new().unwrap();
    let cfg = ConnectionConfig {
        read_only: true,
        ..config(Engine::Redis, 6379, "1")
    };
    let ro = connect_as(&rt, &cfg);
    for cmd in [
        "SET k v",
        "del k",
        "EVAL \"return 1\" 0",
        "HSET h f v",
        "NOSUCHCMD x",
    ] {
        let err = run(&rt, &*ro, cmd).unwrap_err();
        assert!(err.contains("read-only"), "{cmd}: {err}");
    }
    for cmd in ["GET k", "CONFIG GET maxmemory", "SCAN 0", "PING"] {
        run(&rt, &*ro, cmd).unwrap_or_else(|e| panic!("{cmd}: {e}"));
    }
    let s = rt.block_on(ro.classify("CLIENT LIST")).unwrap();
    assert!(!s.changes());
}

mod table_edits {
    use std::collections::BTreeMap;

    use deadlock_core::edit::{
        self, Change, ChangeKind, Edit, Pending, RowId, TableInfo, TableRef, page_sql, parse_input,
    };

    use super::*;

    fn table(name: &str) -> TableRef {
        TableRef {
            database: "deadlock".into(),
            table: name.into(),
        }
    }

    fn info(rt: &Runtime, d: &dyn Driver, t: &TableRef) -> TableInfo {
        rt.block_on(d.table_info(t)).unwrap()
    }

    fn all(rt: &Runtime, d: &dyn Driver, t: &TableRef, i: &TableInfo) -> Vec<Vec<Value>> {
        rows(
            rt,
            d,
            &edit::select_sql(t, i, "", &i.key_order().unwrap_or_default()).unwrap(),
        )
    }

    fn apply(rt: &Runtime, d: &dyn Driver, changes: &[Change], auto: bool) -> Result<(), String> {
        rt.block_on(d.apply(changes, auto))
            .map_err(|e| e.to_string())
    }

    fn fresh(rt: &Runtime, d: &dyn Driver, name: &str, ddl: &str) -> TableRef {
        run(rt, d, &format!("DROP TABLE IF EXISTS {name}")).unwrap();
        run(rt, d, &format!("CREATE TABLE {name} ({ddl})")).unwrap();
        table(name)
    }

    /// Typed input → pending insert and update → generated SQL → the server → the same
    /// values read back, with backslash escapes on and off.
    fn literals_round_trip(engine: Engine, port: u16) {
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &config(engine, port, "deadlock"));
        let t = fresh(
            &rt,
            &*d,
            "p6_types",
            "id INT AUTO_INCREMENT PRIMARY KEY, ti TINYINT, bu BIGINT UNSIGNED, \
             de DECIMAL(10,2), fl DOUBLE, d DATE, dt DATETIME(6), tm TIME(6), yr YEAR, \
             en ENUM('a','it''s'), st SET('x','y'), bt BIT(5), js JSON, vb VARBINARY(16), \
             bl BLOB, tx TEXT, vc VARCHAR(5) NULL, g INT AS (ti + 1) VIRTUAL",
        );
        let i = info(&rt, &*d, &t);
        assert_eq!(i.key, Some(vec![0]));
        assert!(i.backslash_escapes && !i.view);
        assert!(i.columns[0].auto_increment && i.columns[17].generated);
        assert!(i.columns[17].locked().is_some());
        let inputs = [
            ("ti", "-128"),
            ("bu", "18446744073709551615"),
            ("de", "-12345678.90"),
            ("fl", "0.1"),
            ("d", "2026-11-22"),
            ("dt", "2026-11-22 16:05:00.123456"),
            ("tm", "-838:59:59"),
            ("yr", "2026"),
            ("en", "it's"),
            ("st", "x,y"),
            ("bt", "10101"),
            ("js", r#"{"k": "it's \\ \"ok\""}"#),
            ("vb", "0x00FF5C27"),
            ("bl", "0x"),
            ("tx", "O'Brien \\n\\ \"q\" \u{1F600}\nline2 \0 end"),
            ("vc", ""),
        ];
        let pos = |name: &str| i.columns.iter().position(|c| c.name == name).unwrap();
        let mut p = Pending::default();
        let row = p.add(i.columns.len());
        let mut expected = BTreeMap::new();
        for (name, text) in inputs {
            let c = pos(name);
            let v = parse_input(&i.columns[c], text).unwrap_or_else(|e| panic!("{name}: {e}"));
            expected.insert(c, v.clone());
            p.set(row, c, Edit::Value(v), None);
        }
        let changes = p.changes(&t, &i, &[]).unwrap();
        apply(&rt, &*d, &changes, true).unwrap();
        let back = all(&rt, &*d, &t, &i);
        assert_eq!(back.len(), 1);
        let same = |c: usize, got: &Value, want: &Value| {
            if i.columns[c].data_type == "json" {
                let parse =
                    |v: &Value| serde_json::from_str::<serde_json::Value>(&v.to_string()).unwrap();
                assert_eq!(parse(got), parse(want), "{}", i.columns[c].name);
            } else {
                assert_eq!(got.to_string(), want.to_string(), "{}", i.columns[c].name);
            }
        };
        for (c, want) in &expected {
            same(*c, &back[0][*c], want);
        }
        assert_eq!(back[0][pos("g")].to_string(), "-127");

        // Update by key: quotes, backslashes, NULL, a new key.
        let mut p = Pending::default();
        let r = RowId::Loaded(0);
        let tx = Value::Text(r"a\'b''c\\".into());
        p.set(
            r,
            pos("tx"),
            Edit::Value(tx.clone()),
            Some(&back[0][pos("tx")]),
        );
        p.set(
            r,
            pos("vc"),
            Edit::Value(Value::Null),
            Some(&back[0][pos("vc")]),
        );
        p.set(r, 0, Edit::Value(Value::Int(77)), Some(&back[0][0]));
        apply(&rt, &*d, &p.changes(&t, &i, &back).unwrap(), true).unwrap();
        let again = all(&rt, &*d, &t, &i);
        assert_eq!(again[0][0], Value::Int(77));
        assert_eq!(again[0][pos("tx")], tx);
        assert_eq!(again[0][pos("vc")], Value::Null);

        // The same with NO_BACKSLASH_ESCAPES: `\` is plain text.
        run(
            &rt,
            &*d,
            "SET SESSION sql_mode = CONCAT(@@sql_mode, ',NO_BACKSLASH_ESCAPES')",
        )
        .unwrap();
        let i = info(&rt, &*d, &t);
        assert!(!i.backslash_escapes);
        let mut p = Pending::default();
        let tx = Value::Text(r"C:\new\'x'".into());
        p.set(
            r,
            pos("tx"),
            Edit::Value(tx.clone()),
            Some(&again[0][pos("tx")]),
        );
        apply(&rt, &*d, &p.changes(&t, &i, &again).unwrap(), true).unwrap();
        assert_eq!(all(&rt, &*d, &t, &i)[0][pos("tx")], tx);
        run(&rt, &*d, "DROP TABLE p6_types").unwrap();
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mysql_literals_round_trip() {
        literals_round_trip(Engine::Mysql, 3306);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mariadb_literals_round_trip() {
        literals_round_trip(Engine::Mariadb, 3307);
    }

    /// A submit whose row another session removed or re-keyed, or whose statement
    /// fails, leaves the table as it was; in Manual mode it leaves earlier work alone.
    fn failed_submits_change_nothing(engine: Engine, port: u16) {
        let rt = Runtime::new().unwrap();
        let cfg = config(engine, port, "deadlock");
        let (a, b) = (connect_as(&rt, &cfg), connect_as(&rt, &cfg));
        let t = fresh(&rt, &*a, "p6_rows", "id INT PRIMARY KEY, v VARCHAR(10)");
        run(
            &rt,
            &*a,
            "INSERT INTO p6_rows VALUES (1,'a'),(2,'b'),(3,'c')",
        )
        .unwrap();
        let i = info(&rt, &*a, &t);
        let loaded = all(&rt, &*a, &t, &i);
        let mut p = Pending::default();
        p.set(
            RowId::Loaded(0),
            1,
            Edit::Value(Value::Text("A".into())),
            Some(&loaded[0][1]),
        );
        p.set(
            RowId::Loaded(1),
            1,
            Edit::Value(Value::Text("B".into())),
            Some(&loaded[1][1]),
        );
        p.delete(&[RowId::Loaded(2)]);
        let added = p.add(2);
        p.set(added, 0, Edit::Value(Value::Int(4)), None);
        let changes = p.changes(&t, &i, &loaded).unwrap();
        assert_eq!(changes.len(), 4);
        // Someone else re-keys row 2 after it was loaded.
        run(&rt, &*b, "UPDATE p6_rows SET id = 20 WHERE id = 2").unwrap();
        let e = apply(&rt, &*a, &changes, true).unwrap_err();
        assert!(e.contains("change 2 of 4 touched 0 rows"), "{e}");
        let now = all(&rt, &*a, &t, &i);
        assert_eq!(now.len(), 3);
        assert_eq!(now[0][1], Value::Text("a".into()));
        assert!(!open(&rt, &*a));
        // Someone else deletes the row to be deleted.
        run(&rt, &*b, "UPDATE p6_rows SET id = 2 WHERE id = 20").unwrap();
        run(&rt, &*b, "DELETE FROM p6_rows WHERE id = 3").unwrap();
        assert!(
            apply(&rt, &*a, &changes, true)
                .unwrap_err()
                .contains("change 3 of 4")
        );
        // A failing statement (duplicate key) undoes the ones before it.
        run(&rt, &*b, "INSERT INTO p6_rows VALUES (3,'c'),(4,'d')").unwrap();
        let before = all(&rt, &*a, &t, &i);
        let e = apply(&rt, &*a, &changes, true).unwrap_err();
        assert!(
            e.contains("change 4 of 4 failed") && e.contains("Duplicate"),
            "{e}"
        );
        assert_eq!(all(&rt, &*a, &t, &i), before);
        // Setting a value the row already has still counts as one row.
        let same = vec![Change {
            kind: ChangeKind::Update,
            sql: "update `deadlock`.`p6_rows` set `v` = 'a' where `id` = 1".into(),
        }];
        apply(&rt, &*a, &same, true).unwrap();

        // Manual: a failed submit keeps the transaction's earlier work, a good one
        // stays uncommitted until Commit.
        run(&rt, &*b, "DELETE FROM p6_rows WHERE id = 4").unwrap();
        let before = all(&rt, &*a, &t, &i);
        rt.block_on(a.set_auto_commit(false)).unwrap();
        run(&rt, &*a, "INSERT INTO p6_rows VALUES (9,'z')").unwrap();
        let e = apply(&rt, &*a, &changes[1..2], false);
        assert!(e.is_ok(), "{e:?}");
        assert!(open(&rt, &*a));
        let fail = [changes[0].clone(), changes[3].clone(), changes[3].clone()];
        assert!(
            apply(&rt, &*a, &fail, false)
                .unwrap_err()
                .contains("change 3 of 3 failed")
        );
        let mine = all(&rt, &*a, &t, &i);
        assert!(mine.iter().any(|r| r[0] == Value::Int(9)));
        assert!(mine.iter().any(|r| r[1] == Value::Text("B".into())));
        assert!(!mine.iter().any(|r| r[1] == Value::Text("A".into())));
        assert!(open(&rt, &*a));
        assert!(!all(&rt, &*b, &t, &i).iter().any(|r| r[0] == Value::Int(9)));
        rt.block_on(a.end_transaction(false)).unwrap();
        assert_eq!(all(&rt, &*a, &t, &i), before);
        rt.block_on(a.set_auto_commit(true)).unwrap();
        run(&rt, &*a, "DROP TABLE p6_rows").unwrap();
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mysql_failed_submits_change_nothing() {
        failed_submits_change_nothing(Engine::Mysql, 3306);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mariadb_failed_submits_change_nothing() {
        failed_submits_change_nothing(Engine::Mariadb, 3307);
    }

    /// Keys from the server's indexes, views, read-only sessions, and paging.
    fn keys_views_and_pages(engine: Engine, port: u16) {
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &config(engine, port, "deadlock"));
        let t = fresh(&rt, &*d, "p6_keyless", "a INT, b INT NULL, UNIQUE KEY (b)");
        assert_eq!(info(&rt, &*d, &t).key, None);
        assert!(info(&rt, &*d, &t).read_only_reason().is_some());
        let t = fresh(
            &rt,
            &*d,
            "p6_unique",
            "a INT NULL, b INT NOT NULL, c INT NOT NULL, UNIQUE KEY (a), UNIQUE KEY bc (b, c)",
        );
        assert_eq!(info(&rt, &*d, &t).key, Some(vec![1, 2]));
        run(
            &rt,
            &*d,
            "CREATE OR REPLACE VIEW p6_view AS SELECT b FROM p6_unique",
        )
        .unwrap();
        let v = info(&rt, &*d, &table("p6_view"));
        assert!(v.view && v.key.is_none());
        assert!(rt.block_on(d.table_info(&table("p6_nope"))).is_err());

        let t = fresh(&rt, &*d, "p6_pages", "id INT PRIMARY KEY");
        run(&rt, &*d, "INSERT INTO p6_pages WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < 600) SELECT n FROM s").unwrap();
        run(
            &rt,
            &*d,
            "INSERT INTO p6_pages SELECT id + 600 FROM p6_pages",
        )
        .unwrap();
        let i = info(&rt, &*d, &t);
        let sql = edit::select_sql(&t, &i, "id > 100", "id desc").unwrap();
        let sizes: Vec<usize> = [0, 500, 1000]
            .map(|o| rows(&rt, &*d, &page_sql(&sql, false, 500, o)).len())
            .into();
        assert_eq!(sizes, [500, 500, 100]);
        let bad = edit::select_sql(&t, &i, "nope = 1", "").unwrap();
        let e = rt
            .block_on(d.execute(&page_sql(&bad, false, 500, 0)))
            .err()
            .unwrap();
        assert!(e.to_string().contains("nope"), "{e}");
        assert_eq!(
            rows(&rt, &*d, &edit::count_sql(&t, &i, "id <= 7", None).unwrap())[0][0].to_string(),
            "7"
        );

        let ro = connect_as(
            &rt,
            &ConnectionConfig {
                read_only: true,
                ..config(engine, port, "deadlock")
            },
        );
        let del = [Change {
            kind: ChangeKind::Delete,
            sql: "delete from p6_pages where id = 1".into(),
        }];
        assert!(
            apply(&rt, &*ro, &del, true)
                .unwrap_err()
                .contains("read-only")
        );
        for sql in [
            "DROP VIEW p6_view",
            "DROP TABLE p6_keyless",
            "DROP TABLE p6_unique",
            "DROP TABLE p6_pages",
        ] {
            run(&rt, &*d, sql).unwrap();
        }
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mysql_keys_views_and_pages() {
        keys_views_and_pages(Engine::Mysql, 3306);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mariadb_keys_views_and_pages() {
        keys_views_and_pages(Engine::Mariadb, 3307);
    }
}

mod table_structure {
    use deadlock_core::edit::TableRef;
    use deadlock_core::structure::{
        self, IndexKind, IndexPart, NewColumn, NewIndex, Position, TableStructure,
    };

    use super::*;

    fn table(name: &str) -> TableRef {
        TableRef {
            database: "deadlock".into(),
            table: name.into(),
        }
    }

    fn read(rt: &Runtime, d: &dyn Driver, name: &str) -> TableStructure {
        rt.block_on(d.structure(&table(name))).unwrap()
    }

    /// Composite keys, prefix, descending, fulltext and unique indexes, generated
    /// columns, a foreign key with both actions, referenced-by, a view; then the
    /// generated ALTERs add and drop a column, an index and a foreign key.
    fn structure_reads_and_alters(engine: Engine, port: u16) {
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &config(engine, port, "deadlock"));
        let cleanup = [
            "DROP VIEW IF EXISTS p7_view",
            "DROP TABLE IF EXISTS p7_grand",
            "DROP TABLE IF EXISTS p7_child",
            "DROP TABLE IF EXISTS p7_parent",
        ];
        for sql in cleanup {
            run(&rt, &*d, sql).unwrap();
        }
        for sql in [
            "CREATE TABLE p7_parent (a INT, b VARCHAR(20), PRIMARY KEY (a, b))",
            "CREATE TABLE p7_child (
               id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
               pa INT, pb VARCHAR(20),
               title VARCHAR(200) NOT NULL DEFAULT 'it''s',
               body TEXT,
               qty INT NOT NULL DEFAULT 0 COMMENT 'how many',
               created DATETIME DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
               total DECIMAL(10,2) AS (qty * 2) STORED,
               v INT AS (qty + 1) VIRTUAL,
               UNIQUE KEY uq_title (title(50)),
               KEY ix_desc (qty DESC, created),
               FULLTEXT KEY ft_body (body),
               CONSTRAINT fk_parent FOREIGN KEY (pa, pb) REFERENCES p7_parent (a, b)
                 ON DELETE CASCADE ON UPDATE SET NULL
             ) COMMENT 'kids'",
            "CREATE TABLE p7_grand (id INT PRIMARY KEY, child_id BIGINT UNSIGNED,
               CONSTRAINT fk_grand FOREIGN KEY (child_id) REFERENCES p7_child (id))",
            "CREATE VIEW p7_view AS SELECT id, title FROM p7_child",
        ] {
            run(&rt, &*d, sql).unwrap();
        }
        let s = read(&rt, &*d, "p7_child");
        assert!(!s.view);
        let names: Vec<&str> = s.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "id", "pa", "pb", "title", "body", "qty", "created", "total", "v"
            ]
        );
        let c = |n: &str| s.columns.iter().find(|c| c.name == n).unwrap();
        assert_eq!(c("id").key_label(), "PK");
        assert_eq!(c("id").extra, "auto_increment");
        assert_eq!(c("pa").key_label(), "FK");
        assert_eq!(c("title").default.as_deref(), Some("'it''s'"));
        assert_eq!(c("qty").default.as_deref(), Some("0"));
        assert_eq!(c("qty").comment, "how many");
        assert!(
            c("created")
                .default_label()
                .to_ascii_lowercase()
                .starts_with("current_timestamp"),
            "{:?}",
            c("created")
        );
        assert!(
            c("created")
                .on_update
                .as_deref()
                .is_some_and(|u| u.to_ascii_lowercase().starts_with("current_timestamp")),
            "{:?}",
            c("created")
        );
        assert_eq!(
            c("total").default_label(),
            "generated always as (`qty` * 2) stored"
        );
        assert!(c("v").default_label().ends_with(" virtual"));
        let ix = |n: &str| s.indexes.iter().find(|i| i.name == n).unwrap();
        assert!(s.indexes[0].primary);
        assert_eq!(ix("uq_title").parts_label(), "title(50)");
        assert_eq!(ix("uq_title").kind_label(), "unique");
        assert_eq!(ix("ix_desc").parts_label(), "qty desc, created");
        assert_eq!(ix("ft_body").kind_label(), "fulltext");
        assert_eq!(s.foreign_keys.len(), 1);
        let fk = &s.foreign_keys[0];
        assert_eq!(fk.name, "fk_parent");
        assert_eq!(
            (fk.from_columns.as_slice(), fk.to_columns.as_slice()),
            (
                &["pa".to_owned(), "pb".to_owned()][..],
                &["a".to_owned(), "b".to_owned()][..]
            )
        );
        assert_eq!(fk.to, table("p7_parent"));
        assert_eq!(
            (fk.on_delete.as_str(), fk.on_update.as_str()),
            ("cascade", "set null")
        );
        assert_eq!(s.referenced_by.len(), 1);
        assert_eq!(s.referenced_by[0].from, table("p7_grand"));
        assert_eq!(s.facts.engine.as_deref(), Some("InnoDB"));
        assert_eq!(s.facts.comment, "kids");
        assert!(s.ddl.starts_with("CREATE TABLE `p7_child`"), "{}", s.ddl);
        let parent = read(&rt, &*d, "p7_parent");
        assert_eq!(parent.referenced_by[0].from, table("p7_child"));
        assert_eq!(parent.columns[0].key_label(), "PK");
        let view = read(&rt, &*d, "p7_view");
        assert!(view.view && view.indexes.is_empty() && view.foreign_keys.is_empty());
        assert_eq!(view.columns.len(), 2);
        assert!(view.ddl.contains("VIEW `p7_view`"), "{}", view.ddl);

        // Add a column and an index through the generator, then drop them.
        let t = table("p7_child");
        let add = structure::add_column_sql(
            &t,
            &NewColumn {
                name: "odd `name`".into(),
                data_type: "varchar".into(),
                length: "30".into(),
                nullable: false,
                default: r"a'b\c".into(),
                auto_increment: false,
                comment: "new one".into(),
                position: Position::After("pb".into()),
                ..NewColumn::default()
            },
            &s,
        )
        .unwrap();
        run(&rt, &*d, &add[0]).unwrap();
        let idx = structure::add_index_sql(
            &t,
            &NewIndex {
                name: "ix two".into(),
                kind: IndexKind::Index,
                parts: vec![
                    IndexPart {
                        column: "odd `name`".into(),
                        prefix: Some(10),
                        ..IndexPart::default()
                    },
                    IndexPart {
                        column: "qty".into(),
                        descending: true,
                        ..IndexPart::default()
                    },
                ],
            },
            &s,
        )
        .unwrap();
        run(&rt, &*d, &idx).unwrap();
        let s2 = read(&rt, &*d, "p7_child");
        assert_eq!(s2.columns[3].name, "odd `name`");
        assert_eq!(s2.columns[3].default.as_deref(), Some(r"'a''b\\c'"));
        assert_eq!(s2.columns[3].comment, "new one");
        let two = s2.indexes.iter().find(|i| i.name == "ix two").unwrap();
        assert_eq!(two.parts_label(), "odd `name`(10), qty desc");
        for sql in [
            structure::drop_index_sql(&t, two, &s2),
            structure::drop_column_sql(&t, "odd `name`", &s2),
            structure::drop_foreign_key_sql(&t, "fk_parent", &s2),
        ] {
            run(&rt, &*d, &sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
        let s3 = read(&rt, &*d, "p7_child");
        assert_eq!(s3.columns.len(), 9);
        assert!(s3.foreign_keys.is_empty());
        assert!(!s3.indexes.iter().any(|i| i.name == "ix two"));

        // A read-only connection refuses the ALTER before sending it.
        let ro = connect_as(
            &rt,
            &ConnectionConfig {
                read_only: true,
                ..config(engine, port, "deadlock")
            },
        );
        let err = run(&rt, &*ro, &structure::drop_column_sql(&t, "body", &s2)).unwrap_err();
        assert!(err.contains("read-only"), "{err}");
        assert!(
            read(&rt, &*d, "p7_child")
                .columns
                .iter()
                .any(|c| c.name == "body")
        );
        for sql in cleanup {
            run(&rt, &*d, sql).unwrap();
        }
    }

    /// Reads `name`, edits column `column` with `f` through `modify_column_sql`, runs
    /// the statements and re-reads: every other column must come back unchanged.
    fn edit_column(
        rt: &Runtime,
        d: &dyn Driver,
        name: &str,
        column: &str,
        f: impl FnOnce(&mut NewColumn),
    ) -> Result<TableStructure, String> {
        let s = read(rt, d, name);
        let old = s.columns.iter().find(|c| c.name == column).unwrap();
        let mut new = NewColumn::from_existing(old, &s);
        f(&mut new);
        for sql in structure::modify_column_sql(&table(name), old, &new, &s)? {
            run(rt, d, &sql).map_err(|e| format!("{sql}: {e}"))?;
        }
        let after = read(rt, d, name);
        for c in s.columns.iter().filter(|c| c.name != column) {
            let now = after.columns.iter().find(|x| x.name == c.name);
            assert_eq!(now, Some(c), "{column} edit touched {}", c.name);
        }
        Ok(after)
    }

    /// Phase 15: every attribute of a column changes alone; a no-op edit leaves
    /// `SHOW CREATE TABLE` byte-identical.
    fn columns_edit(engine: Engine, port: u16) {
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &config(engine, port, "deadlock"));
        let mariadb = engine == Engine::Mariadb;
        run(&rt, &*d, "DROP TABLE IF EXISTS p15_edit").unwrap();
        // Members with a comma, a quote and a backslash (a SET member cannot hold a
        // comma: 1367); MySQL's SRID, MariaDB's
        // column-level checks (JSON's json_valid and a typed one).
        let more = if mariadb {
            "j JSON, ck INT CHECK (ck > 0)"
        } else {
            "pt POINT NOT NULL SRID 4326"
        };
        run(
            &rt,
            &*d,
            &format!(
                "CREATE TABLE p15_edit (
                   id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
                   name VARCHAR(20) COLLATE utf8mb4_bin NOT NULL DEFAULT 'x' COMMENT 'c',
                   ts DATETIME(3) DEFAULT CURRENT_TIMESTAMP(3) ON UPDATE CURRENT_TIMESTAMP(3),
                   amount DECIMAL(10,2) UNSIGNED,
                   kind ENUM('a','b'),
                   g DECIMAL(12,2) AS (amount * 2) VIRTUAL,
                   u CHAR(36) DEFAULT (uuid()),
                   e ENUM('a,b','it''s','x\\\\y'),
                   {more}
                 )"
            ),
        )
        .unwrap();
        let point = if mariadb {
            ""
        } else {
            ", ST_SRID(POINT(1, 2), 4326)"
        };
        run(
            &rt,
            &*d,
            &format!(
                "INSERT INTO p15_edit (name, amount, kind, e{}) VALUES ('n', 1.5, 'a', 'a,b'{point})",
                if mariadb { "" } else { ", pt" }
            ),
        )
        .unwrap();
        let s = read(&rt, &*d, "p15_edit");
        let ddl = s.ddl.clone();
        let e = s.columns.iter().find(|c| c.name == "e").unwrap();
        assert_eq!(
            NewColumn::from_existing(e, &s).length,
            r"'a,b','it''s','x\\y'"
        );
        if mariadb {
            let j = s.columns.iter().find(|c| c.name == "j").unwrap();
            assert_eq!(j.check.as_deref(), Some("json_valid(`j`)"));
            let ck = s.columns.iter().find(|c| c.name == "ck").unwrap();
            assert_eq!(ck.check.as_deref(), Some("`ck` > 0"));
            assert!(s.checks.is_empty(), "{:?}", s.checks);
        } else {
            let pt = s.columns.iter().find(|c| c.name == "pt").unwrap();
            assert_eq!(pt.srid, Some(4326));
        }
        // (a) Round trip: restating each column as it is changes nothing.
        for c in read(&rt, &*d, "p15_edit").columns {
            edit_column(&rt, &*d, "p15_edit", &c.name, |_| {}).unwrap();
            assert_eq!(read(&rt, &*d, "p15_edit").ddl, ddl, "{}", c.name);
        }
        let col =
            |s: &TableStructure, n: &str| s.columns.iter().find(|c| c.name == n).cloned().unwrap();
        // (b) Rename keeps collation, default and comment.
        let s = edit_column(&rt, &*d, "p15_edit", "name", |c| c.name = "title".into()).unwrap();
        let title = col(&s, "title");
        assert_eq!(title.collation.as_deref(), Some("utf8mb4_bin"));
        assert_eq!(title.default.as_deref(), Some("'x'"));
        assert_eq!(title.comment, "c");
        assert!(!title.nullable);
        // (c) Longer.
        let s = edit_column(&rt, &*d, "p15_edit", "title", |c| c.length = "40".into()).unwrap();
        assert_eq!(col(&s, "title").column_type, "varchar(40)");
        assert_eq!(col(&s, "title").collation.as_deref(), Some("utf8mb4_bin"));
        // (d) Null and back.
        let s = edit_column(&rt, &*d, "p15_edit", "title", |c| c.nullable = true).unwrap();
        assert!(col(&s, "title").nullable);
        let s = edit_column(&rt, &*d, "p15_edit", "title", |c| c.nullable = false).unwrap();
        assert!(!col(&s, "title").nullable);
        // (e) Another default, then none.
        let s = edit_column(&rt, &*d, "p15_edit", "title", |c| c.default = "it's".into()).unwrap();
        assert_eq!(col(&s, "title").default.as_deref(), Some("'it''s'"));
        let s = edit_column(&rt, &*d, "p15_edit", "amount", |c| {
            c.default = String::new()
        })
        .unwrap();
        assert_eq!(col(&s, "amount").default, None);
        let s = edit_column(&rt, &*d, "p15_edit", "amount", |c| c.default = "2.5".into()).unwrap();
        assert_eq!(col(&s, "amount").default.as_deref(), Some("2.50"));
        // (f) On update off, then on again with the column's precision.
        let s = edit_column(&rt, &*d, "p15_edit", "ts", |c| c.on_update = false).unwrap();
        assert_eq!(col(&s, "ts").on_update, None);
        assert!(col(&s, "ts").default.is_some());
        let s = edit_column(&rt, &*d, "p15_edit", "ts", |c| c.on_update = true).unwrap();
        assert!(
            col(&s, "ts")
                .on_update
                .is_some_and(|u| u.eq_ignore_ascii_case("current_timestamp(3)")),
            "{:?}",
            col(&s, "ts")
        );
        // (g) Comment.
        let s = edit_column(&rt, &*d, "p15_edit", "title", |c| {
            c.comment = "a 'new' one".into()
        })
        .unwrap();
        assert_eq!(col(&s, "title").comment, "a 'new' one");
        // (h) Move first, then back after id.
        let s = edit_column(&rt, &*d, "p15_edit", "kind", |c| {
            c.position = Position::First
        })
        .unwrap();
        assert_eq!(s.columns[0].name, "kind");
        let s = edit_column(&rt, &*d, "p15_edit", "kind", |c| {
            c.position = Position::After("id".into());
        })
        .unwrap();
        let names: Vec<&str> = s.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names[..8],
            ["id", "kind", "title", "ts", "amount", "g", "u", "e"]
        );
        // (i) The expression.
        let s = edit_column(&rt, &*d, "p15_edit", "g", |c| {
            c.generated.as_mut().unwrap().expression = "amount * 3".into();
        })
        .unwrap();
        let g = col(&s, "g").generated.unwrap();
        assert!(g.expression.contains("* 3") && !g.stored, "{g:?}");
        // (j) Stored and virtual do not switch.
        let err = edit_column(&rt, &*d, "p15_edit", "g", |c| {
            c.generated.as_mut().unwrap().stored = true;
        })
        .unwrap_err();
        assert!(err.contains("stored and virtual"), "{err}");
        // The row survived every edit.
        assert_eq!(count(&rt, &*d, "p15_edit"), "1");
        if mariadb {
            // MariaDB refuses `null` after a generated column's definition.
            let err = run(
                &rt,
                &*d,
                "ALTER TABLE p15_edit CHANGE COLUMN g g DECIMAL(12,2) GENERATED ALWAYS AS (amount * 2) VIRTUAL NULL",
            )
            .unwrap_err();
            assert!(err.contains("1064") || err.contains("syntax"), "{err}");
            // A column's own check stays through an edit and follows a rename.
            let s = edit_column(&rt, &*d, "p15_edit", "j", |c| {
                c.name = "doc".into();
                c.comment = "doc".into();
            })
            .unwrap();
            assert_eq!(col(&s, "doc").check.as_deref(), Some("json_valid(`doc`)"));
            assert!(s.ddl.contains("CHECK (json_valid(`doc`))"), "{}", s.ddl);
            assert!(run(&rt, &*d, "UPDATE p15_edit SET doc = 'not json'").is_err());
            let s = edit_column(&rt, &*d, "p15_edit", "ck", |c| {
                c.data_type = "bigint".into()
            })
            .unwrap();
            assert_eq!(col(&s, "ck").check.as_deref(), Some("`ck` > 0"));
            assert!(run(&rt, &*d, "UPDATE p15_edit SET ck = -1").is_err());
        } else {
            let s =
                edit_column(&rt, &*d, "p15_edit", "pt", |c| c.comment = "where".into()).unwrap();
            assert_eq!(col(&s, "pt").srid, Some(4326));
        }
        // A member with a comma still matches after an edit of the enum.
        let s = edit_column(&rt, &*d, "p15_edit", "e", |c| {
            c.length.push_str(",'new'");
        })
        .unwrap();
        assert_eq!(
            col(&s, "e").column_type,
            r"enum('a,b','it''s','x\\y','new')"
        );
        assert_eq!(
            rows(&rt, &*d, "SELECT e FROM p15_edit"),
            [[Value::Text("a,b".into())]]
        );
        run(&rt, &*d, "DROP TABLE p15_edit").unwrap();
    }

    /// Phase 16: truncate, rename and drop a table, and the server's refusals while
    /// another table's key points at it.
    fn table_actions(engine: Engine, port: u16) {
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &config(engine, port, "deadlock"));
        for sql in [
            "DROP TABLE IF EXISTS p16_kid, p16_child, p16_parent",
            "CREATE TABLE p16_parent (id INT PRIMARY KEY)",
            "CREATE TABLE p16_child (id INT PRIMARY KEY, parent_id INT, \
             CONSTRAINT p16_fk FOREIGN KEY (parent_id) REFERENCES p16_parent (id))",
            "INSERT INTO p16_parent VALUES (1)",
            "INSERT INTO p16_child VALUES (1, 1)",
        ] {
            run(&rt, &*d, sql).unwrap();
        }
        let s = read(&rt, &*d, "p16_parent");
        assert_eq!(s.referenced_by.len(), 1);
        let parent = table("p16_parent");
        assert!(run(&rt, &*d, &structure::truncate_table_sql(&parent, &s)).is_err());
        assert_eq!(count(&rt, &*d, "p16_parent"), "1");
        let child = table("p16_child");
        run(&rt, &*d, &structure::truncate_table_sql(&child, &s)).unwrap();
        assert_eq!(count(&rt, &*d, "p16_child"), "0");
        let sql = structure::rename_table_sql(&child, "p16_kid", &s).unwrap();
        run(&rt, &*d, &sql).unwrap();
        let kid = read(&rt, &*d, "p16_kid");
        assert_eq!(kid.foreign_keys.len(), 1);
        assert_eq!(kid.foreign_keys[0].to, parent);
        assert!(rt.block_on(d.structure(&child)).is_err());
        assert!(run(&rt, &*d, &structure::drop_table_sql(&parent, true, &s)).is_err());
        for t in ["p16_kid", "p16_parent"] {
            run(&rt, &*d, &structure::drop_table_sql(&table(t), false, &s)).unwrap();
            assert!(rt.block_on(d.structure(&table(t))).is_err(), "{t}");
        }

        // An InnoDB estimate of 0 on a table with rows reads as not known.
        run(&rt, &*d, "DROP TABLE IF EXISTS p16_est").unwrap();
        run(&rt, &*d, "CREATE TABLE p16_est (id INT PRIMARY KEY)").unwrap();
        assert_eq!(read(&rt, &*d, "p16_est").facts.rows, Some(0));
        run(&rt, &*d, "INSERT INTO p16_est VALUES (1)").unwrap();
        let estimate = rows(
            &rt,
            &*d,
            "SELECT TABLE_ROWS FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = 'deadlock' AND TABLE_NAME = 'p16_est'",
        );
        let rows_now = read(&rt, &*d, "p16_est").facts.rows;
        if matches!(estimate[0][0], Value::UInt(0) | Value::Int(0)) {
            eprintln!("{engine:?}: the estimate says 0 with a row");
            assert_eq!(rows_now, None);
        } else {
            assert_ne!(rows_now, Some(0), "{estimate:?}");
        }
        run(&rt, &*d, "DROP TABLE p16_est").unwrap();
    }

    /// Phase 17: a named composite foreign key with both actions, an unnamed one, a
    /// check that refuses a row; all listed after a re-read, then dropped. Then a key
    /// into another schema, and the collation hint against the server's refusal.
    fn keys_and_checks(engine: Engine, port: u16) {
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &config(engine, port, "deadlock"));
        let mariadb = engine == Engine::Mariadb;
        // MariaDB's JSON column brings a column-level json_valid check, which is not
        // listed: `drop constraint` refuses it.
        let doc = if mariadb { ", doc JSON" } else { "" };
        for sql in [
            "DROP TABLE IF EXISTS p17_child, p17_parent".to_owned(),
            "CREATE TABLE p17_parent (a INT, b VARCHAR(20), PRIMARY KEY (a, b))".to_owned(),
            format!(
                "CREATE TABLE p17_child (id INT PRIMARY KEY, pa INT, pb VARCHAR(20), qty INT{doc})"
            ),
        ] {
            run(&rt, &*d, &sql).unwrap();
        }
        let child = table("p17_child");
        let s = read(&rt, &*d, "p17_child");
        assert!(!s.checks_unsupported);
        assert!(s.checks.is_empty(), "{:?}", s.checks);
        let key = structure::NewForeignKey {
            name: "p17_fk".into(),
            columns: vec!["pa".into(), "pb".into()],
            ref_table: table("p17_parent"),
            ref_columns: vec!["a".into(), "b".into()],
            on_delete: "cascade".into(),
            on_update: "set null".into(),
        };
        let parent = read(&rt, &*d, "p17_parent");
        assert!(structure::foreign_key_hints(&key, &s, &parent).is_empty());
        assert!(
            structure::foreign_key_index_note(&key, &s)
                .is_some_and(|n| n.starts_with("(pa, pb) have no index yet"))
        );
        run(
            &rt,
            &*d,
            &structure::add_foreign_key_sql(&child, &key, &s).unwrap(),
        )
        .unwrap();
        // The server added the index the note announced, named after the key.
        let added = read(&rt, &*d, "p17_child");
        assert!(
            added.indexes.iter().any(|ix| ix.name == "p17_fk"),
            "{:?}",
            added.indexes
        );
        assert_eq!(structure::foreign_key_index_note(&key, &added), None);
        let unnamed = structure::NewForeignKey {
            name: String::new(),
            on_delete: "restrict".into(),
            on_update: "no action".into(),
            ..key.clone()
        };
        let sql = structure::add_foreign_key_sql(&child, &unnamed, &s).unwrap();
        run(&rt, &*d, &sql).unwrap();
        let sql = structure::add_check_sql(&child, "p17_ck", "qty >= 0", &s).unwrap();
        run(&rt, &*d, &sql).unwrap();

        let s = read(&rt, &*d, "p17_child");
        assert_eq!(s.foreign_keys.len(), 2, "{:?}", s.foreign_keys);
        let named = s.foreign_keys.iter().find(|k| k.name == "p17_fk").unwrap();
        assert_eq!(named.from_columns, ["pa", "pb"]);
        assert_eq!(named.to_columns, ["a", "b"]);
        assert_eq!(
            (named.on_delete.as_str(), named.on_update.as_str()),
            ("cascade", "set null")
        );
        let other = s.foreign_keys.iter().find(|k| k.name != "p17_fk").unwrap();
        assert!(!other.name.is_empty());
        assert_eq!(other.on_delete, "restrict");
        let parent = read(&rt, &*d, "p17_parent");
        assert_eq!(parent.referenced_by.len(), 2);
        let ck = s.checks.iter().find(|c| c.name == "p17_ck").unwrap();
        let condition = if mariadb {
            "`qty` >= 0"
        } else {
            "(`qty` >= 0)"
        };
        assert_eq!(ck.condition, condition);
        assert!(ck.enabled);
        assert!(run(&rt, &*d, "INSERT INTO p17_child (id, qty) VALUES (1, -1)").is_err());
        run(&rt, &*d, "INSERT INTO p17_child (id, qty) VALUES (1, 1)").unwrap();

        for c in &s.checks {
            run(&rt, &*d, &s.drop_constraint_sql(&child, &c.name)).unwrap();
        }
        for k in &s.foreign_keys {
            run(
                &rt,
                &*d,
                &structure::drop_foreign_key_sql(&child, &k.name, &s),
            )
            .unwrap();
        }
        let s = read(&rt, &*d, "p17_child");
        assert!(s.foreign_keys.is_empty() && s.checks.is_empty(), "{s:?}");

        // A key into another schema: the statement qualifies the referenced table.
        let mut admin = config(engine, port, "");
        admin.user = "root".into();
        let root = connect_as(&rt, &admin);
        for sql in [
            "DROP DATABASE IF EXISTS p17_other",
            "CREATE DATABASE p17_other",
            "CREATE TABLE p17_other.p17_ref (id INT PRIMARY KEY)",
            "GRANT SELECT, REFERENCES ON p17_other.* TO 'deadlock'@'%'",
        ] {
            run(&rt, &*root, sql).unwrap();
        }
        let other = TableRef {
            database: "p17_other".into(),
            table: "p17_ref".into(),
        };
        let cross = structure::NewForeignKey {
            name: "p17_cross_fk".into(),
            columns: vec!["qty".into()],
            ref_table: other.clone(),
            ref_columns: vec!["id".into()],
            on_delete: "restrict".into(),
            on_update: "restrict".into(),
        };
        let sql = structure::add_foreign_key_sql(&child, &cross, &s).unwrap();
        assert!(
            sql.contains("references `p17_other`.`p17_ref` (`id`)"),
            "{sql}"
        );
        run(&rt, &*d, "DELETE FROM p17_child").unwrap();
        run(&rt, &*d, &sql).unwrap();
        let s = read(&rt, &*d, "p17_child");
        assert_eq!(s.foreign_keys.len(), 1, "{:?}", s.foreign_keys);
        assert_eq!(s.foreign_keys[0].to, other);
        let target = rt.block_on(root.structure(&other)).unwrap();
        assert_eq!(target.referenced_by.len(), 1, "{:?}", target.referenced_by);
        run(
            &rt,
            &*d,
            &structure::drop_foreign_key_sql(&child, "p17_cross_fk", &s),
        )
        .unwrap();

        // Text columns whose collations differ: the hint says so, and the server refuses.
        run(
            &rt,
            &*root,
            "CREATE TABLE p17_other.p17_txt (k VARCHAR(20) COLLATE utf8mb4_bin PRIMARY KEY)",
        )
        .unwrap();
        run(&rt, &*d, "DROP TABLE IF EXISTS p17_tchild").unwrap();
        run(
            &rt,
            &*d,
            "CREATE TABLE p17_tchild (k VARCHAR(20) COLLATE utf8mb4_general_ci)",
        )
        .unwrap();
        let txt = TableRef {
            database: "p17_other".into(),
            table: "p17_txt".into(),
        };
        let by_text = structure::NewForeignKey {
            name: String::new(),
            columns: vec!["k".into()],
            ref_table: txt.clone(),
            ref_columns: vec!["k".into()],
            ..cross.clone()
        };
        let tchild = read(&rt, &*d, "p17_tchild");
        let hints = structure::foreign_key_hints(
            &by_text,
            &tchild,
            &rt.block_on(root.structure(&txt)).unwrap(),
        );
        assert_eq!(hints.len(), 1, "{hints:?}");
        assert!(
            hints[0].starts_with("k uses collation utf8mb4_general_ci"),
            "{hints:?}"
        );
        let sql = structure::add_foreign_key_sql(&table("p17_tchild"), &by_text, &tchild).unwrap();
        let err = run(&rt, &*d, &sql).unwrap_err();
        assert!(err.contains(if mariadb { "1005" } else { "3780" }), "{err}");
        run(&rt, &*d, "DROP TABLE p17_tchild, p17_child, p17_parent").unwrap();
        run(&rt, &*root, "DROP DATABASE p17_other").unwrap();
    }

    /// Phase 18: the New table example as `p18_new`: 5 columns, composite primary key,
    /// a default, on update (MySQL / MariaDB) and comments.
    pub(super) fn p18_table(oracle: bool) -> structure::NewTable {
        let col = |name: &str, ty: &str, length: &str| NewColumn {
            name: name.into(),
            data_type: ty.into(),
            length: length.into(),
            ..NewColumn::default()
        };
        let (id, int, text, temporal, now) = if oracle {
            ("number", "number", "varchar2", "timestamp", "systimestamp")
        } else {
            (
                "bigint unsigned",
                "int",
                "varchar",
                "datetime",
                "current_timestamp(3)",
            )
        };
        structure::NewTable {
            name: "p18_new".into(),
            columns: vec![
                NewColumn {
                    auto_increment: true,
                    comment: "row id".into(),
                    ..col("id", id, "")
                },
                col("line", int, if oracle { "10" } else { "" }),
                NewColumn {
                    default: "new".into(),
                    collation: (!oracle).then(|| "utf8mb4_bin".into()),
                    ..col("status", text, "20")
                },
                NewColumn {
                    default: now.into(),
                    on_update: !oracle,
                    ..col("created", temporal, "3")
                },
                NewColumn {
                    nullable: true,
                    comment: "free text".into(),
                    ..col("note", if oracle { "clob" } else { "text" }, "")
                },
            ],
            primary_key: vec!["id".into(), "line".into()],
            engine: (!oracle).then(|| "InnoDB".into()),
            collation: (!oracle).then(|| "utf8mb4_bin".into()),
            comment: "orders".into(),
        }
    }

    /// Phase 18: `create_table_sql` on the server, then the table reads back as drafted.
    fn create_table(engine: Engine, port: u16) {
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &config(engine, port, "deadlock"));
        run(&rt, &*d, "DROP TABLE IF EXISTS p18_new").unwrap();
        let flags = TableStructure {
            mariadb: engine == Engine::Mariadb,
            backslash_escapes: true,
            ..TableStructure::default()
        };
        for sql in structure::create_table_sql("deadlock", &p18_table(false), &flags).unwrap() {
            run(&rt, &*d, &sql).unwrap();
        }
        let s = read(&rt, &*d, "p18_new");
        let names: Vec<&str> = s.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "line", "status", "created", "note"]);
        assert!(s.indexes[0].primary);
        assert_eq!(s.indexes[0].parts_label(), "id, line");
        let c = |n: &str| s.columns.iter().find(|c| c.name == n).unwrap();
        assert!(c("id").extra.contains("auto_increment"), "{:?}", c("id"));
        assert_eq!(c("status").default.as_deref(), Some("'new'"));
        assert_eq!(c("status").collation.as_deref(), Some("utf8mb4_bin"));
        assert_eq!(c("created").column_type, "datetime(3)");
        assert_eq!(
            c("created")
                .on_update
                .as_deref()
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("current_timestamp(3)"),
            "{:?}",
            c("created")
        );
        assert_eq!(c("id").comment, "row id");
        assert_eq!(c("note").comment, "free text");
        assert!(c("note").nullable && !c("line").nullable);
        assert_eq!(s.facts.comment, "orders");
        assert_eq!(s.facts.engine.as_deref(), Some("InnoDB"));
        assert_eq!(s.facts.collation.as_deref(), Some("utf8mb4_bin"));
        run(&rt, &*d, "DROP TABLE p18_new").unwrap();

        // A session with NO_BACKSLASH_ESCAPES: the draft quotes for the mode it reads.
        let nb = connect_as(&rt, &config(engine, port, "deadlock"));
        run(
            &rt,
            &*nb,
            "SET SESSION sql_mode = CONCAT(@@SESSION.sql_mode, ',NO_BACKSLASH_ESCAPES')",
        )
        .unwrap();
        let mode = match &rows(&rt, &*nb, "SELECT @@SESSION.sql_mode")[0][0] {
            Value::Text(m) => m.clone(),
            v => panic!("{v:?}"),
        };
        let flags = TableStructure {
            backslash_escapes: deadlock_core::edit::backslash_escapes(&mode),
            ..flags
        };
        assert!(!flags.backslash_escapes, "{mode}");
        let mut draft = p18_table(false);
        draft.comment = r"C:\dir\".into();
        draft.columns[2].default = r"a\b".into();
        // A value ending in `\` before one holding `;`: still one statement here.
        draft.columns[1].comment = r"ends\".into();
        draft.columns[4].comment = r"it's \n; x".into();
        // The key's order is its own, not the columns' (no auto_increment: InnoDB wants
        // that column first in a key).
        draft.columns[0].auto_increment = false;
        draft.primary_key = vec!["line".into(), "id".into()];
        draft.columns.push(NewColumn {
            name: "e".into(),
            data_type: "enum".into(),
            length: r"'x\\y','a,b'".into(),
            nullable: true,
            ..NewColumn::default()
        });
        for sql in structure::create_table_sql("deadlock", &draft, &flags).unwrap() {
            assert!(sql.contains(r"'C:\dir\'"), "{sql}");
            run(&rt, &*nb, &sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
        let s = read(&rt, &*d, "p18_new");
        assert_eq!(s.facts.comment, r"C:\dir\");
        assert_eq!(s.indexes[0].parts_label(), "line, id");
        let c = |n: &str| s.columns.iter().find(|c| c.name == n).unwrap();
        assert_eq!(c("line").comment, r"ends\");
        assert_eq!(c("note").comment, r"it's \n; x");
        assert_eq!(c("e").column_type, r"enum('x\\y','a,b')");
        assert_eq!(
            rows(
                &rt,
                &*d,
                "SELECT COLUMN_DEFAULT FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = 'deadlock' AND TABLE_NAME = 'p18_new' AND COLUMN_NAME = 'status'"
            )[0][0],
            // MariaDB writes COLUMN_DEFAULT as SQL, MySQL as the bare value.
            Value::Text(
                if engine == Engine::Mariadb {
                    r"'a\\b'"
                } else {
                    r"a\b"
                }
                .into()
            )
        );
        run(&rt, &*d, "DROP TABLE p18_new").unwrap();
    }

    /// The Collation lists: every collation a column reports is offered, with its
    /// character set, the server default included (MariaDB 11's `uca1400` ones too).
    fn collations_cover_column_collations(engine: Engine, port: u16) {
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &config(engine, port, "deadlock"));
        let list = rt.block_on(d.collations()).unwrap();
        assert!(list.windows(2).all(|w| w[0].0 <= w[1].0), "sorted by name");
        run(&rt, &*d, "DROP TABLE IF EXISTS p15_coll").unwrap();
        run(
            &rt,
            &*d,
            "CREATE TABLE p15_coll (a VARCHAR(5), b VARCHAR(5) COLLATE utf8mb4_bin, \
             c VARCHAR(5) CHARACTER SET latin1)",
        )
        .unwrap();
        let s = read(&rt, &*d, "p15_coll");
        for c in &s.columns {
            let (Some(collation), Some(charset)) = (&c.collation, &c.charset) else {
                panic!("{c:?}");
            };
            assert!(
                list.iter().any(|(n, cs)| n == collation && cs == charset),
                "{collation} ({charset}) missing"
            );
        }
        run(&rt, &*d, "DROP TABLE p15_coll").unwrap();
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mysql_collations_cover_column_collations() {
        collations_cover_column_collations(Engine::Mysql, 3306);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mariadb_collations_cover_column_collations() {
        collations_cover_column_collations(Engine::Mariadb, 3307);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mysql_create_table() {
        create_table(Engine::Mysql, 3306);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mariadb_create_table() {
        create_table(Engine::Mariadb, 3307);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mysql_keys_and_checks() {
        keys_and_checks(Engine::Mysql, 3306);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mariadb_keys_and_checks() {
        keys_and_checks(Engine::Mariadb, 3307);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mysql_table_actions() {
        table_actions(Engine::Mysql, 3306);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mariadb_table_actions() {
        table_actions(Engine::Mariadb, 3307);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mysql_columns_edit() {
        columns_edit(Engine::Mysql, 3306);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mariadb_columns_edit() {
        columns_edit(Engine::Mariadb, 3307);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mysql_table_structure() {
        structure_reads_and_alters(Engine::Mysql, 3306);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mariadb_table_structure() {
        structure_reads_and_alters(Engine::Mariadb, 3307);
    }
}

mod explain {
    use super::*;
    use deadlock_core::plan::{Op, Plan};

    const JOIN: &str = "SELECT c.name, COUNT(*) FROM p8_orders o JOIN p8_customers c \
                        ON c.id = o.customer_id GROUP BY c.name";

    fn plan(rt: &Runtime, d: &dyn Driver, sql: &str, analyze: bool) -> Result<Plan, String> {
        rt.block_on(d.explain(sql, analyze))
            .map_err(|e| e.to_string())
    }

    /// Estimated and measured plans of a join, a measured UPDATE that stays inside a
    /// manual transaction, and a read-only session that refuses to measure a write.
    fn explains(engine: Engine, port: u16) {
        let rt = Runtime::new().unwrap();
        let cfg = ConnectionConfig {
            encryption: Encryption::Off,
            ..config(engine, port, "deadlock")
        };
        let d = connect_as(&rt, &cfg);
        assert!(d.capabilities().explain);
        for sql in [
            "DROP TABLE IF EXISTS p8_orders, p8_customers",
            "CREATE TABLE p8_customers (id INT PRIMARY KEY, name VARCHAR(20))",
            "CREATE TABLE p8_orders (id INT PRIMARY KEY, customer_id INT, total INT)",
            "INSERT INTO p8_customers SELECT seq, CONCAT('c', seq % 50) FROM \
             (SELECT a.n * 10 + b.n AS seq FROM \
              (SELECT 0 n UNION SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4 \
               UNION SELECT 5 UNION SELECT 6 UNION SELECT 7 UNION SELECT 8 UNION SELECT 9) a, \
              (SELECT 0 n UNION SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4 \
               UNION SELECT 5 UNION SELECT 6 UNION SELECT 7 UNION SELECT 8 UNION SELECT 9) b) s",
            "INSERT INTO p8_orders SELECT a.id * 1000 + b.id, a.id, b.id FROM p8_customers a \
             JOIN p8_customers b ON b.id < 20",
        ] {
            run(&rt, &*d, sql).unwrap();
        }
        let est = plan(&rt, &*d, JOIN, false).unwrap();
        assert!(!est.measured && est.execution_ms.is_none());
        assert_eq!(est.database.as_deref(), Some("deadlock"));
        let tables: Vec<_> = est
            .walk()
            .iter()
            .filter_map(|(_, n)| n.table.clone())
            .collect();
        assert!(
            tables.contains(&"o".into()) && tables.contains(&"c".into()),
            "{tables:?}"
        );
        assert!(est.raw.trim_start().starts_with('{'));

        let measured = plan(&rt, &*d, JOIN, true).unwrap();
        assert!(measured.measured && measured.execution_ms.is_some_and(|t| t > 0.));
        assert!(measured.slowest().is_some());
        assert!(measured.walk().iter().any(|(_, n)| n.actual_rows.is_some()));

        // Measured UPDATE in Manual mode: it runs inside the transaction and rolls back.
        let update = "UPDATE p8_orders o JOIN p8_customers c ON c.id = o.customer_id \
                      SET o.total = o.total + 1 WHERE c.id < 10";
        let sum = || rows(&rt, &*d, "SELECT SUM(total) FROM p8_orders")[0][0].to_string();
        let before = sum();
        rt.block_on(d.set_auto_commit(false)).unwrap();
        let p = plan(&rt, &*d, update, true).unwrap();
        assert!(
            p.walk()
                .iter()
                .any(|(_, n)| n.op == Op::Write || n.op == Op::Join)
        );
        rt.block_on(d.end_transaction(false)).unwrap();
        rt.block_on(d.set_auto_commit(true)).unwrap();
        assert_eq!(sum(), before);

        let ro = connect_as(
            &rt,
            &ConnectionConfig {
                read_only: true,
                ..cfg
            },
        );
        let err = plan(&rt, &*ro, update, true).unwrap_err();
        assert!(err.contains("read-only") && err.contains("UPDATE"), "{err}");
        // Both servers refuse even to estimate a write in a READ ONLY session.
        let err = plan(&rt, &*ro, update, false).unwrap_err();
        assert!(err.contains("READ ONLY"), "{err}");
        assert!(plan(&rt, &*ro, JOIN, true).is_ok());
        run(&rt, &*d, "DROP TABLE p8_orders, p8_customers").unwrap();
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mysql_explains() {
        explains(Engine::Mysql, 3306);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mariadb_explains() {
        explains(Engine::Mariadb, 3307);
    }
}

/// Sessions and locks against real lock waits between real sessions.
mod sessions {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use deadlock_core::connection::{ConnectionConfig, Engine};
    use deadlock_core::driver::{Driver, Runtime, Value};
    use deadlock_core::sessions::{LockKind, Snapshot, State, parse_deadlock};
    use tokio::sync::oneshot;

    use super::{config, connect_as, run};

    fn root(engine: Engine, port: u16) -> ConnectionConfig {
        ConnectionConfig {
            user: "root".into(),
            ..config(engine, port, "deadlock")
        }
    }

    fn id(rt: &Runtime, d: &dyn Driver) -> u64 {
        match &super::rows(rt, d, "SELECT CONNECTION_ID()")[0][0] {
            Value::UInt(n) => *n,
            Value::Int(n) => u64::try_from(*n).unwrap(),
            v => panic!("{v:?}"),
        }
    }

    /// Runs `sql` on `d` without waiting for it.
    fn start(
        rt: &Runtime,
        d: &Arc<dyn Driver>,
        sql: &str,
    ) -> oneshot::Receiver<Result<(), String>> {
        let (d, sql) = (Arc::clone(d), sql.to_owned());
        rt.spawn(async move {
            let s = d.execute(&sql).await.map_err(|e| e.to_string())?;
            s.collect().await.map(drop).map_err(|e| e.to_string())
        })
    }

    fn finish(rt: &Runtime, rx: oneshot::Receiver<Result<(), String>>) -> Result<(), String> {
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(20), rx)
                .await
                .expect("statement still blocked")
                .unwrap()
        })
    }

    /// Snapshots until `ok` holds, for up to ten seconds.
    fn until(rt: &Runtime, d: &dyn Driver, ok: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let s = rt.block_on(d.sessions()).unwrap();
            if ok(&s) {
                return s;
            }
            assert!(Instant::now() < end, "timed out: {s:#?}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn setup(rt: &Runtime, d: &dyn Driver, table: &str) {
        run(rt, d, &format!("DROP TABLE IF EXISTS {table}")).unwrap();
        run(
            rt,
            d,
            &format!("CREATE TABLE {table} (id INT PRIMARY KEY, total INT)"),
        )
        .unwrap();
        run(
            rt,
            d,
            &format!("INSERT INTO {table} VALUES (1, 10), (2, 20)"),
        )
        .unwrap();
    }

    fn row_lock_wait_cancel_and_end(engine: Engine, port: u16) {
        let rt = Runtime::new().unwrap();
        let cfg = root(engine, port);
        let (holder, waiter, screen) = (
            connect_as(&rt, &cfg),
            connect_as(&rt, &cfg),
            connect_as(&rt, &cfg),
        );
        let t = "p9_row_wait";
        setup(&rt, &*screen, t);
        let (h, w) = (id(&rt, &*holder), id(&rt, &*waiter));
        run(&rt, &*holder, "START TRANSACTION").unwrap();
        run(
            &rt,
            &*holder,
            &format!("UPDATE {t} SET total = 11 WHERE id = 1"),
        )
        .unwrap();
        let blocked = start(
            &rt,
            &waiter,
            &format!("UPDATE {t} SET total = 12 WHERE id = 1"),
        );
        // INNODB_TRX is a cache that may lag 0.1 s behind.
        let s = until(&rt, &*screen, |s| {
            s.wait_of(w).is_some() && s.session(h).is_some_and(|x| x.transaction.is_some())
        });
        for x in [h, w, id(&rt, &*screen)] {
            assert!(s.session(x).unwrap().deadlock, "{x} is Deadlock's");
        }
        let wait = s.wait_of(w).unwrap();
        assert_eq!(wait.blocker, Some(h));
        assert_eq!(wait.wants.kind, LockKind::Row);
        assert_eq!(
            (wait.wants.schema.as_str(), wait.wants.table.as_str()),
            ("deadlock", t)
        );
        assert_eq!(wait.wants.row.as_deref(), Some("1"));
        assert_eq!(s.session(w).unwrap().state, State::Waiting);
        let holder_s = s.session(h).unwrap();
        assert_eq!(holder_s.state, State::IdleInTransaction, "{:?}", s.notes);
        assert!(holder_s.transaction.is_some());
        assert!(
            holder_s.holds.iter().any(|l| l.table == t),
            "{:?}",
            holder_s.holds
        );
        assert!(s.sessions.iter().any(|s| s.own));
        assert!(s.own_only.is_none());
        // Other tests may hold locks on the same server at the same time.
        let chains = s.chains();
        let ours = chains.iter().find(|c| c.levels[0] == [h]).unwrap();
        assert_eq!(ours.levels, vec![vec![h], vec![w]]);
        assert!(rt.block_on(screen.waiting_sessions()).unwrap() >= 1);

        // Cancel the waiter's statement: it fails, its session stays.
        rt.block_on(screen.cancel_statement(w)).unwrap();
        assert!(finish(&rt, blocked).is_err());
        assert_eq!(id(&rt, &*waiter), w);

        // Wait again, end the holder: the waiter goes through.
        let blocked = start(
            &rt,
            &waiter,
            &format!("UPDATE {t} SET total = 12 WHERE id = 1"),
        );
        until(&rt, &*screen, |s| s.wait_of(w).is_some());
        rt.block_on(screen.end_session(h)).unwrap();
        finish(&rt, blocked).unwrap();
        // The holder's change rolled back, the waiter's stands.
        assert_eq!(
            super::rows(
                &rt,
                &*screen,
                &format!("SELECT total FROM {t} WHERE id = 1")
            )[0][0]
                .to_string(),
            "12"
        );
        let own = id(&rt, &*screen);
        let err = rt
            .block_on(screen.end_session(own))
            .unwrap_err()
            .to_string();
        assert!(err.contains("screen itself"), "{err}");
        run(&rt, &*screen, &format!("DROP TABLE {t}")).unwrap();
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mysql_row_lock_wait_cancel_and_end() {
        row_lock_wait_cancel_and_end(Engine::Mysql, 3306);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mariadb_row_lock_wait_cancel_and_end() {
        row_lock_wait_cancel_and_end(Engine::Mariadb, 3307);
    }

    /// Holder → middle → last on two rows: the chain starts at the holder.
    fn three_session_chain(engine: Engine, port: u16) {
        let rt = Runtime::new().unwrap();
        let cfg = root(engine, port);
        let [a, b, c, screen] = [(); 4].map(|()| connect_as(&rt, &cfg));
        let t = "p9_chain";
        setup(&rt, &*screen, t);
        let (ia, ib, ic) = (id(&rt, &*a), id(&rt, &*b), id(&rt, &*c));
        run(&rt, &*a, "START TRANSACTION").unwrap();
        run(&rt, &*a, &format!("UPDATE {t} SET total = 0 WHERE id = 1")).unwrap();
        run(&rt, &*b, "START TRANSACTION").unwrap();
        run(&rt, &*b, &format!("UPDATE {t} SET total = 0 WHERE id = 2")).unwrap();
        let rb = start(&rt, &b, &format!("UPDATE {t} SET total = 1 WHERE id = 1"));
        let rc = start(&rt, &c, &format!("UPDATE {t} SET total = 1 WHERE id = 2"));
        let s = until(&rt, &*screen, |s| {
            s.wait_of(ib).is_some() && s.wait_of(ic).is_some()
        });
        let chains = s.chains();
        let ours = chains.iter().find(|c| c.levels[0] == [ia]).unwrap();
        assert_eq!(ours.levels, vec![vec![ia], vec![ib], vec![ic]]);
        run(&rt, &*a, "ROLLBACK").unwrap();
        finish(&rt, rb).unwrap();
        run(&rt, &*b, "ROLLBACK").unwrap();
        finish(&rt, rc).unwrap();
        run(&rt, &*screen, &format!("DROP TABLE {t}")).unwrap();
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mysql_three_session_chain() {
        three_session_chain(Engine::Mysql, 3306);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mariadb_three_session_chain() {
        three_session_chain(Engine::Mariadb, 3307);
    }

    /// An open transaction that read the table blocks an ALTER on it.
    fn metadata_lock_wait(engine: Engine, port: u16) {
        let rt = Runtime::new().unwrap();
        let cfg = root(engine, port);
        let [reader, alter, screen] = [(); 3].map(|()| connect_as(&rt, &cfg));
        let t = "p9_mdl";
        setup(&rt, &*screen, t);
        let mariadb = engine == Engine::Mariadb;
        if mariadb {
            // First without the plugin: the wait shows, its holder is not named.
            let _ = run(&rt, &*screen, "UNINSTALL SONAME 'metadata_lock_info'");
        }
        let (ir, ia) = (id(&rt, &*reader), id(&rt, &*alter));
        run(&rt, &*reader, "START TRANSACTION").unwrap();
        run(&rt, &*reader, &format!("SELECT * FROM {t}")).unwrap();
        let ra = start(&rt, &alter, &format!("ALTER TABLE {t} ADD COLUMN note INT"));
        if mariadb {
            let s = until(&rt, &*screen, |s| s.wait_of(ia).is_some());
            assert_eq!(s.wait_of(ia).unwrap().blocker, None);
            assert!(
                s.notes.iter().any(|n| n.contains("metadata_lock_info")),
                "{:?}",
                s.notes
            );
            run(&rt, &*screen, "INSTALL SONAME 'metadata_lock_info'").unwrap();
        }
        let s = until(&rt, &*screen, |s| s.blockers(ia) == vec![ir]);
        let wait = s.wait_of(ia).unwrap();
        assert_eq!(wait.wants.kind, LockKind::Metadata);
        assert_eq!(wait.wants.table, t);
        assert_eq!(s.session(ia).unwrap().state, State::Waiting);
        assert!(
            s.session(ir)
                .unwrap()
                .holds
                .iter()
                .any(|l| l.kind == LockKind::Metadata && l.table == t)
        );
        run(&rt, &*reader, "COMMIT").unwrap();
        finish(&rt, ra).unwrap();
        if mariadb {
            run(&rt, &*screen, "UNINSTALL SONAME 'metadata_lock_info'").unwrap();
        }
        run(&rt, &*screen, &format!("DROP TABLE {t}")).unwrap();
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mysql_metadata_lock_wait() {
        metadata_lock_wait(Engine::Mysql, 3306);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mariadb_metadata_lock_wait() {
        metadata_lock_wait(Engine::Mariadb, 3307);
    }

    /// Two transactions take two rows in opposite order; the server's report parses.
    fn deadlock_report(engine: Engine, port: u16) {
        let rt = Runtime::new().unwrap();
        let cfg = root(engine, port);
        let [a, b, screen] = [(); 3].map(|()| connect_as(&rt, &cfg));
        let t = "p9_deadlock";
        setup(&rt, &*screen, t);
        let (ia, ib) = (id(&rt, &*a), id(&rt, &*b));
        run(&rt, &*a, "START TRANSACTION").unwrap();
        run(&rt, &*a, &format!("UPDATE {t} SET total = 0 WHERE id = 1")).unwrap();
        run(&rt, &*b, "START TRANSACTION").unwrap();
        run(&rt, &*b, &format!("UPDATE {t} SET total = 0 WHERE id = 2")).unwrap();
        let ra = start(&rt, &a, &format!("UPDATE {t} SET total = 1 WHERE id = 2"));
        until(&rt, &*screen, |s| s.wait_of(ia).is_some());
        let rb = run(&rt, &*b, &format!("UPDATE {t} SET total = 1 WHERE id = 1"));
        let ra = finish(&rt, ra);
        assert!(
            ra.is_err() != rb.is_err(),
            "one is rolled back: {ra:?} {rb:?}"
        );
        let _ = run(&rt, &*a, "ROLLBACK");
        let _ = run(&rt, &*b, "ROLLBACK");
        let raw = rt.block_on(screen.last_deadlock()).unwrap().unwrap();
        let d = parse_deadlock(&raw).unwrap();
        let threads: Vec<_> = d.transactions.iter().map(|t| t.thread).collect();
        assert!(
            threads.contains(&Some(ia)) && threads.contains(&Some(ib)),
            "{threads:?}"
        );
        let victim = d.rolled_back.unwrap();
        let rolled = d.transactions.iter().find(|t| t.number == victim).unwrap();
        assert_eq!(rolled.thread == Some(ia), ra.is_err());
        assert!(
            d.transactions
                .iter()
                .all(|t| !t.holds.is_empty() && !t.waits_for.is_empty())
        );
        run(&rt, &*screen, &format!("DROP TABLE {t}")).unwrap();
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mysql_deadlock_report() {
        deadlock_report(Engine::Mysql, 3306);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mariadb_deadlock_report() {
        deadlock_report(Engine::Mariadb, 3307);
    }

    /// The `deadlock` user has no PROCESS privilege.
    fn without_process(engine: Engine, port: u16) {
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &config(engine, port, "deadlock"));
        let _root = connect_as(&rt, &root(engine, port));
        let s = rt.block_on(d.sessions()).unwrap();
        assert_eq!(s.own_only, Some("PROCESS"));
        assert!(s.sessions.iter().all(|s| s.user == "deadlock"), "{s:#?}");
        assert!(s.sessions.iter().any(|s| s.own));
        assert!(s.notes.iter().any(|n| n.contains("PROCESS")));
        assert_eq!(rt.block_on(d.waiting_sessions()).unwrap(), 0);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mysql_without_process() {
        without_process(Engine::Mysql, 3306);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mariadb_without_process() {
        without_process(Engine::Mariadb, 3307);
    }
}

/// The Server configuration screen. Every variable changed here is set back, and
/// MySQL's persisted settings are reset.
mod config_screen {
    use super::*;
    use deadlock_core::config::{
        Applies, Category, ConfigChange, Mode, NewValue, ServerConfig, Source, ValueType,
    };

    fn root(engine: Engine, port: u16) -> ConnectionConfig {
        ConnectionConfig {
            user: "root".into(),
            ..config(engine, port, "")
        }
    }

    fn load(rt: &Runtime, d: &dyn Driver) -> ServerConfig {
        rt.block_on(d.config()).unwrap()
    }

    fn set(name: &str, v: &str, mode: Mode) -> ConfigChange {
        ConfigChange {
            name: name.into(),
            value: NewValue::Set(v.into()),
            mode,
        }
    }

    fn reads_everything(engine: Engine, port: u16) {
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &root(engine, port));
        let cfg = load(&rt, &*d);
        assert!(cfg.params.len() > 600, "{}", cfg.params.len());
        assert_eq!(cfg.read_only, None);
        assert_eq!(cfg.persist, engine == Engine::Mysql);
        let mc = cfg.param("max_connections").unwrap();
        assert_eq!(mc.value_type, ValueType::Integer);
        assert_eq!(
            mc.min.as_deref().map(|m| m.parse::<i64>().unwrap() > 0),
            Some(true)
        );
        assert_eq!(mc.applies, Applies::Now);
        assert_eq!(mc.category, Category::Connections);
        let bp = cfg.param("innodb_buffer_pool_size").unwrap();
        assert_eq!(bp.unit, Some(deadlock_core::config::Unit::Bytes));
        // Both images start from an option file.
        let file = cfg.config_file.as_deref().unwrap_or_default();
        assert!(file.contains(".cnf"), "{file:?}");
        assert!(
            cfg.params
                .iter()
                .any(|p| matches!(p.source, Source::ConfigFile(Some(_))))
        );
        if engine == Engine::Mariadb {
            let bf = cfg.param("binlog_format").unwrap();
            assert!(matches!(&bf.value_type, ValueType::Enum(v) if v.contains(&"ROW".to_owned())));
            assert!(bf.default.is_some() && bf.description.is_some());
            assert_eq!(
                cfg.param("general_log").unwrap().value_type,
                ValueType::Boolean
            );
            assert!(
                cfg.param("innodb_buffer_pool_size")
                    .unwrap()
                    .block
                    .is_some()
            );
        } else {
            assert_eq!(
                cfg.param("innodb_buffer_pool_instances").unwrap().applies,
                Applies::Restart
            );
            assert_eq!(cfg.param("datadir").unwrap().applies, Applies::Fixed);
        }
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mysql_reads_the_variables() {
        reads_everything(Engine::Mysql, 3306);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mariadb_reads_the_variables() {
        reads_everything(Engine::Mariadb, 3307);
    }

    /// A dynamic variable changes for this run and reads back (`max_connections` belongs
    /// to the persist test, which runs at the same time); a refused value stops
    /// the apply there; the original comes back.
    fn changes_and_stops_at_errors(engine: Engine, port: u16) {
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &root(engine, port));
        let cfg = load(&rt, &*d);
        let original = cfg
            .param("max_connect_errors")
            .unwrap()
            .value
            .clone()
            .unwrap();
        let next = (original.parse::<u64>().unwrap() + 7).to_string();
        let done = rt
            .block_on(d.apply_config(&cfg, &[set("max_connect_errors", &next, Mode::RunOnly)]))
            .unwrap();
        assert_eq!((done.done, done.error), (1, None));
        let after = load(&rt, &*d);
        let p = after.param("max_connect_errors").unwrap();
        assert_eq!(p.value.as_deref(), Some(next.as_str()));
        assert!(matches!(p.source, Source::Runtime { .. }), "{:?}", p.source);
        assert_eq!(p.persisted, None);

        // Out of range: refused before anything runs.
        assert!(
            after
                .statements(&set("max_connect_errors", "0", Mode::RunOnly))
                .is_err()
        );
        // The server refuses the second; the third never runs.
        let changes = [
            set("max_connect_errors", &original, Mode::RunOnly),
            set("default_storage_engine", "NoSuchEngine", Mode::RunOnly),
            set("max_connect_errors", &next, Mode::RunOnly),
        ];
        let done = rt.block_on(d.apply_config(&after, &changes)).unwrap();
        assert_eq!(done.done, 1);
        assert!(done.error.unwrap().contains("NoSuchEngine"));
        let back = load(&rt, &*d);
        assert_eq!(
            back.param("max_connect_errors").unwrap().value.as_deref(),
            Some(original.as_str())
        );
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mysql_changes_and_stops_at_errors() {
        changes_and_stops_at_errors(Engine::Mysql, 3306);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mariadb_changes_and_stops_at_errors() {
        changes_and_stops_at_errors(Engine::Mariadb, 3307);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mysql_persists_and_resets() {
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &root(Engine::Mysql, 3306));
        let cfg = load(&rt, &*d);
        let mc = cfg.param("max_connections").unwrap();
        let original = mc.value.clone().unwrap();
        let instances = cfg.param("innodb_buffer_pool_instances").unwrap();
        let running = instances.value.clone().unwrap();
        let other = if running == "2" { "3" } else { "2" };
        let changes = [
            set("max_connections", "222", Mode::Persist),
            set("innodb_buffer_pool_instances", other, Mode::Persist),
        ];
        let done = rt.block_on(d.apply_config(&cfg, &changes)).unwrap();
        let after = load(&rt, &*d);
        // Reset first, so a failed assertion leaves nothing persisted.
        run(&rt, &*d, "RESET PERSIST").unwrap();
        run(
            &rt,
            &*d,
            &format!("SET GLOBAL max_connections = {original}"),
        )
        .unwrap();
        assert_eq!((done.done, done.error), (2, None));
        let mc = after.param("max_connections").unwrap();
        assert_eq!(mc.value.as_deref(), Some("222"));
        assert_eq!(mc.persisted.as_deref(), Some("222"));
        assert!(
            matches!(mc.source, Source::Runtime { .. }),
            "{:?}",
            mc.source
        );
        let ip = after.param("innodb_buffer_pool_instances").unwrap();
        assert_eq!(ip.value.as_deref(), Some(running.as_str()), "restart-only");
        assert_eq!(ip.persisted.as_deref(), Some(other));
        assert!(ip.restart_pending());

        // Revert to default: SET GLOBAL … = DEFAULT and RESET PERSIST.
        let done = rt.block_on(d.apply_config(&cfg, &changes)).unwrap();
        assert_eq!(done.done, 2);
        let persisted = load(&rt, &*d);
        let revert = |name: &str| ConfigChange {
            name: name.into(),
            value: NewValue::Default,
            mode: Mode::Persist,
        };
        let reverts = [
            revert("max_connections"),
            revert("innodb_buffer_pool_instances"),
        ];
        let statements: Vec<String> = reverts
            .iter()
            .flat_map(|c| persisted.statements(c).unwrap())
            .collect();
        assert_eq!(
            statements,
            [
                "SET GLOBAL max_connections = DEFAULT",
                "RESET PERSIST max_connections",
                "RESET PERSIST innodb_buffer_pool_instances"
            ]
        );
        let done = rt.block_on(d.apply_config(&persisted, &reverts)).unwrap();
        let reset = load(&rt, &*d);
        run(&rt, &*d, "RESET PERSIST").unwrap();
        run(
            &rt,
            &*d,
            &format!("SET GLOBAL max_connections = {original}"),
        )
        .unwrap();
        assert_eq!((done.done, done.error), (2, None));
        assert_eq!(reset.param("max_connections").unwrap().persisted, None);
        assert_eq!(
            reset.param("max_connections").unwrap().value.as_deref(),
            Some(original.as_str())
        );
        assert_eq!(
            reset
                .param("innodb_buffer_pool_instances")
                .unwrap()
                .persisted,
            None
        );
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mariadb_cannot_change_restart_only_variables() {
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &root(Engine::Mariadb, 3307));
        let cfg = load(&rt, &*d);
        let p = cfg.param("innodb_buffer_pool_chunk_size").unwrap();
        assert_eq!(p.applies, Applies::Restart);
        let c = set("innodb_buffer_pool_chunk_size", "2097152", Mode::RunOnly);
        assert!(cfg.statements(&c).unwrap_err().contains("option file"));
        let line = cfg.config_line(&set("max_connections", "200", Mode::RunOnly));
        assert_eq!(line.as_deref(), Some("max_connections = 200"));
    }

    /// The `deadlock` user has no SYSTEM_VARIABLES_ADMIN / SUPER: the screen is read-only
    /// and nothing runs.
    fn without_privilege(engine: Engine, port: u16, privilege: &str) {
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &config(engine, port, "deadlock"));
        let cfg = load(&rt, &*d);
        assert!(cfg.params.len() > 600);
        let why = cfg.read_only.clone().unwrap_or_default();
        assert!(why.contains(privilege), "{why}");
        let c = set("max_connections", "300", Mode::RunOnly);
        assert_eq!(cfg.statements(&c), Err(why.clone()));
        let done = rt.block_on(d.apply_config(&cfg, &[c])).unwrap();
        assert_eq!((done.done, done.error), (0, Some(why)));
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mysql_without_privilege() {
        without_privilege(Engine::Mysql, 3306, "SYSTEM_VARIABLES_ADMIN");
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mariadb_without_privilege() {
        without_privilege(Engine::Mariadb, 3307, "SUPER");
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn read_only_connections_apply_nothing() {
        let rt = Runtime::new().unwrap();
        let d = connect_as(
            &rt,
            &ConnectionConfig {
                read_only: true,
                ..root(Engine::Mysql, 3306)
            },
        );
        let cfg = load(&rt, &*d);
        let c = set("max_connections", "300", Mode::RunOnly);
        assert!(rt.block_on(d.apply_config(&cfg, &[c])).is_err());
    }
}

/// Oracle, as the DBA (`SYSTEM`) and as the plain `deadlock` user. Objects are
/// prefixed `P11_` and dropped again.
mod oracle {
    use super::*;
    use deadlock_core::driver::{NodeKind, ObjectPath};

    pub(super) fn plain() -> ConnectionConfig {
        config(Engine::Oracle, 1521, "FREEPDB1")
    }

    pub(super) fn dba() -> ConnectionConfig {
        ConnectionConfig {
            user: "system".into(),
            ..plain()
        }
    }

    pub(super) fn exec(rt: &Runtime, d: &dyn Driver, sql: &str) {
        if let Err(e) = run(rt, d, sql) {
            panic!("{sql}: {e}");
        }
    }

    fn path(p: &[&str]) -> ObjectPath {
        ObjectPath(p.iter().map(|s| (*s).to_owned()).collect())
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn oracle_browses_schema() {
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &plain());
        for sql in [
            "DROP VIEW IF EXISTS p11_totals",
            "DROP MATERIALIZED VIEW IF EXISTS p11_mv",
            "DROP TABLE IF EXISTS p11_orders",
            "DROP TABLE IF EXISTS p11_customers",
            "DROP SEQUENCE IF EXISTS p11_seq",
            "CREATE TABLE p11_customers (id NUMBER(10) PRIMARY KEY, name VARCHAR2(40 CHAR))",
            "CREATE TABLE p11_orders (id NUMBER PRIMARY KEY, customer_id NUMBER(10) \
             REFERENCES p11_customers(id), total NUMBER(12,2), at TIMESTAMP WITH TIME ZONE)",
            "CREATE VIEW p11_totals AS SELECT customer_id, SUM(total) s FROM p11_orders GROUP BY customer_id",
            "CREATE MATERIALIZED VIEW p11_mv AS SELECT COUNT(*) n FROM p11_orders",
            "CREATE SEQUENCE p11_seq",
            "CREATE OR REPLACE PROCEDURE p11_tree_bad AS BEGIN x := ; END;",
        ] {
            let _ = run(&rt, &*d, sql);
        }
        let schemas = rt.block_on(d.children(&path(&[]))).unwrap();
        assert_eq!(schemas[0].name, "DEADLOCK");
        assert!(!schemas[0].system);
        let sys = schemas.iter().position(|s| s.name == "SYS").unwrap();
        assert!(schemas[sys].system, "SYS is Oracle-maintained");
        assert!(!schemas[0].system);

        let groups = rt.block_on(d.children(&path(&["DEADLOCK"]))).unwrap();
        let names: Vec<_> = groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Tables",
                "Views",
                "Materialized views",
                "Sequences",
                "Procedures",
                "Functions",
                "Packages",
                "Triggers",
                "Synonyms",
                "Types"
            ]
        );
        let find = |g: usize, name: &str| {
            groups[g]
                .children
                .as_ref()
                .unwrap()
                .iter()
                .find(|n| n.name == name)
                .cloned()
        };
        let orders = find(0, "P11_ORDERS").unwrap();
        let cols = orders.children.unwrap();
        let col = |n: &str| cols.iter().find(|c| c.name == n).unwrap();
        assert!(col("ID").primary_key && col("ID").detail == "number");
        assert!(col("CUSTOMER_ID").foreign_key && col("CUSTOMER_ID").detail == "number(10)");
        assert_eq!(col("TOTAL").detail, "number(12,2)");
        assert_eq!(col("AT").detail, "timestamp(6) with time zone");
        assert!(
            find(0, "P11_MV").is_none(),
            "the MV's table is not listed as a table"
        );
        assert_eq!(find(1, "P11_TOTALS").unwrap().kind, NodeKind::View);
        assert_eq!(find(2, "P11_MV").unwrap().children.unwrap()[0].name, "N");
        assert!(find(3, "P11_SEQ").is_some());
        assert_eq!(find(4, "P11_TREE_BAD").unwrap().detail, "invalid");
        let direct = rt
            .block_on(d.children(&path(&["DEADLOCK", "Tables", "P11_CUSTOMERS"])))
            .unwrap();
        assert_eq!(direct[1].detail, "varchar2(40 char)");

        // The DBA sees other users' schemas, the plain user's first among them.
        let admin = connect_as(&rt, &dba());
        let all = rt.block_on(admin.children(&path(&[]))).unwrap();
        assert_eq!(all[0].name, "SYSTEM");
        let deadlock = all.iter().position(|s| s.name == "DEADLOCK").unwrap();
        let first_system = all
            .iter()
            .position(|s| s.system && s.name != "SYSTEM")
            .unwrap();
        assert!(deadlock < first_system);
        for sql in [
            "DROP VIEW p11_totals",
            "DROP MATERIALIZED VIEW p11_mv",
            "DROP TABLE p11_orders",
            "DROP TABLE p11_customers",
            "DROP SEQUENCE p11_seq",
            "DROP PROCEDURE p11_tree_bad",
        ] {
            exec(&rt, &*d, sql);
        }
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn oracle_query_workspace() {
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &plain());
        // DBMS_OUTPUT comes back with the statement.
        let s = rt
            .block_on(d.execute(
                "BEGIN DBMS_OUTPUT.PUT_LINE('hello'); DBMS_OUTPUT.PUT_LINE('wörld'); END;",
            ))
            .unwrap();
        assert_eq!(s.output, ["hello", "wörld"]);
        let s = rt.block_on(d.execute("BEGIN NULL; END;")).unwrap();
        assert!(s.output.is_empty());
        // Binds by name; None is NULL.
        let binds = [
            ("A".to_owned(), Some("7".to_owned())),
            ("B".to_owned(), None),
        ];
        let got = rt.block_on(async {
            d.execute_bound("SELECT :a + 1, NVL(:b, 'null') FROM dual", &binds)
                .await
                .unwrap()
                .collect()
                .await
                .unwrap()
        });
        assert_eq!(
            got[0],
            [Value::Decimal("8".into()), Value::Text("null".into())]
        );
        // DESC is answered by the driver.
        exec(
            &rt,
            &*d,
            "CREATE TABLE p11_desc (id NUMBER(5) NOT NULL, note VARCHAR2(10 CHAR))",
        );
        let got = rows(&rt, &*d, "DESC p11_desc");
        assert_eq!(
            got,
            [
                vec![
                    Value::Text("ID".into()),
                    Value::Text("NOT NULL".into()),
                    Value::Text("NUMBER(5)".into())
                ],
                vec![
                    Value::Text("NOTE".into()),
                    Value::Null,
                    Value::Text("VARCHAR2(10 CHAR)".into())
                ],
            ]
        );
        assert_eq!(rows(&rt, &*d, "describe deadlock.p11_desc").len(), 2);
        let e = run(&rt, &*d, "DESC p11_nope").unwrap_err();
        assert!(e.contains("no table or view P11_NOPE"), "{e}");
        exec(&rt, &*d, "DROP TABLE p11_desc");
        // Compilation errors come with line and column.
        let e = run(
            &rt,
            &*d,
            "CREATE OR REPLACE PROCEDURE p11_bad AS\nBEGIN\n  x := ;\nEND;",
        )
        .unwrap_err();
        assert!(
            e.contains("PROCEDURE P11_BAD was created with compilation errors"),
            "{e}"
        );
        assert!(e.contains("Line 3, column 8: PLS-00103"), "{e}");
        let e = run(&rt, &*d, "SELECT 1 FROM p11_bad").unwrap_err();
        assert!(e.contains("ORA-"), "{e}");
        exec(&rt, &*d, "DROP PROCEDURE p11_bad");
        // Types render losslessly; the session's NLS settings are fixed.
        let got = rows(
            &rt,
            &*d,
            "SELECT 123456789012345678901234567890.123456789, DATE '2026-11-22', \
             TIMESTAMP '2026-01-02 03:04:05.123456789', \
             TIMESTAMP '2026-01-02 03:04:05.5 +02:00', INTERVAL '3 04:05:06.7' DAY TO SECOND, \
             INTERVAL '-2-3' YEAR TO MONTH, HEXTORAW('DEAD'), TO_CLOB('long'), \
             TO_BLOB(HEXTORAW('BEEF')), TO_CHAR(DATE '2026-11-22'), \
             CAST(TIMESTAMP '2026-01-02 03:04:05 +00:00' AS VARCHAR2(40)) FROM dual",
        );
        let text: Vec<String> = got[0].iter().map(ToString::to_string).collect();
        assert_eq!(
            text,
            [
                "123456789012345678901234567890.123456789",
                "2026-11-22 00:00:00",
                "2026-01-02 03:04:05.123456789",
                // Thin hands time zones over in UTC; Thick keeps the offset.
                if thick() {
                    "2026-01-02 03:04:05.500000 +02:00"
                } else {
                    "2026-01-02 01:04:05.500000 +00:00"
                },
                "+3 04:05:06.700000000",
                "-2-3",
                "0xDEAD",
                "long",
                "0xBEEF",
                "2026-11-22 00:00:00",
                "2026-01-02 03:04:05.000000000 +00:00",
            ]
        );
        let rowid = rows(&rt, &*d, "SELECT ROWID FROM dual");
        assert!(matches!(&rowid[0][0], Value::Text(r) if r.len() == 18));
    }

    mod editor {
        use std::collections::BTreeMap;

        use deadlock_core::edit::{
            self, Change, ChangeKind, Edit, Pending, RowId, TableInfo, TableRef, page_sql,
            parse_input,
        };

        use super::*;

        fn table(name: &str) -> TableRef {
            TableRef {
                database: "DEADLOCK".into(),
                table: name.into(),
            }
        }

        fn info(rt: &Runtime, d: &dyn Driver, t: &TableRef) -> TableInfo {
            rt.block_on(d.table_info(t)).unwrap()
        }

        fn all(rt: &Runtime, d: &dyn Driver, t: &TableRef, i: &TableInfo) -> Vec<Vec<Value>> {
            rows(
                rt,
                d,
                &edit::select_sql(t, i, "", &i.key_order().unwrap_or_default()).unwrap(),
            )
        }

        fn apply(rt: &Runtime, d: &dyn Driver, c: &[Change], auto: bool) -> Result<(), String> {
            rt.block_on(d.apply(c, auto)).map_err(|e| e.to_string())
        }

        fn fresh(rt: &Runtime, d: &dyn Driver, name: &str, ddl: &str) -> TableRef {
            exec(rt, d, &format!("DROP TABLE IF EXISTS {name} PURGE"));
            exec(rt, d, &format!("CREATE TABLE {name} ({ddl})"));
            table(&name.to_ascii_uppercase())
        }

        #[test]
        #[ignore = "needs docker compose"]
        fn oracle_literals_round_trip() {
            let rt = Runtime::new().unwrap();
            let d = connect_as(&rt, &plain());
            let t = fresh(
                &rt,
                &*d,
                "p11_types",
                "id NUMBER GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, n NUMBER, \
                 de NUMBER(10,2), bd BINARY_DOUBLE, d DATE, ts TIMESTAMP(9), \
                 tz TIMESTAMP(6) WITH TIME ZONE, ids INTERVAL DAY(3) TO SECOND(6), \
                 iym INTERVAL YEAR(4) TO MONTH, r RAW(16), bl BLOB, cl CLOB, \
                 vc VARCHAR2(5 CHAR), \"Mixed\" VARCHAR2(10), g NUMBER GENERATED ALWAYS AS (de * 2) VIRTUAL",
            );
            let i = info(&rt, &*d, &t);
            assert!(i.oracle && !i.view && i.key == Some(vec![0]) && i.key_note().is_none());
            assert!(i.columns[0].auto_increment && !i.columns[0].generated);
            let pos = |name: &str| i.columns.iter().position(|c| c.name == name).unwrap();
            assert!(i.columns[pos("G")].locked().is_some());
            let inputs = [
                ("N", "123456789012345678901234567890.5"),
                ("DE", "-12345678.91"),
                ("BD", "0.1"),
                ("D", "2026-11-22 16:05:07"),
                ("TS", "2026-11-22 16:05:00.123456789"),
                ("TZ", "2026-11-22 16:05:00.250000 +00:00"),
                ("IDS", "+3 04:05:06.700000000"),
                ("IYM", "-2-3"),
                ("R", "0x00FF5C27"),
                ("BL", "0xDEADBEEF"),
                ("CL", "O'Brien \\ \"q\" \u{1F600}\nline2"),
                ("VC", "it''s"),
                ("Mixed", "x"),
            ];
            let mut p = Pending::default();
            let row = p.add(i.columns.len());
            let mut expected = BTreeMap::new();
            for (name, text) in inputs {
                let c = pos(name);
                let v = parse_input(&i.columns[c], text).unwrap_or_else(|e| panic!("{name}: {e}"));
                expected.insert(c, v.clone());
                p.set(row, c, Edit::Value(v), None);
            }
            let changes = p.changes(&t, &i, &[]).unwrap();
            assert!(
                changes[0].sql.contains("HEXTORAW('00FF5C27')"),
                "{}",
                changes[0].sql
            );
            apply(&rt, &*d, &changes, true).unwrap();
            let back = all(&rt, &*d, &t, &i);
            assert_eq!(back.len(), 1);
            for (c, want) in &expected {
                assert_eq!(
                    back[0][*c].to_string(),
                    want.to_string(),
                    "{}",
                    i.columns[*c].name
                );
            }
            assert_eq!(back[0][pos("G")].to_string(), "-24691357.82");
            // Update every typed column by key: the WHERE matches each original value.
            let mut p = Pending::default();
            let r = RowId::Loaded(0);
            p.set(
                r,
                pos("VC"),
                Edit::Value(Value::Null),
                Some(&back[0][pos("VC")]),
            );
            p.set(
                r,
                0,
                Edit::Value(Value::Decimal("77".into())),
                Some(&back[0][0]),
            );
            apply(&rt, &*d, &p.changes(&t, &i, &back).unwrap(), true).unwrap();
            let again = all(&rt, &*d, &t, &i);
            assert_eq!(again[0][0], Value::Decimal("77".into()));
            assert_eq!(again[0][pos("VC")], Value::Null);
            exec(&rt, &*d, "DROP TABLE p11_types PURGE");
        }

        #[test]
        #[ignore = "needs docker compose"]
        fn oracle_failed_submits_change_nothing() {
            let rt = Runtime::new().unwrap();
            let (a, b) = (connect_as(&rt, &plain()), connect_as(&rt, &plain()));
            let t = fresh(
                &rt,
                &*a,
                "p11_rows",
                "id NUMBER PRIMARY KEY, v VARCHAR2(10)",
            );
            exec(
                &rt,
                &*a,
                "INSERT INTO p11_rows SELECT LEVEL, CHR(96 + LEVEL) FROM dual CONNECT BY LEVEL <= 3",
            );
            let i = info(&rt, &*a, &t);
            let loaded = all(&rt, &*a, &t, &i);
            let mut p = Pending::default();
            p.set(
                RowId::Loaded(0),
                1,
                Edit::Value(Value::Text("A".into())),
                Some(&loaded[0][1]),
            );
            p.set(
                RowId::Loaded(1),
                1,
                Edit::Value(Value::Text("B".into())),
                Some(&loaded[1][1]),
            );
            p.delete(&[RowId::Loaded(2)]);
            let added = p.add(2);
            p.set(added, 0, Edit::Value(Value::Int(4)), None);
            let changes = p.changes(&t, &i, &loaded).unwrap();
            assert_eq!(changes.len(), 4);
            exec(&rt, &*b, "UPDATE p11_rows SET id = 20 WHERE id = 2");
            let e = apply(&rt, &*a, &changes, true).unwrap_err();
            assert!(e.contains("change 2 of 4 touched 0 rows"), "{e}");
            let now = all(&rt, &*a, &t, &i);
            assert_eq!(now[0][1], Value::Text("a".into()));
            assert!(!open(&rt, &*a));
            exec(&rt, &*b, "UPDATE p11_rows SET id = 2 WHERE id = 20");
            exec(&rt, &*b, "INSERT INTO p11_rows VALUES (4, 'd')");
            let before = all(&rt, &*a, &t, &i);
            let e = apply(&rt, &*a, &changes, true).unwrap_err();
            assert!(
                e.contains("change 4 of 4 failed") && e.contains("ORA-00001"),
                "{e}"
            );
            assert_eq!(all(&rt, &*a, &t, &i), before);
            assert!(!open(&rt, &*a));
            // Manual: earlier work survives a failed submit; a good one stays open.
            exec(&rt, &*b, "DELETE FROM p11_rows WHERE id = 4");
            let before = all(&rt, &*a, &t, &i);
            rt.block_on(a.set_auto_commit(false)).unwrap();
            exec(&rt, &*a, "INSERT INTO p11_rows VALUES (9, 'z')");
            apply(&rt, &*a, &changes[1..2], false).unwrap();
            let fail = [changes[0].clone(), changes[3].clone(), changes[3].clone()];
            assert!(
                apply(&rt, &*a, &fail, false)
                    .unwrap_err()
                    .contains("change 3 of 3 failed")
            );
            let mine = all(&rt, &*a, &t, &i);
            assert!(mine.iter().any(|r| r[0] == Value::Decimal("9".into())));
            assert!(mine.iter().any(|r| r[1] == Value::Text("B".into())));
            assert!(!mine.iter().any(|r| r[1] == Value::Text("A".into())));
            assert!(open(&rt, &*a));
            rt.block_on(a.end_transaction(false)).unwrap();
            assert_eq!(all(&rt, &*a, &t, &i), before);
            rt.block_on(a.set_auto_commit(true)).unwrap();
            exec(&rt, &*a, "DROP TABLE p11_rows PURGE");
        }

        #[test]
        #[ignore = "needs docker compose"]
        fn oracle_rowid_views_and_pages() {
            let rt = Runtime::new().unwrap();
            let d = connect_as(&rt, &plain());
            // No key: rows by ROWID, and it says so.
            let t = fresh(&rt, &*d, "p11_heap", "a NUMBER, b VARCHAR2(10)");
            exec(&rt, &*d, "INSERT INTO p11_heap VALUES (1, 'x')");
            exec(&rt, &*d, "INSERT INTO p11_heap VALUES (1, 'x')");
            let i = info(&rt, &*d, &t);
            assert!(i.read_only_reason().is_none());
            assert!(i.key_note().unwrap().contains("ROWID"));
            assert_eq!(i.columns.last().unwrap().name, "ROWID");
            assert!(i.columns.last().unwrap().locked().is_some());
            let loaded = all(&rt, &*d, &t, &i);
            let mut p = Pending::default();
            p.set(
                RowId::Loaded(1),
                1,
                Edit::Value(Value::Text("y".into())),
                Some(&loaded[1][1]),
            );
            let added = p.add(i.columns.len());
            p.set(added, 0, Edit::Value(Value::Int(3)), None);
            let changes = p.changes(&t, &i, &loaded).unwrap();
            assert!(
                changes[0].sql.contains("where ROWID = '"),
                "{}",
                changes[0].sql
            );
            assert!(!changes[1].sql.contains("ROWID"), "{}", changes[1].sql);
            apply(&rt, &*d, &changes, true).unwrap();
            let got: Vec<String> = all(&rt, &*d, &t, &i)
                .iter()
                .map(|r| r[1].to_string())
                .collect();
            assert_eq!(got.iter().filter(|v| *v == "y").count(), 1);
            assert_eq!(got.len(), 3);
            // An insert of defaults only.
            let mut p = Pending::default();
            p.add(i.columns.len());
            apply(&rt, &*d, &p.changes(&t, &i, &[]).unwrap(), true).unwrap();
            // Unique key on NOT NULL columns beats ROWID.
            let t = fresh(
                &rt,
                &*d,
                "p11_unique",
                "a NUMBER NULL UNIQUE, b NUMBER NOT NULL, c NUMBER NOT NULL, CONSTRAINT p11_bc UNIQUE (b, c)",
            );
            assert_eq!(info(&rt, &*d, &t).key, Some(vec![1, 2]));
            exec(
                &rt,
                &*d,
                "CREATE OR REPLACE VIEW p11_view AS SELECT b FROM p11_unique",
            );
            let v = info(&rt, &*d, &table("P11_VIEW"));
            assert!(v.view && v.key.is_none() && v.read_only_reason().is_some());
            assert!(rt.block_on(d.table_info(&table("P11_NOPE"))).is_err());
            // Paging with OFFSET … FETCH.
            let t = fresh(&rt, &*d, "p11_pages", "id NUMBER PRIMARY KEY");
            exec(
                &rt,
                &*d,
                "INSERT INTO p11_pages SELECT LEVEL FROM dual CONNECT BY LEVEL <= 1200",
            );
            let i = info(&rt, &*d, &t);
            let sql = edit::select_sql(&t, &i, "id > 100", "\"ID\" desc").unwrap();
            let sizes: Vec<usize> = [0, 500, 1000]
                .map(|o| rows(&rt, &*d, &page_sql(&sql, true, 500, o)).len())
                .into();
            assert_eq!(sizes, [500, 500, 100]);
            assert_eq!(
                rows(&rt, &*d, &edit::count_sql(&t, &i, "id <= 7", None).unwrap())[0][0]
                    .to_string(),
                "7"
            );
            let ro = connect_as(
                &rt,
                &ConnectionConfig {
                    read_only: true,
                    ..plain()
                },
            );
            let del = [Change {
                kind: ChangeKind::Delete,
                sql: "delete from p11_pages where id = 1".into(),
            }];
            assert!(
                apply(&rt, &*ro, &del, true)
                    .unwrap_err()
                    .contains("read-only")
            );
            for sql in [
                "DROP VIEW p11_view",
                "DROP TABLE p11_heap PURGE",
                "DROP TABLE p11_unique PURGE",
                "DROP TABLE p11_pages PURGE",
            ] {
                exec(&rt, &*d, sql);
            }
        }
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn oracle_table_structure() {
        use deadlock_core::edit::TableRef;
        use deadlock_core::structure::{self, IndexKind, IndexPart, NewColumn, NewIndex, Position};
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &plain());
        for sql in [
            "DROP TABLE IF EXISTS p11_line PURGE",
            "DROP TABLE IF EXISTS p11_ord PURGE",
            "DROP TABLE IF EXISTS p11_cust PURGE",
            "CREATE TABLE p11_cust (id NUMBER(10) PRIMARY KEY, name VARCHAR2(40 CHAR) NOT NULL)",
            "CREATE TABLE p11_ord (id NUMBER GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, \
             cust_id NUMBER(10) CONSTRAINT p11_ord_cust REFERENCES p11_cust(id) ON DELETE CASCADE, \
             qty NUMBER(10) DEFAULT 0 NOT NULL CONSTRAINT p11_qty_pos CHECK (qty >= 0), \
             note VARCHAR2(100), twice NUMBER GENERATED ALWAYS AS (qty * 2) VIRTUAL, \
             CONSTRAINT p11_ord_uq UNIQUE (cust_id, note))",
            "COMMENT ON TABLE p11_ord IS 'orders'",
            "CREATE BITMAP INDEX p11_ord_bm ON p11_ord (qty)",
            "CREATE INDEX p11_ord_fn ON p11_ord (LOWER(note), qty DESC)",
            "CREATE TABLE p11_line (ord_id NUMBER REFERENCES p11_ord(id))",
            "INSERT INTO p11_cust VALUES (1, 'a')",
            "INSERT INTO p11_ord (cust_id, qty) VALUES (1, 2)",
        ] {
            exec(&rt, &*d, sql);
        }
        let t = TableRef {
            database: "DEADLOCK".into(),
            table: "P11_ORD".into(),
        };
        let read = || rt.block_on(d.structure(&t)).unwrap();
        let s = read();
        assert!(s.oracle && !s.view);
        let c = |n: &str| s.columns.iter().find(|c| c.name == n).unwrap();
        assert_eq!(c("ID").key_label(), "PK");
        assert!(
            c("ID")
                .default_label()
                .contains("generated by default as identity")
        );
        assert_eq!(c("CUST_ID").key_label(), "FK");
        assert_eq!(c("QTY").default.as_deref(), Some("0"));
        assert!(!c("QTY").nullable);
        assert_eq!(
            c("TWICE").default_label(),
            "generated always as (\"QTY\"*2) virtual"
        );
        let ix = |n: &str| s.indexes.iter().find(|i| i.name == n).unwrap();
        assert!(s.indexes[0].primary);
        assert_eq!(ix("P11_ORD_BM").kind_label(), "bitmap");
        assert_eq!(
            ix("P11_ORD_FN").parts_label(),
            "(LOWER(\"NOTE\")), QTY desc"
        );
        assert_eq!(ix("P11_ORD_UQ").constraint.as_deref(), Some("P11_ORD_UQ"));
        assert_eq!(s.foreign_keys[0].name, "P11_ORD_CUST");
        assert_eq!(s.foreign_keys[0].on_delete, "cascade");
        assert_eq!(s.referenced_by[0].from.table, "P11_LINE");
        assert_eq!(s.checks.len(), 1);
        assert_eq!(s.checks[0].condition, "qty >= 0");
        assert_eq!(s.facts.comment, "orders");
        assert!(s.facts.tablespace.is_some());
        assert!(s.facts.size().is_some_and(|n| n > 0), "{:?}", s.facts);
        assert!(
            s.ddl.starts_with("CREATE TABLE \"DEADLOCK\".\"P11_ORD\""),
            "{}",
            s.ddl
        );
        assert!(
            s.ddl.ends_with(';') && !s.ddl.contains("STORAGE("),
            "{}",
            s.ddl
        );

        // Add column and index, then drop each kind of thing.
        let add = structure::add_column_sql(
            &t,
            &NewColumn {
                name: "Odd name".into(),
                data_type: "varchar2".into(),
                length: "20".into(),
                nullable: false,
                default: "it's".into(),
                auto_increment: false,
                comment: String::new(),
                position: Position::Last,
                ..NewColumn::default()
            },
            &s,
        )
        .unwrap();
        assert_eq!(
            add,
            [
                "alter table \"DEADLOCK\".\"P11_ORD\" add (\"Odd name\" varchar2(20) default 'it''s' not null)"
            ]
        );
        exec(&rt, &*d, &add[0]);
        let idx = structure::add_index_sql(
            &t,
            &NewIndex {
                name: String::new(),
                kind: IndexKind::Unique,
                parts: vec![IndexPart {
                    column: "Odd name".into(),
                    descending: true,
                    ..IndexPart::default()
                }],
            },
            &s,
        )
        .unwrap();
        exec(&rt, &*d, &idx);
        let s2 = read();
        assert_eq!(c("QTY").name, "QTY");
        let odd = s2.columns.iter().find(|c| c.name == "Odd name").unwrap();
        assert_eq!(odd.default.as_deref(), Some("'it''s'"));
        let new_ix = s2
            .indexes
            .iter()
            .find(|i| i.name == "P11_ORD_Odd name_IX")
            .unwrap();
        assert_eq!(new_ix.parts_label(), "Odd name desc");
        let uq = s2.indexes.iter().find(|i| i.name == "P11_ORD_UQ").unwrap();
        for sql in [
            structure::drop_index_sql(&t, new_ix, &s2),
            structure::drop_index_sql(&t, uq, &s2),
            structure::drop_column_sql(&t, "Odd name", &s2),
            structure::drop_foreign_key_sql(&t, "P11_ORD_CUST", &s2),
            s2.drop_constraint_sql(&t, "P11_QTY_POS"),
        ] {
            run(&rt, &*d, &sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
        let s3 = read();
        assert!(s3.foreign_keys.is_empty() && s3.checks.is_empty());
        assert!(
            !s3.indexes
                .iter()
                .any(|i| i.name == "P11_ORD_UQ" || i.name.contains("Odd"))
        );
        assert!(!s3.columns.iter().any(|c| c.name == "Odd name"));
        // A view: columns and DDL.
        exec(
            &rt,
            &*d,
            "CREATE OR REPLACE VIEW p11_v AS SELECT id, qty FROM p11_ord",
        );
        let v = rt
            .block_on(d.structure(&TableRef {
                database: "DEADLOCK".into(),
                table: "P11_V".into(),
            }))
            .unwrap();
        assert!(
            v.view && v.columns.len() == 2 && v.ddl.contains("VIEW"),
            "{}",
            v.ddl
        );
        // The DBA reads another user's table, sizes included.
        let admin = connect_as(&rt, &dba());
        let other = rt.block_on(admin.structure(&t)).unwrap();
        assert!(other.facts.size().is_some() && other.ddl.starts_with("CREATE TABLE"));
        for sql in [
            "DROP VIEW p11_v",
            "DROP TABLE p11_line PURGE",
            "DROP TABLE p11_ord PURGE",
            "DROP TABLE p11_cust PURGE",
        ] {
            exec(&rt, &*d, sql);
        }
    }

    /// Phase 15: Oracle edits write only what changed.
    #[test]
    #[ignore = "needs docker compose"]
    fn oracle_columns_edit() {
        use deadlock_core::edit::TableRef;
        use deadlock_core::structure::{self, NewColumn, Position, TableStructure};
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &plain());
        for sql in [
            "DROP TABLE IF EXISTS p15_ora PURGE",
            "CREATE TABLE p15_ora (id NUMBER GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, \
             qty NUMBER(10) DEFAULT 0 NOT NULL, note VARCHAR2(40 CHAR), amt NUMBER(10), \
             twice NUMBER AS (qty * 2) VIRTUAL)",
            "INSERT INTO p15_ora (qty, note, amt) VALUES (2, 'n', 5)",
        ] {
            exec(&rt, &*d, sql);
        }
        let t = TableRef {
            database: "DEADLOCK".into(),
            table: "P15_ORA".into(),
        };
        let read = || rt.block_on(d.structure(&t)).unwrap();
        let col =
            |s: &TableStructure, n: &str| s.columns.iter().find(|c| c.name == n).cloned().unwrap();
        let edit = |column: &str, f: &dyn Fn(&mut NewColumn)| -> Result<TableStructure, String> {
            let s = read();
            let old = col(&s, column);
            let mut new = NewColumn::from_existing(&old, &s);
            f(&mut new);
            for sql in structure::modify_column_sql(&t, &old, &new, &s)? {
                run(&rt, &*d, &sql).map_err(|e| format!("{sql}: {e}"))?;
            }
            let after = read();
            for c in s
                .columns
                .iter()
                .filter(|c| c.name != column && c.name != new.name)
            {
                assert_eq!(after.columns.iter().find(|x| x.name == c.name), Some(c));
            }
            Ok(after)
        };
        // No edit, no statements.
        let s = read();
        for c in &s.columns {
            let same = NewColumn::from_existing(c, &s);
            assert_eq!(
                structure::modify_column_sql(&t, c, &same, &s),
                Ok(vec![]),
                "{}",
                c.name
            );
        }
        let s = edit("NOTE", &|c| c.name = "REMARK".into()).unwrap();
        assert_eq!(col(&s, "REMARK").column_type, "varchar2(40 char)");
        let s = edit("AMT", &|c| c.length = "12".into()).unwrap();
        assert_eq!(col(&s, "AMT").column_type, "number(12)");
        // A column a virtual column reads cannot change: the form says so before
        // Apply, and the server agrees.
        let s = read();
        let qty = col(&s, "QTY");
        let mut longer = NewColumn::from_existing(&qty, &s);
        longer.length = "12".into();
        assert_eq!(
            structure::modify_column_hints(&qty, &longer, &s),
            [
                "Oracle cannot change a column that virtual column TWICE reads: drop or change \
              TWICE first."
            ]
        );
        let err = edit("QTY", &|c| c.length = "12".into()).unwrap_err();
        assert!(err.contains("ORA-54031"), "{err}");
        // Rename and retype: the rename runs, the modify fails. Rebased on the column
        // re-read by its new name, the same form builds only what is left.
        longer.name = "QTY2".into();
        let sql = structure::modify_column_sql(&t, &qty, &longer, &s).unwrap();
        let labels: Vec<String> = sql.iter().map(|q| structure::statement_label(q)).collect();
        assert_eq!(labels, ["rename column", "modify"]);
        run(&rt, &*d, &sql[0]).unwrap();
        assert!(run(&rt, &*d, &sql[1]).unwrap_err().contains("ORA-54031"));
        let s = read();
        let qty2 = col(&s, "QTY2");
        let rest = structure::modify_column_sql(&t, &qty2, &longer, &s).unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(structure::statement_label(&rest[0]), "modify");
        let back = NewColumn {
            name: "QTY".into(),
            ..NewColumn::from_existing(&qty2, &s)
        };
        for sql in structure::modify_column_sql(&t, &qty2, &back, &s).unwrap() {
            exec(&rt, &*d, &sql);
        }
        // Not null, null, not null: only the change is written (ORA-01451 / ORA-01442).
        let s = edit("AMT", &|c| c.nullable = false).unwrap();
        assert!(!col(&s, "AMT").nullable);
        let s = edit("AMT", &|c| c.nullable = true).unwrap();
        assert!(col(&s, "AMT").nullable);
        let s = edit("AMT", &|c| c.default = "7".into()).unwrap();
        assert_eq!(col(&s, "AMT").default.as_deref(), Some("7"));
        assert_eq!(col(&s, "AMT").column_type, "number(12)");
        let s = edit("REMARK", &|c| c.default = "it's".into()).unwrap();
        assert_eq!(col(&s, "REMARK").default.as_deref(), Some("'it''s'"));
        let s = edit("REMARK", &|c| c.default = String::new()).unwrap();
        assert_eq!(col(&s, "REMARK").default, None);
        let s = edit("REMARK", &|c| c.comment = "a note".into()).unwrap();
        assert_eq!(col(&s, "REMARK").comment, "a note");
        let s = edit("REMARK", &|c| c.comment = String::new()).unwrap();
        assert_eq!(col(&s, "REMARK").comment, "");
        let s = edit("REMARK", &|c| c.length = "60 char".into()).unwrap();
        assert_eq!(col(&s, "REMARK").column_type, "varchar2(60 char)");
        let err = edit("REMARK", &|c| c.position = Position::First).unwrap_err();
        assert!(err.contains("reorder"), "{err}");
        // A virtual column's expression changes in place.
        let s = edit("TWICE", &|c| {
            c.generated.as_mut().unwrap().expression = "(qty * 3)".into();
        })
        .unwrap();
        assert!(
            col(&s, "TWICE")
                .generated
                .unwrap()
                .expression
                .contains("*3"),
            "{:?}",
            col(&s, "TWICE")
        );
        // Add with a comment: two statements.
        let add = structure::add_column_sql(
            &t,
            &NewColumn {
                name: "EXTRA".into(),
                data_type: "number".into(),
                nullable: true,
                comment: "added".into(),
                ..NewColumn::default()
            },
            &s,
        )
        .unwrap();
        assert_eq!(add.len(), 2);
        for sql in &add {
            exec(&rt, &*d, sql);
        }
        assert_eq!(col(&read(), "EXTRA").comment, "added");
        let err = edit("QTY", &|c| c.auto_increment = true).unwrap_err();
        assert!(err.contains("identity"), "{err}");
        let s = edit("ID", &|c| c.auto_increment = false).unwrap();
        assert_eq!(col(&s, "ID").extra, "");
        assert_eq!(count(&rt, &*d, "p15_ora"), "1");
        exec(&rt, &*d, "DROP TABLE p15_ora PURGE");
    }

    /// Phase 16: truncate, rename and drop; `cascade constraints` drops the child's key.
    #[test]
    #[ignore = "needs docker compose"]
    fn oracle_table_actions() {
        use deadlock_core::edit::TableRef;
        use deadlock_core::structure;
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &plain());
        for sql in [
            "DROP TABLE IF EXISTS p16_kid PURGE",
            "DROP TABLE IF EXISTS p16_child PURGE",
            "DROP TABLE IF EXISTS p16_parent PURGE",
            "CREATE TABLE p16_parent (id NUMBER PRIMARY KEY)",
            "CREATE TABLE p16_child (id NUMBER PRIMARY KEY, parent_id NUMBER \
             CONSTRAINT p16_fk REFERENCES p16_parent (id))",
            "INSERT INTO p16_parent VALUES (1)",
            "INSERT INTO p16_child VALUES (1, 1)",
            "COMMIT",
        ] {
            exec(&rt, &*d, sql);
        }
        let t = |name: &str| TableRef {
            database: "DEADLOCK".into(),
            table: name.into(),
        };
        let read = |name: &str| rt.block_on(d.structure(&t(name)));
        let s = read("P16_PARENT").unwrap();
        assert_eq!(s.referenced_by.len(), 1);
        let parent = t("P16_PARENT");
        let err = run(&rt, &*d, &structure::truncate_table_sql(&parent, &s)).unwrap_err();
        assert!(err.contains("ORA-02266"), "{err}");
        run(
            &rt,
            &*d,
            &structure::truncate_table_sql(&t("P16_CHILD"), &s),
        )
        .unwrap();
        assert_eq!(count(&rt, &*d, "p16_child"), "0");
        exec(
            &rt,
            &*d,
            "CREATE TRIGGER p16_child_ai AFTER INSERT ON p16_child FOR EACH ROW \
             BEGIN NULL; END;",
        );
        let sql = structure::rename_table_sql(&t("P16_CHILD"), "P16_KID", &s).unwrap();
        exec(&rt, &*d, &sql);
        assert_eq!(read("P16_KID").unwrap().foreign_keys.len(), 1);
        // Oracle rewrites the trigger's source for the new name: the body reads the same.
        let trigger = deadlock_core::objects::ObjectRef {
            schema: "DEADLOCK".into(),
            name: "P16_CHILD_AI".into(),
            kind: deadlock_core::objects::ObjectKind::Trigger,
        };
        let def = rt.block_on(d.object(&trigger)).unwrap();
        match def.props {
            deadlock_core::objects::Properties::Trigger(t) => {
                assert_eq!(t.table, "P16_KID");
                assert_eq!(t.body, "BEGIN NULL; END;");
            }
            other => panic!("{other:?}"),
        }
        assert!(read("P16_CHILD").is_err());
        let err = run(&rt, &*d, &structure::drop_table_sql(&parent, false, &s)).unwrap_err();
        assert!(err.contains("ORA-02449"), "{err}");
        exec(&rt, &*d, &structure::drop_table_sql(&parent, true, &s));
        assert!(read("P16_PARENT").is_err());
        assert!(read("P16_KID").unwrap().foreign_keys.is_empty());
        exec(&rt, &*d, "DROP TABLE p16_kid PURGE");
        exec(&rt, &*d, "PURGE TABLE p16_parent");

        // Statistics gathered while empty say 0 rows after an insert: not known then.
        for sql in [
            "DROP TABLE IF EXISTS p16_est PURGE",
            "CREATE TABLE p16_est (id NUMBER PRIMARY KEY)",
            "BEGIN DBMS_STATS.GATHER_TABLE_STATS(USER, 'P16_EST'); END;",
        ] {
            exec(&rt, &*d, sql);
        }
        assert_eq!(read("P16_EST").unwrap().facts.rows, Some(0));
        exec(&rt, &*d, "INSERT INTO p16_est VALUES (1)");
        exec(&rt, &*d, "COMMIT");
        assert_eq!(
            rows(
                &rt,
                &*d,
                "SELECT TO_CHAR(NUM_ROWS) FROM USER_TABLES WHERE TABLE_NAME = 'P16_EST'"
            ),
            [[Value::Text("0".into())]]
        );
        assert_eq!(read("P16_EST").unwrap().facts.rows, None);
        exec(&rt, &*d, "DROP TABLE p16_est PURGE");
    }

    /// Phase 18: the New table example with an identity column and `comment on`
    /// statements; the quoted lower-case name stays lower case.
    #[test]
    #[ignore = "needs docker compose"]
    fn oracle_create_table() {
        use deadlock_core::edit::TableRef;
        use deadlock_core::structure::{self, TableStructure};
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &plain());
        exec(&rt, &*d, r#"DROP TABLE IF EXISTS "p18_new" PURGE"#);
        let flags = TableStructure {
            oracle: true,
            ..TableStructure::default()
        };
        let t = super::table_structure::p18_table(true);
        let stmts = structure::create_table_sql("DEADLOCK", &t, &flags).unwrap();
        assert_eq!(stmts.len(), 4);
        for sql in &stmts {
            exec(&rt, &*d, sql);
        }
        let s = rt
            .block_on(d.structure(&TableRef {
                database: "DEADLOCK".into(),
                table: "p18_new".into(),
            }))
            .unwrap();
        let names: Vec<&str> = s.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "line", "status", "created", "note"]);
        let c = |n: &str| s.columns.iter().find(|c| c.name == n).unwrap();
        assert!(c("id").extra.starts_with("generated "), "{:?}", c("id"));
        assert!(s.indexes[0].primary);
        assert_eq!(s.indexes[0].parts_label(), "id, line");
        assert_eq!(c("status").default.as_deref(), Some("'new'"));
        assert!(!c("status").nullable && c("note").nullable);
        assert_eq!(c("id").comment, "row id");
        assert_eq!(c("note").comment, "free text");
        assert_eq!(s.facts.comment, "orders");
        exec(&rt, &*d, r#"DROP TABLE "p18_new" PURGE"#);
    }

    /// Phase 17: a foreign key with on delete set null (on update is not written), an
    /// unnamed one, a check that refuses a row; listed after a re-read, then dropped.
    #[test]
    #[ignore = "needs docker compose"]
    fn oracle_keys_and_checks() {
        use deadlock_core::edit::TableRef;
        use deadlock_core::structure;
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &plain());
        for sql in [
            "DROP TABLE IF EXISTS p17_child PURGE",
            "DROP TABLE IF EXISTS p17_parent PURGE",
            "CREATE TABLE p17_parent (a NUMBER, b VARCHAR2(20), u NUMBER UNIQUE, \
             PRIMARY KEY (a, b))",
            "CREATE TABLE p17_child (id NUMBER PRIMARY KEY, pa NUMBER, pb VARCHAR2(20), \
             pu NUMBER, qty NUMBER)",
        ] {
            exec(&rt, &*d, sql);
        }
        let t = |name: &str| TableRef {
            database: "DEADLOCK".into(),
            table: name.into(),
        };
        let read = |name: &str| rt.block_on(d.structure(&t(name))).unwrap();
        let child = t("P17_CHILD");
        let s = read("P17_CHILD");
        let key = structure::NewForeignKey {
            name: "P17_FK".into(),
            columns: vec!["PA".into(), "PB".into()],
            ref_table: t("P17_PARENT"),
            ref_columns: vec!["A".into(), "B".into()],
            on_delete: "set null".into(),
            on_update: "cascade".into(),
        };
        let parent = read("P17_PARENT");
        assert!(structure::foreign_key_hints(&key, &s, &parent).is_empty());
        let sql = structure::add_foreign_key_sql(&child, &key, &s).unwrap();
        assert!(!sql.contains("on update"), "{sql}");
        exec(&rt, &*d, &sql);
        let unnamed = structure::NewForeignKey {
            name: String::new(),
            columns: vec!["PU".into()],
            ref_columns: vec!["U".into()],
            on_delete: "no action".into(),
            ..key.clone()
        };
        assert!(structure::foreign_key_hints(&unnamed, &s, &parent).is_empty());
        exec(
            &rt,
            &*d,
            &structure::add_foreign_key_sql(&child, &unnamed, &s).unwrap(),
        );
        exec(
            &rt,
            &*d,
            &structure::add_check_sql(&child, "P17_CK", "qty >= 0", &s).unwrap(),
        );

        let s = read("P17_CHILD");
        assert_eq!(s.foreign_keys.len(), 2, "{:?}", s.foreign_keys);
        let named = s.foreign_keys.iter().find(|k| k.name == "P17_FK").unwrap();
        assert_eq!(named.from_columns, ["PA", "PB"]);
        assert_eq!(named.on_delete, "set null");
        let other = s.foreign_keys.iter().find(|k| k.name != "P17_FK").unwrap();
        assert!(!other.name.is_empty());
        assert_eq!(other.on_delete, "no action");
        assert_eq!(read("P17_PARENT").referenced_by.len(), 2);
        assert_eq!(s.checks.len(), 1, "{:?}", s.checks);
        assert_eq!(s.checks[0].name, "P17_CK");
        let err = run(&rt, &*d, "INSERT INTO p17_child (id, qty) VALUES (1, -1)").unwrap_err();
        assert!(err.contains("ORA-02290"), "{err}");

        exec(&rt, &*d, &s.drop_constraint_sql(&child, "P17_CK"));
        for k in &s.foreign_keys {
            exec(
                &rt,
                &*d,
                &structure::drop_foreign_key_sql(&child, &k.name, &s),
            );
        }
        let s = read("P17_CHILD");
        assert!(s.foreign_keys.is_empty() && s.checks.is_empty(), "{s:?}");

        // A key into another schema: refused (shown as the server says it) until that
        // schema grants REFERENCES, then listed with its schema.
        let admin = connect_as(&rt, &dba());
        let _ = run(&rt, &*admin, "DROP USER p17_other CASCADE");
        for sql in [
            "CREATE USER p17_other IDENTIFIED BY deadlock QUOTA UNLIMITED ON USERS",
            "CREATE TABLE p17_other.p17_ref (id NUMBER PRIMARY KEY)",
        ] {
            exec(&rt, &*admin, sql);
        }
        let other = TableRef {
            database: "P17_OTHER".into(),
            table: "P17_REF".into(),
        };
        let cross = structure::NewForeignKey {
            name: "P17_CROSS_FK".into(),
            columns: vec!["QTY".into()],
            ref_table: other.clone(),
            ref_columns: vec!["ID".into()],
            on_delete: "no action".into(),
            ..key.clone()
        };
        let sql = structure::add_foreign_key_sql(&child, &cross, &s).unwrap();
        assert!(
            sql.contains(r#"references "P17_OTHER"."P17_REF" ("ID")"#),
            "{sql}"
        );
        exec(&rt, &*d, "DELETE FROM p17_child");
        let err = run(&rt, &*d, &sql).unwrap_err();
        // Without a grant the table is hidden from this user.
        assert!(err.contains("ORA-00942"), "{err}");
        exec(
            &rt,
            &*admin,
            "GRANT REFERENCES ON p17_other.p17_ref TO deadlock",
        );
        exec(&rt, &*d, &sql);
        let s = read("P17_CHILD");
        assert_eq!(s.foreign_keys.len(), 1, "{:?}", s.foreign_keys);
        assert_eq!(s.foreign_keys[0].to, other);
        exec(
            &rt,
            &*d,
            &structure::drop_foreign_key_sql(&child, "P17_CROSS_FK", &s),
        );
        exec(&rt, &*d, "DROP TABLE p17_child PURGE");
        exec(&rt, &*d, "DROP TABLE p17_parent PURGE");
        exec(&rt, &*admin, "DROP USER p17_other CASCADE");
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn oracle_explains() {
        use deadlock_core::plan::Op;
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &plain());
        exec(&rt, &*d, "DROP TABLE IF EXISTS p11_ex PURGE");
        exec(
            &rt,
            &*d,
            "CREATE TABLE p11_ex (id NUMBER PRIMARY KEY, v NUMBER)",
        );
        exec(
            &rt,
            &*d,
            "INSERT INTO p11_ex SELECT LEVEL, MOD(LEVEL, 7) FROM dual CONNECT BY LEVEL <= 1000",
        );
        let explain = |d: &dyn Driver, sql: &str, analyze: bool| {
            rt.block_on(d.explain(sql, analyze))
                .map_err(|e| e.to_string())
        };
        // Estimated, as a plain user; the transaction state is left as it was.
        let p = explain(&*d, "SELECT * FROM p11_ex WHERE id = 5", false).unwrap();
        assert!(!p.measured && p.raw.contains("Plan hash value"));
        assert!(p.walk().iter().any(|(_, n)| n.op == Op::Lookup));
        assert_eq!(p.database.as_deref(), Some("DEADLOCK"));
        assert!(!open(&rt, &*d));
        rt.block_on(d.set_auto_commit(false)).unwrap();
        exec(&rt, &*d, "UPDATE p11_ex SET v = 0 WHERE id = 1");
        explain(&*d, "SELECT COUNT(*) FROM p11_ex", false).unwrap();
        assert!(open(&rt, &*d), "the user's transaction survives");
        rt.block_on(d.end_transaction(false)).unwrap();
        rt.block_on(d.set_auto_commit(true)).unwrap();
        // Measured needs the dictionary: refused before anything runs.
        let e = explain(&*d, "DELETE FROM p11_ex", true).unwrap_err();
        assert!(e.contains("SELECT_CATALOG_ROLE"), "{e}");
        assert_eq!(
            rows(&rt, &*d, "SELECT COUNT(*) FROM p11_ex")[0][0].to_string(),
            "1000"
        );
        // The DBA measures; a write runs and (Auto) commits.
        let admin = connect_as(&rt, &dba());
        let m = explain(
            &*admin,
            "SELECT v, COUNT(*) FROM deadlock.p11_ex GROUP BY v",
            true,
        )
        .unwrap();
        assert!(
            m.measured && m.rows_returned == Some(7.),
            "{:?}",
            m.rows_returned
        );
        assert!(m.raw.contains("A-Rows"));
        let m = explain(
            &*admin,
            "WITH t AS (SELECT * FROM deadlock.p11_ex WHERE v = 1) SELECT COUNT(*) FROM t",
            true,
        )
        .unwrap();
        assert!(m.measured && m.walk().iter().any(|(_, n)| n.actual_rows.is_some()));
        let m = explain(
            &*admin,
            "UPDATE deadlock.p11_ex SET v = 9 WHERE id <= 10",
            true,
        )
        .unwrap();
        assert!(m.walk().iter().any(|(_, n)| n.op == Op::Write));
        assert_eq!(
            rows(&rt, &*d, "SELECT COUNT(*) FROM p11_ex WHERE v = 9")[0][0].to_string(),
            "10"
        );
        assert!(
            explain(&*d, "SELECT * FROM p11_nope", false)
                .unwrap_err()
                .contains("ORA-00942")
        );
        let ro = connect_as(
            &rt,
            &ConnectionConfig {
                read_only: true,
                ..dba()
            },
        );
        assert!(
            explain(&*ro, "DELETE FROM deadlock.p11_ex", true)
                .unwrap_err()
                .contains("read-only")
        );
        exec(&rt, &*d, "DROP TABLE p11_ex PURGE");
    }

    mod locks {
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        use deadlock_core::sessions::{LockKind, Snapshot, State};
        use tokio::sync::oneshot;

        use super::*;

        fn sid(rt: &Runtime, d: &dyn Driver) -> u64 {
            rows(rt, d, "SELECT SYS_CONTEXT('USERENV', 'SID') FROM dual")[0][0]
                .to_string()
                .parse()
                .unwrap()
        }

        fn start(
            rt: &Runtime,
            d: &Arc<dyn Driver>,
            sql: &str,
        ) -> oneshot::Receiver<Result<(), String>> {
            let (d, sql) = (Arc::clone(d), sql.to_owned());
            rt.spawn(async move {
                let s = d.execute(&sql).await.map_err(|e| e.to_string())?;
                s.collect().await.map(drop).map_err(|e| e.to_string())
            })
        }

        fn finish(rt: &Runtime, rx: oneshot::Receiver<Result<(), String>>) -> Result<(), String> {
            rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(20), rx)
                    .await
                    .expect("statement still blocked")
                    .unwrap()
            })
        }

        fn until(rt: &Runtime, d: &dyn Driver, ok: impl Fn(&Snapshot) -> bool) -> Snapshot {
            let end = Instant::now() + Duration::from_secs(10);
            loop {
                let s = rt.block_on(d.sessions()).unwrap();
                if ok(&s) {
                    return s;
                }
                assert!(Instant::now() < end, "timed out: {s:#?}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }

        /// Two plain-user sessions contend for a row; the DBA's screen sees the wait,
        /// cancels the waiter's statement and ends the holder's session.
        #[test]
        #[ignore = "needs docker compose"]
        fn oracle_row_lock_wait_cancel_and_end() {
            let rt = Runtime::new().unwrap();
            let (holder, waiter, screen) = (
                connect_as(&rt, &plain()),
                connect_as(&rt, &plain()),
                connect_as(&rt, &dba()),
            );
            exec(&rt, &*holder, "DROP TABLE IF EXISTS p11_lock PURGE");
            exec(
                &rt,
                &*holder,
                "CREATE TABLE p11_lock (id NUMBER PRIMARY KEY, total NUMBER)",
            );
            exec(&rt, &*holder, "INSERT INTO p11_lock VALUES (1, 10)");
            let (h, w) = (sid(&rt, &*holder), sid(&rt, &*waiter));
            rt.block_on(holder.set_auto_commit(false)).unwrap();
            exec(&rt, &*holder, "UPDATE p11_lock SET total = 11 WHERE id = 1");
            let blocked = start(&rt, &waiter, "UPDATE p11_lock SET total = 12 WHERE id = 1");
            let s = until(&rt, &*screen, |s| s.waits.iter().any(|x| x.waiter == w));
            let wait = s.wait_of(w).unwrap();
            assert_eq!(wait.blocker, Some(h));
            assert_eq!(
                (wait.wants.kind, wait.wants.table.as_str()),
                (LockKind::Row, "P11_LOCK")
            );
            assert!(
                wait.wants.describe().contains("with ROWID = "),
                "{}",
                wait.wants.describe()
            );
            assert_eq!(s.session(w).unwrap().state, State::Waiting);
            assert!(
                s.session(w)
                    .unwrap()
                    .statement
                    .as_deref()
                    .is_some_and(|q| q.contains("total = 12"))
            );
            let holder_row = s.session(h).unwrap();
            assert_eq!(holder_row.state, State::IdleInTransaction);
            assert!(holder_row.transaction.is_some() && holder_row.deadlock);
            assert!(holder_row.holds.iter().any(|l| l.kind == LockKind::Table
                && l.mode == "row exclusive"
                && l.table == "P11_LOCK"));
            assert!(s.sessions.iter().any(|x| x.own));
            assert!(s.own_only.is_none() && s.notes.is_empty(), "{:?}", s.notes);
            // Other tests may wait on locks at the same time: look for this chain only.
            assert!(
                s.chains()
                    .iter()
                    .any(|c| c.levels.first() == Some(&vec![h]))
            );
            assert!(rt.block_on(screen.waiting_sessions()).unwrap() >= 1);
            // Cancel the waiter's statement: it fails, its session stays.
            rt.block_on(screen.cancel_statement(w)).unwrap();
            let e = finish(&rt, blocked).unwrap_err();
            assert!(e.contains("ORA-01013"), "{e}");
            assert_eq!(rows(&rt, &*waiter, "SELECT 1 FROM dual").len(), 1);
            // End the holder: its change rolls back, its connection is gone.
            rt.block_on(screen.end_session(h)).unwrap();
            until(&rt, &*screen, |s| {
                s.session(h).is_none_or(|x| x.transaction.is_none())
            });
            assert!(run(&rt, &*holder, "SELECT 1 FROM dual").is_err());
            assert_eq!(
                rows(&rt, &*waiter, "SELECT total FROM p11_lock")[0][0].to_string(),
                "10"
            );
            let own = sid(&rt, &*screen);
            assert!(
                rt.block_on(screen.end_session(own))
                    .unwrap_err()
                    .to_string()
                    .contains("screen itself")
            );
            assert!(
                rt.block_on(screen.last_deadlock())
                    .unwrap_err()
                    .to_string()
                    .contains("trace file")
            );
            exec(&rt, &*waiter, "DROP TABLE p11_lock PURGE");
        }

        /// A plain user sees their own session only, and is told which privilege shows
        /// the rest.
        #[test]
        #[ignore = "needs docker compose"]
        fn oracle_sessions_without_privilege() {
            let rt = Runtime::new().unwrap();
            let d = connect_as(&rt, &plain());
            let _other = connect_as(&rt, &dba());
            let s = rt.block_on(d.sessions()).unwrap();
            assert_eq!(s.own_only, Some("SELECT ANY DICTIONARY"));
            assert_eq!(s.sessions.len(), 1);
            assert!(s.sessions[0].own && s.sessions[0].user == "DEADLOCK");
            assert!(s.notes[0].contains("SELECT_CATALOG_ROLE"));
            assert_eq!(rt.block_on(d.waiting_sessions()).unwrap(), 0);
            let e = rt.block_on(d.cancel_statement(1)).unwrap_err().to_string();
            assert!(e.contains("SELECT ANY DICTIONARY"), "{e}");
        }
    }

    /// Changes harmless parameters of the test PDB and restores each, memory and spfile.
    #[test]
    #[ignore = "needs docker compose"]
    fn oracle_server_configuration() {
        use deadlock_core::config::{Applies, ConfigChange, Mode, NewValue, Source};
        let rt = Runtime::new().unwrap();
        let d = connect_as(&rt, &dba());
        let load = || rt.block_on(d.config()).unwrap();
        let cfg = load();
        assert!(cfg.oracle && cfg.persist && cfg.read_only.is_none());
        assert!(
            cfg.config_file
                .as_deref()
                .is_some_and(|f| f.ends_with(".ora"))
        );
        assert!(
            cfg.container
                .as_deref()
                .is_some_and(|c| c.contains("FREEPDB1")),
            "{:?}",
            cfg.container
        );
        let p = |c: &deadlock_core::config::ServerConfig, n: &str| c.param(n).cloned().unwrap();
        let adj = p(&cfg, "optimizer_index_cost_adj");
        assert_eq!(
            (adj.value.as_deref(), adj.applies, adj.source.clone()),
            (Some("100"), Applies::Now, Source::Default)
        );
        assert!(adj.description.is_some());
        assert_eq!(p(&cfg, "processes").applies, Applies::Fixed);
        assert!(
            cfg.locked(&p(&cfg, "processes"))
                .unwrap()
                .contains("CDB$ROOT")
        );
        assert_eq!(p(&cfg, "commit_point_strength").applies, Applies::Restart);
        assert!(p(&cfg, "session_cached_cursors").deferred);
        let set = |n: &str, v: &str, mode: Mode| ConfigChange {
            name: n.into(),
            value: NewValue::Set(v.into()),
            mode,
        };
        let reset = |n: &str, mode: Mode| ConfigChange {
            name: n.into(),
            value: NewValue::Default,
            mode,
        };
        let apply = |cfg: &deadlock_core::config::ServerConfig, c: &[ConfigChange]| {
            rt.block_on(d.apply_config(cfg, c)).unwrap()
        };
        // Memory and spfile, then a run-only change, then restart-only and deferred.
        let done = apply(
            &cfg,
            &[
                set("optimizer_index_cost_adj", "50", Mode::Persist),
                set("commit_point_strength", "2", Mode::Persist),
                set("session_cached_cursors", "60", Mode::RunOnly),
            ],
        );
        assert_eq!((done.done, done.error), (3, None));
        let now = load();
        let adj = p(&now, "optimizer_index_cost_adj");
        assert_eq!(
            (adj.value.as_deref(), adj.persisted.as_deref()),
            (Some("50"), Some("50"))
        );
        assert!(matches!(adj.source, Source::Runtime { .. }));
        let cps = p(&now, "commit_point_strength");
        assert_eq!(
            (cps.value.as_deref(), cps.persisted.as_deref()),
            (Some("1"), Some("2"))
        );
        assert!(cps.restart_pending());
        assert_eq!(
            p(&now, "session_cached_cursors").value.as_deref(),
            Some("60")
        );
        // A bad value stops the run; the change before it stays.
        let bad = apply(
            &now,
            &[
                set("optimizer_index_cost_adj", "40", Mode::RunOnly),
                ConfigChange {
                    name: "optimizer_mode".into(),
                    value: NewValue::Set("BOGUS".into()),
                    mode: Mode::Persist,
                },
            ],
        );
        assert_eq!(bad.done, 1);
        assert!(bad.error.unwrap().contains("optimizer_mode"));
        // Restore everything, memory and spfile.
        let done = apply(
            &now,
            &[
                reset("optimizer_index_cost_adj", Mode::Persist),
                reset("commit_point_strength", Mode::Persist),
                reset("session_cached_cursors", Mode::Persist),
            ],
        );
        assert_eq!((done.done, done.error), (3, None));
        let back = load();
        for (n, v) in [
            ("optimizer_index_cost_adj", "100"),
            ("commit_point_strength", "1"),
            ("session_cached_cursors", "50"),
        ] {
            let x = p(&back, n);
            assert_eq!(
                (x.value.as_deref(), x.persisted.as_deref()),
                (Some(v), None),
                "{n}"
            );
        }
        // A plain user: told what is missing, not an ORA- error.
        let plain = connect_as(&rt, &plain());
        let e = rt.block_on(plain.config()).unwrap_err().to_string();
        assert!(
            e.contains("SELECT ANY DICTIONARY") && !e.contains("ORA-"),
            "{e}"
        );
        let ro = connect_as(
            &rt,
            &ConnectionConfig {
                read_only: true,
                ..dba()
            },
        );
        assert!(
            rt.block_on(ro.apply_config(
                &back,
                &[set("optimizer_index_cost_adj", "50", Mode::RunOnly)]
            ))
            .is_err()
        );
    }

    /// A user with CREATE SESSION only, and SELECT on one table of another schema:
    /// every screen answers with what it can, or names the missing privilege.
    #[test]
    #[ignore = "needs docker compose"]
    fn oracle_minimal_user_on_every_screen() {
        use deadlock_core::driver::ObjectPath;
        use deadlock_core::edit::TableRef;
        let rt = Runtime::new().unwrap();
        let admin = connect_as(&rt, &dba());
        let owner = connect_as(&rt, &plain());
        let _ = run(&rt, &*admin, "DROP USER p11_min CASCADE");
        exec(&rt, &*admin, "CREATE USER p11_min IDENTIFIED BY deadlock");
        exec(&rt, &*admin, "GRANT CREATE SESSION TO p11_min");
        exec(&rt, &*owner, "DROP TABLE IF EXISTS p11_shared PURGE");
        exec(
            &rt,
            &*owner,
            "CREATE TABLE p11_shared (id NUMBER PRIMARY KEY, v VARCHAR2(10))",
        );
        exec(&rt, &*owner, "INSERT INTO p11_shared VALUES (1, 'a')");
        exec(
            &rt,
            &*owner,
            "GRANT SELECT, UPDATE ON p11_shared TO p11_min",
        );
        let d = connect_as(
            &rt,
            &ConnectionConfig {
                user: "p11_min".into(),
                ..plain()
            },
        );
        // Schema browser: their own (empty) schema first, the shared table under its owner.
        let schemas = rt.block_on(d.children(&ObjectPath(vec![]))).unwrap();
        assert_eq!(schemas[0].name, "P11_MIN");
        let groups = rt
            .block_on(d.children(&ObjectPath(vec!["DEADLOCK".into()])))
            .unwrap();
        let tables = groups[0].children.as_ref().unwrap();
        assert_eq!(
            tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["P11_SHARED"]
        );
        // Data editor and structure on the granted table.
        let t = TableRef {
            database: "DEADLOCK".into(),
            table: "P11_SHARED".into(),
        };
        let info = rt.block_on(d.table_info(&t)).unwrap();
        assert_eq!(info.key, Some(vec![0]));
        let s = rt.block_on(d.structure(&t)).unwrap();
        assert_eq!(s.columns.len(), 2);
        assert!(
            s.facts.size().is_none(),
            "no segment views for another user's table"
        );
        assert!(
            s.ddl.starts_with("-- Oracle did not return the DDL"),
            "{}",
            s.ddl
        );
        // Explain estimated works; measured names the privilege.
        assert!(
            rt.block_on(d.explain("SELECT * FROM deadlock.p11_shared", false))
                .is_ok()
        );
        let e = rt
            .block_on(d.explain("SELECT * FROM deadlock.p11_shared", true))
            .unwrap_err()
            .to_string();
        assert!(e.contains("SELECT_CATALOG_ROLE"), "{e}");
        // Sessions: own only. Configuration: the privilege, not an ORA- error. Stop: same.
        let snap = rt.block_on(d.sessions()).unwrap();
        assert!(snap.own_only.is_some() && snap.sessions.len() == 1);
        let e = rt.block_on(d.config()).unwrap_err().to_string();
        assert!(e.contains("SELECT ANY DICTIONARY"), "{e}");
        // Thick breaks the call on its own session, which needs no privilege.
        if thick() {
            rt.block_on(d.cancel()).unwrap();
        } else {
            let e = rt.block_on(d.cancel()).unwrap_err().to_string();
            assert!(e.contains("ALTER SYSTEM privilege"), "{e}");
        }
        drop(d);
        exec(&rt, &*owner, "DROP TABLE p11_shared PURGE");
        exec(&rt, &*admin, "DROP USER p11_min CASCADE");
    }

    /// By SID instead of service name: the container database's SID `FREE`.
    #[test]
    #[ignore = "needs docker compose"]
    fn oracle_connects_by_sid() {
        let rt = Runtime::new().unwrap();
        let d = connect_as(
            &rt,
            &ConnectionConfig {
                sid: true,
                database: "FREE".into(),
                ..dba()
            },
        );
        let got = rows(
            &rt,
            &*d,
            "SELECT SYS_CONTEXT('USERENV', 'CON_NAME') FROM dual",
        );
        assert_eq!(got[0][0].to_string(), "CDB$ROOT");
    }
}

/// The diagram's bulk load: tables only, key flags, a composite foreign key with its
/// column pairs in order, and a self-reference.
fn loads_diagram(
    engine: Engine,
    port: u16,
    database: &str,
    schema: &str,
    setup: &[&str],
    drop: &[&str],
) {
    let rt = Runtime::new().unwrap();
    let driver = rt
        .block_on(connect(&config(engine, port, database), PASSWORD))
        .unwrap();
    for sql in drop {
        let _ = run(&rt, &*driver, sql);
    }
    for sql in setup {
        rows(&rt, &*driver, sql);
    }
    let d = rt.block_on(driver.diagram(schema)).unwrap();
    for sql in drop {
        let _ = run(&rt, &*driver, sql);
    }
    let name = |n: &str| {
        if engine == Engine::Oracle {
            n.to_uppercase()
        } else {
            n.to_owned()
        }
    };
    let table = |n: &str| d.tables.iter().find(|t| t.name == name(n)).unwrap();
    assert!(
        d.tables.iter().all(|t| t.name != name("p13_v")),
        "views are left out"
    );
    let parent = table("p13_parent");
    assert!(parent.columns[0].primary_key && parent.columns[1].primary_key);
    let child = table("p13_child");
    let col = |n: &str| child.columns.iter().find(|c| c.name == name(n)).unwrap();
    assert!(col("a").foreign_key && col("b").foreign_key && !col("note").foreign_key);
    assert!(col("id").primary_key && col("parent_id").foreign_key);
    let links: Vec<_> = d
        .links
        .iter()
        .filter(|l| l.from == name("p13_child"))
        .collect();
    assert_eq!(links.len(), 2, "{links:?}");
    let composite = links.iter().find(|l| l.to == name("p13_parent")).unwrap();
    assert_eq!(composite.from_columns, [name("a"), name("b")]);
    assert_eq!(composite.to_columns, [name("x"), name("y")]);
    let selfref = links.iter().find(|l| l.to == name("p13_child")).unwrap();
    assert_eq!(
        (selfref.from_columns.clone(), selfref.to_columns.clone()),
        (vec![name("parent_id")], vec![name("id")])
    );
}

const P13_DROP: [&str; 3] = [
    "DROP VIEW p13_v",
    "DROP TABLE p13_child",
    "DROP TABLE p13_parent",
];

fn mysql_diagram(port: u16) {
    loads_diagram(
        Engine::Mysql,
        port,
        "deadlock",
        "deadlock",
        &[
            "CREATE TABLE p13_parent (x INT, y INT, PRIMARY KEY (x, y))",
            "CREATE TABLE p13_child (id INT PRIMARY KEY, parent_id INT, b INT, a INT, note TEXT, \
             FOREIGN KEY (a, b) REFERENCES p13_parent (x, y), \
             FOREIGN KEY (parent_id) REFERENCES p13_child (id))",
            "CREATE VIEW p13_v AS SELECT id FROM p13_child",
        ],
        &P13_DROP,
    );
}

#[test]
#[ignore = "needs docker compose"]
fn mysql_loads_diagram() {
    mysql_diagram(3306);
}

#[test]
#[ignore = "needs docker compose"]
fn mariadb_loads_diagram() {
    mysql_diagram(3307);
}

#[test]
#[ignore = "needs docker compose"]
fn oracle_loads_diagram() {
    loads_diagram(
        Engine::Oracle,
        1521,
        "FREEPDB1",
        "DEADLOCK",
        &[
            "CREATE TABLE p13_parent (x NUMBER, y NUMBER, PRIMARY KEY (x, y))",
            "CREATE TABLE p13_child (id NUMBER PRIMARY KEY, parent_id NUMBER, b NUMBER, a NUMBER, \
             note VARCHAR2(20), FOREIGN KEY (a, b) REFERENCES p13_parent (x, y), \
             FOREIGN KEY (parent_id) REFERENCES p13_child (id))",
            "CREATE VIEW p13_v AS SELECT id FROM p13_child",
        ],
        &P13_DROP,
    );
}

/// Exports `select` to every format, checks each file, then re-imports the SQL INSERT
/// file into `copy` and compares the rows.
fn export_round_trip(
    cfg: &ConnectionConfig,
    setup: &[&str],
    select: &str,
    copy: &str,
    drop: &[&str],
) {
    use deadlock_core::export::{self, Dialect, Format, Spec};
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    let rt = Runtime::new().unwrap();
    let driver = connect_as(&rt, cfg);
    for s in drop {
        let _ = run(&rt, &*driver, s);
    }
    for s in setup {
        run(&rt, &*driver, s).unwrap();
    }
    let dialect = if cfg.engine == Engine::Oracle {
        Dialect::Oracle
    } else {
        Dialect::Mysql {
            backslash_escapes: true,
        }
    };
    let dir = tempfile::tempdir().unwrap();
    let export = |format: Format| {
        let path = dir.path().join(format!("out.{}", format.extension()));
        let spec = Spec {
            format,
            dialect,
            table: copy.into(),
            keep: None,
        };
        let progress = Arc::new(AtomicU64::new(0));
        let (_keep, cancel) = tokio::sync::oneshot::channel();
        let n = rt
            .block_on(export::run(
                Arc::clone(&driver),
                select.into(),
                Vec::new(),
                spec,
                path.clone(),
                Arc::clone(&progress),
                cancel,
            ))
            .unwrap();
        assert_eq!(n, 3);
        std::fs::read_to_string(path).unwrap()
    };
    let csv = export(Format::Csv);
    assert_eq!(csv.matches("\r\n").count(), 4, "{csv}");
    assert!(csv.contains("\"it's \"\"q\"\"\nnext\""), "{csv}");
    let json: serde_json::Value = serde_json::from_str(&export(Format::Json)).unwrap();
    assert_eq!(json.as_array().unwrap().len(), 3);
    let lines = export(Format::JsonLines);
    assert_eq!(lines.lines().count(), 3);
    assert!(export(Format::Tsv).contains('\t'));
    let sql = export(Format::SqlInsert);
    for range in deadlock_core::split::split(&sql, cfg.engine) {
        let stmt = sql[range].trim().trim_end_matches(';');
        run(&rt, &*driver, stmt).unwrap();
    }
    let order = |t: &str| select.replace("p14_src", t);
    assert_eq!(
        rows(&rt, &*driver, &order("p14_src")),
        rows(&rt, &*driver, &order(copy))
    );
    for s in drop {
        run(&rt, &*driver, s).unwrap();
    }
}

#[test]
#[ignore = "needs docker compose"]
fn mysql_exports_every_format_and_reimports() {
    let table = |name: &str| {
        format!(
            "CREATE TABLE {name} (id INT PRIMARY KEY, name VARCHAR(50), amount DECIMAL(30,4), \
             big BIGINT UNSIGNED, f DOUBLE, d DATE, dt DATETIME(3), t TIME, b VARBINARY(8), n INT)"
        )
    };
    let (src, copy) = (table("p14_src"), table("p14_copy"));
    export_round_trip(
        &config(Engine::Mysql, 3306, "deadlock"),
        &[
            &src,
            &copy,
            "INSERT INTO p14_src VALUES \
             (1, 'it''s \"q\"\nnext', 12345678901234567890.1234, 18446744073709551615, 0.1, \
              '2024-05-01', '2024-05-01 13:45:00.250', '-838:59:59', x'DEAD', NULL), \
             (2, 'back\\\\slash, comma', -1.5, 0, 1e300, NULL, NULL, '12:00:00', x'', NULL), \
             (3, '', NULL, NULL, NULL, NULL, NULL, NULL, NULL, 7)",
        ],
        "SELECT * FROM p14_src ORDER BY id",
        "p14_copy",
        &[
            "DROP TABLE IF EXISTS p14_src",
            "DROP TABLE IF EXISTS p14_copy",
        ],
    );
}

#[test]
#[ignore = "needs docker compose"]
fn oracle_exports_every_format_and_reimports() {
    let table = |name: &str| {
        format!(
            "CREATE TABLE {name} (id NUMBER PRIMARY KEY, name VARCHAR2(50), amount NUMBER(30,4), \
             f BINARY_DOUBLE, d DATE, ts TIMESTAMP(3), b RAW(8), n NUMBER)"
        )
    };
    let (src, copy) = (table("p14_src"), table("P14_COPY"));
    let drop = [
        "BEGIN EXECUTE IMMEDIATE 'DROP TABLE p14_src'; EXCEPTION WHEN OTHERS THEN NULL; END;",
        "BEGIN EXECUTE IMMEDIATE 'DROP TABLE p14_copy'; EXCEPTION WHEN OTHERS THEN NULL; END;",
    ];
    export_round_trip(
        &config(Engine::Oracle, 1521, "FREEPDB1"),
        &[
            &src,
            &copy,
            "INSERT INTO p14_src VALUES (1, 'it''s \"q\"' || CHR(10) || 'next', \
             12345678901234567890.1234, 0.1, DATE '2024-05-01', \
             TIMESTAMP '2024-05-01 13:45:00.250', HEXTORAW('DEAD'), NULL)",
            "INSERT INTO p14_src VALUES (2, 'back\\slash, comma', -1.5, 1e300d, NULL, NULL, NULL, NULL)",
            "INSERT INTO p14_src VALUES (3, NULL, NULL, NULL, NULL, NULL, NULL, 7)",
            "COMMIT",
        ],
        "SELECT * FROM p14_src ORDER BY id",
        "P14_COPY",
        &drop,
    );
}

#[test]
#[ignore = "needs docker compose"]
fn mysql_export_cancels_and_leaves_no_file() {
    use deadlock_core::export::{self, Dialect, Format, Spec};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    let rt = Runtime::new().unwrap();
    let driver = connect_as(&rt, &config(Engine::Mysql, 3306, "deadlock"));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.csv");
    let progress = Arc::new(AtomicU64::new(0));
    let (stop, cancel) = tokio::sync::oneshot::channel();
    let job = rt.spawn(export::run(
        Arc::clone(&driver),
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000) \
         SELECT a.i, b.i, REPEAT('x', 100) FROM n a, n b"
            .into(),
        Vec::new(),
        Spec {
            format: Format::Csv,
            dialect: Dialect::Mysql {
                backslash_escapes: true,
            },
            table: "t".into(),
            keep: None,
        },
        path.clone(),
        Arc::clone(&progress),
        cancel,
    ));
    while progress.load(Ordering::Relaxed) < 10_000 {
        std::thread::sleep(Duration::from_millis(10));
    }
    stop.send(()).unwrap();
    let started = Instant::now();
    let out = rt.block_on(job).unwrap();
    assert_eq!(out, Err("cancelled".into()));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    // The session is free again soon: the statement was killed, not read to its end.
    assert_eq!(rows(&rt, &*driver, "SELECT 1")[0][0], Value::Int(1));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

mod tunnel {
    use super::*;
    use deadlock_core::ssh::{self, KnownHosts, SshAuth, SshConfig, TunnelError};

    fn fixture(name: &str) -> String {
        format!("{}/tests/fixtures/ssh/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    fn ssh_config(auth: SshAuth, key: &str) -> SshConfig {
        SshConfig {
            host: "127.0.0.1".into(),
            port: 2222,
            user: "deadlock".into(),
            auth,
            key_file: if key.is_empty() {
                String::new()
            } else {
                fixture(key)
            },
        }
    }

    /// Known-hosts files in a fresh folder; the server's key is trusted first unless
    /// `trusted` is off.
    fn known(rt: &Runtime, dir: &tempfile::TempDir, trusted: bool) -> KnownHosts {
        let known = KnownHosts {
            user: dir.path().join("user"),
            app: dir.path().join("app"),
        };
        if trusted {
            let err = rt
                .block_on(ssh::open(
                    &ssh_config(SshAuth::Password, ""),
                    "deadlock",
                    "mysql",
                    3306,
                    known.clone(),
                ))
                .err()
                .unwrap();
            let TunnelError::UnknownHost(key) = err else {
                panic!("{err}")
            };
            assert!(
                key.fingerprint.starts_with("SHA256:"),
                "{}",
                key.fingerprint
            );
            ssh::trust(&key, &known).unwrap();
        }
        known
    }

    fn query_through(
        rt: &Runtime,
        cfg: &SshConfig,
        secret: &str,
        known: KnownHosts,
    ) -> Result<Value, String> {
        let tunnel = rt
            .block_on(ssh::open(cfg, secret, "mysql", 3306, known))
            .map_err(|e| e.to_string())?;
        let db = config(Engine::Mysql, tunnel.local.port(), "deadlock");
        let driver = rt
            .block_on(connect(&db, PASSWORD))
            .map_err(|e| e.to_string())?;
        let v = rows(rt, &*driver, "SELECT @@hostname IS NOT NULL")[0][0].clone();
        drop(driver);
        drop(tunnel);
        Ok(v)
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn tunnels_to_mysql_with_each_sign_in() {
        let rt = Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let known = known(&rt, &dir, true);
        let one = Value::Int(1);
        assert_eq!(
            query_through(
                &rt,
                &ssh_config(SshAuth::Key, "id_ed25519"),
                "",
                known.clone()
            ),
            Ok(one.clone())
        );
        assert_eq!(
            query_through(
                &rt,
                &ssh_config(SshAuth::Key, "id_ed25519_pass"),
                "deadlock",
                known.clone()
            ),
            Ok(one.clone())
        );
        assert_eq!(
            query_through(
                &rt,
                &ssh_config(SshAuth::Password, ""),
                "deadlock",
                known.clone()
            ),
            Ok(one)
        );
        // The user's own known_hosts was never created.
        assert!(!known.user.exists());
    }

    /// Oracle through the tunnel, on the driver the run picked (Thin, or Thick with
    /// `DEADLOCK_TEST_ORACLE=thick`).
    #[test]
    #[ignore = "needs docker compose"]
    fn tunnels_to_oracle() {
        let rt = Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let known = known(&rt, &dir, true);
        let tunnel = rt
            .block_on(ssh::open(
                &ssh_config(SshAuth::Key, "id_ed25519"),
                "",
                "oracle",
                1521,
                known,
            ))
            .unwrap();
        let db = config(Engine::Oracle, tunnel.local.port(), "FREEPDB1");
        let driver = rt.block_on(connect(&db, PASSWORD)).unwrap();
        assert_eq!(
            rows(&rt, &*driver, "SELECT 1 FROM dual"),
            [[Value::Decimal("1".into())]]
        );
        drop(driver);
        drop(tunnel);
    }

    /// Needs an agent holding `fixtures/ssh/id_ed25519`, so it runs only when asked:
    /// `eval $(ssh-agent) && ssh-add tests/fixtures/ssh/id_ed25519 &&
    /// DEADLOCK_TEST_AGENT=1 cargo test … tunnels_with_the_ssh_agent -- --ignored`.
    #[test]
    #[ignore = "needs docker compose and a test SSH agent"]
    fn tunnels_with_the_ssh_agent() {
        if std::env::var_os("DEADLOCK_TEST_AGENT").is_none() {
            eprintln!("skipped: DEADLOCK_TEST_AGENT is not set");
            return;
        }
        let rt = Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let known = known(&rt, &dir, true);
        assert_eq!(
            query_through(&rt, &ssh_config(SshAuth::Agent, ""), "", known),
            Ok(Value::Int(1))
        );
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn refuses_wrong_credentials_and_says_which_hop() {
        let rt = Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let known = known(&rt, &dir, true);
        let wrong = query_through(
            &rt,
            &ssh_config(SshAuth::Key, "wrong_ed25519"),
            "",
            known.clone(),
        )
        .unwrap_err();
        assert!(
            wrong.contains("SSH server 127.0.0.1:2222") && wrong.contains("refused the sign-in"),
            "{wrong}"
        );
        let pass = query_through(
            &rt,
            &ssh_config(SshAuth::Key, "id_ed25519_pass"),
            "nope",
            known.clone(),
        )
        .unwrap_err();
        assert!(pass.contains("passphrase"), "{pass}");
        let pw = query_through(
            &rt,
            &ssh_config(SshAuth::Password, ""),
            "nope",
            known.clone(),
        )
        .unwrap_err();
        assert!(pw.contains("refused the sign-in"), "{pw}");
        // The SSH hop works, the database host behind it does not exist.
        let t = rt
            .block_on(ssh::open(
                &ssh_config(SshAuth::Password, ""),
                "deadlock",
                "no-such-db",
                3306,
                known,
            ))
            .unwrap();
        let db = config(Engine::Mysql, t.local.port(), "deadlock");
        assert!(rt.block_on(connect(&db, PASSWORD)).is_err());
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn unknown_and_changed_host_keys_are_refused() {
        let rt = Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let known = known(&rt, &dir, false);
        let cfg = ssh_config(SshAuth::Password, "");
        let err = query_through(&rt, &cfg, "deadlock", known.clone()).unwrap_err();
        assert!(
            err.contains("is not known") && err.contains("SHA256:"),
            "{err}"
        );
        // Another key recorded for the server in the user's file: changed, refused.
        std::fs::write(
            &known.user,
            "[127.0.0.1]:2222 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICFru5zMP1qFOlqDaKeDD0Cl39iOXzVn4f0/XZVqC4qv\n",
        )
        .unwrap();
        let err = rt
            .block_on(ssh::open(&cfg, "deadlock", "mysql", 3306, known.clone()))
            .err()
            .unwrap();
        assert!(matches!(err, TunnelError::ChangedHost(_)), "{err}");
        assert!(err.to_string().contains("CHANGED"), "{err}");
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn a_dropped_tunnel_is_a_lost_connection() {
        let rt = Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let known = known(&rt, &dir, true);
        let tunnel = rt
            .block_on(ssh::open(
                &ssh_config(SshAuth::Password, ""),
                "deadlock",
                "mysql",
                3306,
                known,
            ))
            .unwrap();
        let driver = rt
            .block_on(connect(
                &config(Engine::Mysql, tunnel.local.port(), "deadlock"),
                PASSWORD,
            ))
            .unwrap();
        assert_eq!(rows(&rt, &*driver, "SELECT 1")[0][0], Value::Int(1));
        drop(tunnel);
        assert!(run(&rt, &*driver, "SELECT 1").is_err());
        assert!(rt.block_on(driver.transaction_open()).is_err());
    }
}

/// Exports `select` (over `p14i_src`) to CSV, imports the file into `copy` with
/// `import::run` on a session of its own, and compares every row. Then an import
/// whose third row fails leaves `copy` as it was.
fn import_round_trip(
    cfg: &ConnectionConfig,
    setup: &[&str],
    select: &str,
    copy: deadlock_core::edit::TableRef,
    drop: &[&str],
) {
    use deadlock_core::export::{self, Dialect, Format, Spec};
    use deadlock_core::import::{self, Job, Mapping, Options, Reader};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64};

    let rt = Runtime::new().unwrap();
    let driver = connect_as(&rt, cfg);
    for s in drop {
        let _ = run(&rt, &*driver, s);
    }
    for s in setup {
        run(&rt, &*driver, s).unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("src.csv");
    let (_keep, cancel) = tokio::sync::oneshot::channel();
    let spec = Spec {
        format: Format::Csv,
        dialect: Dialect::Mysql {
            backslash_escapes: true,
        },
        table: String::new(),
        keep: None,
    };
    let n = rt
        .block_on(export::run(
            Arc::clone(&driver),
            select.into(),
            Vec::new(),
            spec,
            path.clone(),
            Arc::new(AtomicU64::new(0)),
            cancel,
        ))
        .unwrap();
    assert_eq!(n, 3);

    let info = rt.block_on(driver.table_info(&copy)).unwrap();
    let options = Options {
        separator: b',',
        encoding: encoding_rs::UTF_8,
        header: true,
        empty_is_null: true,
    };
    let header = |path: &std::path::Path| {
        let mut rec = Vec::new();
        let mut r = Reader::new(std::fs::File::open(path).unwrap(), b',', encoding_rs::UTF_8);
        assert!(r.record(&mut rec).unwrap());
        rec.into_iter().map(|f| f.text).collect::<Vec<_>>()
    };
    let map = Mapping::auto(header(&path), &info, true);
    assert!(map.targets.iter().all(Option::is_some), "{map:?}");
    let checked = import::check(
        &path,
        &options,
        &info,
        &map,
        &AtomicU64::new(0),
        &AtomicBool::new(false),
    )
    .unwrap()
    .unwrap();
    assert_eq!((checked.rows, checked.bad), (3, 0), "{checked:?}");
    let job = |path: std::path::PathBuf| Job {
        path,
        options: options.clone(),
        table: copy.clone(),
        info: info.clone(),
        map: map.clone(),
        skip_bad: false,
    };
    let import = |job: Job| {
        let fresh = connect_as(&rt, cfg);
        let (_keep, cancel) = tokio::sync::oneshot::channel();
        rt.block_on(import::run(fresh, job, Arc::new(AtomicU64::new(0)), cancel))
    };
    assert_eq!(import(job(path.clone())), Ok(3));
    let copy_select = select.replace("p14i_src", &copy.table);
    assert_eq!(
        rows(&rt, &*driver, select),
        rows(&rt, &*driver, &copy_select)
    );

    // Row 3 repeats row 1's key: nothing of the file stays.
    let csv = std::fs::read_to_string(&path).unwrap();
    let head = csv.split("\r\n").next().unwrap();
    // The last record (id 3) has no line break inside.
    let simple = csv.trim_end().rsplit("\r\n").next().unwrap();
    let bad = dir.path().join("bad.csv");
    let id_first = |l: &str, id: u32| format!("{id}{}", &l[l.find(',').unwrap()..]);
    std::fs::write(
        &bad,
        format!(
            "{head}\r\n{}\r\n{}\r\n{}\r\n",
            id_first(simple, 100),
            id_first(simple, 101),
            id_first(simple, 100)
        ),
    )
    .unwrap();
    let e = import(job(bad)).unwrap_err();
    assert!(e.starts_with("row 3: "), "{e}");
    assert_eq!(count(&rt, &*driver, &copy.table), "3");
    for s in drop {
        run(&rt, &*driver, s).unwrap();
    }
}

#[test]
#[ignore = "needs docker compose"]
fn mysql_imports_its_own_export() {
    mysql_like_import(3306);
}

#[test]
#[ignore = "needs docker compose"]
fn mariadb_imports_its_own_export() {
    mysql_like_import(3307);
}

fn mysql_like_import(port: u16) {
    let table = |name: &str| {
        format!(
            "CREATE TABLE {name} (id INT PRIMARY KEY, name VARCHAR(50), amount DECIMAL(30,4), \
             big BIGINT UNSIGNED, f DOUBLE, d DATE, dt DATETIME(3), t TIME, b VARBINARY(8), \
             n INT, e ENUM('a','b'), j JSON)"
        )
    };
    let engine = if port == 3306 {
        Engine::Mysql
    } else {
        Engine::Mariadb
    };
    let (src, copy) = (table("p14i_src"), table("p14i_copy"));
    import_round_trip(
        &config(engine, port, "deadlock"),
        &[
            &src,
            &copy,
            "INSERT INTO p14i_src VALUES \
             (1, 'it''s \"q\"\nnext, čšž', 12345678901234567890.1234, 18446744073709551615, 0.1, \
              '2024-05-01', '2024-05-01 13:45:00.250', '-838:59:59', x'DEAD', NULL, 'a', \
              '{\"k\": [1, 2]}'), \
             (2, 'back\\\\slash; semi', -1.5, 0, 1e300, NULL, NULL, '12:00:00', x'', NULL, NULL, \
              NULL), \
             (3, '', NULL, NULL, NULL, NULL, NULL, NULL, NULL, 7, 'b', NULL)",
        ],
        "SELECT * FROM p14i_src ORDER BY id",
        deadlock_core::edit::TableRef {
            database: "deadlock".into(),
            table: "p14i_copy".into(),
        },
        &[
            "DROP TABLE IF EXISTS p14i_src",
            "DROP TABLE IF EXISTS p14i_copy",
        ],
    );
}

#[test]
#[ignore = "needs docker compose"]
fn oracle_imports_its_own_export() {
    let table = |name: &str| {
        format!(
            "CREATE TABLE {name} (ID NUMBER PRIMARY KEY, NAME VARCHAR2(50), AMOUNT NUMBER(30,4), \
             F BINARY_DOUBLE, D DATE, TS TIMESTAMP(3), B RAW(8), N NUMBER)"
        )
    };
    let drop = [
        "BEGIN EXECUTE IMMEDIATE 'DROP TABLE p14i_src'; EXCEPTION WHEN OTHERS THEN NULL; END;",
        "BEGIN EXECUTE IMMEDIATE 'DROP TABLE p14i_copy'; EXCEPTION WHEN OTHERS THEN NULL; END;",
    ];
    import_round_trip(
        &config(Engine::Oracle, 1521, "FREEPDB1"),
        &[
            &table("p14i_src"),
            &table("p14i_copy"),
            "INSERT INTO p14i_src VALUES (1, 'it''s \"q\"' || CHR(10) || 'next, čšž', \
             12345678901234567890.1234, 0.1, DATE '2024-05-01', \
             TIMESTAMP '2024-05-01 13:45:00.250', HEXTORAW('DEAD'), NULL)",
            "INSERT INTO p14i_src VALUES (2, 'back\\slash; semi', -1.5, 1e300d, NULL, NULL, NULL, \
             NULL)",
            "INSERT INTO p14i_src VALUES (3, NULL, NULL, NULL, NULL, NULL, NULL, 7)",
            "COMMIT",
        ],
        "SELECT * FROM p14i_src ORDER BY id",
        deadlock_core::edit::TableRef {
            database: "DEADLOCK".into(),
            table: "P14I_COPY".into(),
        },
        &drop,
    );
}

/// Search everywhere finds `ps_` objects by name, a column, and source text, with a
/// LIKE wildcard in the text matched literally.
fn searches_everywhere(engine: Engine, port: u16) {
    use deadlock_core::search::Group;
    use std::sync::atomic::AtomicBool;
    let rt = Runtime::new().unwrap();
    let driver = rt
        .block_on(connect(&config(engine, port, "deadlock"), PASSWORD))
        .unwrap();
    for sql in [
        "DROP VIEW IF EXISTS ps_view",
        "DROP PROCEDURE IF EXISTS ps_proc",
        "DROP TABLE IF EXISTS ps_stock",
        "CREATE TABLE ps_stock (id INT PRIMARY KEY, ps_qty_left INT)",
        "CREATE VIEW ps_view AS SELECT id AS ps_view_marker FROM ps_stock",
        "CREATE PROCEDURE ps_proc() BEGIN\n  SELECT 1;\n  UPDATE ps_stock SET ps_qty_left = 0 WHERE id = 42;\nEND",
    ] {
        rows(&rt, &*driver, sql);
    }
    // MySQL with its binary log needs SUPER for triggers; MariaDB lets the owner.
    let trigger = run(
        &rt,
        &*driver,
        "CREATE TRIGGER ps_trig BEFORE INSERT ON ps_stock FOR EACH ROW SET NEW.ps_qty_left = 7",
    )
    .is_ok();
    let stop = AtomicBool::new(false);
    let search = |text: &str| {
        rt.block_on(driver.search_everywhere(text, &[], &stop))
            .unwrap()
    };

    let hits = search("PS_STOCK");
    assert!(
        hits.iter()
            .any(|h| h.group == Group::Objects && h.name == "ps_stock" && h.kind == "table")
    );
    let code = hits
        .iter()
        .find(|h| h.group == Group::Code && h.name == "ps_proc")
        .unwrap();
    assert_eq!(code.kind, "procedure");
    let line = code.line.as_ref().unwrap();
    assert!(line.text.contains("UPDATE ps_stock"), "{line:?}");

    let hits = search("qty_left");
    let c = hits.iter().find(|h| h.group == Group::Columns).unwrap();
    assert_eq!(
        (c.name.as_str(), c.column.as_ref().unwrap().0.as_str()),
        ("ps_stock", "ps_qty_left")
    );
    assert!(c.column.as_ref().unwrap().1.starts_with("int"));

    // `_` is no wildcard: `ps_view_marker` is in the view, `psXview` matches nothing.
    assert!(
        search("view_marker")
            .iter()
            .any(|h| h.group == Group::Code && h.kind == "view")
    );
    assert!(search("ps%view").is_empty());

    // A hidden schema is neither scanned nor listed: objects, columns and code.
    for text in ["ps_stock", "qty_left", "ps_qty_left = 0"] {
        let hidden = ["deadlock".to_owned()];
        let hits = rt
            .block_on(driver.search_everywhere(text, &hidden, &stop))
            .unwrap();
        assert!(hits.iter().all(|h| h.schema != "deadlock"), "{hits:?}");
    }

    let def = rt
        .block_on(driver.definition("deadlock", "ps_proc", "procedure"))
        .unwrap();
    assert!(def.starts_with("CREATE") && def.contains("ps_qty_left = 0"));
    if trigger {
        let trig = search("ps_qty_left = 7");
        assert!(
            trig.iter()
                .any(|h| h.group == Group::Code && h.kind == "trigger"),
            "{trig:?}"
        );
        let def = rt
            .block_on(driver.definition("deadlock", "ps_trig", "trigger"))
            .unwrap();
        assert!(
            def.starts_with("CREATE") && def.contains("ps_qty_left = 7"),
            "{def}"
        );
    }
    let (l, _) = deadlock_core::search::locate(&def, line, "ps_stock").unwrap();
    assert!(def.lines().nth(l).unwrap().contains("UPDATE ps_stock"));

    // A newer search stops this one before its first query.
    let stopped = AtomicBool::new(true);
    assert!(
        rt.block_on(driver.search_everywhere("ps_", &[], &stopped))
            .unwrap()
            .is_empty()
    );
    for sql in [
        "DROP VIEW ps_view",
        "DROP PROCEDURE ps_proc",
        "DROP TABLE ps_stock",
    ] {
        rows(&rt, &*driver, sql);
    }
}

#[test]
#[ignore = "needs docker compose"]
fn mysql_searches_everywhere() {
    searches_everywhere(Engine::Mysql, 3306);
}

#[test]
#[ignore = "needs docker compose"]
fn mariadb_searches_everywhere() {
    searches_everywhere(Engine::Mariadb, 3307);
}

#[test]
#[ignore = "needs docker compose"]
fn oracle_searches_everywhere() {
    use deadlock_core::search::Group;
    use std::sync::atomic::AtomicBool;
    let rt = Runtime::new().unwrap();
    let driver = rt
        .block_on(connect(&config(Engine::Oracle, 1521, "FREEPDB1"), PASSWORD))
        .unwrap();
    for sql in [
        "BEGIN EXECUTE IMMEDIATE 'DROP TABLE ps_stock'; EXCEPTION WHEN OTHERS THEN NULL; END;",
        "CREATE TABLE ps_stock (id NUMBER PRIMARY KEY, ps_qty_left NUMBER(10))",
        "CREATE OR REPLACE VIEW ps_view AS SELECT id AS ps_view_marker FROM ps_stock",
        "CREATE OR REPLACE PROCEDURE ps_proc AS\nBEGIN\n  UPDATE ps_stock SET ps_qty_left = 0;\nEND;",
    ] {
        rows(&rt, &*driver, sql);
    }
    let stop = AtomicBool::new(false);
    let search = |text: &str| {
        rt.block_on(driver.search_everywhere(text, &[], &stop))
            .unwrap()
    };
    let hits = search("ps_stock");
    assert!(
        hits.iter()
            .any(|h| h.group == Group::Objects && h.name == "PS_STOCK")
    );
    let code = hits
        .iter()
        .find(|h| h.group == Group::Code && h.name == "PS_PROC")
        .unwrap();
    let line = code.line.as_ref().unwrap();
    assert_eq!(line.number, 3, "{line:?}");
    let c = search("QTY_LEFT")
        .into_iter()
        .find(|h| h.group == Group::Columns)
        .unwrap();
    assert_eq!(
        c.column.unwrap(),
        ("PS_QTY_LEFT".into(), "number(10)".into())
    );
    assert!(
        search("view_marker")
            .iter()
            .any(|h| h.group == Group::Code && h.kind == "view")
    );
    assert!(search("ps%view").is_empty());
    for text in ["ps_stock", "qty_left", "view_marker", "update ps_stock"] {
        let hidden = ["DEADLOCK".to_owned()];
        let hits = rt
            .block_on(driver.search_everywhere(text, &hidden, &stop))
            .unwrap();
        assert!(hits.iter().all(|h| h.schema != "DEADLOCK"), "{hits:?}");
    }
    let def = rt
        .block_on(driver.definition("DEADLOCK", "PS_PROC", "procedure"))
        .unwrap();
    assert_eq!(
        deadlock_core::search::locate(&def, line, "ps_stock"),
        Some((2, 9))
    );
    for sql in [
        "DROP VIEW ps_view",
        "DROP PROCEDURE ps_proc",
        "DROP TABLE ps_stock",
    ] {
        rows(&rt, &*driver, sql);
    }
}

#[test]
#[ignore = "needs docker compose"]
fn redis_searches_key_names() {
    use std::sync::atomic::AtomicBool;
    let rt = Runtime::new().unwrap();
    let driver = rt
        .block_on(connect(&config(Engine::Redis, 6379, "1"), PASSWORD))
        .unwrap();
    rows(&rt, &*driver, "SET ps:search:key 1");
    let stop = AtomicBool::new(false);
    let hits = rt
        .block_on(driver.search_everywhere("search:k", &[], &stop))
        .unwrap();
    let h = hits.iter().find(|h| h.name == "ps:search:key").unwrap();
    assert_eq!((h.schema.as_str(), h.kind.as_str()), ("db1", "string"));
    let hidden = ["db1".to_owned()];
    let hits = rt
        .block_on(driver.search_everywhere("search:k", &hidden, &stop))
        .unwrap();
    assert!(hits.iter().all(|h| h.schema != "db1"));
    rows(&rt, &*driver, "DEL ps:search:key");
}

mod objects {
    //! The object editor's engine layer: load, plan, apply, restore, drop.

    use deadlock_core::connection::{ConnectionConfig, Engine};
    use deadlock_core::driver::{Driver, NodeKind, ObjectPath, Runtime, Value, connect};
    use deadlock_core::objects::{
        ApplyOutcome, ChangePlan, FinalState, Flavor, IntervalUnit, ObjectDefinition, ObjectKind,
        ObjectRef, Param, Properties, Schedule, StepKind, StepStatus, Timing, TriggerEvent,
        TriggerOrder, ViewCheck, drop_plan, plan, recompile_plan, rename_plan, revert_plan,
        template,
    };

    use super::{PASSWORD, config, run};

    fn apply(rt: &Runtime, d: &dyn Driver, p: &ChangePlan) -> ApplyOutcome {
        rt.block_on(d.apply_object(p, None)).unwrap()
    }

    fn load(
        rt: &Runtime,
        d: &dyn Driver,
        schema: &str,
        name: &str,
        kind: ObjectKind,
    ) -> ObjectDefinition {
        rt.block_on(d.object(&ObjectRef::new(schema, name, kind)))
            .unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn one(rt: &Runtime, d: &dyn Driver, sql: &str) -> String {
        match &run(rt, d, sql).unwrap()[0][0] {
            Value::Text(s) | Value::Decimal(s) => s.clone(),
            v => v.to_string(),
        }
    }

    fn group<'a>(
        groups: &'a [deadlock_core::driver::ObjectNode],
        name: &str,
    ) -> &'a [deadlock_core::driver::ObjectNode] {
        groups
            .iter()
            .find(|g| g.name == name)
            .and_then(|g| g.children.as_deref())
            .unwrap_or_else(|| panic!("no group {name}"))
    }

    fn manages_objects(engine: Engine, port: u16) {
        let rt = Runtime::new().unwrap();
        // Root: with binary logging on, MySQL wants SUPER for triggers and functions.
        let root_cfg = ConnectionConfig {
            user: "root".into(),
            ..config(engine, port, "deadlock")
        };
        let d = rt.block_on(connect(&root_cfg, PASSWORD)).unwrap();
        let d = &*d;
        let user = rt
            .block_on(connect(&config(engine, port, "deadlock"), PASSWORD))
            .unwrap();
        let flavor = d.object_flavor().unwrap();
        let maria = engine == Engine::Mariadb;
        assert_eq!(
            flavor,
            if maria {
                Flavor::Mariadb
            } else {
                Flavor::Mysql
            }
        );
        for sql in [
            "DROP TRIGGER IF EXISTS ob_trig",
            "DROP TABLE IF EXISTS _dl_check_ob_t",
            "DROP TABLE IF EXISTS ob_t",
            "DROP PROCEDURE IF EXISTS ob_proc",
            "DROP FUNCTION IF EXISTS ob_fn",
            "DROP EVENT IF EXISTS ob_ev",
            "DROP EVENT IF EXISTS ob_ev2",
            "DROP EVENT IF EXISTS ob_ev3",
            "DROP VIEW IF EXISTS ob_v",
            "DROP VIEW IF EXISTS ob_v2",
            "CREATE TABLE ob_t (id INT PRIMARY KEY, n INT)",
        ] {
            run(&rt, d, sql).unwrap();
        }
        if maria {
            run(&rt, d, "DROP SEQUENCE IF EXISTS ob_seq").unwrap();
        }
        let s = "deadlock";

        // Trigger: create from the template, see it in the tree, change it.
        let mut t = template(flavor, ObjectKind::Trigger, s);
        t.object.name = "ob_trig".into();
        let Properties::Trigger(tp) = &mut t.props else {
            panic!()
        };
        tp.table = "ob_t".into();
        tp.timing = Timing::Before;
        tp.events = vec![TriggerEvent::Insert];
        tp.body = "BEGIN\n  SET NEW.n = 1;\nEND".into();
        let out = apply(&rt, d, &plan(flavor, None, &t).unwrap());
        assert_eq!(out.state, FinalState::Changed, "{out:?}");
        let groups = rt
            .block_on(d.children(&ObjectPath(vec![s.into()])))
            .unwrap();
        let trig = group(&groups, "Triggers")
            .iter()
            .find(|n| n.name == "ob_trig")
            .unwrap();
        assert_eq!(
            (trig.kind, trig.detail.as_str()),
            (NodeKind::Routine, "ob_t")
        );
        let table = group(&groups, "Tables")
            .iter()
            .find(|n| n.name == "ob_t")
            .unwrap();
        let under = table.children.as_ref().unwrap().last().unwrap();
        assert_eq!(under.name, "Triggers");
        assert_eq!(under.children.as_ref().unwrap()[0].detail, "before insert");
        assert!(group(&groups, "Events").iter().all(|n| n.name != "ob_ev"));

        let def = load(&rt, d, s, "ob_trig", ObjectKind::Trigger);
        let Properties::Trigger(tp) = &def.props else {
            panic!()
        };
        assert_eq!(
            (tp.table.as_str(), tp.timing, &tp.events),
            ("ob_t", Timing::Before, &vec![TriggerEvent::Insert])
        );
        assert!(tp.body.contains("SET NEW.n = 1"), "{}", tp.body);
        assert_eq!(def.siblings, ["ob_trig"]);
        let mut draft = def.draft();
        let Properties::Trigger(tp) = &mut draft.props else {
            panic!()
        };
        tp.body = "BEGIN\n  SET NEW.n = 2;\nEND".into();
        let p = plan(flavor, Some(&def), &draft).unwrap();
        let kinds: Vec<_> = p.steps.iter().map(|s| s.kind).collect();
        if maria {
            assert_eq!(kinds, [StepKind::Replace]);
        } else {
            assert_eq!(kinds, [StepKind::Check, StepKind::Swap, StepKind::Restore]);
        }
        let out = apply(&rt, d, &p);
        assert_eq!(out.state, FinalState::Changed, "{out:?}");
        run(&rt, d, "INSERT INTO ob_t (id) VALUES (1)").unwrap();
        assert_eq!(one(&rt, d, "SELECT n FROM ob_t WHERE id = 1"), "2");
        assert_eq!(
            one(
                &rt,
                d,
                "SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = 'deadlock' AND TABLE_NAME = '_dl_check_ob_t'"
            ),
            "0"
        );

        // A syntax error: the check (MySQL) or the replace (MariaDB) fails, nothing changes.
        let def = load(&rt, d, s, "ob_trig", ObjectKind::Trigger);
        let mut bad = def.draft();
        let Properties::Trigger(tp) = &mut bad.props else {
            panic!()
        };
        tp.body = "BEGIN\n  SET NEW.n = ;\nEND".into();
        let out = apply(&rt, d, &plan(flavor, Some(&def), &bad).unwrap());
        assert_eq!(out.state, FinalState::Unchanged, "{out:?}");
        assert!(out.errors[0].code.starts_with("1064"), "{:?}", out.errors);
        assert_eq!(out.errors[0].body_line, Some(2), "{:?}", out.errors);

        // MySQL: the check passes but the CREATE fails (FOLLOWS a trigger that is not
        // there, which the check leaves out): the original comes back.
        let mut follows = def.draft();
        let Properties::Trigger(tp) = &mut follows.props else {
            panic!()
        };
        tp.body = "BEGIN\n  SET NEW.n = 3;\nEND".into();
        tp.order = Some((TriggerOrder::Follows, "ob_nope".into()));
        let out = apply(&rt, d, &plan(flavor, Some(&def), &follows).unwrap());
        if maria {
            assert_eq!(out.state, FinalState::Unchanged, "{out:?}");
        } else {
            assert_eq!(out.state, FinalState::Restored, "{out:?}");
            let st: Vec<_> = out.steps.iter().map(|s| s.status).collect();
            assert_eq!(st, [StepStatus::Done, StepStatus::Failed, StepStatus::Done]);
        }
        run(&rt, d, "INSERT INTO ob_t (id) VALUES (2)").unwrap();
        assert_eq!(one(&rt, d, "SELECT n FROM ob_t WHERE id = 2"), "2");

        // Procedure and function.
        let mut pr = template(flavor, ObjectKind::Procedure, s);
        pr.object.name = "ob_proc".into();
        let Properties::Routine(r) = &mut pr.props else {
            panic!()
        };
        r.params = vec![Param {
            name: "a".into(),
            data_type: "INT".into(),
            ..Param::default()
        }];
        r.body = "BEGIN\n  SELECT a + 1 AS v;\nEND".into();
        assert_eq!(
            apply(&rt, d, &plan(flavor, None, &pr).unwrap()).state,
            FinalState::Changed
        );
        let def = load(&rt, d, s, "ob_proc", ObjectKind::Procedure);
        let Properties::Routine(r) = &def.props else {
            panic!()
        };
        assert!(
            r.params[0].name == "a" && r.params[0].data_type.starts_with("int"),
            "{:?}",
            r.params
        );
        let as_user = load(&rt, &*user, s, "ob_proc", ObjectKind::Procedure);
        assert!(
            !as_user.notices.is_empty(),
            "grants are not readable as deadlock"
        );
        let mut draft = def.draft();
        let Properties::Routine(r) = &mut draft.props else {
            panic!()
        };
        r.body = "BEGIN\n  SELECT a + 10 AS v;\nEND".into();
        let out = apply(&rt, d, &plan(flavor, Some(&def), &draft).unwrap());
        assert_eq!(out.state, FinalState::Changed, "{out:?}");
        assert_eq!(one(&rt, d, "CALL ob_proc(1)"), "11");
        let def = load(&rt, d, s, "ob_proc", ObjectKind::Procedure);
        let mut draft = def.draft();
        let Properties::Routine(r) = &mut draft.props else {
            panic!()
        };
        r.comment = "it's a comment".into();
        let p = plan(flavor, Some(&def), &draft).unwrap();
        assert_eq!(p.verb(), "ALTER");
        apply(&rt, d, &p);
        let Properties::Routine(r) = &load(&rt, d, s, "ob_proc", ObjectKind::Procedure).props
        else {
            panic!()
        };
        assert_eq!(r.comment, "it's a comment");

        let mut f = template(flavor, ObjectKind::Function, s);
        f.object.name = "ob_fn".into();
        let Properties::Routine(r) = &mut f.props else {
            panic!()
        };
        r.params = vec![Param {
            name: "x".into(),
            data_type: "INT".into(),
            ..Param::default()
        }];
        r.body = "BEGIN\n  RETURN x + 1;\nEND".into();
        assert_eq!(
            apply(&rt, d, &plan(flavor, None, &f).unwrap()).state,
            FinalState::Changed
        );
        assert_eq!(one(&rt, d, "SELECT ob_fn(1)"), "2");
        let def = load(&rt, d, s, "ob_fn", ObjectKind::Function);
        let mut draft = def.draft();
        let Properties::Routine(r) = &mut draft.props else {
            panic!()
        };
        r.body = "BEGIN\n  RETURN x + 2;\nEND".into();
        assert_eq!(
            apply(&rt, d, &plan(flavor, Some(&def), &draft).unwrap()).state,
            FinalState::Changed
        );
        assert_eq!(one(&rt, d, "SELECT ob_fn(1)"), "3");

        // Grants survive the replace (MySQL re-runs them; MariaDB keeps them).
        let root = d;
        for sql in [
            "DROP USER IF EXISTS 'ob_user'@'%'",
            "CREATE USER 'ob_user'@'%' IDENTIFIED BY 'x'",
            "GRANT EXECUTE ON PROCEDURE deadlock.ob_proc TO 'ob_user'@'%'",
        ] {
            run(&rt, root, sql).unwrap();
        }
        let def = load(&rt, root, s, "ob_proc", ObjectKind::Procedure);
        assert!(
            def.grants
                .iter()
                .any(|g| g.user == "ob_user" && g.privileges == ["EXECUTE"]),
            "{:?}",
            def.grants
        );
        let mut draft = def.draft();
        let Properties::Routine(r) = &mut draft.props else {
            panic!()
        };
        r.body = "BEGIN\n  SELECT a + 20 AS v;\nEND".into();
        let p = plan(flavor, Some(&def), &draft).unwrap();
        assert_eq!(p.steps.iter().any(|s| s.kind == StepKind::Grants), !maria);
        assert_eq!(apply(&rt, root, &p).state, FinalState::Changed);
        let grants: Vec<String> = run(&rt, root, "SHOW GRANTS FOR 'ob_user'@'%'")
            .unwrap()
            .into_iter()
            .map(|r| r[0].to_string())
            .collect();
        assert!(
            grants
                .iter()
                .any(|g| g.contains("EXECUTE ON PROCEDURE `deadlock`.`ob_proc`")),
            "{grants:?}"
        );
        run(&rt, root, "DROP USER 'ob_user'@'%'").unwrap();

        // Event: create, alter every field, rename.
        let mut e = template(flavor, ObjectKind::Event, s);
        e.object.name = "ob_ev".into();
        let Properties::Event(ep) = &mut e.props else {
            panic!()
        };
        ep.body = "UPDATE ob_t SET n = n".into();
        assert_eq!(
            apply(&rt, d, &plan(flavor, None, &e).unwrap()).state,
            FinalState::Changed
        );
        let def = load(&rt, d, s, "ob_ev", ObjectKind::Event);
        let mut draft = def.draft();
        draft.object.name = "ob_ev2".into();
        let Properties::Event(ep) = &mut draft.props else {
            panic!()
        };
        let Schedule::Every { value, unit, .. } = &mut ep.schedule else {
            panic!()
        };
        (*value, *unit) = ("2".into(), IntervalUnit::Hour);
        ep.comment = "every two hours".into();
        let p = plan(flavor, Some(&def), &draft).unwrap();
        assert_eq!(p.steps.len(), 1);
        assert_eq!(apply(&rt, d, &p).state, FinalState::Changed);
        let def = load(&rt, d, s, "ob_ev2", ObjectKind::Event);
        let Properties::Event(ep) = &def.props else {
            panic!()
        };
        assert_eq!(ep.comment, "every two hours");
        assert!(
            matches!(&ep.schedule, Schedule::Every { value, unit: IntervalUnit::Hour, .. } if value == "2")
        );
        assert_eq!(
            apply(&rt, d, &rename_plan(&def, "ob_ev3").unwrap()).state,
            FinalState::Changed
        );
        let ev3 = load(&rt, d, s, "ob_ev3", ObjectKind::Event);

        // View: create, replace, rename.
        let mut v = template(flavor, ObjectKind::View, s);
        v.object.name = "ob_v".into();
        let Properties::View(vp) = &mut v.props else {
            panic!()
        };
        vp.query = "SELECT id FROM ob_t".into();
        assert_eq!(
            apply(&rt, d, &plan(flavor, None, &v).unwrap()).state,
            FinalState::Changed
        );
        let def = load(&rt, d, s, "ob_v", ObjectKind::View);
        let mut draft = def.draft();
        let Properties::View(vp) = &mut draft.props else {
            panic!()
        };
        vp.query = "SELECT id, n FROM ob_t WHERE n > 0".into();
        vp.check = ViewCheck::Cascaded;
        assert_eq!(
            apply(&rt, d, &plan(flavor, Some(&def), &draft).unwrap()).state,
            FinalState::Changed
        );
        let def = load(&rt, d, s, "ob_v", ObjectKind::View);
        let Properties::View(vp) = &def.props else {
            panic!()
        };
        assert_eq!(vp.check, ViewCheck::Cascaded);
        assert!(vp.query.contains("`n` > 0"), "{}", vp.query);
        assert_eq!(
            apply(&rt, d, &rename_plan(&def, "ob_v2").unwrap()).state,
            FinalState::Changed
        );
        let v2 = load(&rt, d, s, "ob_v2", ObjectKind::View);

        // MariaDB sequence.
        let mut dropped = vec![ev3.object, v2.object];
        if maria {
            let mut q = template(flavor, ObjectKind::Sequence, s);
            q.object.name = "ob_seq".into();
            assert_eq!(
                apply(&rt, d, &plan(flavor, None, &q).unwrap()).state,
                FinalState::Changed
            );
            let def = load(&rt, d, s, "ob_seq", ObjectKind::Sequence);
            let mut draft = def.draft();
            let Properties::Sequence(sq) = &mut draft.props else {
                panic!()
            };
            sq.increment = "5".into();
            sq.cache = "0".into();
            sq.restart_with = "100".into();
            assert_eq!(
                apply(&rt, d, &plan(flavor, Some(&def), &draft).unwrap()).state,
                FinalState::Changed
            );
            assert_eq!(one(&rt, d, "SELECT NEXTVAL(ob_seq)"), "100");
            let def = load(&rt, d, s, "ob_seq", ObjectKind::Sequence);
            let Properties::Sequence(sq) = &def.props else {
                panic!()
            };
            assert_eq!((sq.increment.as_str(), sq.cache.as_str()), ("5", "0"));
            let groups = rt
                .block_on(d.children(&ObjectPath(vec![s.into()])))
                .unwrap();
            assert!(
                group(&groups, "Sequences")
                    .iter()
                    .any(|n| n.name == "ob_seq")
            );
            assert!(group(&groups, "Tables").iter().all(|n| n.name != "ob_seq"));
            dropped.push(def.object);
        }

        // A read-only connection refuses, then drop everything.
        let ro = rt
            .block_on(connect(
                &ConnectionConfig {
                    read_only: true,
                    ..config(engine, port, "deadlock")
                },
                PASSWORD,
            ))
            .unwrap();
        let p = drop_plan(flavor, &ObjectRef::new(s, "ob_trig", ObjectKind::Trigger));
        assert!(rt.block_on(ro.apply_object(&p, None)).is_err());
        assert!(
            load(&rt, &*ro, s, "ob_proc", ObjectKind::Procedure)
                .read_only
                .is_some()
        );
        dropped.extend([
            ObjectRef::new(s, "ob_trig", ObjectKind::Trigger),
            ObjectRef::new(s, "ob_proc", ObjectKind::Procedure),
            ObjectRef::new(s, "ob_fn", ObjectKind::Function),
        ]);
        for o in &dropped {
            let out = apply(&rt, d, &drop_plan(flavor, o));
            assert_eq!(out.state, FinalState::Changed, "{o:?} {out:?}");
        }
        // Gone, not a failed read: restored tabs rely on telling these apart.
        let gone = rt.block_on(d.object(&dropped[0]));
        assert!(
            matches!(gone, Err(deadlock_core::driver::DriverError::NotFound(_))),
            "{gone:?}"
        );
        run(&rt, d, "DROP TABLE ob_t").unwrap();
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mysql_manages_objects() {
        manages_objects(Engine::Mysql, 3306);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn mariadb_manages_objects() {
        manages_objects(Engine::Mariadb, 3307);
    }

    #[test]
    #[ignore = "needs docker compose"]
    fn oracle_manages_objects() {
        let rt = Runtime::new().unwrap();
        let d = rt
            .block_on(connect(&config(Engine::Oracle, 1521, "FREEPDB1"), PASSWORD))
            .unwrap();
        let d = &*d;
        let f = Flavor::Oracle;
        assert_eq!(d.object_flavor(), Some(f));
        for sql in [
            "DROP TRIGGER OB_T_BIU",
            "DROP TRIGGER OB_T_BIU2",
            "DROP TABLE OB_T PURGE",
            "DROP FUNCTION OB_FN",
            "DROP PACKAGE OB_PKG",
            "DROP SEQUENCE OB_SEQ",
            "DROP SEQUENCE OB_SEQ2",
            "DROP VIEW OB_V",
            "DROP VIEW OB_V2",
            "DROP SYNONYM OB_SYN",
            "DROP MATERIALIZED VIEW OB_MV",
            "DROP TYPE OB_ADDR_T",
        ] {
            let _ = run(&rt, d, sql);
        }
        run(
            &rt,
            d,
            "CREATE TABLE OB_T (id NUMBER PRIMARY KEY, n NUMBER, status VARCHAR2(10))",
        )
        .unwrap();
        let s = "DEADLOCK";

        // Trigger: create, read back, disable, rename, a failed compile and revert.
        let mut t = template(f, ObjectKind::Trigger, s);
        t.object.name = "ob_t_biu".into();
        let Properties::Trigger(tp) = &mut t.props else {
            panic!()
        };
        tp.table = "OB_T".into();
        tp.events = vec![TriggerEvent::Insert, TriggerEvent::Update];
        tp.update_columns = vec!["STATUS".into()];
        tp.when = "new.n IS NULL".into();
        tp.body = "BEGIN\n  :NEW.n := 1;\nEND;".into();
        let out = apply(&rt, d, &plan(f, None, &t).unwrap());
        assert_eq!(out.state, FinalState::Changed, "{out:?}");
        let def = load(&rt, d, s, "OB_T_BIU", ObjectKind::Trigger);
        let Properties::Trigger(tp) = &def.props else {
            panic!("{:?}", def.props)
        };
        assert_eq!(tp.events, [TriggerEvent::Insert, TriggerEvent::Update]);
        assert_eq!(
            (tp.update_columns.as_slice(), tp.when.as_str()),
            (&["STATUS".to_owned()][..], "new.n IS NULL")
        );
        assert!(tp.enabled && tp.for_each_row && tp.body.starts_with("BEGIN"));
        assert_eq!(def.valid(), Some(true));
        let groups = rt
            .block_on(d.children(&ObjectPath(vec![s.into()])))
            .unwrap();
        assert_eq!(
            group(&groups, "Triggers")
                .iter()
                .find(|n| n.name == "OB_T_BIU")
                .unwrap()
                .detail,
            "OB_T"
        );
        let table = group(&groups, "Tables")
            .iter()
            .find(|n| n.name == "OB_T")
            .unwrap();
        assert_eq!(
            table.children.as_ref().unwrap().last().unwrap().name,
            "Triggers"
        );

        let mut off = def.draft();
        let Properties::Trigger(tp) = &mut off.props else {
            panic!()
        };
        tp.enabled = false;
        let p = plan(f, Some(&def), &off).unwrap();
        assert_eq!(p.verb(), "ALTER");
        assert_eq!(apply(&rt, d, &p).state, FinalState::Changed);
        let def = load(&rt, d, s, "OB_T_BIU", ObjectKind::Trigger);
        let Properties::Trigger(tp) = &def.props else {
            panic!()
        };
        assert!(!tp.enabled);
        assert_eq!(
            apply(&rt, d, &rename_plan(&def, "ob_t_biu2").unwrap()).state,
            FinalState::Changed
        );
        let good = load(&rt, d, s, "OB_T_BIU2", ObjectKind::Trigger);
        let mut broken = good.draft();
        let Properties::Trigger(tp) = &mut broken.props else {
            panic!()
        };
        tp.body = "BEGIN\n  :NEW.n := ob_undefined_x;\nEND;".into();
        let out = apply(&rt, d, &plan(f, Some(&good), &broken).unwrap());
        assert_eq!(out.state, FinalState::Invalid, "{out:?}");
        let e = out.errors.iter().find(|e| !e.warning).unwrap();
        assert!(
            e.code.starts_with("PLS-") && e.body_line == Some(2),
            "{:?}",
            out.errors
        );
        let now = load(&rt, d, s, "OB_T_BIU2", ObjectKind::Trigger);
        assert_eq!(now.valid(), Some(false));
        assert!(!now.errors.is_empty());
        let out = apply(&rt, d, &revert_plan(&now, &good).unwrap());
        assert_eq!(out.state, FinalState::Changed, "{out:?}");
        assert_eq!(
            load(&rt, d, s, "OB_T_BIU2", ObjectKind::Trigger).valid(),
            Some(true)
        );

        // Function.
        let mut fun = template(f, ObjectKind::Function, s);
        fun.object.name = "ob_fn".into();
        let Properties::Routine(r) = &mut fun.props else {
            panic!()
        };
        r.params = vec![Param {
            name: "p_x".into(),
            data_type: "NUMBER".into(),
            default: "0".into(),
            ..Param::default()
        }];
        r.body = "BEGIN\n  RETURN p_x + 1;\nEND;".into();
        assert_eq!(
            apply(&rt, d, &plan(f, None, &fun).unwrap()).state,
            FinalState::Changed
        );
        assert_eq!(one(&rt, d, "SELECT OB_FN(1) FROM dual"), "2");
        let def = load(&rt, d, s, "OB_FN", ObjectKind::Function);
        let Properties::Routine(r) = &def.props else {
            panic!("{:?}", def.props)
        };
        assert_eq!(
            (
                r.params[0].name.as_str(),
                r.params[0].default.as_str(),
                r.returns.as_str()
            ),
            ("P_X", "0", "NUMBER")
        );
        let mut draft = def.draft();
        let Properties::Routine(r) = &mut draft.props else {
            panic!()
        };
        r.body = "BEGIN\n  RETURN p_x + 2;\nEND;".into();
        r.deterministic = true;
        assert_eq!(
            apply(&rt, d, &plan(f, Some(&def), &draft).unwrap()).state,
            FinalState::Changed
        );
        assert_eq!(one(&rt, d, "SELECT OB_FN(1) FROM dual"), "3");

        // Package: spec change recompiles the body, which then fails; fix the body.
        let mut pk = template(f, ObjectKind::Package, s);
        let Properties::Source(src) = &mut pk.props else {
            panic!()
        };
        src.spec = src.spec.replace("NEW_PACKAGE", "OB_PKG");
        src.body = src.body.replace("NEW_PACKAGE", "OB_PKG");
        assert_eq!(
            apply(&rt, d, &plan(f, None, &pk).unwrap()).state,
            FinalState::Changed
        );
        let def = load(&rt, d, s, "OB_PKG", ObjectKind::Package);
        assert_eq!(def.status.len(), 2);
        let mut draft = def.draft();
        let Properties::Source(src) = &mut draft.props else {
            panic!()
        };
        src.spec = "PACKAGE OB_PKG AS\n  PROCEDURE run;\n  PROCEDURE stop;\nEND OB_PKG;".into();
        let p = plan(f, Some(&def), &draft).unwrap();
        assert_eq!(
            p.steps.iter().map(|s| s.kind).collect::<Vec<_>>(),
            [StepKind::Replace, StepKind::Compile]
        );
        let out = apply(&rt, d, &p);
        assert_eq!(out.state, FinalState::Invalid, "{out:?}");
        assert!(
            out.errors.iter().any(|e| e.part == "PACKAGE BODY"),
            "{:?}",
            out.errors
        );
        let def = load(&rt, d, s, "OB_PKG", ObjectKind::Package);
        let mut draft = def.draft();
        let Properties::Source(src) = &mut draft.props else {
            panic!()
        };
        src.body = "PACKAGE BODY OB_PKG AS\n  PROCEDURE run IS BEGIN NULL; END run;\n  PROCEDURE stop IS BEGIN NULL; END stop;\nEND OB_PKG;".into();
        assert_eq!(
            apply(&rt, d, &plan(f, Some(&def), &draft).unwrap()).state,
            FinalState::Changed
        );
        assert_eq!(
            apply(&rt, d, &recompile_plan(f, &def.object).unwrap()).state,
            FinalState::Changed
        );
        assert_eq!(
            load(&rt, d, s, "OB_PKG", ObjectKind::Package).valid(),
            Some(true)
        );

        // Sequence.
        let mut q = template(f, ObjectKind::Sequence, s);
        q.object.name = "ob_seq".into();
        assert_eq!(
            apply(&rt, d, &plan(f, None, &q).unwrap()).state,
            FinalState::Changed
        );
        let def = load(&rt, d, s, "OB_SEQ", ObjectKind::Sequence);
        let mut draft = def.draft();
        let Properties::Sequence(sq) = &mut draft.props else {
            panic!()
        };
        sq.cache = "100".into();
        sq.restart_with = "50".into();
        assert_eq!(
            apply(&rt, d, &plan(f, Some(&def), &draft).unwrap()).state,
            FinalState::Changed
        );
        assert_eq!(one(&rt, d, "SELECT OB_SEQ.NEXTVAL FROM dual"), "50");
        let def = load(&rt, d, s, "OB_SEQ", ObjectKind::Sequence);
        let Properties::Sequence(sq) = &def.props else {
            panic!()
        };
        assert_eq!(sq.cache, "100");
        assert_eq!(
            apply(&rt, d, &rename_plan(&def, "ob_seq2").unwrap()).state,
            FinalState::Changed
        );

        // View.
        let mut v = template(f, ObjectKind::View, s);
        v.object.name = "ob_v".into();
        let Properties::View(vp) = &mut v.props else {
            panic!()
        };
        vp.query = "SELECT id FROM ob_t".into();
        assert_eq!(
            apply(&rt, d, &plan(f, None, &v).unwrap()).state,
            FinalState::Changed
        );
        let def = load(&rt, d, s, "OB_V", ObjectKind::View);
        let mut draft = def.draft();
        let Properties::View(vp) = &mut draft.props else {
            panic!("{:?}", def.props)
        };
        assert!(
            vp.query.to_ascii_uppercase().contains("FROM OB_T"),
            "{}",
            vp.query
        );
        vp.query = "SELECT id, n FROM ob_t".into();
        vp.columns = String::new();
        vp.check = ViewCheck::ReadOnly;
        assert_eq!(
            apply(&rt, d, &plan(f, Some(&def), &draft).unwrap()).state,
            FinalState::Changed
        );
        let def = load(&rt, d, s, "OB_V", ObjectKind::View);
        let Properties::View(vp) = &def.props else {
            panic!()
        };
        assert_eq!(vp.check, ViewCheck::ReadOnly);
        assert_eq!(
            apply(&rt, d, &rename_plan(&def, "ob_v2").unwrap()).state,
            FinalState::Changed
        );

        // Synonym.
        let mut sy = template(f, ObjectKind::Synonym, s);
        sy.object.name = "ob_syn".into();
        let Properties::Synonym(sp) = &mut sy.props else {
            panic!()
        };
        (sp.target_schema, sp.target) = (s.into(), "ob_t".into());
        assert_eq!(
            apply(&rt, d, &plan(f, None, &sy).unwrap()).state,
            FinalState::Changed
        );
        let def = load(&rt, d, s, "OB_SYN", ObjectKind::Synonym);
        assert!(
            def.facts
                .iter()
                .any(|(k, v)| k == "resolves to" && v == "TABLE DEADLOCK.OB_T"),
            "{:?}",
            def.facts
        );
        let mut draft = def.draft();
        let Properties::Synonym(sp) = &mut draft.props else {
            panic!()
        };
        sp.target = "OB_V2".into();
        assert_eq!(
            apply(&rt, d, &plan(f, Some(&def), &draft).unwrap()).state,
            FinalState::Changed
        );

        // Materialized view: refresh options, then a new query (drop and create).
        let mut mv = template(f, ObjectKind::MaterializedView, s);
        mv.object.name = "ob_mv".into();
        let Properties::MaterializedView(m) = &mut mv.props else {
            panic!()
        };
        m.query = "SELECT COUNT(*) n FROM ob_t".into();
        assert_eq!(
            apply(&rt, d, &plan(f, None, &mv).unwrap()).state,
            FinalState::Changed
        );
        let def = load(&rt, d, s, "OB_MV", ObjectKind::MaterializedView);
        let mut draft = def.draft();
        let Properties::MaterializedView(m) = &mut draft.props else {
            panic!()
        };
        assert!(
            m.query.to_ascii_uppercase().contains("COUNT(*)"),
            "{}",
            m.query
        );
        m.method = deadlock_core::objects::RefreshMethod::Complete;
        let p = plan(f, Some(&def), &draft).unwrap();
        assert_eq!(p.verb(), "ALTER");
        assert_eq!(apply(&rt, d, &p).state, FinalState::Changed);
        let def = load(&rt, d, s, "OB_MV", ObjectKind::MaterializedView);
        let mut draft = def.draft();
        let Properties::MaterializedView(m) = &mut draft.props else {
            panic!()
        };
        m.query = "SELECT COUNT(*) n, MAX(id) top FROM ob_t".into();
        let p = plan(f, Some(&def), &draft).unwrap();
        assert!(!p.atomic);
        let out = apply(&rt, d, &p);
        assert_eq!(out.state, FinalState::Changed, "{out:?}");
        assert_eq!(
            one(
                &rt,
                d,
                "SELECT COUNT(*) FROM all_tab_columns WHERE owner = 'DEADLOCK' AND table_name = 'OB_MV'"
            ),
            "2"
        );

        // Type.
        let mut ty = template(f, ObjectKind::Type, s);
        let Properties::Source(src) = &mut ty.props else {
            panic!()
        };
        src.spec =
            "TYPE OB_ADDR_T AS OBJECT (\n  street VARCHAR2(80),\n  city VARCHAR2(40)\n);".into();
        assert_eq!(
            apply(&rt, d, &plan(f, None, &ty).unwrap()).state,
            FinalState::Changed
        );
        let def = load(&rt, d, s, "OB_ADDR_T", ObjectKind::Type);
        assert!(
            def.facts
                .iter()
                .any(|(k, v)| k == "attribute STREET" && v == "VARCHAR2(80)"),
            "{:?}",
            def.facts
        );

        // Read-only refuses; then drop everything.
        let ro = rt
            .block_on(connect(
                &ConnectionConfig {
                    read_only: true,
                    ..config(Engine::Oracle, 1521, "FREEPDB1")
                },
                PASSWORD,
            ))
            .unwrap();
        let p = drop_plan(f, &def.object);
        assert!(rt.block_on(ro.apply_object(&p, None)).is_err());
        for (name, kind) in [
            ("OB_ADDR_T", ObjectKind::Type),
            ("OB_MV", ObjectKind::MaterializedView),
            ("OB_SYN", ObjectKind::Synonym),
            ("OB_V2", ObjectKind::View),
            ("OB_SEQ2", ObjectKind::Sequence),
            ("OB_PKG", ObjectKind::Package),
            ("OB_FN", ObjectKind::Function),
            ("OB_T_BIU2", ObjectKind::Trigger),
        ] {
            let out = apply(&rt, d, &drop_plan(f, &ObjectRef::new(s, name, kind)));
            assert_eq!(out.state, FinalState::Changed, "{name} {out:?}");
        }
        // Gone, not a failed read: restored tabs rely on telling these apart.
        let gone = rt.block_on(d.object(&ObjectRef::new(s, "OB_V2", ObjectKind::View)));
        assert!(
            matches!(gone, Err(deadlock_core::driver::DriverError::NotFound(_))),
            "{gone:?}"
        );
        run(&rt, d, "DROP TABLE OB_T PURGE").unwrap();
    }
}

/// Another session kills the app's own: a read reconnects and runs again; a write, and
/// a read inside an open transaction, fail plainly without running again; the call
/// after each runs on the new session, in the same transaction mode.
fn heals_after_its_session_is_killed(engine: Engine, port: u16, database: &str, admin: &str) {
    let rt = Runtime::new().unwrap();
    let cfg = ConnectionConfig {
        encryption: Encryption::Off,
        ..config(engine, port, database)
    };
    let admin = connect_as(
        &rt,
        &ConnectionConfig {
            user: admin.into(),
            ..cfg.clone()
        },
    );
    let app = connect_as(&rt, &cfg);
    let one = if engine == Engine::Oracle {
        "SELECT 1 FROM dual"
    } else {
        "SELECT 1"
    };
    let count = |d: &dyn Driver| count(&rt, d, "deadlock.p_heal");
    let kill = || {
        let id = app.session_id().unwrap();
        let snap = rt.block_on(admin.sessions()).unwrap();
        let s = snap.session(id).unwrap();
        assert!(s.this_app && !s.own, "{s:?}");
        rt.block_on(admin.end_session(id)).unwrap();
        // Oracle marks the session killed at once but ends it a moment later.
        std::thread::sleep(Duration::from_millis(500));
        id
    };
    run(&rt, &*app, "DROP TABLE IF EXISTS deadlock.p_heal").unwrap();
    run(
        &rt,
        &*app,
        "CREATE TABLE deadlock.p_heal (id INT PRIMARY KEY)",
    )
    .unwrap();

    let before = kill();
    assert_eq!(count(&*app), "0", "a read heals");
    // Oracle may give the new session the old SID.
    if engine != Engine::Oracle {
        assert_ne!(app.session_id(), Some(before));
    }

    kill();
    let e = run(&rt, &*app, "INSERT INTO deadlock.p_heal VALUES (1)").unwrap_err();
    assert!(e.contains("did not run it again"), "{e}");
    assert_eq!(count(&*admin), "0", "the write did not run again");
    run(&rt, &*app, "INSERT INTO deadlock.p_heal VALUES (2)").unwrap();

    rt.block_on(app.set_auto_commit(false)).unwrap();
    run(&rt, &*app, "INSERT INTO deadlock.p_heal VALUES (3)").unwrap();
    assert!(open(&rt, &*app));
    kill();
    let e = run(&rt, &*app, one).unwrap_err();
    assert!(e.contains("transaction was rolled back"), "{e}");
    assert!(!open(&rt, &*app));
    assert_eq!(count(&*app), "1", "the insert was rolled back");
    // Still manual: an insert stays uncommitted until rolled back.
    run(&rt, &*app, "INSERT INTO deadlock.p_heal VALUES (4)").unwrap();
    assert!(open(&rt, &*app));
    rt.block_on(app.end_transaction(false)).unwrap();
    assert_eq!(count(&*admin), "1");
    rt.block_on(app.set_auto_commit(true)).unwrap();
    // A killed Oracle session keeps its locks until the server has cleaned it up.
    let until = Instant::now() + Duration::from_secs(30);
    while let Err(e) = run(&rt, &*admin, "DROP TABLE deadlock.p_heal") {
        assert!(Instant::now() < until && e.contains("ORA-00054"), "{e}");
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[test]
#[ignore = "needs docker compose"]
fn mysql_heals_after_its_session_is_killed() {
    heals_after_its_session_is_killed(Engine::Mysql, 3306, "deadlock", "root");
}

#[test]
#[ignore = "needs docker compose"]
fn mariadb_heals_after_its_session_is_killed() {
    heals_after_its_session_is_killed(Engine::Mariadb, 3307, "deadlock", "root");
}

#[test]
#[ignore = "needs docker compose"]
fn oracle_heals_after_its_session_is_killed() {
    heals_after_its_session_is_killed(Engine::Oracle, 1521, "FREEPDB1", "system");
}
