//! The sidebar tree on Oracle: schemas are users, each loaded whole from the `ALL_*`
//! views in four queries.

use std::collections::{HashMap, HashSet};

use super::Connection;

use super::{col, texts};
use crate::driver::{DriverError, NodeKind, ObjectNode, Result};

/// The groups under a schema, in order, with the `ALL_OBJECTS` type each lists.
const GROUPS: [(&str, &str); 10] = [
    ("Tables", "TABLE"),
    ("Views", "VIEW"),
    ("Materialized views", "MATERIALIZED VIEW"),
    ("Sequences", "SEQUENCE"),
    ("Procedures", "PROCEDURE"),
    ("Functions", "FUNCTION"),
    ("Packages", "PACKAGE"),
    ("Triggers", "TRIGGER"),
    ("Synonyms", "SYNONYM"),
    ("Types", "TYPE"),
];

pub(super) fn children(conn: &Connection, path: &[String]) -> Result<Vec<ObjectNode>> {
    let Some((schema, rest)) = path.split_first() else {
        let rows = texts(
            conn,
            "SELECT USERNAME, ORACLE_MAINTAINED FROM ALL_USERS u WHERE USERNAME = USER \
             OR EXISTS (SELECT 1 FROM ALL_OBJECTS o WHERE o.OWNER = u.USERNAME) \
             ORDER BY CASE WHEN USERNAME = USER THEN 0 WHEN ORACLE_MAINTAINED = 'Y' THEN 2 \
             ELSE 1 END, USERNAME",
            &[],
        )?;
        return Ok(rows
            .iter()
            .map(|r| ObjectNode {
                system: col(r, 1) == "Y",
                ..ObjectNode::new(col(r, 0), NodeKind::Database)
            })
            .collect());
    };
    let tables = texts(
        conn,
        "SELECT TABLE_NAME FROM ALL_TABLES WHERE OWNER = :1 AND NESTED = 'NO' \
         AND SECONDARY = 'N' AND DROPPED = 'NO' AND (IOT_TYPE IS NULL OR IOT_TYPE = 'IOT') \
         ORDER BY TABLE_NAME",
        &[schema],
    )?;
    let objects = texts(
        conn,
        "SELECT OBJECT_NAME, OBJECT_TYPE, STATUS FROM ALL_OBJECTS WHERE OWNER = :1 \
         AND SUBOBJECT_NAME IS NULL AND GENERATED = 'N' AND OBJECT_TYPE IN ('VIEW', 'MATERIALIZED VIEW', \
         'SEQUENCE', 'PROCEDURE', 'FUNCTION', 'PACKAGE', 'TRIGGER', 'SYNONYM', 'TYPE') \
         ORDER BY OBJECT_NAME",
        &[schema],
    )?;
    let columns = texts(
        conn,
        "SELECT TABLE_NAME, COLUMN_NAME, DATA_TYPE, TO_CHAR(DATA_LENGTH), \
         TO_CHAR(DATA_PRECISION), TO_CHAR(DATA_SCALE), TO_CHAR(CHAR_LENGTH), CHAR_USED \
         FROM ALL_TAB_COLUMNS WHERE OWNER = :1 ORDER BY TABLE_NAME, COLUMN_ID",
        &[schema],
    )?;
    let keys = texts(
        conn,
        "SELECT c.TABLE_NAME, cc.COLUMN_NAME, c.CONSTRAINT_TYPE FROM ALL_CONSTRAINTS c \
         JOIN ALL_CONS_COLUMNS cc ON cc.OWNER = c.OWNER \
         AND cc.CONSTRAINT_NAME = c.CONSTRAINT_NAME AND cc.TABLE_NAME = c.TABLE_NAME \
         WHERE c.OWNER = :1 AND c.CONSTRAINT_TYPE IN ('P', 'R')",
        &[schema],
    )?;
    let triggers = texts(
        conn,
        "SELECT TRIGGER_NAME, TABLE_OWNER, TABLE_NAME, STATUS FROM ALL_TRIGGERS WHERE OWNER = :1",
        &[schema],
    )?;
    let mut nodes = schema_children(&tables, &objects, &columns, &keys, (schema, &triggers));
    for name in rest {
        nodes = nodes
            .into_iter()
            .find(|n| &n.name == name)
            .and_then(|n| n.children)
            .ok_or_else(|| DriverError::Invalid(format!("no object named {name}")))?;
    }
    Ok(nodes)
}

/// The schema's tables as the tree loads them, plus its foreign keys within the schema,
/// column pairs by position.
pub(super) fn diagram(conn: &Connection, schema: &str) -> Result<crate::diagram::Diagram> {
    let groups = children(conn, &[schema.to_owned()])?;
    let keys = texts(
        conn,
        "SELECT c.CONSTRAINT_NAME, c.TABLE_NAME, cc.COLUMN_NAME, r.TABLE_NAME, rc.COLUMN_NAME \
         FROM ALL_CONSTRAINTS c \
         JOIN ALL_CONS_COLUMNS cc ON cc.OWNER = c.OWNER AND cc.CONSTRAINT_NAME = c.CONSTRAINT_NAME \
         JOIN ALL_CONSTRAINTS r ON r.OWNER = c.R_OWNER AND r.CONSTRAINT_NAME = c.R_CONSTRAINT_NAME \
         JOIN ALL_CONS_COLUMNS rc ON rc.OWNER = r.OWNER AND rc.CONSTRAINT_NAME = r.CONSTRAINT_NAME \
          AND rc.POSITION = cc.POSITION \
         WHERE c.OWNER = :1 AND c.CONSTRAINT_TYPE = 'R' AND c.R_OWNER = c.OWNER \
         ORDER BY c.TABLE_NAME, c.CONSTRAINT_NAME, cc.POSITION",
        &[&schema.to_owned()],
    )?;
    let keys = keys
        .iter()
        .map(|r| (col(r, 0), col(r, 1), col(r, 2), col(r, 3), col(r, 4)))
        .collect();
    Ok(crate::diagram::Diagram::from_tree(groups, keys))
}

/// A column type as Oracle writes it, lower case: `varchar2(20 char)`, `number(10,2)`.
/// Columns are `DATA_TYPE, DATA_LENGTH, DATA_PRECISION, DATA_SCALE, CHAR_LENGTH,
/// CHAR_USED` starting at `at`.
pub(super) fn type_label(r: &[Option<String>], at: usize) -> String {
    let ty = col(r, at);
    let (length, precision, scale, chars, used) = (
        col(r, at + 1),
        col(r, at + 2),
        col(r, at + 3),
        col(r, at + 4),
        col(r, at + 5),
    );
    let label = match ty.as_str() {
        "NUMBER" => match (precision.as_str(), scale.as_str()) {
            ("", "0") => "NUMBER(*,0)".to_owned(),
            ("", _) => ty,
            (p, "0" | "") => format!("NUMBER({p})"),
            (p, s) => format!("NUMBER({p},{s})"),
        },
        "FLOAT" if !precision.is_empty() => format!("FLOAT({precision})"),
        "VARCHAR2" | "CHAR" if used == "C" => format!("{ty}({chars} CHAR)"),
        "VARCHAR2" | "CHAR" | "RAW" => format!("{ty}({length})"),
        "NVARCHAR2" | "NCHAR" => format!("{ty}({chars})"),
        _ => ty,
    };
    label.to_ascii_lowercase()
}

/// The groups of one schema with every column inside, empty ones included. Rows are
/// `(table)`, `(object, type, status)`, columns as [`type_label`] reads them after
/// `(table, column)`, `(table, column, P|R)`, and the schema with its triggers'
/// `(trigger, table owner, table, status)`. A trigger shows its table (or
/// `invalid`, `disabled`); a table with triggers also lists them in a Triggers
/// group after its columns.
fn schema_children(
    tables: &[Vec<Option<String>>],
    objects: &[Vec<Option<String>>],
    columns: &[Vec<Option<String>>],
    keys: &[Vec<Option<String>>],
    (schema, triggers): (&str, &[Vec<Option<String>>]),
) -> Vec<ObjectNode> {
    let mut trigger_detail: HashMap<String, String> = HashMap::new();
    let mut on_table: HashMap<String, Vec<ObjectNode>> = HashMap::new();
    for r in triggers {
        let (name, owner, table) = (col(r, 0), col(r, 1), col(r, 2));
        let detail = if col(r, 3) == "DISABLED" {
            "disabled".to_owned()
        } else {
            table.clone()
        };
        if owner == schema && !table.is_empty() {
            on_table.entry(table).or_default().push(ObjectNode {
                detail: if detail == "disabled" {
                    detail.clone()
                } else {
                    String::new()
                },
                ..ObjectNode::new(name.clone(), NodeKind::Routine)
            });
        }
        trigger_detail.insert(name, detail);
    }
    let key: HashSet<(String, String, String)> = keys
        .iter()
        .map(|r| (col(r, 0), col(r, 1), col(r, 2)))
        .collect();
    let mut by_table: HashMap<String, Vec<ObjectNode>> = HashMap::new();
    for r in columns {
        let (table, name) = (col(r, 0), col(r, 1));
        let has = |t: &str| key.contains(&(table.clone(), name.clone(), t.to_owned()));
        let node = ObjectNode {
            detail: type_label(r, 2),
            primary_key: has("P"),
            foreign_key: has("R"),
            ..ObjectNode::new(name.clone(), NodeKind::Column)
        };
        by_table.entry(table).or_default().push(node);
    }
    let mut groups: Vec<Vec<ObjectNode>> = vec![Vec::new(); GROUPS.len()];
    // A materialized view also lists as the table that holds its rows.
    let views: HashSet<String> = objects
        .iter()
        .filter(|r| col(r, 1) == "MATERIALIZED VIEW")
        .map(|r| col(r, 0))
        .collect();
    for r in tables {
        let name = col(r, 0);
        if views.contains(&name) {
            continue;
        }
        let mut children = by_table.remove(&name).unwrap_or_default();
        if let Some(t) = on_table.remove(&name) {
            children.push(ObjectNode {
                children: Some(t),
                ..ObjectNode::new("Triggers", NodeKind::Group)
            });
        }
        groups[0].push(ObjectNode {
            children: Some(children),
            ..ObjectNode::new(name, NodeKind::Table)
        });
    }
    for r in objects {
        let (name, ty) = (col(r, 0), col(r, 1));
        let Some(g) = GROUPS.iter().position(|(_, t)| *t == ty) else {
            continue;
        };
        let node = if g <= 2 {
            ObjectNode {
                children: Some(by_table.remove(&name).unwrap_or_default()),
                ..ObjectNode::new(name, NodeKind::View)
            }
        } else {
            ObjectNode {
                detail: if col(r, 2) == "INVALID" {
                    "invalid".into()
                } else if ty == "TRIGGER" {
                    trigger_detail.get(&name).cloned().unwrap_or_default()
                } else {
                    String::new()
                },
                ..ObjectNode::new(name, NodeKind::Routine)
            }
        };
        groups[g].push(node);
    }
    GROUPS
        .iter()
        .zip(groups)
        .map(|((name, _), children)| ObjectNode {
            children: Some(children),
            ..ObjectNode::new(*name, NodeKind::Group)
        })
        .collect()
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
    fn types_read_like_oracle_writes_them() {
        let t = |v: &[&str]| type_label(&r(v), 0);
        assert_eq!(t(&["NUMBER", "22", "10", "2", "0", ""]), "number(10,2)");
        assert_eq!(t(&["NUMBER", "22", "", "0", "0", ""]), "number(*,0)");
        assert_eq!(t(&["NUMBER", "22", "", "", "0", ""]), "number");
        assert_eq!(
            t(&["VARCHAR2", "80", "", "", "20", "C"]),
            "varchar2(20 char)"
        );
        assert_eq!(t(&["VARCHAR2", "20", "", "", "20", "B"]), "varchar2(20)");
        assert_eq!(t(&["NVARCHAR2", "40", "", "", "20", "C"]), "nvarchar2(20)");
        assert_eq!(t(&["RAW", "16", "", "", "0", ""]), "raw(16)");
        assert_eq!(
            t(&["TIMESTAMP(6) WITH TIME ZONE", "13", "", "6", "0", ""]),
            "timestamp(6) with time zone"
        );
    }

    #[test]
    fn schema_rows_become_groups_with_columns() {
        let groups = schema_children(
            &[r(&["ORDERS"]), r(&["MV_TOTALS"])],
            &[
                r(&["MV_TOTALS", "MATERIALIZED VIEW", "VALID"]),
                r(&["ORDERS_SEQ", "SEQUENCE", "VALID"]),
                r(&["BAD_PROC", "PROCEDURE", "INVALID"]),
                r(&["ORDERS_BIU", "TRIGGER", "VALID"]),
                r(&["OFF_T", "TRIGGER", "VALID"]),
                r(&["V_ORDERS", "VIEW", "VALID"]),
            ],
            &[
                r(&["ORDERS", "ID", "NUMBER", "22", "", "0", "0", ""]),
                r(&["ORDERS", "CUSTOMER_ID", "NUMBER", "22", "", "0", "0", ""]),
                r(&["V_ORDERS", "ID", "NUMBER", "22", "", "0", "0", ""]),
                r(&["MV_TOTALS", "N", "NUMBER", "22", "", "", "0", ""]),
            ],
            &[
                r(&["ORDERS", "ID", "P"]),
                r(&["ORDERS", "CUSTOMER_ID", "R"]),
            ],
            (
                "APP",
                &[
                    r(&["ORDERS_BIU", "APP", "ORDERS", "ENABLED"]),
                    r(&["OFF_T", "APP", "ORDERS", "DISABLED"]),
                ],
            ),
        );
        let names: Vec<_> = groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, GROUPS.map(|g| g.0));
        let count = |i: usize| groups[i].children.as_ref().map_or(0, Vec::len);
        assert_eq!(
            (count(0), count(1), count(2), count(3), count(4)),
            (1, 1, 1, 1, 1)
        );
        let orders = &groups[0].children.as_ref().unwrap()[0];
        let cols = orders.children.as_ref().unwrap();
        let on = cols.last().unwrap();
        assert_eq!(
            (on.name.as_str(), on.children.as_ref().unwrap().len()),
            ("Triggers", 2)
        );
        assert_eq!(groups[9].name, "Types");
        let trig: Vec<_> = groups[7]
            .children
            .as_ref()
            .unwrap()
            .iter()
            .map(|t| (t.name.as_str(), t.detail.as_str()))
            .collect();
        assert_eq!(trig, [("ORDERS_BIU", "ORDERS"), ("OFF_T", "disabled")]);
        assert!(cols[0].primary_key && !cols[0].foreign_key);
        assert!(cols[1].foreign_key);
        assert_eq!(cols[0].detail, "number(*,0)");
        let mv = &groups[2].children.as_ref().unwrap()[0];
        assert_eq!(
            (mv.kind, mv.children.as_ref().unwrap().len()),
            (NodeKind::View, 1)
        );
        assert_eq!(groups[4].children.as_ref().unwrap()[0].detail, "invalid");
        assert!(!groups[3].children.as_ref().unwrap()[0].has_children());
    }
}
