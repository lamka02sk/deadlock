//! The Redis key browser, key tabs and admin screens against the Redis in
//! `docker-compose.yml`. Ignored by default; run with the stack up:
//! `cargo test -p deadlock-core --test redis -- --ignored --test-threads 1`.
//!
//! Each test uses its own database index and empties it before and after.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use deadlock_core::connection::{ConnectionConfig, Engine};
use deadlock_core::driver::{Driver, ObjectPath, Row, Runtime, Value, connect};
use deadlock_core::edit::{Edit, Pending, RowId};
use deadlock_core::redis::{self as rk, Command, KeyType};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PASSWORD: &str = "deadlock";

fn config(db: u32) -> ConnectionConfig {
    ConnectionConfig {
        id: "test".into(),
        name: "test".into(),
        host: "127.0.0.1".into(),
        port: 6379,
        database: db.to_string(),
        ..ConnectionConfig::new(Engine::Redis)
    }
}

fn open(rt: &Runtime, db: u32) -> Arc<dyn Driver> {
    rt.block_on(connect(&config(db), PASSWORD)).unwrap()
}

/// A plain connection to `db` for setup and checks.
fn raw(rt: &Runtime, db: u32) -> redis::aio::MultiplexedConnection {
    let url = format!("redis://:{PASSWORD}@127.0.0.1:6379/{db}");
    rt.block_on(async {
        redis::Client::open(url)
            .unwrap()
            .get_multiplexed_async_connection()
            .await
            .unwrap()
    })
}

fn cmd<T: redis::FromRedisValue>(
    rt: &Runtime,
    c: &mut redis::aio::MultiplexedConnection,
    args: &[&[u8]],
) -> T {
    let mut cmd = redis::cmd(std::str::from_utf8(args[0]).unwrap());
    for a in &args[1..] {
        cmd.arg(*a);
    }
    rt.block_on(cmd.query_async(c)).unwrap()
}

fn flush(rt: &Runtime, db: u32) {
    let mut c = raw(rt, db);
    let _: () = cmd(rt, &mut c, &[b"FLUSHDB"]);
}

fn all_rows(
    rt: &Runtime,
    d: &dyn Driver,
    key: &[u8],
    kind: &KeyType,
    pattern: Option<&str>,
) -> Vec<Row> {
    let r = d.as_redis().unwrap();
    rt.block_on(async {
        let stream = r.rows(key, kind, pattern.map(str::to_owned)).unwrap();
        stream.collect().await
    })
    .unwrap()
}

/// Every command the server receives while `f` runs, as `MONITOR` prints them.
fn monitored(rt: &Runtime, f: impl FnOnce()) -> Vec<String> {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&lines);
    let (ready_tx, ready) = std::sync::mpsc::channel();
    let task = rt.spawn(async move {
        let mut s = tokio::net::TcpStream::connect("127.0.0.1:6379")
            .await
            .unwrap();
        s.write_all(format!("AUTH {PASSWORD}\r\nMONITOR\r\n").as_bytes())
            .await
            .unwrap();
        let mut r = BufReader::new(s);
        let mut line = String::new();
        let mut oks = 0;
        loop {
            line.clear();
            if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            if line.starts_with("+OK") {
                oks += 1;
                if oks == 2 {
                    let _ = ready_tx.send(());
                }
                continue;
            }
            seen.lock().unwrap().push(line.trim_end().to_owned());
        }
    });
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    f();
    std::thread::sleep(Duration::from_millis(300));
    drop(task);
    lines.lock().unwrap().clone()
}

/// A `MONITOR` line's arguments, unquoted: `+1.2 [3 127.0.0.1:1] "SCAN" "0"`.
fn monitor_args(line: &str) -> Vec<String> {
    let Some(rest) = line.split_once("] ").map(|(_, r)| r) else {
        return Vec::new();
    };
    rk::split_args(rest)
        .unwrap_or_default()
        .into_iter()
        .map(|a| String::from_utf8_lossy(&a).to_ascii_uppercase())
        .collect()
}

/// A command whose cost grows with the keyspace or a whole collection.
fn forbidden(args: &[String]) -> bool {
    let a = |i: usize| args.get(i).map_or("", String::as_str);
    match a(0) {
        "KEYS" | "HGETALL" | "HKEYS" | "HVALS" | "SMEMBERS" | "SUNION" | "SINTER" | "SDIFF"
        | "FLUSHDB" | "FLUSHALL" => true,
        "LRANGE" | "ZRANGE" => a(2) == "0" && a(3) == "-1",
        "XRANGE" | "XREVRANGE" => !args.iter().any(|x| x == "COUNT"),
        "SCAN" | "HSCAN" | "SSCAN" | "ZSCAN" => !args.iter().any(|x| x == "COUNT"),
        _ => false,
    }
}

#[test]
#[ignore = "needs docker compose"]
fn browser_and_key_tabs_never_block_the_server() {
    const DB: u32 = 11;
    let rt = Runtime::new().unwrap();
    flush(&rt, DB);
    let mut c = raw(&rt, DB);
    let mut pipe = redis::pipe();
    for i in 0..1500 {
        pipe.cmd("SET").arg(format!("user:{i}")).arg(i).ignore();
        pipe.cmd("HSET")
            .arg("big:hash")
            .arg(format!("f{i}"))
            .arg(i)
            .ignore();
        pipe.cmd("SADD")
            .arg("big:set")
            .arg(format!("m{i}"))
            .ignore();
        pipe.cmd("ZADD")
            .arg("big:zset")
            .arg(i)
            .arg(format!("m{i}"))
            .ignore();
        pipe.cmd("RPUSH").arg("big:list").arg(i).ignore();
        pipe.cmd("XADD")
            .arg("big:stream")
            .arg("*")
            .arg("n")
            .arg(i)
            .ignore();
    }
    pipe.cmd("SET").arg("plain").arg("text").ignore();
    pipe.cmd("JSON.SET")
        .arg("doc")
        .arg("$")
        .arg(r#"{"a":1}"#)
        .ignore();
    pipe.cmd("BF.ADD").arg("bloom").arg("x").ignore();
    rt.block_on(pipe.query_async::<()>(&mut c)).unwrap();

    let d = open(&rt, DB);
    let r = d.as_redis().unwrap();
    let lines = monitored(&rt, || {
        let path = |p: &[&str]| ObjectPath(p.iter().map(|s| (*s).to_owned()).collect());
        let dbs = rt.block_on(d.children(&path(&[]))).unwrap();
        let db = dbs.iter().find(|n| n.name == format!("db{DB}")).unwrap();
        assert_eq!(db.detail, "1 508 keys");
        let top = rt
            .block_on(d.children(&path(&[&format!("db{DB}")])))
            .unwrap();
        assert!(top.iter().any(|n| n.name == "user:"), "{top:?}");
        let more = format!("{}keys", rk::MORE);
        assert!(top.iter().any(|n| n.name == more));
        rt.block_on(d.children(&path(&[&format!("db{DB}"), &more])))
            .unwrap();
        let user = rt
            .block_on(d.children(&path(&[&format!("db{DB}"), "user:"])))
            .unwrap();
        assert!(
            user.iter().any(|n| n.name == rk::MORE),
            "a namespace page ends in Load more"
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let stop = std::sync::atomic::AtomicBool::new(false);
        let end = rt.block_on(r.search(DB, "big:*", &stop, &tx)).unwrap();
        assert_eq!(end, deadlock_core::redis_driver::SearchEnd::Complete);
        drop(tx);
        let mut found = 0;
        while rx.try_recv().is_ok() {
            found += 1;
        }
        assert_eq!(found, 5);
        for (key, kind) in [
            ("big:hash", KeyType::Hash),
            ("big:set", KeyType::Set),
            ("big:zset", KeyType::ZSet),
            ("big:list", KeyType::List),
            ("big:stream", KeyType::Stream),
        ] {
            let f = rt.block_on(r.facts(key.as_bytes())).unwrap();
            assert_eq!(f.kind, kind);
            assert_eq!(f.len, Some(1500));
            assert!(f.memory.is_some());
            assert_eq!(f.pttl, -1);
            let rows = all_rows(&rt, &*d, key.as_bytes(), &kind, None);
            assert!(rows.len() >= 1500, "{key}: {}", rows.len());
        }
        let rows = all_rows(&rt, &*d, b"big:hash", &KeyType::Hash, Some("f1*"));
        assert_eq!(rows.len(), 1 + 10 + 100 + 500);
        let f = rt.block_on(r.facts(b"plain")).unwrap();
        assert_eq!(
            rt.block_on(r.text(b"plain", &f)).unwrap().value,
            Value::Text("text".into())
        );
        let f = rt.block_on(r.facts(b"doc")).unwrap();
        assert_eq!(f.kind, KeyType::Json);
        assert!(
            rt.block_on(r.text(b"doc", &f))
                .unwrap()
                .value
                .to_string()
                .contains("\"a\":1")
        );
        let f = rt.block_on(r.facts(b"bloom")).unwrap();
        assert!(matches!(f.kind, KeyType::Other(ref t) if t.starts_with("MBbloom")));
        assert!(f.dump_len.is_some_and(|n| n > 0));
    });
    let bad: Vec<&String> = lines
        .iter()
        .filter(|l| forbidden(&monitor_args(l)))
        .collect();
    assert!(
        lines.iter().any(|l| l.contains("\"SCAN\"")),
        "MONITOR saw the browser"
    );
    assert!(bad.is_empty(), "forbidden commands: {bad:?}");
    flush(&rt, DB);
}

/// The forbidden-command check catches what it must.
#[test]
fn the_monitor_check_knows_the_blocking_commands() {
    let f = |l: &str| forbidden(&monitor_args(l));
    assert!(f(r#"+1.0 [0 127.0.0.1:1] "KEYS" "*""#));
    assert!(f(r#"+1.0 [0 127.0.0.1:1] "hgetall" "k""#));
    assert!(f(r#"+1.0 [0 127.0.0.1:1] "LRANGE" "k" "0" "-1""#));
    assert!(f(r#"+1.0 [0 127.0.0.1:1] "XRANGE" "k" "-" "+""#));
    assert!(!f(r#"+1.0 [0 127.0.0.1:1] "LRANGE" "k" "0" "499""#));
    assert!(!f(r#"+1.0 [0 127.0.0.1:1] "SCAN" "0" "COUNT" "1000""#));
}

/// Loads `key`, applies `edit` to its pending changes, submits, and returns the
/// rows after a reload.
fn submit(
    rt: &Runtime,
    d: &dyn Driver,
    key: &[u8],
    kind: &KeyType,
    edit: impl FnOnce(&mut Pending, &[Row]),
) -> Vec<Row> {
    let r = d.as_redis().unwrap();
    rt.block_on(r.facts(key)).unwrap();
    let rows = all_rows(rt, d, key, kind, None);
    let mut p = Pending::default();
    edit(&mut p, &rows);
    let cmds = rk::commands(key, kind, &p, &rows, b"\0deadlock-deleted").unwrap();
    rt.block_on(r.transact(&cmds)).unwrap();
    all_rows(rt, d, key, kind, None)
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

#[test]
#[ignore = "needs docker compose"]
fn every_type_round_trips_binary_and_unicode() {
    const DB: u32 = 12;
    let rt = Runtime::new().unwrap();
    flush(&rt, DB);
    let mut c = raw(&rt, DB);
    let bin = vec![0u8, 0xff, b'\n', b'"'];
    let uni = "žluťoučký kůň 🎉 \"q\"";
    let key = "k:ü \u{1F600}".as_bytes();
    let _: () = cmd(&rt, &mut c, &[b"HSET", key, b"a", b"1", b"b", b"2"]);
    let d = open(&rt, DB);
    let set = |p: &mut Pending, r: usize, col: usize, v: Value, rows: &[Row]| {
        p.set(RowId::Loaded(r), col, Edit::Value(v), rows[r].get(col));
    };
    // Hash: update, rename, delete, add with binary and Unicode.
    let rows = submit(&rt, &*d, key, &KeyType::Hash, |p, rows| {
        let a = rows.iter().position(|r| r[0] == text("a")).unwrap();
        let b = 1 - a;
        set(p, a, 1, text(uni), rows);
        p.delete(&[RowId::Loaded(b)]);
        let n = p.add(2);
        p.set(n, 0, Edit::Value(Value::Bytes(bin.clone())), None);
        p.set(n, 1, Edit::Value(Value::Bytes(bin.clone())), None);
    });
    let mut got: Vec<(Value, Value)> = rows
        .into_iter()
        .map(|r| (r[0].clone(), r[1].clone()))
        .collect();
    got.sort_by_key(|(k, _)| k.to_string());
    assert_eq!(
        got,
        [
            (Value::Bytes(bin.clone()), Value::Bytes(bin.clone())),
            (text("a"), text(uni)),
        ]
    );
    // List: set, delete two, push head and tail.
    let _: () = cmd(&rt, &mut c, &[b"RPUSH", b"l", b"x", b"y", b"z", b"w"]);
    let rows = submit(&rt, &*d, b"l", &KeyType::List, |p, rows| {
        set(p, 0, 1, Value::Bytes(bin.clone()), rows);
        p.delete(&[RowId::Loaded(1), RowId::Loaded(3)]);
        let h = p.add(2);
        p.set(h, 0, Edit::Value(text(rk::HEAD)), None);
        p.set(h, 1, Edit::Value(text(uni)), None);
        let t = p.add(2);
        p.set(t, 0, Edit::Value(text(rk::TAIL)), None);
        p.set(t, 1, Edit::Value(text("end")), None);
    });
    let values: Vec<Value> = rows.into_iter().map(|r| r[1].clone()).collect();
    assert_eq!(
        values,
        [text(uni), Value::Bytes(bin.clone()), text("z"), text("end")]
    );
    // Set: rename a member, delete one, add a binary one.
    let _: () = cmd(&rt, &mut c, &[b"SADD", b"s", b"one", b"two"]);
    let rows = submit(&rt, &*d, b"s", &KeyType::Set, |p, rows| {
        let one = rows.iter().position(|r| r[0] == text("one")).unwrap();
        set(p, one, 0, text(uni), rows);
        p.delete(&[RowId::Loaded(1 - one)]);
        let n = p.add(1);
        p.set(n, 0, Edit::Value(Value::Bytes(bin.clone())), None);
    });
    let mut members: Vec<String> = rows.iter().map(|r| r[0].to_string()).collect();
    members.sort();
    let mut want = vec![Value::Bytes(bin.clone()).to_string(), uni.to_owned()];
    want.sort();
    assert_eq!(members, want);
    // Sorted set: change a score, rename, add with inf; ordered by score.
    let _: () = cmd(&rt, &mut c, &[b"ZADD", b"z", b"1", b"a", b"2", b"b"]);
    let rows = submit(&rt, &*d, b"z", &KeyType::ZSet, |p, rows| {
        set(p, 0, 1, Value::Float(5.5), rows);
        set(p, 1, 0, text(uni), rows);
        let n = p.add(2);
        p.set(n, 0, Edit::Value(Value::Bytes(bin.clone())), None);
        p.set(n, 1, Edit::Value(Value::Float(f64::NEG_INFINITY)), None);
    });
    assert_eq!(
        rows,
        [
            vec![Value::Bytes(bin.clone()), Value::Float(f64::NEG_INFINITY)],
            vec![text(uni), Value::Float(2.0)],
            vec![text("a"), Value::Float(5.5)],
        ]
    );
    // Stream: newest first; delete one, add one.
    let _: () = cmd(&rt, &mut c, &[b"XADD", b"x", b"1-1", b"t", b"a"]);
    let _: () = cmd(&rt, &mut c, &[b"XADD", b"x", b"2-1", b"t", b"b"]);
    let rows = submit(&rt, &*d, b"x", &KeyType::Stream, |p, rows| {
        assert_eq!(rows[0][0], text("2-1"));
        p.delete(&[RowId::Loaded(1)]);
        let n = p.add(2);
        p.set(
            n,
            1,
            Edit::Value(text(&format!(r#"{{"u":"{}"}}"#, uni.replace('"', "\\\"")))),
            None,
        );
    });
    assert_eq!(rows.len(), 2);
    assert!(rows[0][1].to_string().contains("žluťoučký"), "{rows:?}");
    assert_eq!(rows[1][0], text("2-1"));
    // String and JSON text, and the string keeps its expiry.
    let r = d.as_redis().unwrap();
    let _: () = cmd(&rt, &mut c, &[b"SET", b"str", b"old", b"EX", b"1000"]);
    rt.block_on(r.transact(&[rk::text_command(b"str", &KeyType::String, uni).unwrap()]))
        .unwrap();
    let f = rt.block_on(r.facts(b"str")).unwrap();
    assert!(f.pttl > 0, "KEEPTTL kept the expiry");
    assert_eq!(rt.block_on(r.text(b"str", &f)).unwrap().value, text(uni));
    let _: () = cmd(&rt, &mut c, &[b"SET", b"bin", &bin]);
    let f = rt.block_on(r.facts(b"bin")).unwrap();
    assert_eq!(
        rt.block_on(r.text(b"bin", &f)).unwrap().value,
        Value::Bytes(bin.clone())
    );
    let _: () = cmd(&rt, &mut c, &[b"JSON.SET", b"j", b"$", b"{}"]);
    rt.block_on(r.facts(b"j")).unwrap();
    rt.block_on(
        r.transact(&[rk::text_command(
            b"j",
            &KeyType::Json,
            &format!(r#"{{"name":"{uni}"}}"#).replace("\"q\"", "q"),
        )
        .unwrap()]),
    )
    .unwrap();
    let f = rt.block_on(r.facts(b"j")).unwrap();
    assert!(
        rt.block_on(r.text(b"j", &f))
            .unwrap()
            .value
            .to_string()
            .contains("žluťoučký")
    );
    flush(&rt, DB);
}

#[test]
#[ignore = "needs docker compose"]
fn a_concurrent_change_aborts_the_submit() {
    const DB: u32 = 13;
    let rt = Runtime::new().unwrap();
    flush(&rt, DB);
    let mut c = raw(&rt, DB);
    let _: () = cmd(&rt, &mut c, &[b"HSET", b"h", b"plan", b"free"]);
    let d = open(&rt, DB);
    let r = d.as_redis().unwrap();
    rt.block_on(r.facts(b"h")).unwrap();
    // Someone else changes the key after it loaded.
    let _: () = cmd(&rt, &mut c, &[b"HSET", b"h", b"plan", b"team"]);
    let e = rt
        .block_on(r.transact(&[Command::new("HSET").arg("h").arg("plan").value("pro")]))
        .unwrap_err()
        .to_string();
    assert!(e.contains("changed on the server"), "{e}");
    let v: String = cmd(&rt, &mut c, &[b"HGET", b"h", b"plan"]);
    assert_eq!(v, "team", "nothing was overwritten");
    // After a reload the submit goes through.
    rt.block_on(r.facts(b"h")).unwrap();
    rt.block_on(r.transact(&[Command::new("HSET").arg("h").arg("plan").value("pro")]))
        .unwrap();
    let v: String = cmd(&rt, &mut c, &[b"HGET", b"h", b"plan"]);
    assert_eq!(v, "pro");
    // A command refused inside the transaction: the others ran, and the error says so.
    rt.block_on(r.facts(b"h")).unwrap();
    let e = rt
        .block_on(r.transact(&[
            Command::new("HSET").arg("h").arg("x").value("1"),
            Command::new("LPUSH").arg("h").value("wrong type"),
        ]))
        .unwrap_err()
        .to_string();
    assert!(e.contains("no rollback") && e.contains("WRONGTYPE"), "{e}");
    flush(&rt, DB);
}

#[test]
#[ignore = "needs docker compose"]
fn read_only_connections_refuse_key_writes() {
    const DB: u32 = 14;
    let rt = Runtime::new().unwrap();
    flush(&rt, DB);
    let mut c = raw(&rt, DB);
    let _: () = cmd(&rt, &mut c, &[b"SET", b"k", b"v"]);
    let d = rt
        .block_on(connect(
            &ConnectionConfig {
                read_only: true,
                ..config(DB)
            },
            PASSWORD,
        ))
        .unwrap();
    let r = d.as_redis().unwrap();
    rt.block_on(r.facts(b"k")).unwrap();
    let set = Command::new("SET").arg("k").value("x");
    assert!(
        rt.block_on(r.transact(std::slice::from_ref(&set)))
            .unwrap_err()
            .to_string()
            .contains("read-only")
    );
    assert!(
        rt.block_on(r.run(&[set]))
            .unwrap_err()
            .to_string()
            .contains("read-only")
    );
    assert!(rt.block_on(r.run(&[Command::new("TTL").arg("k")])).is_ok());
    flush(&rt, DB);
}

#[test]
#[ignore = "needs docker compose"]
fn console_classifies_dangerous_commands_and_renders_resp3() {
    let rt = Runtime::new().unwrap();
    flush(&rt, 15);
    let d = open(&rt, 15);
    for (line, danger) in [
        ("FLUSHDB", true),
        ("FLUSHALL", true),
        ("CONFIG SET maxmemory 0", true),
        ("SHUTDOWN", true),
        ("DEBUG SLEEP 0", true),
        ("KEYS *", true),
        ("GET k", false),
        ("HSET k f v", false),
    ] {
        let s = rt.block_on(d.classify(line)).unwrap();
        assert_eq!(s.danger.is_some(), danger, "{line}");
    }
    let got = rt.block_on(async {
        d.execute("HSET h f v").await.unwrap();
        let s = d.execute("HGETALL h").await.unwrap();
        let cols: Vec<String> = s.columns.iter().map(|c| c.name.clone()).collect();
        (cols, s.collect().await.unwrap())
    });
    assert_eq!(got.0, ["Field", "Value"]);
    assert_eq!(got.1, [vec![text("f"), text("v")]]);
    let nested = rt.block_on(async {
        d.execute("XADD s 1-1 a b").await.unwrap();
        d.execute("XRANGE s - + COUNT 1")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap()
    });
    assert_eq!(nested[0][0], text(r#"1) 1) "1-1""#));
    flush(&rt, 15);
}

#[test]
#[ignore = "needs docker compose"]
fn acl_users_sign_in_and_see_only_their_rights() {
    let rt = Runtime::new().unwrap();
    let mut c = raw(&rt, 0);
    let _: () = cmd(
        &rt,
        &mut c,
        &[
            b"ACL",
            b"SETUSER",
            b"deadlock_acl",
            b"on",
            b">acl-pass",
            b"~*",
            b"+@all",
            b"-@dangerous",
        ],
    );
    let cfg = ConnectionConfig {
        user: "deadlock_acl".into(),
        ..config(15)
    };
    let d = rt.block_on(connect(&cfg, "acl-pass"));
    let wrong = rt
        .block_on(connect(&cfg, "nope"))
        .err()
        .map(|e| e.to_string());
    // The user's own rights apply: KEYS is @dangerous.
    let keys = d
        .as_ref()
        .ok()
        .map(|d| rt.block_on(async { d.execute("KEYS *").await.map(|_| ()) }));
    let _: () = cmd(&rt, &mut c, &[b"ACL", b"DELUSER", b"deadlock_acl"]);
    assert!(d.is_ok(), "{:?}", d.err().map(|e| e.to_string()));
    assert!(wrong.is_some_and(|e| e.contains("WRONGPASS") || e.contains("invalid")));
    let e = keys.unwrap().unwrap_err().to_string();
    assert!(e.contains("NOPERM") || e.contains("permission"), "{e}");
}

#[test]
#[ignore = "needs docker compose"]
fn tunnels_to_redis() {
    use deadlock_core::ssh::{self, KnownHosts, SshAuth, SshConfig};
    let rt = Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let known = KnownHosts {
        user: dir.path().join("user_known_hosts"),
        app: dir.path().join("app_known_hosts"),
    };
    let ssh_cfg = SshConfig {
        host: "127.0.0.1".into(),
        port: 2222,
        user: "deadlock".into(),
        auth: SshAuth::Password,
        key_file: String::new(),
    };
    if let Err(ssh::TunnelError::UnknownHost(key)) = rt.block_on(ssh::open(
        &ssh_cfg,
        "deadlock",
        "redis",
        6379,
        known.clone(),
    )) {
        ssh::trust(&key, &known).unwrap();
    }
    let tunnel = rt
        .block_on(ssh::open(&ssh_cfg, "deadlock", "redis", 6379, known))
        .unwrap();
    let cfg = ConnectionConfig {
        port: tunnel.local.port(),
        ..config(0)
    };
    let d = rt.block_on(connect(&cfg, PASSWORD)).unwrap();
    assert!(d.server_version().starts_with("Redis"));
    drop(d);
    drop(tunnel);
}

/// TLS against a server started for it: `DEADLOCK_TEST_REDIS_TLS=<port>`, with a
/// self-signed certificate (so Verify certificate must refuse it).
#[test]
#[ignore = "needs a TLS Redis on DEADLOCK_TEST_REDIS_TLS"]
fn tls_require_and_verify() {
    let Some(port) = std::env::var("DEADLOCK_TEST_REDIS_TLS")
        .ok()
        .and_then(|p| p.parse().ok())
    else {
        return;
    };
    let rt = Runtime::new().unwrap();
    let cfg = |e| ConnectionConfig {
        port,
        encryption: e,
        ..config(0)
    };
    use deadlock_core::connection::Encryption;
    let d = rt
        .block_on(connect(&cfg(Encryption::Require), PASSWORD))
        .unwrap();
    assert!(d.server_version().starts_with("Redis"));
    assert!(
        rt.block_on(connect(&cfg(Encryption::VerifyCertificate), PASSWORD))
            .is_err()
    );
    assert!(
        rt.block_on(connect(&cfg(Encryption::Off), PASSWORD))
            .is_err()
    );
}

/// A cluster node is refused with a message: `DEADLOCK_TEST_REDIS_CLUSTER=<port>`.
#[test]
#[ignore = "needs a cluster-mode Redis on DEADLOCK_TEST_REDIS_CLUSTER"]
fn cluster_nodes_are_refused_with_a_message() {
    let Some(port) = std::env::var("DEADLOCK_TEST_REDIS_CLUSTER")
        .ok()
        .and_then(|p| p.parse().ok())
    else {
        return;
    };
    let rt = Runtime::new().unwrap();
    let cfg = ConnectionConfig { port, ..config(0) };
    let e = rt
        .block_on(connect(&cfg, PASSWORD))
        .err()
        .unwrap()
        .to_string();
    assert!(e.contains("Redis Cluster") && e.contains("MOVED"), "{e}");
}

#[test]
#[ignore = "needs docker compose"]
fn clients_slow_log_and_configuration() {
    let rt = Runtime::new().unwrap();
    let d = open(&rt, 0);
    let mut c = raw(&rt, 0);
    // A client blocked in BLPOP shows as blocked; Unblock frees it.
    let mut blocker = raw(&rt, 0);
    let blocked = rt.spawn(async move {
        redis::cmd("BLPOP")
            .arg("deadlock:never")
            .arg(30)
            .query_async::<redis::Value>(&mut blocker)
            .await
    });
    std::thread::sleep(Duration::from_millis(200));
    let snap = rt.block_on(d.sessions()).unwrap();
    let own = snap.sessions.iter().find(|s| s.own).unwrap();
    assert!(own.deadlock && own.age.is_some());
    let b = snap
        .sessions
        .iter()
        .find(|s| s.state == deadlock_core::sessions::State::Blocked)
        .unwrap();
    assert!(b.statement.as_deref() == Some("blpop") && b.running);
    assert!(snap.slow_log.is_some() && snap.waits.is_empty());
    rt.block_on(d.cancel_statement(b.id)).unwrap();
    // UNBLOCK answers the blocked command as a timeout: nil.
    assert!(matches!(
        rt.block_on(blocked).unwrap(),
        Ok(redis::Value::Nil)
    ));
    assert!(rt.block_on(d.end_session(own.id)).is_err(), "never its own");
    // Slow log: an entry logged with a 0 µs threshold, then Reset empties it.
    let threshold: Vec<String> = cmd(
        &rt,
        &mut c,
        &[b"CONFIG", b"GET", b"slowlog-log-slower-than"],
    );
    let _: () = cmd(
        &rt,
        &mut c,
        &[b"CONFIG", b"SET", b"slowlog-log-slower-than", b"0"],
    );
    let _: () = cmd(&rt, &mut c, &[b"SET", b"deadlock:slow", b"x"]);
    let logged = rt.block_on(d.sessions()).unwrap().slow_log.unwrap();
    let _: () = cmd(
        &rt,
        &mut c,
        &[
            b"CONFIG",
            b"SET",
            b"slowlog-log-slower-than",
            threshold[1].as_bytes(),
        ],
    );
    assert!(
        logged
            .iter()
            .any(|e| e.command.starts_with("SET deadlock:slow")),
        "{logged:?}"
    );
    rt.block_on(d.reset_slow_log()).unwrap();
    let _: () = cmd(&rt, &mut c, &[b"DEL", b"deadlock:slow"]);
    // Configuration: CONFIG SET applies; CONFIG REWRITE fails without a config file.
    use deadlock_core::config::{ConfigChange, Mode, NewValue};
    let cfg = rt.block_on(d.config()).unwrap();
    assert!(cfg.redis && cfg.config_file.is_none());
    let p = cfg.param("slowlog-max-len").unwrap();
    let old = p.value.clone().unwrap();
    assert!(
        cfg.locked(cfg.param("databases").unwrap()).is_some(),
        "immutable"
    );
    assert_eq!(
        cfg.param("requirepass").unwrap().value.as_deref(),
        Some(rk::MASKED)
    );
    let change = |v: &str, mode| ConfigChange {
        name: "slowlog-max-len".into(),
        value: NewValue::Set(v.into()),
        mode,
    };
    let a = rt
        .block_on(d.apply_config(&cfg, &[change("200", Mode::RunOnly)]))
        .unwrap();
    assert_eq!((a.done, a.error), (1, None));
    let a = rt
        .block_on(d.apply_config(&cfg, &[change(&old, Mode::Persist)]))
        .unwrap();
    assert_eq!(a.done, 1);
    assert!(a.error.unwrap().contains("CONFIG REWRITE failed"));
    let now: Vec<String> = cmd(&rt, &mut c, &[b"CONFIG", b"GET", b"slowlog-max-len"]);
    assert_eq!(now[1], old, "restored");
}

/// Timings against the 1,000,000-key data set in db 0 (loaded by hand). Prints them.
#[test]
#[ignore = "needs the 1M-key data set in db 0"]
fn million_key_timings() {
    use std::time::Instant;
    let rt = Runtime::new().unwrap();
    let d = open(&rt, 0);
    let path = |p: &[&str]| ObjectPath(p.iter().map(|s| (*s).to_owned()).collect());
    let t = Instant::now();
    let dbs = rt.block_on(d.children(&path(&[]))).unwrap();
    let top = rt.block_on(d.children(&path(&["db0"]))).unwrap();
    println!(
        "first paint of db0 (INFO keyspace + DBSIZE + first SCAN batch with TYPE): {:?}, {} nodes, {}",
        t.elapsed(),
        top.len(),
        dbs[0].detail
    );
    let more = format!("{}keys", rk::MORE);
    let mut times = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        rt.block_on(d.children(&path(&["db0", &more]))).unwrap();
        rt.block_on(d.children(&path(&["db0"]))).unwrap();
        times.push(t.elapsed());
    }
    println!("load more (+1000 keys, re-level): {times:?}");
    let r = d.as_redis().unwrap();
    for pattern in ["*user:1002*", "user:100[2]", "nothing-matches-*"] {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let stop = std::sync::atomic::AtomicBool::new(false);
        let t = Instant::now();
        let end = rt.block_on(r.search(0, pattern, &stop, &tx)).unwrap();
        drop(tx);
        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        println!("search {pattern}: {:?}, {n} found, {end:?}", t.elapsed());
    }
    let t = Instant::now();
    let f = rt.block_on(r.facts(b"big:set")).unwrap();
    let first = rt.block_on(async {
        let mut s = r.rows(b"big:set", &f.kind, None).unwrap();
        s.next_page().await.unwrap().unwrap().len()
    });
    println!(
        "open big:set ({} members): first {first} rows in {:?}",
        f.len.unwrap_or(0),
        t.elapsed()
    );
}

/// Another client kills the app's: a read reconnects and runs again; the connection
/// keeps working.
#[test]
#[ignore = "needs docker compose"]
fn a_killed_client_reconnects() {
    let rt = Runtime::new().unwrap();
    let (app, screen) = (open(&rt, 10), open(&rt, 10));
    let rows =
        |d: &dyn Driver, line: &str| rt.block_on(async { d.execute(line).await?.collect().await });
    rows(&*app, "SET heal:k v").unwrap();
    let id = rows(&*app, "CLIENT ID").unwrap()[0][0].to_string();
    rows(&*screen, &format!("CLIENT KILL ID {id}")).unwrap();
    assert_eq!(rows(&*app, "GET heal:k").unwrap()[0][0].to_string(), "v");
    assert_ne!(rows(&*app, "CLIENT ID").unwrap()[0][0].to_string(), id);
    rows(&*app, "DEL heal:k").unwrap();
}
