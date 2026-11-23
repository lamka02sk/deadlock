//! The data editor on Oracle: what a table's columns hold, which key identifies its
//! rows (primary key, unique key, else `ROWID`), and submitting changes.

use super::Connection;

use super::schema::type_label;
use super::{col, num, texts};
use crate::driver::{DriverError, Result};
use crate::edit::{Change, ColumnInfo, TableInfo, TableRef, pick_key};

/// The edit kind of an Oracle type: the [`crate::edit::parse_input`] name whose checks
/// and literals fit it. `r` holds `DATA_TYPE, DATA_LENGTH, DATA_PRECISION, DATA_SCALE`
/// from `at`.
pub(super) fn edit_kind(r: &[Option<String>], at: usize) -> String {
    let ty = col(r, at);
    let has_precision = !col(r, at + 2).is_empty();
    match ty.as_str() {
        "NUMBER" if has_precision => "decimal",
        "NUMBER" | "FLOAT" => "number",
        "BINARY_DOUBLE" => "double",
        "BINARY_FLOAT" => "float",
        "DATE" => "datetime",
        "VARCHAR2" | "NVARCHAR2" => "varchar",
        "CHAR" | "NCHAR" => "char",
        "RAW" => "varbinary",
        "BLOB" | "LONG RAW" => "longblob",
        "CLOB" | "NCLOB" | "LONG" => "longtext",
        "JSON" => "json",
        t if t.starts_with("TIMESTAMP") && t.contains("TIME ZONE") => "timestamp with time zone",
        t if t.starts_with("TIMESTAMP") => "timestamp",
        t => return t.to_ascii_lowercase(),
    }
    .to_owned()
}

pub(super) fn table_info(conn: &Connection, t: &TableRef) -> Result<TableInfo> {
    let (owner, name) = (&t.database, &t.table);
    let kind = texts(
        conn,
        "SELECT 'TABLE', TO_CHAR(NUM_ROWS), \
           (SELECT 'Y' FROM ALL_EXTERNAL_TABLES e WHERE e.OWNER = t.OWNER AND e.TABLE_NAME = t.TABLE_NAME), \
           (SELECT 'Y' FROM ALL_MVIEWS m WHERE m.OWNER = t.OWNER AND m.MVIEW_NAME = t.TABLE_NAME) \
         FROM ALL_TABLES t WHERE OWNER = :1 AND TABLE_NAME = :2 \
         UNION ALL SELECT 'VIEW', NULL, NULL, NULL FROM ALL_VIEWS WHERE OWNER = :3 AND VIEW_NAME = :4",
        &[owner, name, owner, name],
    )?;
    let Some(kind) = kind.first() else {
        return Err(DriverError::Invalid(format!(
            "{owner}.{name} does not exist, or you may not read it"
        )));
    };
    let rows = texts(
        conn,
        "SELECT c.COLUMN_NAME, c.DATA_TYPE, TO_CHAR(c.DATA_LENGTH), TO_CHAR(c.DATA_PRECISION), \
           TO_CHAR(c.DATA_SCALE), TO_CHAR(c.CHAR_LENGTH), c.CHAR_USED, c.NULLABLE, \
           c.VIRTUAL_COLUMN, i.GENERATION_TYPE \
         FROM ALL_TAB_COLS c LEFT JOIN ALL_TAB_IDENTITY_COLS i ON i.OWNER = c.OWNER \
           AND i.TABLE_NAME = c.TABLE_NAME AND i.COLUMN_NAME = c.COLUMN_NAME \
         WHERE c.OWNER = :1 AND c.TABLE_NAME = :2 AND c.HIDDEN_COLUMN = 'NO' \
         ORDER BY c.COLUMN_ID",
        &[owner, name],
    )?;
    let keys = texts(
        conn,
        // A unique constraint and its index share a name: UNION keeps one of each part.
        "SELECT DECODE(c.CONSTRAINT_TYPE, 'P', 'PRIMARY', c.CONSTRAINT_NAME), cc.COLUMN_NAME, \
           cc.POSITION \
         FROM ALL_CONSTRAINTS c JOIN ALL_CONS_COLUMNS cc ON cc.OWNER = c.OWNER \
           AND cc.CONSTRAINT_NAME = c.CONSTRAINT_NAME AND cc.TABLE_NAME = c.TABLE_NAME \
         WHERE c.OWNER = :1 AND c.TABLE_NAME = :2 AND c.CONSTRAINT_TYPE IN ('P', 'U') \
         UNION \
         SELECT ic.INDEX_NAME, ic.COLUMN_NAME, ic.COLUMN_POSITION FROM ALL_INDEXES ix \
         JOIN ALL_IND_COLUMNS ic ON ic.INDEX_OWNER = ix.OWNER AND ic.INDEX_NAME = ix.INDEX_NAME \
         WHERE ix.TABLE_OWNER = :3 AND ix.TABLE_NAME = :4 AND ix.UNIQUENESS = 'UNIQUE' \
         ORDER BY 1, 3",
        &[owner, name, owner, name],
    )?;
    let mut columns: Vec<ColumnInfo> = rows
        .iter()
        .map(|r| ColumnInfo {
            name: col(r, 0),
            data_type: edit_kind(r, 1),
            column_type: type_label(r, 1),
            nullable: col(r, 7) == "Y",
            auto_increment: !col(r, 9).is_empty(),
            // An identity column GENERATED ALWAYS takes no values either.
            generated: col(r, 8) == "YES" || col(r, 9) == "ALWAYS",
            pseudo: false,
        })
        .collect();
    let indexes: Vec<_> = keys
        .iter()
        .map(|r| (col(r, 0), false, Some(col(r, 1))))
        .collect();
    let view = kind.first().cloned().flatten().as_deref() == Some("VIEW") || col(kind, 3) == "Y";
    let external = col(kind, 2) == "Y";
    let mut key = if view || external {
        None
    } else {
        pick_key(&columns, &indexes)
    };
    if key.is_none() && !view && !external {
        columns.push(ColumnInfo {
            name: "ROWID".into(),
            data_type: "rowid".into(),
            column_type: "rowid".into(),
            pseudo: true,
            ..ColumnInfo::default()
        });
        key = Some(vec![columns.len() - 1]);
    }
    Ok(TableInfo {
        columns,
        key,
        view,
        estimated_rows: num(kind, 1),
        backslash_escapes: false,
        oracle: true,
        read_only: external
            .then_some("External tables open read-only: Oracle cannot change their rows."),
    })
}

/// Runs `changes` behind a savepoint; each must touch exactly one row, or every one is
/// undone. `auto`: commits at the end (or rolls back).
pub(super) fn apply(conn: &Connection, changes: &[Change], auto: bool) -> Result<()> {
    conn.execute("SAVEPOINT deadlock_submit", &[])?;
    let undo = |e: DriverError| {
        let undone = if auto {
            conn.rollback()
        } else {
            conn.execute("ROLLBACK TO SAVEPOINT deadlock_submit", &[])
                .map(|_| ())
        };
        match undone {
            Ok(()) => e,
            Err(u) => DriverError::Invalid(format!("{e} Undoing it failed too: {u}")),
        }
    };
    for (i, c) in changes.iter().enumerate() {
        let outcome = match conn.execute(&c.sql, &[]) {
            Ok(1) => Ok(()),
            Ok(n) => Err(DriverError::Invalid(format!(
                "change {} of {} touched {} rows instead of 1: someone else may have changed \
                 or removed the row. Nothing was submitted.",
                i + 1,
                changes.len(),
                n
            ))),
            Err(e) => Err(DriverError::Invalid(format!(
                "change {} of {} failed: {e}. Nothing was submitted.",
                i + 1,
                changes.len()
            ))),
        };
        if let Err(e) = outcome {
            return Err(undo(e));
        }
    }
    if auto && let Err(e) = conn.commit() {
        return Err(undo(e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oracle_types_map_to_edit_kinds() {
        let k = |ty: &str, precision: &str| {
            edit_kind(
                &[
                    Some(ty.into()),
                    Some("22".into()),
                    Some(precision.into()).filter(|p: &String| !p.is_empty()),
                ],
                0,
            )
        };
        assert_eq!(k("NUMBER", "10"), "decimal");
        assert_eq!(k("NUMBER", ""), "number");
        assert_eq!(k("DATE", ""), "datetime");
        assert_eq!(k("TIMESTAMP(6)", ""), "timestamp");
        assert_eq!(
            k("TIMESTAMP(6) WITH LOCAL TIME ZONE", ""),
            "timestamp with time zone"
        );
        assert_eq!(k("RAW", ""), "varbinary");
        assert_eq!(k("BLOB", ""), "longblob");
        assert_eq!(k("CLOB", ""), "longtext");
        assert_eq!(
            k("INTERVAL DAY(2) TO SECOND(6)", ""),
            "interval day(2) to second(6)"
        );
    }
}
