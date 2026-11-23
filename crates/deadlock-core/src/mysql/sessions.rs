//! Sessions and locks on MySQL 8.4 and MariaDB 11. Every source but the session list
//! is optional: one that cannot be read becomes a note, never a failed screen.

use std::collections::HashMap;

use mysql_async::prelude::{FromRow, Queryable};
use mysql_async::{Conn, from_row_opt};

use crate::driver::Result;
use crate::sessions::{
    Lock, LockKind, LockWait, SessionId, Snapshot, State, Transaction, innodb_mode, mdl_blocks,
    mdl_mode,
};

/// `ER_SPECIFIC_ACCESS_DENIED_ERROR`: the `PROCESS` privilege is missing.
const NO_PRIVILEGE: u16 = 1227;
/// What a session waiting on a metadata lock says it does.
const MDL_WAIT: &str = "Waiting for table metadata lock";

/// Session list table: MySQL deprecates the `information_schema` one.
fn process_list(mariadb: bool) -> &'static str {
    if mariadb {
        "information_schema.PROCESSLIST"
    } else {
        "performance_schema.processlist"
    }
}

/// Rows as `T`. A row that does not fit (an unexpected NULL) is skipped: converting
/// it straight away would panic.
async fn optional<T: FromRow + Send + 'static>(
    conn: &mut Conn,
    sql: &str,
) -> std::result::Result<Vec<T>, mysql_async::Error> {
    let rows: Vec<mysql_async::Row> = conn.query(sql).await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| from_row_opt(r).ok())
        .collect())
}

fn server_code(e: &mysql_async::Error) -> Option<u16> {
    match e {
        mysql_async::Error::Server(s) => Some(s.code),
        _ => None,
    }
}

/// `` `db`.`table` `` → `(db, table)`.
fn split_table(name: &str) -> (String, String) {
    let clean = |s: &str| s.trim_matches('`').replace("``", "`");
    match name.split_once("`.`") {
        Some((db, t)) => (clean(db), clean(t)),
        None => (String::new(), clean(name)),
    }
}

fn secs(v: Option<i64>) -> Option<u64> {
    v.and_then(|n| u64::try_from(n).ok())
}

type ListRow = (
    u64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<String>,
);

pub(super) async fn snapshot(conn: &mut Conn, mariadb: bool, own: u32) -> Result<Snapshot> {
    let list: Vec<ListRow> = optional(
        conn,
        &format!(
            "SELECT ID, USER, HOST, DB, COMMAND, TIME, STATE, INFO FROM {} \
             WHERE COMMAND <> 'Daemon' ORDER BY ID",
            process_list(mariadb)
        ),
    )
    .await?;
    let mut snap = Snapshot::default();
    let mut notes = Vec::new();
    // Thread id → session id, for the performance_schema tables (MySQL).
    let threads: HashMap<u64, SessionId> = if mariadb {
        HashMap::new()
    } else {
        match optional::<(u64, u64)>(
            conn,
            "SELECT THREAD_ID, PROCESSLIST_ID FROM performance_schema.threads \
             WHERE PROCESSLIST_ID IS NOT NULL",
        )
        .await
        {
            Ok(rows) => rows.into_iter().collect(),
            Err(e) => {
                notes.push(format!(
                    "Applications, last statements and locks are not shown: {e}"
                ));
                HashMap::new()
            }
        }
    };
    let pid = |thread: u64| {
        if mariadb {
            Some(thread)
        } else {
            threads.get(&thread).copied()
        }
    };
    let apps: HashMap<SessionId, String> = optional::<(u64, String)>(
        conn,
        "SELECT PROCESSLIST_ID, ATTR_VALUE FROM performance_schema.session_connect_attrs \
         WHERE ATTR_NAME = 'program_name'",
    )
    .await
    .map(|rows| rows.into_iter().collect())
    .unwrap_or_default();
    let last: HashMap<SessionId, String> = optional::<(u64, String)>(
        conn,
        "SELECT THREAD_ID, SQL_TEXT FROM performance_schema.events_statements_current \
         WHERE SQL_TEXT IS NOT NULL",
    )
    .await
    .map(|rows| {
        rows.into_iter()
            .filter_map(|(t, s)| Some((pid(t)?, s)))
            .collect()
    })
    .unwrap_or_default();

    // `(session, trx id, started, seconds open, seconds waiting)`.
    type TrxRow = (u64, String, String, Option<i64>, Option<i64>);
    let trx: Vec<TrxRow> = match optional(
        conn,
        "SELECT trx_mysql_thread_id, CAST(trx_id AS CHAR), CAST(trx_started AS CHAR), \
         TIMESTAMPDIFF(SECOND, trx_started, NOW()), \
         TIMESTAMPDIFF(SECOND, trx_wait_started, NOW()) FROM information_schema.INNODB_TRX",
    )
    .await
    {
        Ok(rows) => rows,
        Err(e) if server_code(&e) == Some(NO_PRIVILEGE) => {
            snap.own_only = Some("PROCESS");
            Vec::new()
        }
        Err(e) => {
            notes.push(format!("Transactions are not shown: {e}"));
            Vec::new()
        }
    };
    let trx_session: HashMap<&str, SessionId> = trx.iter().map(|t| (t.1.as_str(), t.0)).collect();
    let row_wait: HashMap<SessionId, Option<u64>> = trx.iter().map(|t| (t.0, secs(t.4))).collect();

    for (id, user, host, db, command, time, state, info) in list {
        let command = command.unwrap_or_default();
        let idle = command == "Sleep";
        let transaction = trx.iter().find(|t| t.0 == id).map(|t| Transaction {
            started: t.2.clone(),
            seconds: secs(t.3),
        });
        let state_text = state.unwrap_or_default();
        let application = apps.get(&id).cloned();
        let running = info.is_some();
        snap.sessions.push(crate::sessions::Session {
            id,
            user: user.unwrap_or_default(),
            // `host:port` → `host`
            host: host
                .map(|h| match h.rsplit_once(':') {
                    Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => h.to_owned(),
                    _ => h,
                })
                .unwrap_or_default(),
            deadlock: application.as_deref() == Some(super::PROGRAM_NAME),
            age: None,
            application,
            database: db,
            state: match (idle, &transaction) {
                (false, _) => State::Active,
                (true, Some(_)) => State::IdleInTransaction,
                (true, None) => State::Idle,
            },
            detail: if state_text.is_empty() {
                command
            } else {
                format!("{command}: {state_text}")
            },
            seconds: secs(time),
            statement: info.or_else(|| last.get(&id).cloned()),
            running,
            transaction,
            holds: Vec::new(),
            own: id == u64::from(own),
        });
    }

    if snap.own_only.is_some() {
        // The one reason covers every source it hides.
        notes.clear();
        notes.push(
            "Without the PROCESS privilege the server shows only your own sessions, and \
             no transactions or locks."
                .into(),
        );
    } else if mariadb {
        mariadb_row_locks(conn, &trx_session, &row_wait, &mut snap, &mut notes).await;
        mariadb_metadata_locks(conn, &mut snap, &mut notes).await;
    } else if !threads.is_empty() {
        mysql_row_locks(conn, &pid, &row_wait, &mut snap, &mut notes).await;
        mysql_metadata_locks(conn, &pid, &mut snap, &mut notes).await;
    }
    // A session the server says waits on a metadata lock, without a known holder.
    for s in &snap.sessions {
        if s.detail.ends_with(MDL_WAIT) && !snap.waits.iter().any(|w| w.waiter == s.id) {
            snap.waits.push(LockWait {
                waiter: s.id,
                blocker: None,
                wants: metadata(String::new(), String::new(), ""),
                seconds: s.seconds,
            });
        }
    }
    // InnoDB locks live in a transaction; INNODB_TRX is a cache up to 0.1 s old and
    // may not list it yet.
    for s in &mut snap.sessions {
        if s.state == State::Idle && s.holds.iter().any(|l| l.kind != LockKind::Metadata) {
            s.state = State::IdleInTransaction;
        }
    }
    snap.mark_waiting();
    snap.notes = notes;
    Ok(snap)
}

fn metadata(schema: String, table: String, mode: &str) -> Lock {
    Lock {
        kind: LockKind::Metadata,
        mode: mdl_mode(mode),
        schema,
        table,
        index: None,
        row: None,
        rows: 1,
    }
}

/// Adds a held lock, counting rows of the same kind, mode and table together.
fn hold(snap: &mut Snapshot, id: SessionId, lock: Lock) {
    let Some(s) = snap.sessions.iter_mut().find(|s| s.id == id) else {
        return;
    };
    if let Some(h) = s.holds.iter_mut().find(|h| {
        h.kind == lock.kind
            && h.mode == lock.mode
            && h.schema == lock.schema
            && h.table == lock.table
            && h.index == lock.index
    }) {
        if lock.kind == LockKind::Row {
            h.rows += 1;
        }
        return;
    }
    s.holds.push(lock);
}

fn row_lock(
    schema: String,
    table: String,
    kind: &str,
    mode: &str,
    index: Option<String>,
    data: Option<String>,
) -> Lock {
    Lock {
        kind: if kind == "TABLE" {
            LockKind::Table
        } else {
            LockKind::Row
        },
        mode: innodb_mode(mode),
        schema,
        table,
        index,
        // The page's upper bound is no row a person would recognise.
        row: data.filter(|d| d != "supremum pseudo-record"),
        rows: 1,
    }
}

/// `(thread, schema, table, index, type, mode, status, data, lock id)`.
type DataLock = (
    u64,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    Option<String>,
    String,
);

async fn mysql_row_locks(
    conn: &mut Conn,
    pid: &impl Fn(u64) -> Option<SessionId>,
    row_wait: &HashMap<SessionId, Option<u64>>,
    snap: &mut Snapshot,
    notes: &mut Vec<String>,
) {
    // shortcut: at most 5000 row locks are read; a bulk update holds more, and its
    // Holds count stops there.
    let locks: Vec<DataLock> = match optional(
        conn,
        "SELECT THREAD_ID, OBJECT_SCHEMA, OBJECT_NAME, INDEX_NAME, LOCK_TYPE, LOCK_MODE, \
         LOCK_STATUS, LOCK_DATA, ENGINE_LOCK_ID FROM performance_schema.data_locks LIMIT 5000",
    )
    .await
    {
        Ok(l) => l,
        Err(e) => {
            notes.push(format!("Row locks are not shown: {e}"));
            return;
        }
    };
    let waits: Vec<(String, u64, u64)> = match optional(
        conn,
        "SELECT REQUESTING_ENGINE_LOCK_ID, REQUESTING_THREAD_ID, BLOCKING_THREAD_ID \
         FROM performance_schema.data_lock_waits",
    )
    .await
    {
        Ok(w) => w,
        Err(e) => {
            notes.push(format!("Row lock waits are not shown: {e}"));
            Vec::new()
        }
    };
    let by_id: HashMap<&str, &DataLock> = locks.iter().map(|l| (l.8.as_str(), l)).collect();
    for (lock_id, waiter, blocker) in &waits {
        let (Some(waiter), Some(l)) = (pid(*waiter), by_id.get(lock_id.as_str())) else {
            continue;
        };
        snap.waits.push(LockWait {
            waiter,
            blocker: pid(*blocker),
            wants: row_lock(
                l.1.clone(),
                l.2.clone(),
                &l.4,
                &l.5,
                l.3.clone(),
                l.7.clone(),
            ),
            seconds: row_wait.get(&waiter).copied().flatten(),
        });
    }
    for l in locks.into_iter().filter(|l| l.6 == "GRANTED") {
        if let Some(id) = pid(l.0) {
            hold(snap, id, row_lock(l.1, l.2, &l.4, &l.5, l.3, l.7));
        }
    }
}

async fn mysql_metadata_locks(
    conn: &mut Conn,
    pid: &impl Fn(u64) -> Option<SessionId>,
    snap: &mut Snapshot,
    notes: &mut Vec<String>,
) {
    // `(thread, schema, table, type, status)`. Only table locks: global and schema
    // locks (backups, FLUSH) are not explained.
    let rows: Vec<(u64, String, String, String, String)> = match optional(
        conn,
        "SELECT OWNER_THREAD_ID, OBJECT_SCHEMA, OBJECT_NAME, LOCK_TYPE, LOCK_STATUS \
         FROM performance_schema.metadata_locks WHERE OBJECT_TYPE = 'TABLE' \
         AND OBJECT_SCHEMA <> 'performance_schema' AND OBJECT_NAME NOT LIKE '#sql%'",
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            notes.push(format!("Metadata lock waits are not explained: {e}"));
            return;
        }
    };
    let rows: Vec<_> = rows
        .into_iter()
        .filter_map(|(t, db, table, ty, status)| Some((pid(t)?, db, table, ty, status)))
        .collect();
    for (waiter, db, table, ty, _) in rows.iter().filter(|r| r.4 == "PENDING") {
        let seconds = snap.session(*waiter).and_then(|s| s.seconds);
        for (other, _, _, oty, status) in rows
            .iter()
            .filter(|o| o.0 != *waiter && o.1 == *db && o.2 == *table)
        {
            if mdl_blocks(ty, oty, status == "GRANTED") {
                snap.waits.push(LockWait {
                    waiter: *waiter,
                    blocker: Some(*other),
                    wants: metadata(db.clone(), table.clone(), ty),
                    seconds,
                });
            }
        }
    }
    for (id, db, table, ty, status) in rows {
        if status == "GRANTED" {
            hold(snap, id, metadata(db, table, &ty));
        }
    }
}

async fn mariadb_row_locks(
    conn: &mut Conn,
    trx_session: &HashMap<&str, SessionId>,
    row_wait: &HashMap<SessionId, Option<u64>>,
    snap: &mut Snapshot,
    notes: &mut Vec<String>,
) {
    // `(lock id, trx id, mode, type, table, index, data)`: only locks in a wait.
    type InnoLock = (
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
    );
    let locks: Vec<InnoLock> = match optional(
        conn,
        "SELECT lock_id, CAST(lock_trx_id AS CHAR), lock_mode, lock_type, lock_table, \
         lock_index, lock_data FROM information_schema.INNODB_LOCKS",
    )
    .await
    {
        Ok(l) => l,
        Err(e) => {
            notes.push(format!("Row locks are not shown: {e}"));
            return;
        }
    };
    let waits: Vec<(String, String, String, String)> = match optional(
        conn,
        "SELECT CAST(requesting_trx_id AS CHAR), requested_lock_id, \
         CAST(blocking_trx_id AS CHAR), blocking_lock_id FROM information_schema.INNODB_LOCK_WAITS",
    )
    .await
    {
        Ok(w) => w,
        Err(e) => {
            notes.push(format!("Row lock waits are not shown: {e}"));
            return;
        }
    };
    let lock = |id: &str| {
        locks.iter().find(|l| l.0 == id).map(|l| {
            let (db, table) = split_table(&l.4);
            row_lock(db, table, &l.3, &l.2, l.5.clone(), l.6.clone())
        })
    };
    for (wtrx, wlock, btrx, block) in &waits {
        let (Some(&waiter), Some(wants)) = (trx_session.get(wtrx.as_str()), lock(wlock)) else {
            continue;
        };
        let blocker = trx_session.get(btrx.as_str()).copied();
        snap.waits.push(LockWait {
            waiter,
            blocker,
            wants,
            seconds: row_wait.get(&waiter).copied().flatten(),
        });
        if let (Some(b), Some(held)) = (blocker, lock(block)) {
            hold(snap, b, held);
        }
    }
}

async fn mariadb_metadata_locks(conn: &mut Conn, snap: &mut Snapshot, notes: &mut Vec<String>) {
    // `(session, mode, schema, table)`: granted locks only; the plugin lists no waits.
    let rows: Vec<(u64, String, String, String)> = match optional(
        conn,
        "SELECT THREAD_ID, LOCK_MODE, TABLE_SCHEMA, TABLE_NAME \
         FROM information_schema.METADATA_LOCK_INFO WHERE LOCK_TYPE = 'Table metadata lock'",
    )
    .await
    {
        Ok(r) => r,
        Err(_) => {
            // Only tell when someone waits on one.
            if snap.sessions.iter().any(|s| s.detail.ends_with(MDL_WAIT)) {
                notes.push(
                    "Metadata lock waits are not explained: the metadata_lock_info plugin is \
                     not loaded (INSTALL SONAME 'metadata_lock_info')."
                        .into(),
                );
            }
            return;
        }
    };
    // A waiting session that already holds an upgradable lock on a table (ALTER) waits
    // to make it exclusive; others' locks on that table block it. Anyone else waiting
    // gets no named holder.
    let waiting: Vec<(SessionId, Option<u64>)> = snap
        .sessions
        .iter()
        .filter(|s| s.detail.ends_with(MDL_WAIT))
        .map(|s| (s.id, s.seconds))
        .collect();
    for (waiter, seconds) in waiting {
        for (_, _, db, table) in rows.iter().filter(|r| {
            r.0 == waiter
                && matches!(
                    r.1.as_str(),
                    "MDL_SHARED_UPGRADABLE" | "MDL_SHARED_NO_WRITE" | "MDL_SHARED_NO_READ_WRITE"
                )
        }) {
            for (other, mode, ..) in rows
                .iter()
                .filter(|o| o.0 != waiter && o.2 == *db && o.3 == *table)
            {
                if mdl_blocks("EXCLUSIVE", mode, true) {
                    snap.waits.push(LockWait {
                        waiter,
                        blocker: Some(*other),
                        wants: metadata(db.clone(), table.clone(), "EXCLUSIVE"),
                        seconds,
                    });
                }
            }
        }
    }
    for (id, mode, db, table) in rows {
        hold(snap, id, metadata(db, table, &mode));
    }
}

/// Sessions waiting on a lock: row lock waits from InnoDB, plus any session the
/// server says waits for a lock (metadata, table, global). Two cheap queries.
pub(super) async fn waiting(conn: &mut Conn, mariadb: bool) -> Result<usize> {
    let listed: Option<u64> = conn
        .query_first(format!(
            "SELECT COUNT(*) FROM {} WHERE STATE LIKE 'Waiting for%lock'",
            process_list(mariadb)
        ))
        .await?;
    let rows: Option<u64> = conn
        .query_first(
            "SELECT COUNT(*) FROM information_schema.INNODB_TRX WHERE trx_state = 'LOCK WAIT'",
        )
        .await
        .unwrap_or_default();
    usize::try_from(listed.unwrap_or(0) + rows.unwrap_or(0))
        .map_err(|_| crate::driver::DriverError::Invalid("too many sessions".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mariadb_lock_tables_split() {
        assert_eq!(
            split_table("`deadlock`.`p9_orders`"),
            ("deadlock".into(), "p9_orders".into())
        );
        assert_eq!(split_table("`a``b`.`c`"), ("a`b".into(), "c".into()));
    }
}
