//! A table's structure, engine-neutral: columns, indexes, foreign keys both ways,
//! table facts and the server's own DDL. Below it, the MySQL / MariaDB `alter table`
//! text the Structure view's Add column, Add index and drop actions run.

use std::fmt;

use crate::connection::Engine;
use crate::edit::{TableRef, quote_ident, quote_str};

/// One column as the Structure view lists it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StructureColumn {
    pub name: String,
    /// The full type as the server writes it: `varchar(20)`, `int unsigned`.
    pub column_type: String,
    pub nullable: bool,
    /// The default as SQL: `'pending'`, `0`, `CURRENT_TIMESTAMP`. `None`: no default,
    /// or `NULL`.
    pub default: Option<String>,
    pub generated: Option<Generated>,
    /// What else the server says about it: `auto_increment`,
    /// `on update CURRENT_TIMESTAMP`.
    pub extra: String,
    pub comment: String,
    pub primary_key: bool,
    pub foreign_key: bool,
}

/// A generated column's expression, as SQL in parentheses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generated {
    pub expression: String,
    /// Stored (`true`) or virtual.
    pub stored: bool,
}

impl StructureColumn {
    /// The Default cell: the default, or the generation expression, then the extra.
    pub fn default_label(&self) -> String {
        let first = match (&self.generated, &self.default) {
            (Some(g), _) => format!(
                "generated always as {} {}",
                g.expression,
                if g.stored { "stored" } else { "virtual" }
            ),
            (None, Some(d)) => d.clone(),
            (None, None) => String::new(),
        };
        [first.as_str(), self.extra.as_str()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// `PK`, `FK`, `PK FK` or nothing.
    pub fn key_label(&self) -> &'static str {
        match (self.primary_key, self.foreign_key) {
            (true, true) => "PK FK",
            (true, false) => "PK",
            (false, true) => "FK",
            (false, false) => "",
        }
    }
}

/// One key part of an index.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexPart {
    /// A column name, or a functional key part's expression.
    pub column: String,
    pub expression: bool,
    pub descending: bool,
    /// Only the first `n` characters or bytes are indexed.
    pub prefix: Option<u32>,
}

impl fmt::Display for IndexPart {
    /// `title(50)`, `created desc`, `(lower(email))`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.expression {
            write!(f, "({})", self.column)?;
        } else {
            f.write_str(&self.column)?;
        }
        if let Some(n) = self.prefix {
            write!(f, "({n})")?;
        }
        if self.descending {
            f.write_str(" desc")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Index {
    pub name: String,
    pub parts: Vec<IndexPart>,
    /// The engine's method: `BTREE`, `HASH`, `FULLTEXT`, `SPATIAL`.
    pub kind: String,
    pub unique: bool,
    pub primary: bool,
    /// Oracle: the primary or unique constraint this index enforces, which a drop
    /// removes instead of the index.
    pub constraint: Option<String>,
}

impl Index {
    /// The Kind cell: `primary`, `unique`, `fulltext`, `btree`.
    pub fn kind_label(&self) -> String {
        let method = self.kind.to_ascii_lowercase();
        if self.primary {
            "primary".into()
        } else if self.unique && method == "btree" {
            "unique".into()
        } else if self.unique {
            format!("unique {method}")
        } else {
            method
        }
    }

    /// The Columns cell: `qty desc, created`.
    pub fn parts_label(&self) -> String {
        let parts: Vec<String> = self.parts.iter().map(ToString::to_string).collect();
        parts.join(", ")
    }
}

/// A foreign key, from the referencing table to the referenced one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    pub name: String,
    pub from: TableRef,
    pub from_columns: Vec<String>,
    pub to: TableRef,
    pub to_columns: Vec<String>,
    /// Lower case: `cascade`, `set null`, `restrict`, `no action`.
    pub on_delete: String,
    pub on_update: String,
}

/// What the server knows about the table as a whole.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableFacts {
    pub engine: Option<String>,
    pub collation: Option<String>,
    /// From the server's statistics: an estimate for InnoDB.
    pub rows: Option<u64>,
    pub data_bytes: Option<u64>,
    pub index_bytes: Option<u64>,
    pub comment: String,
    /// Oracle: where the rows are stored.
    pub tablespace: Option<String>,
}

impl TableFacts {
    /// Data plus indexes, when the server reports either.
    pub fn size(&self) -> Option<u64> {
        match (self.data_bytes, self.index_bytes) {
            (None, None) => None,
            (d, i) => Some(d.unwrap_or(0).saturating_add(i.unwrap_or(0))),
        }
    }
}

/// A table's or view's structure. Views have columns and the DDL only.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableStructure {
    pub view: bool,
    pub columns: Vec<StructureColumn>,
    pub indexes: Vec<Index>,
    /// Keys of this table.
    pub foreign_keys: Vec<ForeignKey>,
    /// Keys of other tables (or this one) that point at this table.
    pub referenced_by: Vec<ForeignKey>,
    pub facts: TableFacts,
    /// Check constraints (Oracle; MySQL's stay in the DDL).
    pub checks: Vec<Check>,
    /// `CREATE TABLE …` or `CREATE VIEW …`, as the server writes it.
    pub ddl: String,
    /// The session treats `\` in strings as an escape (no `NO_BACKSLASH_ESCAPES`).
    pub backslash_escapes: bool,
    /// Oracle: `"names"` and Oracle DDL.
    pub oracle: bool,
}

/// A check constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    /// The condition as the server writes it: `total >= 0`.
    pub condition: String,
    /// Off (`DISABLE`): the server does not enforce it.
    pub enabled: bool,
}

/// `214 MB`, `16 KB`, `512 B`.
pub fn size_label(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes;
    let mut unit = 0;
    // Whole units, rounded down: the summary line needs no decimals.
    while value >= 1024 && unit + 1 < UNITS.len() {
        value /= 1024;
        unit += 1;
    }
    format!("{value} {}", UNITS[unit])
}

// --- MySQL and MariaDB DDL -------------------------------------------------------

/// The column types the Add column form offers, per flavor.
pub fn column_types(mariadb: bool) -> Vec<&'static str> {
    let mut types = vec![
        "int",
        "int unsigned",
        "bigint",
        "bigint unsigned",
        "tinyint",
        "tinyint unsigned",
        "smallint",
        "smallint unsigned",
        "mediumint",
        "mediumint unsigned",
        "decimal",
        "float",
        "double",
        "bit",
        "boolean",
        "varchar",
        "char",
        "text",
        "tinytext",
        "mediumtext",
        "longtext",
        "binary",
        "varbinary",
        "blob",
        "tinyblob",
        "mediumblob",
        "longblob",
        "enum",
        "set",
        "date",
        "time",
        "datetime",
        "timestamp",
        "year",
        "json",
        "geometry",
        "point",
        "linestring",
        "polygon",
    ];
    if mariadb {
        types.extend(["uuid", "inet4", "inet6"]);
    }
    types
}

/// The column types the Add column form offers on Oracle.
pub const ORACLE_TYPES: [&str; 21] = [
    "number",
    "integer",
    "float",
    "binary_float",
    "binary_double",
    "varchar2",
    "nvarchar2",
    "char",
    "nchar",
    "clob",
    "nclob",
    "blob",
    "raw",
    "date",
    "timestamp",
    "timestamp with time zone",
    "timestamp with local time zone",
    "interval year to month",
    "interval day to second",
    "boolean",
    "json",
];

/// Where a new column goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Position {
    Last,
    First,
    After(String),
}

/// The Add column form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewColumn {
    pub name: String,
    /// From [`column_types`]: `varchar`, `int unsigned`.
    pub data_type: String,
    /// `255`, `10,2`; for `enum` and `set` the members, comma-separated.
    pub length: String,
    pub nullable: bool,
    /// Typed text: empty for none, `NULL`, `CURRENT_TIMESTAMP`, `(expression)`, a
    /// `'quoted literal'`, or a value that is written as a literal.
    pub default: String,
    pub auto_increment: bool,
    pub comment: String,
    pub position: Position,
}

/// `alter table … add column …`.
///
/// # Errors
/// An empty name or type, a length that is not a number, or text that would end the
/// statement.
pub fn add_column_sql(t: &TableRef, c: &NewColumn, s: &TableStructure) -> Result<String, String> {
    let name = c.name.trim();
    if name.is_empty() {
        return Err("enter a column name".into());
    }
    if s.oracle {
        return oracle_add_column(t, name, c);
    }
    let mut sql = format!(
        "alter table {} add column {} {}",
        qualified(t),
        quote_ident(name),
        full_type(&c.data_type, &c.length, s.backslash_escapes)?
    );
    if c.auto_increment {
        // An auto-increment column must be a key: the primary key if there is none.
        let key = if s.indexes.iter().any(|i| i.primary) {
            "unique key"
        } else {
            "primary key"
        };
        sql.push_str(&format!(" not null auto_increment {key}"));
    } else {
        sql.push_str(if c.nullable { " null" } else { " not null" });
        if let Some(d) = default_sql(&c.default, &c.data_type, s.backslash_escapes) {
            sql.push_str(&format!(" default {d}"));
        }
    }
    if !c.comment.is_empty() {
        sql.push_str(&format!(
            " comment {}",
            quote_str(&c.comment, s.backslash_escapes)
        ));
    }
    match &c.position {
        Position::Last => {}
        Position::First => sql.push_str(" first"),
        Position::After(col) => sql.push_str(&format!(" after {}", quote_ident(col))),
    }
    one_statement(sql)
}

/// `varchar(255)`, `int(11) unsigned`, `enum('a','b')`.
fn full_type(data_type: &str, length: &str, backslash: bool) -> Result<String, String> {
    let ty = data_type.trim().to_ascii_lowercase();
    if ty.is_empty() {
        return Err("choose a type".into());
    }
    if !ty
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b' ' || b == b'_')
    {
        return Err(format!("{data_type} is not a type"));
    }
    let length = length.trim();
    if length.is_empty() {
        return match ty.as_str() {
            "varchar" | "varbinary" => Err(format!("{ty} needs a length")),
            "enum" | "set" => Err(format!("{ty} needs its members, like a, b, c")),
            _ => Ok(ty),
        };
    }
    let inner = if matches!(ty.as_str(), "enum" | "set") {
        let members: Vec<String> = length
            .split(',')
            .map(|m| quote_str(m.trim(), backslash))
            .collect();
        members.join(",")
    } else {
        let parts: Vec<&str> = length.split(',').map(str::trim).collect();
        let numbers = parts.len() <= 2
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        if !numbers {
            return Err("length must be a number, or two like 10,2".into());
        }
        parts.join(",")
    };
    // The length goes before `unsigned`.
    Ok(match ty.split_once(' ') {
        Some((base, rest)) => format!("{base}({inner}) {rest}"),
        None => format!("{ty}({inner})"),
    })
}

const NUMERIC: [&str; 12] = [
    "tinyint",
    "smallint",
    "mediumint",
    "int",
    "integer",
    "bigint",
    "decimal",
    "numeric",
    "float",
    "double",
    "real",
    "boolean",
];

const TIME_FUNCTIONS: [&str; 5] = [
    "current_timestamp",
    "now",
    "localtime",
    "localtimestamp",
    "current_date",
];

/// The typed default as SQL, `None` for no default. `NULL`, the current-time
/// functions, `(expressions)` and `'quoted literals'` stay as typed; numbers for
/// numeric types are written bare; anything else becomes a string literal.
pub fn default_sql(text: &str, data_type: &str, backslash: bool) -> Option<String> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    let function = lower.split_once('(').map_or(lower.as_str(), |(f, rest)| {
        // `now()`, `current_timestamp(3)`: digits at most between the parentheses.
        let args = rest.strip_suffix(')').unwrap_or("x");
        if args.bytes().all(|b| b.is_ascii_digit()) {
            f
        } else {
            ""
        }
    });
    let base = data_type
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let number = t.parse::<f64>().is_ok_and(f64::is_finite)
        && t.bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b'e' | b'E'));
    let raw = lower == "null"
        || TIME_FUNCTIONS.contains(&function)
        || t.starts_with('(') && t.ends_with(')')
        || t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'')
        || number && NUMERIC.contains(&base.as_str())
        || base == "bit" && (lower.starts_with("b'") || t.bytes().all(|b| b.is_ascii_digit()));
    Some(if raw {
        t.to_owned()
    } else {
        quote_str(text, backslash)
    })
}

/// What an index is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    Index,
    Unique,
    Fulltext,
    Spatial,
    Primary,
    /// Oracle.
    Bitmap,
}

impl IndexKind {
    pub const ALL: [Self; 5] = [
        Self::Index,
        Self::Unique,
        Self::Fulltext,
        Self::Spatial,
        Self::Primary,
    ];

    /// The kinds the Add index form offers on Oracle.
    pub const ORACLE: [Self; 4] = [Self::Index, Self::Unique, Self::Bitmap, Self::Primary];

    pub fn label(self) -> &'static str {
        match self {
            Self::Index => "index",
            Self::Unique => "unique",
            Self::Fulltext => "fulltext",
            Self::Spatial => "spatial",
            Self::Primary => "primary key",
            Self::Bitmap => "bitmap",
        }
    }
}

/// The Add index form. Parts name columns, never expressions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewIndex {
    /// Empty: the server picks one. Ignored for a primary key.
    pub name: String,
    pub kind: IndexKind,
    pub parts: Vec<IndexPart>,
}

/// `alter table … add index …`.
///
/// # Errors
/// No columns.
pub fn add_index_sql(t: &TableRef, i: &NewIndex, s: &TableStructure) -> Result<String, String> {
    if i.parts.is_empty() {
        return Err("add at least one column".into());
    }
    if s.oracle {
        return oracle_add_index(t, i);
    }
    let parts: Vec<String> = i
        .parts
        .iter()
        .map(|p| {
            let mut s = quote_ident(&p.column);
            if let Some(n) = p.prefix {
                s.push_str(&format!("({n})"));
            }
            if p.descending {
                s.push_str(" desc");
            }
            s
        })
        .collect();
    let name = i.name.trim();
    let named = if name.is_empty() || i.kind == IndexKind::Primary {
        String::new()
    } else {
        format!(" {}", quote_ident(name))
    };
    let kind = match i.kind {
        IndexKind::Index => "index",
        IndexKind::Unique => "unique index",
        IndexKind::Fulltext => "fulltext index",
        IndexKind::Spatial => "spatial index",
        IndexKind::Primary => "primary key",
        IndexKind::Bitmap => return Err("bitmap indexes are Oracle's".into()),
    };
    one_statement(format!(
        "alter table {} add {kind}{named} ({})",
        qualified(t),
        parts.join(", ")
    ))
}

/// `alter table … drop column …`.
pub fn drop_column_sql(t: &TableRef, column: &str, s: &TableStructure) -> String {
    format!(
        "alter table {} drop column {}",
        s.qualified(t),
        s.ident(column)
    )
}

/// `alter table … drop index …`, or `drop primary key`. Oracle: `drop index`, or the
/// constraint the index enforces.
pub fn drop_index_sql(t: &TableRef, index: &Index, s: &TableStructure) -> String {
    match (&index.constraint, index.primary, s.oracle) {
        (_, true, _) => format!("alter table {} drop primary key", s.qualified(t)),
        (Some(c), _, true) => s.drop_constraint_sql(t, c),
        (None, _, true) => format!(
            "drop index {}.{}",
            s.ident(&t.database),
            s.ident(&index.name)
        ),
        (_, _, false) => format!(
            "alter table {} drop index {}",
            s.qualified(t),
            quote_ident(&index.name)
        ),
    }
}

/// `alter table … drop foreign key …`. The index the key used stays.
pub fn drop_foreign_key_sql(t: &TableRef, name: &str, s: &TableStructure) -> String {
    if s.oracle {
        return s.drop_constraint_sql(t, name);
    }
    format!(
        "alter table {} drop foreign key {}",
        s.qualified(t),
        quote_ident(name)
    )
}

impl TableStructure {
    fn ident(&self, name: &str) -> String {
        if self.oracle {
            oracle_ident(name)
        } else {
            quote_ident(name)
        }
    }

    fn qualified(&self, t: &TableRef) -> String {
        format!("{}.{}", self.ident(&t.database), self.ident(&t.table))
    }

    /// `alter table … drop constraint …`: a check constraint, or on Oracle any
    /// constraint.
    pub fn drop_constraint_sql(&self, t: &TableRef, name: &str) -> String {
        let verb = if self.oracle { "constraint" } else { "check" };
        format!(
            "alter table {} drop {verb} {}",
            self.qualified(t),
            self.ident(name)
        )
    }
}

fn qualified(t: &TableRef) -> String {
    format!("{}.{}", quote_ident(&t.database), quote_ident(&t.table))
}

// --- Oracle DDL ------------------------------------------------------------------

fn oracle_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn oracle_qualified(t: &TableRef) -> String {
    format!("{}.{}", oracle_ident(&t.database), oracle_ident(&t.table))
}

/// `alter table … add ("C" type default … not null)`. Oracle adds columns at the end
/// and sets comments with a statement of its own.
fn oracle_add_column(t: &TableRef, name: &str, c: &NewColumn) -> Result<String, String> {
    if c.position != Position::Last {
        return Err("Oracle adds columns at the end only".into());
    }
    if !c.comment.is_empty() {
        return Err(
            "Oracle sets comments with COMMENT ON COLUMN: leave Comment empty and run that \
             once the column exists"
                .into(),
        );
    }
    let mut sql = format!(
        "alter table {} add ({} {}",
        oracle_qualified(t),
        oracle_ident(name),
        oracle_type(&c.data_type, &c.length)?
    );
    if c.auto_increment {
        sql.push_str(" generated by default as identity");
    } else if let Some(d) = oracle_default(&c.default, &c.data_type) {
        sql.push_str(&format!(" default {d}"));
    }
    if !c.nullable || c.auto_increment {
        sql.push_str(" not null");
    }
    sql.push(')');
    one_statement_in(sql, Engine::Oracle)
}

/// `varchar2(20)`, `number(10,2)`, `timestamp(3) with time zone`.
fn oracle_type(data_type: &str, length: &str) -> Result<String, String> {
    let ty = data_type.trim().to_ascii_lowercase();
    if ty.is_empty() {
        return Err("choose a type".into());
    }
    if !ty
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b' ' || b == b'_')
    {
        return Err(format!("{data_type} is not a type"));
    }
    let length = length.trim();
    if length.is_empty() {
        return match ty.as_str() {
            "varchar2" | "nvarchar2" | "raw" => Err(format!("{ty} needs a length")),
            _ => Ok(ty),
        };
    }
    let parts: Vec<&str> = length.split(',').map(str::trim).collect();
    let numbers = parts.len() <= 2
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if !numbers {
        return Err("length must be a number, or two like 10,2".into());
    }
    let inner = parts.join(",");
    match ty.split_once(' ') {
        Some(("timestamp", rest)) if parts.len() == 1 => Ok(format!("timestamp({inner}) {rest}")),
        Some(_) => Err(format!("{ty} takes no length here")),
        None => Ok(format!("{ty}({inner})")),
    }
}

const ORACLE_FUNCTIONS: [&str; 7] = [
    "sysdate",
    "systimestamp",
    "current_date",
    "current_timestamp",
    "localtimestamp",
    "user",
    "sys_guid",
];

/// The typed default as Oracle SQL: `NULL`, the clock functions, `(expressions)`,
/// `'quoted literals'` and numbers stay; anything else becomes a string literal.
fn oracle_default(text: &str, data_type: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    let function = lower.split_once('(').map_or(lower.as_str(), |(f, rest)| {
        let args = rest.strip_suffix(')').unwrap_or("x");
        if args.bytes().all(|b| b.is_ascii_digit()) {
            f
        } else {
            ""
        }
    });
    let numeric = [
        "number",
        "integer",
        "float",
        "binary_float",
        "binary_double",
    ]
    .contains(&data_type.trim().to_ascii_lowercase().as_str());
    let number = t.parse::<f64>().is_ok_and(f64::is_finite)
        && t.bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b'e' | b'E'));
    let raw = lower == "null"
        || ORACLE_FUNCTIONS.contains(&function)
        || t.starts_with('(') && t.ends_with(')')
        || t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'')
        || number && numeric;
    Some(if raw {
        t.to_owned()
    } else {
        quote_str(t, false)
    })
}

/// `create [unique|bitmap] index …`, or `alter table … add primary key …`. Oracle
/// needs a name for an index: an empty one becomes `<TABLE>_<COLUMNS>_IX`.
fn oracle_add_index(t: &TableRef, i: &NewIndex) -> Result<String, String> {
    if i.parts.iter().any(|p| p.prefix.is_some()) {
        return Err("Oracle indexes whole columns: leave the prefix empty".into());
    }
    let parts: Vec<String> = i
        .parts
        .iter()
        .map(|p| {
            let mut s = oracle_ident(&p.column);
            if p.descending {
                s.push_str(" desc");
            }
            s
        })
        .collect();
    let name = i.name.trim();
    let sql = match i.kind {
        IndexKind::Primary => {
            let named = if name.is_empty() {
                String::new()
            } else {
                format!(" constraint {}", oracle_ident(name))
            };
            format!(
                "alter table {} add{named} primary key ({})",
                oracle_qualified(t),
                parts.join(", ")
            )
        }
        IndexKind::Index | IndexKind::Unique | IndexKind::Bitmap => {
            let name = if name.is_empty() {
                let mut n = t.table.clone();
                for p in &i.parts {
                    n.push('_');
                    n.push_str(&p.column);
                }
                n.push_str("_IX");
                n.chars().take(128).collect()
            } else {
                name.to_owned()
            };
            let kind = match i.kind {
                IndexKind::Unique => "unique ",
                IndexKind::Bitmap => "bitmap ",
                _ => "",
            };
            format!(
                "create {kind}index {}.{} on {} ({})",
                oracle_ident(&t.database),
                oracle_ident(&name),
                oracle_qualified(t),
                parts.join(", ")
            )
        }
        IndexKind::Fulltext | IndexKind::Spatial => {
            return Err(format!("Oracle has no {} index here", i.kind.label()));
        }
    };
    one_statement_in(sql, Engine::Oracle)
}

/// Refuses text that would run as more than one statement.
fn one_statement(sql: String) -> Result<String, String> {
    one_statement_in(sql, Engine::Mysql)
}

fn one_statement_in(sql: String, engine: Engine) -> Result<String, String> {
    if crate::split::split(&sql, engine).len() == 1 {
        Ok(sql)
    } else {
        Err("a field ends the statement (;); remove it".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> TableRef {
        TableRef {
            database: "my db".into(),
            table: "or`ders".into(),
        }
    }

    fn column(name: &str, data_type: &str, length: &str) -> NewColumn {
        NewColumn {
            name: name.into(),
            data_type: data_type.into(),
            length: length.into(),
            nullable: true,
            default: String::new(),
            auto_increment: false,
            comment: String::new(),
            position: Position::Last,
        }
    }

    fn with_pk() -> TableStructure {
        TableStructure {
            indexes: vec![Index {
                name: "PRIMARY".into(),
                primary: true,
                unique: true,
                ..Index::default()
            }],
            backslash_escapes: true,
            ..TableStructure::default()
        }
    }

    #[test]
    fn add_column_quotes_names_and_places_the_length() {
        let s = with_pk();
        assert_eq!(
            add_column_sql(&t(), &column("note `x`", "varchar", "200"), &s).unwrap(),
            "alter table `my db`.`or``ders` add column `note ``x``` varchar(200) null"
        );
        let c = NewColumn {
            nullable: false,
            default: "0".into(),
            comment: r"it's a\b".into(),
            position: Position::After("first col".into()),
            ..column("qty", "int unsigned", "11")
        };
        assert_eq!(
            add_column_sql(&t(), &c, &s).unwrap(),
            r"alter table `my db`.`or``ders` add column `qty` int(11) unsigned not null default 0 comment 'it''s a\\b' after `first col`"
        );
        let c = NewColumn {
            position: Position::First,
            ..column("e", "enum", "a, it's ,c")
        };
        assert!(
            add_column_sql(&t(), &c, &s)
                .unwrap()
                .ends_with("`e` enum('a','it''s','c') null first")
        );
        let c = column("d", "decimal", "10, 2");
        assert!(
            add_column_sql(&t(), &c, &s)
                .unwrap()
                .contains("decimal(10,2)")
        );
    }

    #[test]
    fn auto_increment_becomes_a_key() {
        let c = NewColumn {
            auto_increment: true,
            default: "5".into(),
            ..column("id", "bigint unsigned", "")
        };
        assert!(
            add_column_sql(&t(), &c, &with_pk())
                .unwrap()
                .ends_with("bigint unsigned not null auto_increment unique key")
        );
        let s = TableStructure::default();
        assert!(
            add_column_sql(&t(), &c, &s)
                .unwrap()
                .ends_with("not null auto_increment primary key")
        );
    }

    #[test]
    fn bad_column_input_is_refused() {
        let s = with_pk();
        assert!(add_column_sql(&t(), &column(" ", "int", ""), &s).is_err());
        assert!(add_column_sql(&t(), &column("a", "", ""), &s).is_err());
        assert!(add_column_sql(&t(), &column("a", "varchar", ""), &s).is_err());
        assert!(add_column_sql(&t(), &column("a", "int", "1; drop"), &s).is_err());
        assert!(add_column_sql(&t(), &column("a", "int; drop table x", ""), &s).is_err());
        let c = NewColumn {
            default: "(1); drop table x; (1)".into(),
            ..column("a", "int", "")
        };
        assert!(add_column_sql(&t(), &c, &s).is_err());
    }

    #[test]
    fn defaults_become_literals_unless_sql() {
        let d = |text: &str, ty: &str| default_sql(text, ty, true);
        assert_eq!(d("", "int"), None);
        assert_eq!(d("  ", "varchar"), None);
        assert_eq!(d("null", "varchar").as_deref(), Some("null"));
        assert_eq!(d("12.5", "decimal").as_deref(), Some("12.5"));
        assert_eq!(d("-3", "int unsigned").as_deref(), Some("-3"));
        assert_eq!(d("12", "varchar").as_deref(), Some("'12'"));
        assert_eq!(d("it's", "varchar").as_deref(), Some("'it''s'"));
        assert_eq!(d(r"C:\x", "varchar").as_deref(), Some(r"'C:\\x'"));
        assert_eq!(
            default_sql(r"C:\x", "text", false).as_deref(),
            Some(r"'C:\x'")
        );
        assert_eq!(
            d("CURRENT_TIMESTAMP", "datetime").as_deref(),
            Some("CURRENT_TIMESTAMP")
        );
        assert_eq!(
            d("current_timestamp(3)", "datetime").as_deref(),
            Some("current_timestamp(3)")
        );
        assert_eq!(d("now()", "timestamp").as_deref(), Some("now()"));
        assert_eq!(d("now(x)", "varchar").as_deref(), Some("'now(x)'"));
        assert_eq!(d("(uuid())", "char").as_deref(), Some("(uuid())"));
        assert_eq!(d("''", "varchar").as_deref(), Some("''"));
        assert_eq!(d("b'101'", "bit").as_deref(), Some("b'101'"));
        assert_eq!(d("1", "bit").as_deref(), Some("1"));
        assert_eq!(d("1e3", "double").as_deref(), Some("1e3"));
        assert_eq!(d("inf", "double").as_deref(), Some("'inf'"));
        assert_eq!(d("2026-11-22", "date").as_deref(), Some("'2026-11-22'"));
    }

    #[test]
    fn add_index_lists_parts_with_order_and_prefix() {
        let parts = vec![
            IndexPart {
                column: "ti`tle".into(),
                prefix: Some(50),
                ..IndexPart::default()
            },
            IndexPart {
                column: "created at".into(),
                descending: true,
                ..IndexPart::default()
            },
        ];
        let i = NewIndex {
            name: "ix name".into(),
            kind: IndexKind::Unique,
            parts: parts.clone(),
        };
        assert_eq!(
            add_index_sql(&t(), &i, &TableStructure::default()).unwrap(),
            "alter table `my db`.`or``ders` add unique index `ix name` (`ti``tle`(50), `created at` desc)"
        );
        let i = NewIndex {
            name: "ignored".into(),
            kind: IndexKind::Primary,
            parts: parts[..1].to_vec(),
        };
        assert!(
            add_index_sql(&t(), &i, &TableStructure::default())
                .unwrap()
                .ends_with("add primary key (`ti``tle`(50))")
        );
        let i = NewIndex {
            name: String::new(),
            kind: IndexKind::Fulltext,
            parts: parts[1..].to_vec(),
        };
        assert!(
            add_index_sql(&t(), &i, &TableStructure::default())
                .unwrap()
                .ends_with("add fulltext index (`created at` desc)")
        );
        let none = NewIndex {
            name: "x".into(),
            kind: IndexKind::Index,
            parts: vec![],
        };
        assert!(add_index_sql(&t(), &none, &TableStructure::default()).is_err());
    }

    #[test]
    fn drops_name_their_target() {
        assert_eq!(
            drop_column_sql(&t(), "a`b", &TableStructure::default()),
            "alter table `my db`.`or``ders` drop column `a``b`"
        );
        let pk = Index {
            name: "PRIMARY".into(),
            primary: true,
            ..Index::default()
        };
        assert!(
            drop_index_sql(&t(), &pk, &TableStructure::default()).ends_with("drop primary key")
        );
        let ix = Index {
            name: "ix 1".into(),
            ..Index::default()
        };
        assert!(
            drop_index_sql(&t(), &ix, &TableStructure::default()).ends_with("drop index `ix 1`")
        );
        assert!(
            drop_foreign_key_sql(&t(), "fk`1", &TableStructure::default())
                .ends_with("drop foreign key `fk``1`")
        );
    }

    #[test]
    fn labels_for_the_grids() {
        let c = StructureColumn {
            default: Some("CURRENT_TIMESTAMP".into()),
            extra: "on update CURRENT_TIMESTAMP".into(),
            primary_key: true,
            ..StructureColumn::default()
        };
        assert_eq!(
            c.default_label(),
            "CURRENT_TIMESTAMP on update CURRENT_TIMESTAMP"
        );
        assert_eq!(c.key_label(), "PK");
        let g = StructureColumn {
            generated: Some(Generated {
                expression: "(`qty` * 2)".into(),
                stored: true,
            }),
            ..StructureColumn::default()
        };
        assert_eq!(g.default_label(), "generated always as (`qty` * 2) stored");
        let i = Index {
            parts: vec![
                IndexPart {
                    column: "qty".into(),
                    descending: true,
                    ..IndexPart::default()
                },
                IndexPart {
                    column: "lower(`a`)".into(),
                    expression: true,
                    ..IndexPart::default()
                },
            ],
            kind: "BTREE".into(),
            unique: true,
            ..Index::default()
        };
        assert_eq!(i.parts_label(), "qty desc, (lower(`a`))");
        assert_eq!(i.kind_label(), "unique");
        assert_eq!(size_label(214 * 1024 * 1024 + 5), "214 MB");
        assert_eq!(size_label(512), "512 B");
        assert_eq!(TableFacts::default().size(), None);
    }
}
