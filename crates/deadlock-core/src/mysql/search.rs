//! Search everywhere on MySQL and MariaDB: names, columns and source text from
//! `information_schema`, the search text bound as a parameter.

use std::sync::atomic::{AtomicBool, Ordering};

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Value};

use super::SYSTEM_SCHEMAS;
use crate::driver::{DriverError, Result};
use crate::edit::quote_ident;
use crate::search::{Hit, LIMIT, like_contains, like_prefix};

/// Named objects: `(view, schema column, name column, kind expression)`.
const OBJECTS: [(&str, &str, &str, &str); 4] = [
    (
        "TABLES",
        "TABLE_SCHEMA",
        "TABLE_NAME",
        "IF(TABLE_TYPE LIKE '%VIEW', 'view', 'table')",
    ),
    (
        "ROUTINES",
        "ROUTINE_SCHEMA",
        "ROUTINE_NAME",
        "LOWER(ROUTINE_TYPE)",
    ),
    ("TRIGGERS", "TRIGGER_SCHEMA", "TRIGGER_NAME", "'trigger'"),
    ("EVENTS", "EVENT_SCHEMA", "EVENT_NAME", "'event'"),
];

/// Objects with source: as [`OBJECTS`], then the column holding the source.
const CODE: [(&str, &str, &str, &str, &str); 4] = [
    (
        "ROUTINES",
        "ROUTINE_SCHEMA",
        "ROUTINE_NAME",
        "LOWER(ROUTINE_TYPE)",
        "ROUTINE_DEFINITION",
    ),
    (
        "TRIGGERS",
        "TRIGGER_SCHEMA",
        "TRIGGER_NAME",
        "'trigger'",
        "ACTION_STATEMENT",
    ),
    (
        "VIEWS",
        "TABLE_SCHEMA",
        "TABLE_NAME",
        "'view'",
        "VIEW_DEFINITION",
    ),
    (
        "EVENTS",
        "EVENT_SCHEMA",
        "EVENT_NAME",
        "'event'",
        "EVENT_DEFINITION",
    ),
];

/// `schema NOT IN ('information_schema', …) [AND schema NOT IN (?, …)]`: the system
/// schemas, then one placeholder per hidden schema (bind them with [`params`]).
fn user_schemas(column: &str, hidden: &[String]) -> String {
    let names: Vec<String> = SYSTEM_SCHEMAS.iter().map(|s| format!("'{s}'")).collect();
    let mut out = format!("{column} NOT IN ({})", names.join(", "));
    if !hidden.is_empty() {
        let marks = vec!["?"; hidden.len()].join(", ");
        out.push_str(&format!(" AND {column} NOT IN ({marks})"));
    }
    out
}

/// Binds in SQL order: `before` the schema condition, the hidden names, then `after`.
fn params(before: &[&str], hidden: &[String], after: &[&str]) -> Vec<Value> {
    before
        .iter()
        .copied()
        .chain(hidden.iter().map(String::as_str))
        .chain(after.iter().copied())
        .map(Value::from)
        .collect()
}

/// Exact, then prefix, then shorter names first; binds the exact text and the prefix.
fn ranked(name: &str) -> String {
    format!(
        "ORDER BY LOWER({name}) = ? DESC, LOWER({name}) LIKE ? ESCAPE '!' DESC, \
         CHAR_LENGTH({name}), {name} LIMIT {LIMIT}"
    )
}

pub(super) async fn search(
    conn: &mut Conn,
    text: &str,
    hidden: &[String],
    stop: &AtomicBool,
) -> Result<Vec<Hit>> {
    let q = text.trim().to_lowercase();
    let (contains, prefix) = (like_contains(&q), like_prefix(&q));
    let mut out = Vec::new();
    for (view, schema, name, kind) in OBJECTS {
        if stop.load(Ordering::Relaxed) {
            return Ok(out);
        }
        let sql = format!(
            "SELECT {schema}, {name}, {kind} FROM information_schema.{view} \
             WHERE LOWER({name}) LIKE ? ESCAPE '!' AND {} {}",
            user_schemas(schema, hidden),
            ranked(name)
        );
        let rows: Vec<(String, String, String)> = conn
            .exec(sql, params(&[&contains], hidden, &[&q, &prefix]))
            .await?;
        out.extend(rows.into_iter().map(|(s, n, k)| Hit::object(s, n, k)));
    }
    if stop.load(Ordering::Relaxed) {
        return Ok(out);
    }
    let sql = format!(
        "SELECT TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, COLUMN_TYPE \
         FROM information_schema.COLUMNS WHERE LOWER(COLUMN_NAME) LIKE ? ESCAPE '!' AND {} {}",
        user_schemas("TABLE_SCHEMA", hidden),
        ranked("COLUMN_NAME")
    );
    let rows: Vec<(String, String, String, String)> = conn
        .exec(sql, params(&[&contains], hidden, &[&q, &prefix]))
        .await?;
    out.extend(
        rows.into_iter()
            .map(|(s, t, c, ty)| Hit::column(s, t, c, ty)),
    );
    for (view, schema, name, kind, source) in CODE {
        if stop.load(Ordering::Relaxed) {
            return Ok(out);
        }
        // The source is NULL where the account may not read it (another definer's
        // routine without SHOW_ROUTINE): those never match.
        let sql = format!(
            "SELECT {schema}, {name}, {kind}, {source} FROM information_schema.{view} \
             WHERE LOWER({source}) LIKE ? ESCAPE '!' AND {} ORDER BY {schema}, {name} \
             LIMIT {LIMIT}",
            user_schemas(schema, hidden)
        );
        let rows: Vec<(String, String, String, Option<String>)> =
            conn.exec(sql, params(&[&contains], hidden, &[])).await?;
        out.extend(
            rows.into_iter()
                .filter_map(|(s, n, k, src)| Hit::code(s, n, k, &src?, &q)),
        );
    }
    Ok(out)
}

pub(super) async fn definition(
    conn: &mut Conn,
    schema: &str,
    name: &str,
    kind: &str,
) -> Result<String> {
    // The statement's column in each `SHOW CREATE` answer.
    let (what, at) = match kind {
        "procedure" => ("PROCEDURE", 2),
        "function" => ("FUNCTION", 2),
        "trigger" => ("TRIGGER", 2),
        "event" => ("EVENT", 3),
        "view" => ("VIEW", 1),
        "sequence" => ("SEQUENCE", 1),
        _ => return Err(DriverError::Invalid(format!("{kind} has no definition"))),
    };
    let row: Option<mysql_async::Row> = conn
        .query_first(format!(
            "SHOW CREATE {what} {}.{}",
            quote_ident(schema),
            quote_ident(name)
        ))
        .await?;
    row.and_then(|mut r| r.take_opt::<Option<String>, _>(at))
        .and_then(std::result::Result::ok)
        .flatten()
        .ok_or_else(|| {
            DriverError::Invalid(format!(
                "the definition of {schema}.{name} is not visible to this account"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_schemas_become_bound_placeholders() {
        assert!(!user_schemas("S", &[]).contains('?'));
        let sql = user_schemas("S", &["a".into(), "b'c".into()]);
        assert!(sql.ends_with("AND S NOT IN (?, ?)") && !sql.contains("b'c"));
        let p = params(&["x"], &["a".into(), "b".into()], &["y"]);
        assert_eq!(p, ["x", "a", "b", "y"].map(Value::from));
    }
}
