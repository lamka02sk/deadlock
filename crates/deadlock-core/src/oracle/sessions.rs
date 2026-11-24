//! Sessions and locks on Oracle: `V$SESSION` with `V$SQL` and `V$TRANSACTION`, waits
//! from `BLOCKING_SESSION` and `ROW_WAIT_*`, table locks from `V$LOCK`. Without access
//! to the `V$` views a user sees only their own session.

use super::Connection;

use super::{col, num, ora_code, texts, transaction_open};
use crate::driver::{DriverError, Result};
use crate::sessions::{Lock, LockKind, LockWait, Session, SessionId, Snapshot, State, Transaction};

/// What a user needs to see other sessions.
const PRIVILEGE: &str = "SELECT ANY DICTIONARY";
/// The `MODULE` Deadlock's sessions set.
pub(super) const MODULE: &str = "Deadlock";

/// `V$LOCK` modes in words.
fn mode_words(mode: &str) -> &'static str {
    match mode {
        "1" => "null",
        "2" => "row share",
        "3" => "row exclusive",
        "4" => "share",
        "5" => "share row exclusive",
        "6" => "exclusive",
        _ => "no",
    }
}

fn no_access(e: &DriverError) -> bool {
    matches!(ora_code(e), Some(942 | 1031))
}

pub(super) fn snapshot(conn: &Connection, own: SessionId) -> Result<Snapshot> {
    let rows = match texts(
        conn,
        "SELECT TO_CHAR(s.SID), s.USERNAME, s.MACHINE, NVL(s.MODULE, s.PROGRAM), s.SCHEMANAME, \
           s.STATUS, s.EVENT, TO_CHAR(s.LAST_CALL_ET), s.SQL_ID, TO_CHAR(s.BLOCKING_SESSION), \
           w.OWNER, w.OBJECT_NAME, \
           CASE WHEN w.DATA_OBJECT_ID IS NOT NULL AND s.ROW_WAIT_ROW# >= 0 THEN \
             ROWIDTOCHAR(DBMS_ROWID.ROWID_CREATE(1, w.DATA_OBJECT_ID, s.ROW_WAIT_FILE#, \
               s.ROW_WAIT_BLOCK#, s.ROW_WAIT_ROW#)) END, \
           q.SQL_TEXT, TO_CHAR(t.START_DATE, 'YYYY-MM-DD HH24:MI:SS'), \
           TO_CHAR(ROUND((SYSDATE - t.START_DATE) * 86400)), \
           CASE WHEN s.STATE = 'WAITING' THEN TO_CHAR(ROUND(s.WAIT_TIME_MICRO / 1000000)) END, \
           s.MODULE \
         FROM V$SESSION s \
         LEFT JOIN V$TRANSACTION t ON t.ADDR = s.TADDR \
         LEFT JOIN V$SQL q ON q.SQL_ID = NVL(s.SQL_ID, s.PREV_SQL_ID) \
           AND q.CHILD_NUMBER = NVL2(s.SQL_ID, s.SQL_CHILD_NUMBER, s.PREV_CHILD_NUMBER) \
         LEFT JOIN DBA_OBJECTS w ON w.OBJECT_ID = s.ROW_WAIT_OBJ# AND s.BLOCKING_SESSION IS NOT NULL \
         WHERE s.TYPE = 'USER' ORDER BY s.SID",
        &[],
    ) {
        Ok(r) => r,
        Err(e) if no_access(&e) => return own_session(conn, own),
        Err(e) => return Err(e),
    };
    let mut snap = Snapshot::default();
    for r in &rows {
        let id: SessionId = num(r, 0).unwrap_or(0);
        let active = col(r, 5) == "ACTIVE";
        let blocker: Option<SessionId> = num(r, 9);
        let transaction = r.get(14).cloned().flatten().map(|started| Transaction {
            started,
            seconds: num(r, 15),
        });
        let state = match (active, blocker, &transaction) {
            (true, Some(_), _) => State::Waiting,
            (true, None, _) => State::Active,
            (false, _, Some(_)) => State::IdleInTransaction,
            (false, _, None) => State::Idle,
        };
        if let (true, Some(b)) = (active, blocker) {
            let wants = Lock {
                kind: LockKind::Row,
                mode: "exclusive".into(),
                schema: col(r, 10),
                table: col(r, 11),
                index: r.get(12).cloned().flatten().map(|_| "ROWID".to_owned()),
                row: r.get(12).cloned().flatten(),
                rows: 1,
            };
            snap.waits.push(LockWait {
                waiter: id,
                blocker: Some(b),
                wants,
                seconds: num(r, 16),
            });
        }
        snap.sessions.push(Session {
            id,
            user: col(r, 1),
            host: col(r, 2),
            application: r.get(3).cloned().flatten(),
            database: r.get(4).cloned().flatten(),
            state,
            detail: if active { col(r, 6) } else { "inactive".into() },
            seconds: num(r, 7),
            statement: r.get(13).cloned().flatten(),
            running: active && !col(r, 8).is_empty(),
            transaction,
            holds: Vec::new(),
            own: id == own,
            deadlock: col(r, 17) == MODULE,
            this_app: false,
            age: None,
        });
    }
    match texts(
        conn,
        "SELECT TO_CHAR(l.SID), TO_CHAR(l.LMODE), TO_CHAR(l.REQUEST), o.OWNER, o.OBJECT_NAME \
         FROM V$LOCK l JOIN DBA_OBJECTS o ON o.OBJECT_ID = l.ID1 WHERE l.TYPE = 'TM'",
        &[],
    ) {
        Ok(locks) => {
            for l in &locks {
                let sid: SessionId = num(l, 0).unwrap_or(0);
                let lock = |mode: &str| Lock {
                    kind: LockKind::Table,
                    mode: mode_words(mode).into(),
                    schema: col(l, 3),
                    table: col(l, 4),
                    index: None,
                    row: None,
                    rows: 1,
                };
                if col(l, 1) != "0"
                    && let Some(s) = snap.sessions.iter_mut().find(|s| s.id == sid)
                {
                    s.holds.push(lock(&col(l, 1)));
                }
                // A table lock asked for, where the session waits on no row.
                if col(l, 2) != "0"
                    && let Some(w) = snap.waits.iter_mut().find(|w| w.waiter == sid)
                    && w.wants.row.is_none()
                {
                    w.wants = lock(&col(l, 2));
                }
            }
        }
        Err(e) => snap.notes.push(format!("Table locks are not shown: {e}")),
    }
    if !can_alter_system(conn)? {
        snap.notes.push(
            "Cancel statement and End session need the ALTER SYSTEM privilege, which this \
             user does not have."
                .into(),
        );
    }
    Ok(snap)
}

/// Without the `V$` views: the user's own session, from the session context.
fn own_session(conn: &Connection, own: SessionId) -> Result<Snapshot> {
    let r = texts(
        conn,
        "SELECT SYS_CONTEXT('USERENV', 'SESSION_USER'), SYS_CONTEXT('USERENV', 'HOST'), \
           SYS_CONTEXT('USERENV', 'MODULE'), SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') FROM dual",
        &[],
    )?;
    let r = r.first().cloned().unwrap_or_default();
    let open = transaction_open(conn)?;
    Ok(Snapshot {
        sessions: vec![Session {
            id: own,
            user: col(&r, 0),
            host: col(&r, 1),
            application: r.get(2).cloned().flatten(),
            database: r.get(3).cloned().flatten(),
            state: State::Active,
            detail: "this screen".into(),
            seconds: None,
            statement: None,
            running: false,
            transaction: open.then(|| Transaction {
                started: "an earlier statement".into(),
                seconds: None,
            }),
            holds: Vec::new(),
            own: true,
            deadlock: true,
            this_app: true,
            age: None,
        }],
        waits: Vec::new(),
        own_only: Some(PRIVILEGE),
        notes: vec![format!(
            "Oracle lists sessions only to users who may read V$SESSION: this needs \
             {PRIVILEGE} or SELECT_CATALOG_ROLE. Shown is the session this screen uses."
        )],
        slow_log: None,
    })
}

fn can_alter_system(conn: &Connection) -> Result<bool> {
    Ok(!texts(
        conn,
        "SELECT 1 FROM SESSION_PRIVS WHERE PRIVILEGE = 'ALTER SYSTEM'",
        &[],
    )?
    .is_empty())
}

/// How many sessions wait on another now. 0 when the user may not see them.
pub(super) fn waiting(conn: &Connection) -> Result<usize> {
    match texts(
        conn,
        "SELECT TO_CHAR(COUNT(*)) FROM V$SESSION WHERE TYPE = 'USER' \
         AND BLOCKING_SESSION IS NOT NULL AND STATUS = 'ACTIVE'",
        &[],
    ) {
        Ok(r) => Ok(r.first().and_then(|r| num(r, 0)).unwrap_or(0)),
        Err(e) if no_access(&e) => Ok(0),
        Err(e) => Err(e),
    }
}

/// `'sid, serial#'` for `ALTER SYSTEM`: the serial number is read now, so a session
/// that ended since the screen's last look is not mistaken for a new one with its sid.
pub(super) fn address(conn: &Connection, id: SessionId) -> Result<String> {
    let sid = id.to_string();
    let rows = texts(
        conn,
        "SELECT TO_CHAR(SERIAL#) FROM V$SESSION WHERE SID = TO_NUMBER(:1)",
        &[&sid],
    )
    .map_err(|e| {
        if no_access(&e) {
            DriverError::Invalid(format!(
                "Oracle needs {PRIVILEGE} to look the session up, and ALTER SYSTEM to stop it"
            ))
        } else {
            e
        }
    })?;
    let serial = rows
        .first()
        .map(|r| col(r, 0))
        .ok_or_else(|| DriverError::Invalid(format!("session {id} has ended")))?;
    Ok(format!("'{id}, {serial}'"))
}

/// Oracle writes the deadlock graph of an ORA-00060 to a trace file on the server,
/// which SQL cannot read.
pub(super) const NO_DEADLOCK_REPORT: &str = "Oracle does not keep its last deadlock where \
    SQL can read it: when it detects one it rolls back one statement (ORA-00060) and writes \
    the deadlock graph to a trace file on the server. Ask the DBA for the alert log or the \
    trace file named there.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_modes_read_as_words() {
        assert_eq!(mode_words("3"), "row exclusive");
        assert_eq!(mode_words("6"), "exclusive");
        assert_eq!(mode_words("0"), "no");
    }
}
