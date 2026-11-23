//! Explain plans on Oracle. Estimated: `EXPLAIN PLAN` into `PLAN_TABLE`, read back as
//! rows and through `DBMS_XPLAN.DISPLAY`. Measured: the statement runs with
//! `GATHER_PLAN_STATISTICS`, then its cursor's `V$SQL_PLAN_STATISTICS_ALL` rows and
//! `DBMS_XPLAN.DISPLAY_CURSOR` text are read.

use super::Connection;

use super::{col, literal, ora_code, texts, transaction_open};
use crate::driver::{DriverError, Result};
use crate::plan::{self, Plan};

/// The step columns both sources share, as `JSON_OBJECT` keys. An adaptive plan holds
/// steps the run did not use: the plan's display map (in `OTHER_XML`) skips them and
/// renumbers the rest, as `DBMS_XPLAN` shows them.
const STEP: &str = "'id' VALUE NVL(m.dis, p.ID), \
    'parent_id' VALUE CASE WHEN p.ID > 0 THEN NVL(m.par, p.PARENT_ID) END, \
    'operation' VALUE p.OPERATION, 'options' VALUE p.OPTIONS, 'object_owner' VALUE p.OBJECT_OWNER, \
    'object_name' VALUE p.OBJECT_NAME, 'object_alias' VALUE p.OBJECT_ALIAS, \
    'object_type' VALUE p.OBJECT_TYPE, 'cost' VALUE p.COST, 'cardinality' VALUE p.CARDINALITY, \
    'bytes' VALUE p.BYTES, 'cpu_cost' VALUE p.CPU_COST, 'io_cost' VALUE p.IO_COST, \
    'time' VALUE p.TIME, 'access_predicates' VALUE p.ACCESS_PREDICATES, \
    'filter_predicates' VALUE p.FILTER_PREDICATES, 'partition_start' VALUE p.PARTITION_START, \
    'partition_stop' VALUE p.PARTITION_STOP, 'qblock_name' VALUE p.QBLOCK_NAME";

/// Joins the display map read from `SOURCE`'s `OTHER_XML`.
const DISPLAY_MAP: &str = "LEFT JOIN XMLTABLE('/other_xml/display_map/row' PASSING \
    (SELECT XMLTYPE(x.OTHER_XML) FROM SOURCE x WHERE KEY AND x.OTHER_XML IS NOT NULL \
    AND ROWNUM = 1) COLUMNS op NUMBER PATH '@op', dis NUMBER PATH '@dis', \
    par NUMBER PATH '@par', skp NUMBER PATH '@skp') m ON m.op = p.ID";

/// The plan rows of `EXPLAIN PLAN SET STATEMENT_ID = KEY_1`, one JSON object each.
pub const ESTIMATED_ROWS: &str = "SELECT JSON_OBJECT(STEP_COLUMNS ABSENT ON NULL RETURNING VARCHAR2(4000)) \
    FROM PLAN_TABLE p MAP_JOIN \
    WHERE p.STATEMENT_ID = KEY_1 AND NVL(m.skp, 0) = 0 ORDER BY NVL(m.dis, p.ID)";

/// The last run's plan rows of cursor `KEY_1` / `KEY_2`, one JSON object each.
pub const MEASURED_ROWS: &str = "SELECT JSON_OBJECT(STEP_COLUMNS, \
    'starts' VALUE p.LAST_STARTS, 'output_rows' VALUE p.LAST_OUTPUT_ROWS, \
    'elapsed_us' VALUE p.LAST_ELAPSED_TIME, 'buffer_gets' VALUE p.LAST_CR_BUFFER_GETS, \
    'disk_reads' VALUE p.LAST_DISK_READS, 'memory' VALUE p.LAST_MEMORY_USED ABSENT ON NULL \
    RETURNING VARCHAR2(4000)) FROM V$SQL_PLAN_STATISTICS_ALL p MAP_JOIN \
    WHERE p.SQL_ID = KEY_1 AND p.CHILD_NUMBER = KEY_2 AND NVL(m.skp, 0) = 0 \
    ORDER BY NVL(m.dis, p.ID)";

/// What a measured plan reads, for the refusal when it may not.
const MEASURE_PRIVILEGE: &str = "Measured plans read V$SESSION, V$SQL and \
    V$SQL_PLAN_STATISTICS_ALL, which this user may not: it needs SELECT_CATALOG_ROLE (or \
    SELECT ANY DICTIONARY). The estimated plan needs no privilege.";

/// `template` with `keys` in place of `KEY_1`, `KEY_2`, as quoted literals.
fn sql(template: &str, keys: &[&str]) -> String {
    let map = if template.contains("PLAN_TABLE p") {
        DISPLAY_MAP
            .replace("SOURCE", "PLAN_TABLE")
            .replace("KEY", "x.STATEMENT_ID = KEY_1")
    } else {
        DISPLAY_MAP
            .replace("SOURCE", "V$SQL_PLAN")
            .replace("KEY", "x.SQL_ID = KEY_1 AND x.CHILD_NUMBER = KEY_2")
    };
    let mut out = template
        .replace("STEP_COLUMNS", STEP)
        .replace("MAP_JOIN", &map);
    for (i, k) in keys.iter().enumerate() {
        out = out.replace(&format!("KEY_{}", i + 1), &literal(k));
    }
    out
}

/// The rows' JSON objects as one array. (Not `JSON_ARRAYAGG`: a CLOB result trips the
/// driver, see [`super::literal`].)
fn json_array(rows: &[Vec<Option<String>>]) -> String {
    let objects: Vec<String> = rows.iter().map(|r| col(r, 0)).collect();
    format!("[{}]", objects.join(","))
}

/// Text lines of a `DBMS_XPLAN` table function call.
fn xplan(conn: &Connection, call: &str, params: &[&dyn super::conn::Bind]) -> Result<String> {
    let rows = texts(
        conn,
        &format!("SELECT PLAN_TABLE_OUTPUT FROM TABLE({call})"),
        params,
    )?;
    Ok(rows
        .iter()
        .map(|r| col(r, 0))
        .collect::<Vec<_>>()
        .join("\n"))
}

pub(super) fn estimated(conn: &Connection, statement: &str) -> Result<Plan> {
    // EXPLAIN PLAN writes PLAN_TABLE, which opens a transaction: it is undone after.
    let was_open = transaction_open(conn)?;
    conn.execute("SAVEPOINT deadlock_explain", &[])?;
    let result = (|| {
        conn.execute(
            &format!("EXPLAIN PLAN SET STATEMENT_ID = 'deadlock' FOR {statement}"),
            &[],
        )?;
        let rows = texts(conn, &sql(ESTIMATED_ROWS, &["deadlock"]), &[])?;
        let raw = xplan(
            conn,
            "DBMS_XPLAN.DISPLAY('PLAN_TABLE', :1, 'TYPICAL')",
            &[&"deadlock"],
        )?;
        Ok::<_, DriverError>((rows, raw))
    })();
    if was_open {
        conn.execute("ROLLBACK TO SAVEPOINT deadlock_explain", &[])?;
    } else {
        conn.rollback()?;
    }
    let (rows, raw) = result?;
    let json = json_array(&rows);
    plan::oracle(&json, false, raw).map_err(DriverError::Invalid)
}

/// Runs `statement` with run statistics on and reads its cursor's plan. `auto`:
/// commits what it changed, as a run would.
pub(super) fn measured(conn: &Connection, statement: &str, auto: bool) -> Result<Plan> {
    // Refused before anything runs: a write must not happen without its plan.
    for view in ["V$SESSION", "V$SQL_PLAN_STATISTICS_ALL"] {
        if let Err(e) = texts(conn, &format!("SELECT 1 FROM {view} WHERE ROWNUM = 0"), &[]) {
            return Err(match ora_code(&e) {
                Some(942 | 1031) => DriverError::Invalid(MEASURE_PRIVILEGE.into()),
                _ => e,
            });
        }
    }
    let (text, hinted) = with_hint(statement);
    if !hinted {
        conn.execute("ALTER SESSION SET STATISTICS_LEVEL = ALL", &[])?;
    }
    let ran = run_to_end(conn, &text);
    let cursor = ran.and_then(|()| {
        texts(
            conn,
            "SELECT PREV_SQL_ID, TO_CHAR(PREV_CHILD_NUMBER) FROM V$SESSION \
             WHERE SID = SYS_CONTEXT('USERENV', 'SID')",
            &[],
        )
    });
    if !hinted {
        conn.execute("ALTER SESSION SET STATISTICS_LEVEL = TYPICAL", &[])?;
    }
    let cursor = cursor?;
    if auto {
        conn.commit()?;
    }
    let (sql_id, child) = cursor
        .first()
        .map(|r| (col(r, 0), col(r, 1)))
        .ok_or_else(|| {
            DriverError::Invalid("the server did not name the statement's cursor".into())
        })?;
    let rows = texts(conn, &sql(MEASURED_ROWS, &[&sql_id, &child]), &[])?;
    let raw = xplan(
        conn,
        "DBMS_XPLAN.DISPLAY_CURSOR(:1, TO_NUMBER(:2), 'ALLSTATS LAST')",
        &[&sql_id, &child],
    )?;
    let json = json_array(&rows);
    plan::oracle(&json, true, raw).map_err(DriverError::Invalid)
}

/// Runs `text` and reads every row, so each step's statistics are complete.
fn run_to_end(conn: &Connection, text: &str) -> Result<()> {
    conn.stream(text, &[], &mut |_| true, &mut |_| true)?;
    Ok(())
}

/// `statement` with `/*+ GATHER_PLAN_STATISTICS */` after its first keyword, when that
/// keyword starts the query block the hint applies to; otherwise unchanged
/// (`WITH …`), and the caller turns statistics on for the session.
fn with_hint(statement: &str) -> (String, bool) {
    let t = statement.trim_start();
    let end = t
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(t.len());
    let verb = t[..end].to_ascii_uppercase();
    if matches!(
        verb.as_str(),
        "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "MERGE"
    ) {
        (
            format!("{} /*+ GATHER_PLAN_STATISTICS */{}", &t[..end], &t[end..]),
            true,
        )
    } else {
        (statement.to_owned(), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_go_after_the_first_keyword() {
        assert_eq!(
            with_hint("  select * from t"),
            ("select /*+ GATHER_PLAN_STATISTICS */ * from t".into(), true)
        );
        assert!(
            with_hint("UPDATE t SET a = 1")
                .0
                .starts_with("UPDATE /*+ GATHER")
        );
        assert!(!with_hint("WITH x AS (SELECT 1 FROM dual) SELECT * FROM x").1);
        assert!(
            sql(ESTIMATED_ROWS, &["it's"])
                .contains("FROM PLAN_TABLE x WHERE x.STATEMENT_ID = 'it''s'")
        );
        assert!(
            sql(MEASURED_ROWS, &["abc", "0"]).contains("p.SQL_ID = 'abc' AND p.CHILD_NUMBER = '0'")
        );
    }
}
