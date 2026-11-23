//! The Structure view on Oracle: columns, indexes, constraints and facts from the
//! `ALL_*` views, the DDL from `DBMS_METADATA`.

use std::collections::HashSet;

use super::Connection;

use super::schema::type_label;
use super::{col, literal, num, texts};
use crate::driver::{DriverError, Result};
use crate::edit::TableRef;
use crate::structure::{
    Check, ForeignKey, Generated, Index, IndexPart, StructureColumn, TableFacts, TableStructure,
};

pub(super) fn structure(conn: &Connection, t: &TableRef) -> Result<TableStructure> {
    let (owner, name) = (&t.database, &t.table);
    let table = texts(
        conn,
        "SELECT t.TABLESPACE_NAME, TO_CHAR(t.NUM_ROWS), c.COMMENTS, \
           (SELECT 'Y' FROM ALL_MVIEWS m WHERE m.OWNER = t.OWNER AND m.MVIEW_NAME = t.TABLE_NAME) \
         FROM ALL_TABLES t LEFT JOIN ALL_TAB_COMMENTS c ON c.OWNER = t.OWNER \
           AND c.TABLE_NAME = t.TABLE_NAME \
         WHERE t.OWNER = :1 AND t.TABLE_NAME = :2 \
         UNION ALL SELECT NULL, NULL, c.COMMENTS, 'V' FROM ALL_VIEWS v \
         LEFT JOIN ALL_TAB_COMMENTS c ON c.OWNER = v.OWNER AND c.TABLE_NAME = v.VIEW_NAME \
         WHERE v.OWNER = :3 AND v.VIEW_NAME = :4",
        &[owner, name, owner, name],
    )?;
    let Some(table) = table.first() else {
        return Err(DriverError::Invalid(format!(
            "{owner}.{name} does not exist, or you may not read it"
        )));
    };
    let (materialized, view) = (col(table, 3) == "Y", col(table, 3) == "V");
    let columns = texts(
        conn,
        "SELECT c.COLUMN_NAME, c.DATA_TYPE, TO_CHAR(c.DATA_LENGTH), TO_CHAR(c.DATA_PRECISION), \
           TO_CHAR(c.DATA_SCALE), TO_CHAR(c.CHAR_LENGTH), c.CHAR_USED, c.NULLABLE, \
           c.DATA_DEFAULT, c.VIRTUAL_COLUMN, i.GENERATION_TYPE, m.COMMENTS \
         FROM ALL_TAB_COLS c \
         LEFT JOIN ALL_TAB_IDENTITY_COLS i ON i.OWNER = c.OWNER AND i.TABLE_NAME = c.TABLE_NAME \
           AND i.COLUMN_NAME = c.COLUMN_NAME \
         LEFT JOIN ALL_COL_COMMENTS m ON m.OWNER = c.OWNER AND m.TABLE_NAME = c.TABLE_NAME \
           AND m.COLUMN_NAME = c.COLUMN_NAME \
         WHERE c.OWNER = :1 AND c.TABLE_NAME = :2 AND c.HIDDEN_COLUMN = 'NO' \
         ORDER BY c.COLUMN_ID",
        &[owner, name],
    )?;
    let indexes = texts(
        conn,
        "SELECT ix.INDEX_NAME, ix.UNIQUENESS, ix.INDEX_TYPE, ic.COLUMN_NAME, ic.DESCEND, \
           ie.COLUMN_EXPRESSION, k.CONSTRAINT_NAME, k.CONSTRAINT_TYPE \
         FROM ALL_INDEXES ix \
         JOIN ALL_IND_COLUMNS ic ON ic.INDEX_OWNER = ix.OWNER AND ic.INDEX_NAME = ix.INDEX_NAME \
         LEFT JOIN ALL_IND_EXPRESSIONS ie ON ie.INDEX_OWNER = ic.INDEX_OWNER \
           AND ie.INDEX_NAME = ic.INDEX_NAME AND ie.COLUMN_POSITION = ic.COLUMN_POSITION \
         LEFT JOIN ALL_CONSTRAINTS k ON k.OWNER = ix.TABLE_OWNER AND k.TABLE_NAME = ix.TABLE_NAME \
           AND k.INDEX_NAME = ix.INDEX_NAME AND k.CONSTRAINT_TYPE IN ('P', 'U') \
         WHERE ix.TABLE_OWNER = :1 AND ix.TABLE_NAME = :2 AND ix.INDEX_TYPE <> 'LOB' \
         ORDER BY DECODE(k.CONSTRAINT_TYPE, 'P', 0, 1), ix.INDEX_NAME, ic.COLUMN_POSITION",
        &[owner, name],
    )?;
    let keys = texts(
        conn,
        "SELECT c.CONSTRAINT_NAME, c.OWNER, c.TABLE_NAME, cc.COLUMN_NAME, r.OWNER, \
           r.TABLE_NAME, rc.COLUMN_NAME, c.DELETE_RULE \
         FROM ALL_CONSTRAINTS c \
         JOIN ALL_CONS_COLUMNS cc ON cc.OWNER = c.OWNER AND cc.CONSTRAINT_NAME = c.CONSTRAINT_NAME \
         JOIN ALL_CONSTRAINTS r ON r.OWNER = c.R_OWNER AND r.CONSTRAINT_NAME = c.R_CONSTRAINT_NAME \
         JOIN ALL_CONS_COLUMNS rc ON rc.OWNER = r.OWNER AND rc.CONSTRAINT_NAME = r.CONSTRAINT_NAME \
           AND rc.POSITION = cc.POSITION \
         WHERE c.CONSTRAINT_TYPE = 'R' AND (c.OWNER = :1 AND c.TABLE_NAME = :2 \
           OR r.OWNER = :3 AND r.TABLE_NAME = :4) \
         ORDER BY c.OWNER, c.TABLE_NAME, c.CONSTRAINT_NAME, cc.POSITION",
        &[owner, name, owner, name],
    )?;
    let checks = texts(
        conn,
        "SELECT CONSTRAINT_NAME, SEARCH_CONDITION_VC, STATUS, GENERATED FROM ALL_CONSTRAINTS \
         WHERE OWNER = :1 AND TABLE_NAME = :2 AND CONSTRAINT_TYPE = 'C' ORDER BY CONSTRAINT_NAME",
        &[owner, name],
    )?;
    let mut s = from_rows(t, &columns, &indexes, &keys, &checks);
    s.view = view || materialized;
    s.facts = TableFacts {
        rows: num(table, 1),
        comment: col(table, 2),
        tablespace: table.first().cloned().flatten(),
        ..TableFacts::default()
    };
    if !s.view {
        // Sizes need the segment views: the user's own, else DBA_SEGMENTS if granted.
        if let Ok((data, index)) = segments(conn, t) {
            (s.facts.data_bytes, s.facts.index_bytes) = (data, index);
        }
    }
    let kind = match (view, materialized) {
        (true, _) => "VIEW",
        (_, true) => "MATERIALIZED_VIEW",
        _ => "TABLE",
    };
    s.ddl = ddl(conn, kind, t).unwrap_or_else(|e| {
        format!("-- Oracle did not return the DDL: {e}\n-- DBMS_METADATA needs SELECT_CATALOG_ROLE for other users' objects.")
    });
    Ok(s)
}

/// `CREATE TABLE …;` without storage clauses, as `DBMS_METADATA` writes it.
fn ddl(conn: &Connection, kind: &str, t: &TableRef) -> Result<String> {
    conn.execute(
        "BEGIN \
           DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'DEFAULT', TRUE); \
           DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'STORAGE', FALSE); \
           DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'SEGMENT_ATTRIBUTES', FALSE); \
           DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'SQLTERMINATOR', TRUE); \
           DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'PRETTY', TRUE); \
         END;",
        &[],
    )?;
    // In VARCHAR2 pieces of 1000 characters (4000 bytes at most): a CLOB result trips
    // the driver (see `super::literal`).
    let rows = texts(
        conn,
        &format!(
            "WITH d AS (SELECT /*+ MATERIALIZE */ DBMS_METADATA.GET_DDL({}, {}, {}) x FROM dual) \
             SELECT DBMS_LOB.SUBSTR(x, 1000, 1 + (LEVEL - 1) * 1000) FROM d \
             CONNECT BY LEVEL <= CEIL(DBMS_LOB.GETLENGTH(x) / 1000) ORDER BY LEVEL",
            literal(kind),
            literal(&t.table),
            literal(&t.database)
        ),
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| col(r, 0))
        .collect::<String>()
        .trim()
        .to_owned())
}

/// Bytes in the table's segments (with its LOBs) and in its indexes'.
fn segments(conn: &Connection, t: &TableRef) -> Result<(Option<u64>, Option<u64>)> {
    let own = texts(conn, "SELECT USER FROM dual", &[])?
        .first()
        .is_some_and(|r| col(r, 0) == t.database);
    let from = if own {
        "USER_SEGMENTS s WHERE :1 IS NOT NULL"
    } else {
        "DBA_SEGMENTS s WHERE s.OWNER = :1"
    };
    let rows = texts(
        conn,
        &format!(
            "SELECT TO_CHAR(SUM(CASE WHEN s.SEGMENT_TYPE NOT LIKE '%INDEX%' THEN s.BYTES END)), \
               TO_CHAR(SUM(CASE WHEN s.SEGMENT_TYPE LIKE '%INDEX%' THEN s.BYTES END)) \
             FROM {from} AND (s.SEGMENT_NAME = :2 \
               OR s.SEGMENT_NAME IN (SELECT INDEX_NAME FROM ALL_INDEXES WHERE TABLE_OWNER = :3 \
                 AND TABLE_NAME = :4) \
               OR s.SEGMENT_NAME IN (SELECT SEGMENT_NAME FROM ALL_LOBS WHERE OWNER = :5 \
                 AND TABLE_NAME = :6))"
        ),
        &[
            &t.database,
            &t.table,
            &t.database,
            &t.table,
            &t.database,
            &t.table,
        ],
    )?;
    Ok(rows
        .first()
        .map_or((None, None), |r| (num(r, 0), num(r, 1))))
}

/// `"QTY"` → `QTY`: a descending key part is an expression naming one column.
fn quoted_column(expr: &str) -> Option<String> {
    let inner = expr.trim().strip_prefix('"')?.strip_suffix('"')?;
    (!inner.contains('"')).then(|| inner.to_owned())
}

/// The dictionary rows of one table as a [`TableStructure`], without facts and DDL.
fn from_rows(
    t: &TableRef,
    columns: &[Vec<Option<String>>],
    indexes: &[Vec<Option<String>>],
    keys: &[Vec<Option<String>>],
    checks: &[Vec<Option<String>>],
) -> TableStructure {
    let mut out_indexes: Vec<Index> = Vec::new();
    for r in indexes {
        let name = col(r, 0);
        if out_indexes.last().is_none_or(|i| i.name != name) {
            let ty = col(r, 2);
            let kind = if ty.contains("BITMAP") {
                "BITMAP"
            } else if ty.contains("NORMAL") {
                "BTREE"
            } else {
                ty.as_str()
            };
            out_indexes.push(Index {
                primary: col(r, 7) == "P",
                unique: col(r, 1) == "UNIQUE",
                kind: kind.to_owned(),
                parts: Vec::new(),
                constraint: r.get(6).cloned().flatten(),
                name,
            });
        }
        let descending = col(r, 4) == "DESC";
        let expression = col(r, 5);
        let part = match (expression.is_empty(), quoted_column(&expression)) {
            (true, _) => IndexPart {
                column: col(r, 3),
                descending,
                ..IndexPart::default()
            },
            (false, Some(c)) if descending => IndexPart {
                column: c,
                descending,
                ..IndexPart::default()
            },
            (false, _) => IndexPart {
                column: expression.trim().to_owned(),
                expression: true,
                descending,
                prefix: None,
            },
        };
        if let Some(last) = out_indexes.last_mut() {
            last.parts.push(part);
        }
    }
    let mut all: Vec<ForeignKey> = Vec::new();
    for r in keys {
        let (name, from) = (
            col(r, 0),
            TableRef {
                database: col(r, 1),
                table: col(r, 2),
            },
        );
        if all.last().is_none_or(|k| k.name != name || k.from != from) {
            all.push(ForeignKey {
                name,
                from,
                from_columns: Vec::new(),
                to: TableRef {
                    database: col(r, 4),
                    table: col(r, 5),
                },
                to_columns: Vec::new(),
                on_delete: col(r, 7).to_ascii_lowercase(),
                // Oracle never cascades updates of a referenced key.
                on_update: "no action".into(),
            });
        }
        if let Some(k) = all.last_mut() {
            k.from_columns.push(col(r, 3));
            k.to_columns.push(col(r, 6));
        }
    }
    let (mut foreign_keys, mut referenced_by) = (Vec::new(), Vec::new());
    for k in all {
        if k.to == *t {
            referenced_by.push(k.clone());
        }
        if k.from == *t {
            foreign_keys.push(k);
        }
    }
    let primary: HashSet<&str> = out_indexes
        .iter()
        .filter(|i| i.primary)
        .flat_map(|i| i.parts.iter().map(|p| p.column.as_str()))
        .collect();
    let foreign: HashSet<&str> = foreign_keys
        .iter()
        .flat_map(|k| k.from_columns.iter().map(String::as_str))
        .collect();
    let columns = columns
        .iter()
        .map(|r| {
            let name = col(r, 0);
            let default = col(r, 8).trim().to_owned();
            let virtual_column = col(r, 9) == "YES";
            let identity = col(r, 10);
            StructureColumn {
                primary_key: primary.contains(name.as_str()),
                foreign_key: foreign.contains(name.as_str()),
                column_type: type_label(r, 1),
                nullable: col(r, 7) == "Y",
                generated: virtual_column.then(|| Generated {
                    expression: format!("({default})"),
                    stored: false,
                }),
                // An identity column's default is its sequence: the extra says it.
                default: (!virtual_column && identity.is_empty() && !default.is_empty())
                    .then_some(default),
                extra: if identity.is_empty() {
                    String::new()
                } else {
                    format!("generated {} as identity", identity.to_ascii_lowercase())
                },
                comment: col(r, 11),
                name,
            }
        })
        .collect();
    let checks = checks
        .iter()
        .filter(|r| {
            // NOT NULL is the Nullable column, not a check of its own.
            let condition = col(r, 1);
            !(col(r, 3) == "GENERATED NAME"
                && condition.ends_with(" IS NOT NULL")
                && quoted_column(condition.trim_end_matches(" IS NOT NULL")).is_some())
        })
        .map(|r| Check {
            name: col(r, 0),
            condition: col(r, 1),
            enabled: col(r, 2) == "ENABLED",
        })
        .collect();
    TableStructure {
        columns,
        indexes: out_indexes,
        foreign_keys,
        referenced_by,
        checks,
        oracle: true,
        ..TableStructure::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(v: &[&str]) -> Vec<Option<String>> {
        v.iter()
            .map(|s| (!s.is_empty()).then(|| (*s).to_owned()))
            .collect()
    }

    #[test]
    fn dictionary_rows_become_a_structure() {
        let t = TableRef {
            database: "APP".into(),
            table: "ORD".into(),
        };
        let s = from_rows(
            &t,
            &[
                r(&[
                    "ID",
                    "NUMBER",
                    "22",
                    "",
                    "",
                    "0",
                    "",
                    "N",
                    "\"APP\".\"ISEQ$$_1\".nextval",
                    "NO",
                    "BY DEFAULT",
                    "",
                ]),
                r(&[
                    "QTY", "NUMBER", "22", "10", "0", "0", "", "Y", "0 ", "NO", "", "how many",
                ]),
                r(&[
                    "TWICE",
                    "NUMBER",
                    "22",
                    "",
                    "",
                    "0",
                    "",
                    "Y",
                    "\"QTY\"*2",
                    "YES",
                    "",
                    "",
                ]),
            ],
            &[
                r(&["SYS_C1", "UNIQUE", "NORMAL", "ID", "ASC", "", "SYS_C1", "P"]),
                r(&["ORD_BM", "NONUNIQUE", "BITMAP", "QTY", "ASC", "", "", ""]),
                r(&[
                    "ORD_FN",
                    "NONUNIQUE",
                    "FUNCTION-BASED NORMAL",
                    "SYS_NC1$",
                    "ASC",
                    "LOWER(\"NOTE\")",
                    "",
                    "",
                ]),
                r(&[
                    "ORD_DESC",
                    "NONUNIQUE",
                    "FUNCTION-BASED NORMAL",
                    "SYS_NC2$",
                    "DESC",
                    "\"QTY\"",
                    "",
                    "",
                ]),
                r(&[
                    "ORD_UQ", "UNIQUE", "NORMAL", "QTY", "ASC", "", "ORD_UQ_C", "U",
                ]),
            ],
            &[
                r(&["FK_C", "APP", "ORD", "QTY", "APP", "CUST", "ID", "CASCADE"]),
                r(&[
                    "FK_BACK",
                    "APP",
                    "LINE",
                    "ORD_ID",
                    "APP",
                    "ORD",
                    "ID",
                    "NO ACTION",
                ]),
            ],
            &[
                r(&["SYS_C9", "\"QTY\" IS NOT NULL", "ENABLED", "GENERATED NAME"]),
                r(&["QTY_POS", "qty >= 0", "ENABLED", "USER NAME"]),
            ],
        );
        assert_eq!(s.columns[0].key_label(), "PK");
        assert_eq!(
            s.columns[0].default_label(),
            "generated by default as identity"
        );
        assert_eq!(s.columns[1].default.as_deref(), Some("0"));
        assert_eq!(s.columns[1].key_label(), "FK");
        assert_eq!(s.columns[1].column_type, "number(10)");
        assert_eq!(
            s.columns[2].default_label(),
            "generated always as (\"QTY\"*2) virtual"
        );
        let kinds: Vec<_> = s
            .indexes
            .iter()
            .map(|i| (i.name.as_str(), i.kind_label(), i.parts_label()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("SYS_C1", "primary".to_owned(), "ID".to_owned()),
                ("ORD_BM", "bitmap".to_owned(), "QTY".to_owned()),
                ("ORD_FN", "btree".to_owned(), "(LOWER(\"NOTE\"))".to_owned()),
                ("ORD_DESC", "btree".to_owned(), "QTY desc".to_owned()),
                ("ORD_UQ", "unique".to_owned(), "QTY".to_owned()),
            ]
        );
        assert_eq!(s.indexes[4].constraint.as_deref(), Some("ORD_UQ_C"));
        assert_eq!(s.foreign_keys[0].on_delete, "cascade");
        assert_eq!(s.referenced_by[0].from.table, "LINE");
        assert_eq!(s.checks.len(), 1);
        assert_eq!(s.checks[0].condition, "qty >= 0");
    }
}
