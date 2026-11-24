//! Search everywhere on Oracle: names, columns and source text from the `ALL_*`
//! views, so only objects the account can see, in schemas Oracle does not maintain.
//! The search text is bound, upper-cased on both sides.

use super::schema::type_label;
use super::structure::ddl;
use super::{Bind, Connection, col, num, texts};
use crate::driver::{DriverError, Result};
use crate::edit::TableRef;
use crate::search::{Hit, LIMIT, Line, find_all, like_contains, like_prefix};

/// The schemas searched: the account's own and every one Oracle does not maintain.
const OWNERS: &str =
    "OWNER IN (SELECT USERNAME FROM ALL_USERS WHERE ORACLE_MAINTAINED = 'N' OR USERNAME = USER)";

/// ` AND OWNER NOT IN (:n, …)` for the hidden schemas, binds numbered from `first`;
/// owners are the names the tree shows (`ALL_USERS.USERNAME`), compared exactly.
fn not_hidden(first: usize, hidden: &[String]) -> String {
    if hidden.is_empty() {
        return String::new();
    }
    let marks: Vec<String> = (first..first + hidden.len())
        .map(|i| format!(":{i}"))
        .collect();
    format!(" AND OWNER NOT IN ({})", marks.join(", "))
}

/// `head`, the hidden names, then `tail` as binds: the order the placeholders appear
/// in the SQL, which is how plain-SQL binds are matched to values.
fn with_hidden<'a>(
    head: &[&'a dyn Bind],
    hidden: &'a [String],
    tail: &[&'a dyn Bind],
) -> Vec<&'a dyn Bind> {
    head.iter()
        .copied()
        .chain(hidden.iter().map(|h| h as &dyn Bind))
        .chain(tail.iter().copied())
        .collect()
}

/// The search's steps, one dictionary query each; the driver checks for a newer
/// search between them.
pub(super) const STEPS: [Step; 4] = [objects, columns, source, views];

/// One dictionary query of the search.
pub(super) type Step = fn(&Connection, &str, &[String]) -> Result<Vec<Hit>>;

/// `CASE … END` ranking exact (bind `:first`) before prefix (`:first + 1`) matches.
fn ranked(name: &str, first: usize) -> String {
    format!(
        "ORDER BY CASE WHEN UPPER({name}) = :{first} THEN 0 \
         WHEN UPPER({name}) LIKE :{} ESCAPE '!' THEN 1 ELSE 2 END, LENGTH({name}), {name}",
        first + 1
    )
}

fn objects_sql(hidden: &[String]) -> String {
    format!(
        "SELECT OWNER, OBJECT_NAME, LOWER(OBJECT_TYPE) FROM ALL_OBJECTS \
             WHERE UPPER(OBJECT_NAME) LIKE :1 ESCAPE '!' AND SUBOBJECT_NAME IS NULL \
             AND GENERATED = 'N' AND OBJECT_NAME NOT LIKE 'BIN$%' \
             AND OBJECT_TYPE IN ('TABLE', 'VIEW', 'MATERIALIZED VIEW', 'SEQUENCE', 'PROCEDURE', \
             'FUNCTION', 'PACKAGE', 'TRIGGER', 'SYNONYM', 'TYPE') AND {OWNERS}{} {} \
             FETCH FIRST {LIMIT} ROWS ONLY",
        not_hidden(2, hidden),
        ranked("OBJECT_NAME", 2 + hidden.len())
    )
}

fn objects(conn: &Connection, text: &str, hidden: &[String]) -> Result<Vec<Hit>> {
    let q = text.trim().to_uppercase();
    let rows = texts(
        conn,
        &objects_sql(hidden),
        &with_hidden(&[&like_contains(&q)], hidden, &[&q, &like_prefix(&q)]),
    )?;
    let hits: Vec<Hit> = rows
        .iter()
        .map(|r| Hit::object(col(r, 0), col(r, 1), col(r, 2)))
        .collect();
    // A materialized view also lists as the table holding its rows, as in the tree.
    let views: Vec<(String, String)> = hits
        .iter()
        .filter(|h| h.kind == "materialized view")
        .map(|h| (h.schema.clone(), h.name.clone()))
        .collect();
    Ok(hits
        .into_iter()
        .filter(|h| h.kind != "table" || !views.contains(&(h.schema.clone(), h.name.clone())))
        .collect())
}

fn columns_sql(hidden: &[String]) -> String {
    format!(
        "SELECT OWNER, TABLE_NAME, COLUMN_NAME, DATA_TYPE, TO_CHAR(DATA_LENGTH), \
             TO_CHAR(DATA_PRECISION), TO_CHAR(DATA_SCALE), TO_CHAR(CHAR_LENGTH), CHAR_USED \
             FROM ALL_TAB_COLUMNS WHERE UPPER(COLUMN_NAME) LIKE :1 ESCAPE '!' \
             AND TABLE_NAME NOT LIKE 'BIN$%' AND {OWNERS}{} {}, OWNER, TABLE_NAME \
             FETCH FIRST {LIMIT} ROWS ONLY",
        not_hidden(2, hidden),
        ranked("COLUMN_NAME", 2 + hidden.len())
    )
}

fn columns(conn: &Connection, text: &str, hidden: &[String]) -> Result<Vec<Hit>> {
    let q = text.trim().to_uppercase();
    let rows = texts(
        conn,
        &columns_sql(hidden),
        &with_hidden(&[&like_contains(&q)], hidden, &[&q, &like_prefix(&q)]),
    )?;
    Ok(rows
        .iter()
        .map(|r| Hit::column(col(r, 0), col(r, 1), col(r, 2), type_label(r, 3)))
        .collect())
}

/// `ALL_SOURCE` holds procedures, functions, packages, triggers and types line by
/// line: the first matching line of each.
fn source(conn: &Connection, text: &str, hidden: &[String]) -> Result<Vec<Hit>> {
    let q = text.trim().to_uppercase();
    let rows = texts(
        conn,
        &format!(
            "SELECT OWNER, NAME, LOWER(TYPE), TO_CHAR(LINE), TEXT FROM ( \
               SELECT OWNER, NAME, TYPE, LINE, TEXT, \
                 ROW_NUMBER() OVER (PARTITION BY OWNER, NAME, TYPE ORDER BY LINE) RN \
               FROM ALL_SOURCE WHERE UPPER(TEXT) LIKE :1 ESCAPE '!' AND {OWNERS}{}) \
             WHERE RN = 1 ORDER BY OWNER, NAME, TYPE FETCH FIRST {LIMIT} ROWS ONLY",
            not_hidden(2, hidden)
        ),
        &with_hidden(&[&like_contains(&q)], hidden, &[]),
    )?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let text = col(r, 4).trim_end_matches(['\r', '\n']).to_owned();
            let hits = find_all(&text, &q);
            (!hits.is_empty()).then(|| Hit {
                line: Some(Line {
                    number: num(r, 3).unwrap_or(1),
                    text,
                    hits,
                }),
                group: crate::search::Group::Code,
                ..Hit::object(col(r, 0), col(r, 1), col(r, 2))
            })
        })
        .collect())
}

/// Views keep their text in a LONG, which `LIKE` cannot read; `TEXT_VC` is its first
/// 4000 characters, so the end of a longer view is not searched.
fn views(conn: &Connection, text: &str, hidden: &[String]) -> Result<Vec<Hit>> {
    let q = text.trim().to_uppercase();
    let rows = texts(
        conn,
        &format!(
            "SELECT OWNER, VIEW_NAME, TEXT_VC FROM ALL_VIEWS \
             WHERE UPPER(TEXT_VC) LIKE :1 ESCAPE '!' AND {OWNERS}{} \
             ORDER BY OWNER, VIEW_NAME FETCH FIRST {LIMIT} ROWS ONLY",
            not_hidden(2, hidden)
        ),
        &with_hidden(&[&like_contains(&q)], hidden, &[]),
    )?;
    Ok(rows
        .iter()
        .filter_map(|r| Hit::code(col(r, 0), col(r, 1), "view".into(), &col(r, 2), &q))
        .collect())
}

/// Source objects come back from `ALL_SOURCE` as `CREATE OR REPLACE` plus their
/// lines, so line numbers match the search's; a package or type brings its body.
/// Anything else asks `DBMS_METADATA`.
pub(super) fn definition(
    conn: &Connection,
    schema: &str,
    name: &str,
    kind: &str,
) -> Result<String> {
    let types: Vec<String> = match kind {
        "package" => vec!["PACKAGE".into(), "PACKAGE BODY".into()],
        "type" => vec!["TYPE".into(), "TYPE BODY".into()],
        "procedure" | "function" | "trigger" | "package body" | "type body" | "java source" => {
            vec![kind.to_uppercase()]
        }
        _ => {
            let t = TableRef {
                database: schema.to_owned(),
                table: name.to_owned(),
            };
            return ddl(conn, &kind.to_uppercase().replace(' ', "_"), &t);
        }
    };
    let mut out = String::new();
    for ty in &types {
        let rows = texts(
            conn,
            "SELECT TEXT FROM ALL_SOURCE WHERE OWNER = :1 AND NAME = :2 AND TYPE = :3 \
             ORDER BY LINE",
            &[&schema, &name, ty],
        )?;
        if rows.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("CREATE OR REPLACE ");
        out.push_str(
            rows.iter()
                .map(|r| col(r, 0))
                .collect::<String>()
                .trim_end(),
        );
        out.push_str("\n/\n");
    }
    if out.is_empty() {
        return Err(DriverError::Invalid(format!(
            "the source of {schema}.{name} is not visible to this account"
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plain-SQL binds match by position of appearance: the placeholders must read
    /// `:1, :2, …` left to right, one per bind.
    #[test]
    fn placeholders_follow_bind_order() {
        for n in 0..3 {
            let hidden: Vec<String> = (0..n).map(|i| format!("H{i}")).collect();
            check(&objects_sql(&hidden), 3 + n);
            check(&columns_sql(&hidden), 3 + n);
        }
    }

    fn check(sql: &str, binds: usize) {
        let found: Vec<usize> = sql
            .split(':')
            .skip(1)
            .filter_map(|t| {
                let d: String = t.chars().take_while(char::is_ascii_digit).collect();
                d.parse().ok()
            })
            .collect();
        assert_eq!(found, (1..=binds).collect::<Vec<_>>(), "{sql}");
    }

    #[test]
    fn hidden_owners_are_numbered_binds() {
        assert_eq!(not_hidden(4, &[]), "");
        let h = ["A".to_owned(), "b'c".to_owned()];
        assert_eq!(not_hidden(2, &h), " AND OWNER NOT IN (:2, :3)");
    }
}
