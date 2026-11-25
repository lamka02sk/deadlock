//! Table data editing for MySQL and MariaDB: what a table's columns are, which
//! columns identify a row, the check of typed input against a column's type, the
//! pending changes, and the SQL that applies them.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use chrono::{NaiveDate, NaiveDateTime, TimeDelta};

use crate::connection::Engine;
use crate::driver::{Row, Value};

/// A table or view, by database and name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub database: String,
    pub table: String,
}

/// One column, from `information_schema.COLUMNS`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ColumnInfo {
    pub name: String,
    /// `DATA_TYPE`, lowercase: `int`, `varchar`, `enum`.
    pub data_type: String,
    /// `COLUMN_TYPE`, lowercase: `int unsigned`, `varchar(20)`, `enum('a','b')`.
    pub column_type: String,
    pub nullable: bool,
    pub auto_increment: bool,
    /// A virtual or stored generated column: the server computes it.
    pub generated: bool,
    /// Oracle's `ROWID`: selected bare to identify rows, never edited.
    pub pseudo: bool,
}

/// Text cells longer than this, and binary cells, edit in a dialog.
pub const INLINE_LIMIT: usize = 200;

impl ColumnInfo {
    /// Why the column cannot be edited, if it cannot.
    pub fn locked(&self) -> Option<&'static str> {
        if self.pseudo {
            Some("the row's address, which identifies it: it cannot be changed")
        } else if self.generated {
            Some("generated column: the server computes it")
        } else if SPATIAL.contains(&self.data_type.as_str()) {
            Some("spatial values cannot be edited here")
        } else {
            None
        }
    }

    /// Binary strings and BLOBs: edited as hex.
    pub fn is_binary(&self) -> bool {
        matches!(
            self.data_type.as_str(),
            "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob"
        )
    }

    fn unsigned(&self) -> bool {
        self.column_type.contains("unsigned")
    }

    /// The number in `type(n)` or the two in `type(p,s)`.
    fn size(&self) -> (Option<u64>, Option<u64>) {
        let Some(inner) = self
            .column_type
            .split_once('(')
            .and_then(|(_, r)| r.split_once(')'))
            .map(|(i, _)| i)
        else {
            return (None, None);
        };
        let mut parts = inner.split(',').map(|p| p.trim().parse::<u64>().ok());
        (parts.next().flatten(), parts.next().flatten())
    }

    /// The members of an `enum(…)` or `set(…)` type.
    fn members(&self) -> Vec<String> {
        let Some(inner) = self
            .column_type
            .split_once('(')
            .and_then(|(_, r)| r.rsplit_once(')'))
            .map(|(i, _)| i)
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut chars = inner.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\'' {
                continue;
            }
            let mut m = String::new();
            while let Some(c) = chars.next() {
                match c {
                    '\'' if chars.peek() == Some(&'\'') => {
                        chars.next();
                        m.push('\'');
                    }
                    '\'' => break,
                    '\\' => {
                        if let Some(n) = chars.next() {
                            m.push(n);
                        }
                    }
                    c => m.push(c),
                }
            }
            out.push(m);
        }
        out
    }
}

const SPATIAL: [&str; 8] = [
    "geometry",
    "point",
    "linestring",
    "polygon",
    "multipoint",
    "multilinestring",
    "multipolygon",
    "geometrycollection",
];

/// A table's columns and how its rows are told apart.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableInfo {
    pub columns: Vec<ColumnInfo>,
    /// The columns that identify a row: the primary key, else a unique key whose
    /// columns are all `NOT NULL`. `None`: the table opens read-only.
    pub key: Option<Vec<usize>>,
    pub view: bool,
    /// Rows by the server's statistics (`TABLE_ROWS`): an estimate for InnoDB.
    pub estimated_rows: Option<u64>,
    /// The session treats `\` in strings as an escape (no `NO_BACKSLASH_ESCAPES`).
    pub backslash_escapes: bool,
    /// Oracle: `"names"`, Oracle literals and `OFFSET … FETCH` paging.
    pub oracle: bool,
    /// Why the rows cannot be changed although the table has a key (an external
    /// table), if so.
    pub read_only: Option<&'static str>,
}

impl TableInfo {
    /// Why rows cannot be edited, if they cannot.
    pub fn read_only_reason(&self) -> Option<&'static str> {
        if let Some(r) = self.read_only {
            Some(r)
        } else if self.view {
            Some("Views open read-only: a view has no key to identify its rows.")
        } else if self.key.is_none() {
            Some(
                "Read-only: the table has no primary key or unique key on NOT NULL columns \
                 to identify its rows.",
            )
        } else {
            None
        }
    }

    /// Said when rows are identified by something other than a key: Oracle's `ROWID`.
    pub fn key_note(&self) -> Option<&'static str> {
        let rowid = self.key.as_ref().is_some_and(|k| {
            k.iter()
                .any(|&i| self.columns.get(i).is_some_and(|c| c.pseudo))
        });
        rowid.then_some(
            "Rows are identified by ROWID: the table has no primary or unique key. A row \
             moved by the server (a partition change, a shrink) gets a new one.",
        )
    }

    /// A column or table name, quoted for this engine.
    pub fn ident(&self, name: &str) -> String {
        if self.oracle {
            format!("\"{}\"", name.replace('"', "\"\""))
        } else {
            quote_ident(name)
        }
    }

    fn column(&self, i: usize) -> String {
        match self.columns.get(i) {
            Some(c) if c.pseudo => c.name.clone(),
            Some(c) => self.ident(&c.name),
            None => String::new(),
        }
    }

    fn qualified(&self, t: &TableRef) -> String {
        format!("{}.{}", self.ident(&t.database), self.ident(&t.table))
    }

    fn literal(&self, v: &Value) -> Result<String, String> {
        if self.oracle {
            oracle_literal(v)
        } else {
            literal(v, self.backslash_escapes)
        }
    }

    fn engine(&self) -> Engine {
        if self.oracle {
            Engine::Oracle
        } else {
            Engine::Mysql
        }
    }
}

/// Oracle literals: `HEXTORAW('…')`, `DATE '…'`, `TIMESTAMP '…'`, quotes doubled.
/// Time-zone timestamps and intervals arrive as text in the session's fixed format,
/// which Oracle converts.
///
/// # Errors
/// A value Oracle cannot store: a time of day, a float that is not finite.
pub fn oracle_literal(v: &Value) -> Result<String, String> {
    Ok(match v {
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Value::Text(s) => quote_str(s, false),
        // Oracle stores an empty RAW as NULL.
        Value::Bytes(b) if b.is_empty() => "NULL".into(),
        Value::Bytes(b) => {
            let mut out = String::with_capacity(12 + b.len() * 2);
            out.push_str("HEXTORAW('");
            for x in b {
                let _ = write!(out, "{x:02X}");
            }
            out.push_str("')");
            out
        }
        Value::Date(d) => format!("DATE '{d}'"),
        Value::DateTime(d) => format!("TIMESTAMP '{}'", d.format("%Y-%m-%d %H:%M:%S%.f")),
        Value::Time(_) => return Err("Oracle has no time-of-day type".into()),
        // Past NUMBER's range a plain literal overflows: a BINARY_DOUBLE literal holds it.
        Value::Float(f) if f.is_finite() && *f != 0. && !(1e-129..1e125).contains(&f.abs()) => {
            format!("{}d", literal(v, false)?)
        }
        other => literal(other, false)?,
    })
}

/// The key that identifies rows, from `(index name, non-unique, column)` rows in
/// index order: `PRIMARY`, else the unique key with the fewest columns that are all
/// `NOT NULL`. A functional key part (no column) rules its index out.
pub fn pick_key(
    columns: &[ColumnInfo],
    indexes: &[(String, bool, Option<String>)],
) -> Option<Vec<usize>> {
    let mut by_name: BTreeMap<&str, Option<Vec<usize>>> = BTreeMap::new();
    for (name, non_unique, column) in indexes {
        if *non_unique {
            continue;
        }
        let entry = by_name
            .entry(name.as_str())
            .or_insert_with(|| Some(Vec::new()));
        let found = column
            .as_ref()
            .and_then(|c| columns.iter().position(|col| &col.name == c));
        match (entry.as_mut(), found) {
            (Some(cols), Some(i)) => cols.push(i),
            _ => *entry = None,
        }
    }
    if let Some(Some(pk)) = by_name.remove("PRIMARY") {
        return Some(pk);
    }
    by_name
        .into_values()
        .flatten()
        .filter(|cols| !cols.is_empty() && cols.iter().all(|&i| !columns[i].nullable))
        .min_by_key(Vec::len)
}

/// A new value for a cell.
#[derive(Debug, Clone, PartialEq)]
pub enum Edit {
    /// The value, `Value::Null` for NULL.
    Value(Value),
    /// The column's default (`DEFAULT`; left out of an INSERT).
    Default,
}

/// `` `name` ``, with backticks inside doubled.
pub fn quote_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// Whether a session with this `@@sql_mode` treats `\` in strings as an escape.
pub fn backslash_escapes(sql_mode: &str) -> bool {
    !sql_mode.contains("NO_BACKSLASH_ESCAPES")
}

/// A quoted string literal. `'` doubles; with backslash escapes on, `\` doubles and
/// NUL is written `\0`.
pub fn quote_str(s: &str, backslash_escapes: bool) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' if backslash_escapes => out.push_str("\\\\"),
            '\0' if backslash_escapes => out.push_str("\\0"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// A value as an SQL literal.
///
/// # Errors
/// A float that is not finite, or decimal text that is not a number.
pub fn literal(v: &Value, backslash_escapes: bool) -> Result<String, String> {
    Ok(match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "1" } else { "0" }.into(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) if !f.is_finite() => return Err(format!("{f} cannot be stored")),
        // Display is the shortest text that reads back to the same float; very large
        // and very small numbers use an exponent so the server reads a float too.
        Value::Float(f) if *f != 0. && !(1e-5..1e15).contains(&f.abs()) => format!("{f:e}"),
        Value::Float(f) => f.to_string(),
        Value::Decimal(d) if is_decimal(d) => d.clone(),
        Value::Decimal(d) => return Err(format!("{d:?} is not a number")),
        Value::Text(s) => quote_str(s, backslash_escapes),
        Value::Bytes(b) if b.is_empty() => "X''".into(),
        Value::Bytes(b) => {
            let mut out = String::with_capacity(2 + b.len() * 2);
            out.push_str("0x");
            for x in b {
                let _ = write!(out, "{x:02X}");
            }
            out
        }
        Value::Date(_) | Value::DateTime(_) | Value::Time(_) => format!("'{v}'"),
    })
}

fn is_decimal(s: &str) -> bool {
    let digits = s.strip_prefix(['-', '+']).unwrap_or(s);
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    !(int.is_empty() && frac.is_empty())
        && int.bytes().all(|b| b.is_ascii_digit())
        && frac.bytes().all(|b| b.is_ascii_digit())
}

fn edit_literal(e: &Edit, info: &TableInfo) -> Result<String, String> {
    match e {
        Edit::Default => Ok("DEFAULT".into()),
        Edit::Value(v) => info.literal(v),
    }
}

/// `` `id` = 48212 and `k` = 'a' `` from the row's key values.
fn key_where(info: &TableInfo, original: &[Value]) -> Result<String, String> {
    let key = info
        .key
        .as_ref()
        .ok_or("the table has no key to identify rows")?;
    let mut parts = Vec::with_capacity(key.len());
    for &i in key {
        let col = &info.columns[i];
        let v = original
            .get(i)
            .ok_or_else(|| format!("the row has no value for {}", col.name))?;
        if *v == Value::Null {
            return Err(format!(
                "the row has NULL in key column {}, so it cannot be told apart",
                col.name
            ));
        }
        parts.push(format!("{} = {}", info.column(i), info.literal(v)?));
    }
    Ok(parts.join(" and "))
}

/// `update … set … where <original key>`, setting only `set`'s columns.
///
/// # Errors
/// No key, a NULL key value, nothing to set, or a value with no literal.
pub fn update_sql(
    t: &TableRef,
    info: &TableInfo,
    original: &[Value],
    set: &BTreeMap<usize, Edit>,
) -> Result<String, String> {
    if set.is_empty() {
        return Err("nothing to update".into());
    }
    let mut sets = Vec::with_capacity(set.len());
    for (&i, e) in set {
        let col = info.columns.get(i).ok_or("no such column")?;
        if col.pseudo {
            return Err(format!("{} cannot be changed", col.name));
        }
        sets.push(format!("{} = {}", info.column(i), edit_literal(e, info)?));
    }
    Ok(format!(
        "update {} set {} where {}",
        info.qualified(t),
        sets.join(", "),
        key_where(info, original)?
    ))
}

/// `delete from … where <original key>`.
///
/// # Errors
/// No key, or a NULL key value.
pub fn delete_sql(t: &TableRef, info: &TableInfo, original: &[Value]) -> Result<String, String> {
    Ok(format!(
        "delete from {} where {}",
        info.qualified(t),
        key_where(info, original)?
    ))
}

/// `insert into … (…) values (…)` with the columns that have a value; the rest take
/// their defaults.
///
/// # Errors
/// A value with no literal.
pub fn insert_sql(t: &TableRef, info: &TableInfo, values: &[Edit]) -> Result<String, String> {
    let mut names = Vec::new();
    let mut vals = Vec::new();
    for (i, (col, e)) in info.columns.iter().zip(values).enumerate() {
        if let Edit::Value(v) = e
            && !col.pseudo
        {
            names.push(info.column(i));
            vals.push(info.literal(v)?);
        }
    }
    if names.is_empty() && info.oracle {
        // Oracle has no `() values ()`: the first column takes its default.
        let first = info
            .columns
            .iter()
            .position(|c| !c.pseudo)
            .ok_or("no columns")?;
        names.push(info.column(first));
        vals.push("DEFAULT".into());
    }
    Ok(format!(
        "insert into {} ({}) values ({})",
        info.qualified(t),
        names.join(", "),
        vals.join(", ")
    ))
}

/// What a change does; each one must touch exactly one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Update,
    Delete,
    Insert,
}

/// One statement of a submit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub kind: ChangeKind,
    pub sql: String,
}

/// A row of the grid: one fetched from the server, or one added and not yet inserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RowId {
    /// Index into the fetched rows.
    Loaded(usize),
    /// An added row, by an id that stays put while other added rows come and go.
    Added(u64),
}

/// A row's pending state, kept for undo.
#[derive(Debug, Clone, PartialEq)]
enum Snapshot {
    Loaded {
        updates: Option<BTreeMap<usize, Edit>>,
        deleted: bool,
    },
    /// Its place and values, or `None` when the row did not exist.
    Added(Option<(usize, Vec<Edit>)>),
}

/// The pending changes of a table tab: nothing reaches the server until a submit.
#[derive(Debug, Clone, Default)]
pub struct Pending {
    updates: BTreeMap<usize, BTreeMap<usize, Edit>>,
    deleted: BTreeSet<usize>,
    added: Vec<(u64, Vec<Edit>)>,
    next_id: u64,
    /// Each step holds the rows it touched as they were before.
    undo: Vec<Vec<(RowId, Snapshot)>>,
}

impl Pending {
    /// Statements a submit would run: one per updated, deleted or added row.
    pub fn count(&self) -> usize {
        self.updates
            .keys()
            .filter(|r| !self.deleted.contains(r))
            .count()
            + self.deleted.len()
            + self.added.len()
    }

    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }

    /// The added rows, in the order they were added.
    pub fn added(&self) -> impl Iterator<Item = (u64, &[Edit])> {
        self.added.iter().map(|(id, v)| (*id, v.as_slice()))
    }

    pub fn added_len(&self) -> usize {
        self.added.len()
    }

    /// Fetched rows with changed cells and not deleted, by index, with their cells.
    pub fn updated(&self) -> impl Iterator<Item = (usize, &BTreeMap<usize, Edit>)> {
        self.updates
            .iter()
            .filter(|(r, _)| !self.deleted.contains(r))
            .map(|(r, c)| (*r, c))
    }

    /// Fetched rows marked deleted, by index.
    pub fn deleted(&self) -> impl Iterator<Item = usize> + '_ {
        self.deleted.iter().copied()
    }

    /// A cell's pending value, if it has one.
    pub fn cell(&self, row: RowId, col: usize) -> Option<&Edit> {
        match row {
            RowId::Loaded(r) => self.updates.get(&r)?.get(&col),
            RowId::Added(id) => self.added.iter().find(|(i, _)| *i == id)?.1.get(col),
        }
    }

    pub fn is_deleted(&self, row: RowId) -> bool {
        matches!(row, RowId::Loaded(r) if self.deleted.contains(&r))
    }

    /// `M`, `D`, `A` or nothing.
    pub fn mark(&self, row: RowId) -> Option<char> {
        match row {
            RowId::Added(_) => Some('A'),
            RowId::Loaded(r) if self.deleted.contains(&r) => Some('D'),
            RowId::Loaded(r) if self.updates.contains_key(&r) => Some('M'),
            RowId::Loaded(_) => None,
        }
    }

    fn snapshot(&self, row: RowId) -> Snapshot {
        match row {
            RowId::Loaded(r) => Snapshot::Loaded {
                updates: self.updates.get(&r).cloned(),
                deleted: self.deleted.contains(&r),
            },
            RowId::Added(id) => Snapshot::Added(
                self.added
                    .iter()
                    .position(|(i, _)| *i == id)
                    .map(|p| (p, self.added[p].1.clone())),
            ),
        }
    }

    fn restore(&mut self, row: RowId, s: Snapshot) {
        match (row, s) {
            (RowId::Loaded(r), Snapshot::Loaded { updates, deleted }) => {
                match updates {
                    Some(u) => self.updates.insert(r, u),
                    None => self.updates.remove(&r),
                };
                if deleted {
                    self.deleted.insert(r);
                } else {
                    self.deleted.remove(&r);
                }
            }
            (RowId::Added(id), Snapshot::Added(state)) => {
                self.added.retain(|(i, _)| *i != id);
                if let Some((pos, values)) = state {
                    self.added.insert(pos.min(self.added.len()), (id, values));
                }
            }
            _ => {}
        }
    }

    /// Sets a cell. On a fetched row, setting it back to `original` drops the change.
    /// Deleted rows do not change.
    pub fn set(&mut self, row: RowId, col: usize, edit: Edit, original: Option<&Value>) {
        if self.is_deleted(row) {
            return;
        }
        let before = self.snapshot(row);
        match row {
            RowId::Loaded(r) => {
                let same = matches!((&edit, original), (Edit::Value(v), Some(o)) if v == o);
                let cells = self.updates.entry(r).or_default();
                if same {
                    cells.remove(&col);
                } else {
                    cells.insert(col, edit);
                }
                if cells.is_empty() {
                    self.updates.remove(&r);
                }
            }
            RowId::Added(id) => {
                if let Some(cell) = self
                    .added
                    .iter_mut()
                    .find(|(i, _)| *i == id)
                    .and_then(|(_, v)| v.get_mut(col))
                {
                    *cell = edit;
                }
            }
        }
        if self.snapshot(row) != before {
            self.undo.push(vec![(row, before)]);
        }
    }

    /// A new row with every column at its default. Returns its id.
    pub fn add(&mut self, columns: usize) -> RowId {
        self.add_values(vec![Edit::Default; columns])
    }

    /// A new row with these cells, as one undo step: a duplicated row.
    pub fn add_values(&mut self, values: Vec<Edit>) -> RowId {
        let id = self.next_id;
        self.next_id += 1;
        let row = RowId::Added(id);
        self.undo.push(vec![(row, self.snapshot(row))]);
        self.added.push((id, values));
        row
    }

    /// Marks fetched rows deleted and drops added ones, as one undo step.
    pub fn delete(&mut self, rows: &[RowId]) {
        let step: Vec<_> = rows.iter().map(|&r| (r, self.snapshot(r))).collect();
        for &row in rows {
            match row {
                RowId::Loaded(r) => {
                    self.deleted.insert(r);
                }
                RowId::Added(id) => self.added.retain(|(i, _)| *i != id),
            }
        }
        if !step.is_empty() {
            self.undo.push(step);
        }
    }

    /// Drops every pending change of `row`, as one undo step.
    pub fn revert(&mut self, row: RowId) {
        let before = self.snapshot(row);
        match row {
            RowId::Loaded(r) => {
                self.updates.remove(&r);
                self.deleted.remove(&r);
            }
            RowId::Added(id) => self.added.retain(|(i, _)| *i != id),
        }
        if self.snapshot(row) != before {
            self.undo.push(vec![(row, before)]);
        }
    }

    /// Undoes the last change. Returns whether there was one.
    pub fn undo(&mut self) -> bool {
        let Some(step) = self.undo.pop() else {
            return false;
        };
        for (row, s) in step.into_iter().rev() {
            self.restore(row, s);
        }
        true
    }

    /// The statements to run, updates first, then deletes, then inserts. `rows` are
    /// the fetched rows the indexes point into.
    ///
    /// # Errors
    /// A row that cannot be identified or a value with no literal; the message names it.
    pub fn changes(
        &self,
        t: &TableRef,
        info: &TableInfo,
        rows: &[Row],
    ) -> Result<Vec<Change>, String> {
        let row = |r: usize| rows.get(r).ok_or_else(|| format!("row {} is gone", r + 1));
        let mut out = Vec::with_capacity(self.count());
        for (&r, set) in &self.updates {
            if !self.deleted.contains(&r) {
                let sql =
                    update_sql(t, info, row(r)?, set).map_err(|e| format!("row {}: {e}", r + 1))?;
                out.push(Change {
                    kind: ChangeKind::Update,
                    sql,
                });
            }
        }
        for &r in &self.deleted {
            let sql = delete_sql(t, info, row(r)?).map_err(|e| format!("row {}: {e}", r + 1))?;
            out.push(Change {
                kind: ChangeKind::Delete,
                sql,
            });
        }
        for (_, values) in &self.added {
            out.push(Change {
                kind: ChangeKind::Insert,
                sql: insert_sql(t, info, values)?,
            });
        }
        Ok(out)
    }

    /// `Two updates, one delete and one insert.`
    pub fn summary(&self) -> String {
        let updates = self.count() - self.deleted.len() - self.added.len();
        let parts: Vec<String> = [
            (updates, "update"),
            (self.deleted.len(), "delete"),
            (self.added.len(), "insert"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, noun)| format!("{} {noun}{}", number(n), if n == 1 { "" } else { "s" }))
        .collect();
        let text = match parts.as_slice() {
            [] => return String::new(),
            [one] => one.clone(),
            [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
        };
        let mut chars = text.chars();
        chars.next().map_or_else(String::new, |c| {
            format!("{}{}.", c.to_uppercase(), chars.as_str())
        })
    }
}

/// `one` … `ten`, then digits.
fn number(n: usize) -> String {
    const WORDS: [&str; 10] = [
        "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    ];
    n.checked_sub(1)
        .and_then(|i| WORDS.get(i))
        .map_or_else(|| n.to_string(), |w| (*w).to_owned())
}

/// Checks typed text against the column's type and returns the value to store.
/// Binary columns and `bit` take hex (`0x…`); `bit` also takes binary digits.
///
/// # Errors
/// Text the column cannot hold, with a message for the user.
pub fn parse_input(col: &ColumnInfo, text: &str) -> Result<Value, String> {
    let t = text.trim();
    let (size, scale) = col.size();
    match col.data_type.as_str() {
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" => {
            let bits = match col.data_type.as_str() {
                "tinyint" => 8,
                "smallint" => 16,
                "mediumint" => 24,
                "int" | "integer" => 32,
                _ => 64,
            };
            if col.unsigned() {
                let max = if bits == 64 {
                    u64::MAX
                } else {
                    (1u64 << bits) - 1
                };
                match t.parse::<u64>() {
                    Ok(v) if v <= max => Ok(Value::UInt(v)),
                    _ => Err(format!("expected a whole number from 0 to {max}")),
                }
            } else {
                let (min, max) = if bits == 64 {
                    (i64::MIN, i64::MAX)
                } else {
                    (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1)
                };
                match t.parse::<i64>() {
                    Ok(v) if (min..=max).contains(&v) => Ok(Value::Int(v)),
                    _ => Err(format!("expected a whole number from {min} to {max}")),
                }
            }
        }
        "decimal" | "numeric" => {
            let p = size.unwrap_or(10);
            let s = scale.unwrap_or(0);
            if !is_decimal(t) {
                return Err("expected a number like 12.50".into());
            }
            let digits = t.trim_start_matches(['-', '+']);
            let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
            let int = int.trim_start_matches('0');
            if frac.len() as u64 > s {
                return Err(format!("at most {s} digits after the point"));
            }
            if int.len() as u64 > p.saturating_sub(s) {
                return Err(format!(
                    "at most {} digits before the point",
                    p.saturating_sub(s)
                ));
            }
            if t.starts_with('-') && col.unsigned() {
                return Err("expected a number of 0 or more".into());
            }
            Ok(Value::Decimal(t.trim_start_matches('+').to_owned()))
        }
        // Oracle's NUMBER without a precision, and FLOAT: any number, exponent allowed.
        "number" => {
            let plain = is_decimal(t)
                || t.parse::<f64>().is_ok_and(f64::is_finite)
                    && t.bytes().all(|b| {
                        b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b'e' | b'E')
                    });
            if plain {
                Ok(Value::Decimal(t.trim_start_matches('+').to_owned()))
            } else {
                Err("expected a number like 12.50 or 1e-3".into())
            }
        }
        "float" | "double" | "real" => match t.parse::<f64>() {
            Ok(v) if v.is_finite() && !(col.unsigned() && v < 0.) => Ok(Value::Float(v)),
            _ => Err("expected a number like 1.5 or 2e-3".into()),
        },
        "date" => NaiveDate::parse_from_str(t, "%Y-%m-%d")
            .map(Value::Date)
            .map_err(|_| "expected a date like 2026-11-22".into()),
        "datetime" | "timestamp" => parse_datetime(t)
            .map(Value::DateTime)
            .ok_or_else(|| "expected a date and time like 2026-11-22 16:05:00".into()),
        "time" => parse_time(t)
            .filter(|d| d.abs() < TimeDelta::hours(839))
            .map(Value::Time)
            .ok_or_else(|| "expected a time like 16:05:00, up to ±838:59:59".into()),
        "year" => match t.parse::<u64>() {
            Ok(v) if v == 0 || (1901..=2155).contains(&v) => Ok(Value::UInt(v)),
            _ => Err("expected a year from 1901 to 2155".into()),
        },
        "bit" => {
            let bytes = parse_bits(t).ok_or("expected binary digits like 101, or hex like 0x05")?;
            let width = size.unwrap_or(1);
            let used = bytes.iter().position(|b| *b != 0).map_or(0, |i| {
                (bytes.len() - i) as u64 * 8 - u64::from(bytes[i].leading_zeros())
            });
            if used > width {
                return Err(format!("at most {width} bits"));
            }
            Ok(Value::Bytes(bytes))
        }
        "enum" => {
            let members = col.members();
            if members.iter().any(|m| m == text) {
                Ok(Value::Text(text.to_owned()))
            } else {
                Err(format!("expected one of {}", members.join(", ")))
            }
        }
        "set" => {
            let members = col.members();
            match text
                .split(',')
                .find(|p| !p.is_empty() && !members.iter().any(|m| m == p))
            {
                Some(bad) => Err(format!("{bad} is not one of {}", members.join(", "))),
                None => Ok(Value::Text(text.to_owned())),
            }
        }
        "json" => serde_json::from_str::<serde_json::Value>(text)
            .map(|_| Value::Text(text.to_owned()))
            .map_err(|e| format!("not valid JSON: {e}")),
        _ if col.is_binary() => {
            let bytes = parse_hex(t).ok_or("expected hex like 0x89504E47")?;
            let max = match col.data_type.as_str() {
                "binary" | "varbinary" => size,
                "tinyblob" => Some(255),
                "blob" => Some(65_535),
                "mediumblob" => Some(16_777_215),
                _ => None,
            };
            match max {
                Some(m) if bytes.len() as u64 > m => Err(format!("at most {m} bytes")),
                _ => Ok(Value::Bytes(bytes)),
            }
        }
        "char" | "varchar" => match size {
            Some(n) if text.chars().count() as u64 > n => Err(format!("at most {n} characters")),
            _ => Ok(Value::Text(text.to_owned())),
        },
        ty @ ("tinytext" | "text" | "mediumtext") => {
            let max = match ty {
                "tinytext" => 255,
                "text" => 65_535,
                _ => 16_777_215,
            };
            if text.len() > max {
                Err(format!("at most {max} bytes"))
            } else {
                Ok(Value::Text(text.to_owned()))
            }
        }
        _ => Ok(Value::Text(text.to_owned())),
    }
}

fn parse_datetime(t: &str) -> Option<NaiveDateTime> {
    [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
    ]
    .iter()
    .find_map(|f| NaiveDateTime::parse_from_str(t, f).ok())
    .or_else(|| {
        NaiveDate::parse_from_str(t, "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
    })
}

/// `-838:59:59.5` → a signed duration.
pub(crate) fn parse_time(text: &str) -> Option<TimeDelta> {
    let (negative, rest) = match text.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, text),
    };
    let (hms, frac) = rest.split_once('.').unwrap_or((rest, ""));
    let mut parts = hms.split(':').map(str::parse::<i64>);
    let (h, m, s) = (
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    );
    if parts.next().is_some() || !(0..60).contains(&m) || !(0..60).contains(&s) || h < 0 {
        return None;
    }
    let micros = if frac.is_empty() {
        0
    } else if frac.len() <= 6 && frac.bytes().all(|b| b.is_ascii_digit()) {
        format!("{frac:0<6}").parse::<i64>().ok()?
    } else {
        return None;
    };
    let d = TimeDelta::hours(h)
        + TimeDelta::minutes(m)
        + TimeDelta::seconds(s)
        + TimeDelta::microseconds(micros);
    Some(if negative { -d } else { d })
}

/// `0x0A0B` or `0A0B`; an odd digit count gets a leading zero.
fn parse_hex(t: &str) -> Option<Vec<u8>> {
    let h = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let padded = if h.len() % 2 == 1 {
        format!("0{h}")
    } else {
        h.to_owned()
    };
    (0..padded.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&padded[i..i + 2], 16).ok())
        .collect()
}

/// `101`, `b'101'` or hex `0x05`, as big-endian bytes.
fn parse_bits(t: &str) -> Option<Vec<u8>> {
    if t.starts_with("0x") || t.starts_with("0X") {
        return parse_hex(t);
    }
    let digits = t
        .strip_prefix("b'")
        .and_then(|r| r.strip_suffix('\''))
        .unwrap_or(t);
    if digits.is_empty() || !digits.bytes().all(|b| b == b'0' || b == b'1') {
        return None;
    }
    let pad = (8 - digits.len() % 8) % 8;
    let padded = format!("{}{digits}", "0".repeat(pad));
    (0..padded.len())
        .step_by(8)
        .map(|i| u8::from_str_radix(&padded[i..i + 8], 2).ok())
        .collect()
}

/// The text a cell's editor starts with: hex for binary and bit, empty for NULL.
pub fn edit_text(col: &ColumnInfo, v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bytes(b) if col.data_type == "bit" => {
            let bits: String = b.iter().map(|x| format!("{x:08b}")).collect();
            let trimmed = bits.trim_start_matches('0');
            if trimmed.is_empty() {
                "0".into()
            } else {
                trimmed.into()
            }
        }
        v => v.to_string(),
    }
}

/// `select <columns> from <table> where (<filter>) order by <order>, <key>`. The
/// filter and order are the user's own SQL; each sits on its own lines so a trailing
/// `--` comment cannot swallow what follows. The key ends the order so pages are
/// stable.
///
/// # Errors
/// The filter or order holds more than one statement.
pub fn select_sql(
    t: &TableRef,
    info: &TableInfo,
    filter: &str,
    order: &str,
) -> Result<String, String> {
    let cols: Vec<String> = (0..info.columns.len()).map(|i| info.column(i)).collect();
    let mut sql = format!(
        "select {} from {}",
        if cols.is_empty() {
            "*".into()
        } else {
            cols.join(", ")
        },
        info.qualified(t)
    );
    if !filter.trim().is_empty() {
        let _ = write!(sql, "\nwhere (\n{}\n)", filter.trim());
    }
    let mut order_by: Vec<String> = Vec::new();
    if !order.trim().is_empty() {
        order_by.push(format!("\n{}\n", order.trim()));
    }
    if let Some(key) = &info.key {
        order_by.extend(key.iter().map(|&i| info.column(i)));
    }
    if !order_by.is_empty() {
        let _ = write!(sql, "\norder by {}", order_by.join(", "));
    }
    one_statement(&sql, info)?;
    Ok(sql)
}

/// `select count(*) from <table> where (<filter>)`. With `limit`, it counts no more
/// than that many rows, so a table without statistics costs at most that much.
///
/// # Errors
/// The filter holds more than one statement.
pub fn count_sql(
    t: &TableRef,
    info: &TableInfo,
    filter: &str,
    limit: Option<u64>,
) -> Result<String, String> {
    let from = if limit.is_some() { "1" } else { "count(*)" };
    let mut sql = format!("select {from} from {}", info.qualified(t));
    if !filter.trim().is_empty() {
        let _ = write!(sql, "\nwhere (\n{}\n)", filter.trim());
    }
    if let Some(n) = limit {
        sql = if info.oracle {
            format!("select count(*) from ({sql}\nfetch first {n} rows only\n)")
        } else {
            format!("select count(*) from ({sql}\nlimit {n}\n) x")
        };
    }
    one_statement(&sql, info)?;
    Ok(sql)
}

/// Scans literals as the session does (`NO_BACKSLASH_ESCAPES`).
fn one_statement(sql: &str, info: &TableInfo) -> Result<(), String> {
    if crate::split::split_in_mode(sql, info.engine(), info.backslash_escapes).len() == 1 {
        Ok(())
    } else {
        Err("the where and order by fields must not end the statement (;)".into())
    }
}

/// Text that is a JSON object or array: what the Edit JSON dialog opens.
pub fn is_json_doc(text: &str) -> bool {
    let t = text.trim_start();
    (t.starts_with('{') || t.starts_with('['))
        && serde_json::from_str::<serde_json::Value>(t).is_ok()
}

/// Reflows valid JSON: `indent` spaces per level, or `None` for no whitespace at all.
/// Only whitespace outside strings changes, so key order and number text stay exactly
/// as they were.
pub fn json_reflow(text: &str, indent: Option<usize>) -> String {
    let mut out = String::with_capacity(text.len());
    let (mut depth, mut in_str, mut escaped) = (0usize, false, false);
    let mut chars = text.chars().peekable();
    let newline = |out: &mut String, depth: usize| {
        if let Some(n) = indent {
            out.push('\n');
            out.push_str(&" ".repeat(n * depth));
        }
    };
    while let Some(c) = chars.next() {
        if in_str {
            out.push(c);
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            c if c.is_whitespace() => {}
            '"' => {
                in_str = true;
                out.push(c);
            }
            '{' | '[' => {
                out.push(c);
                while chars.next_if(|c| c.is_whitespace()).is_some() {}
                if chars.peek().is_some_and(|n| matches!(n, '}' | ']')) {
                    out.extend(chars.next());
                } else {
                    depth += 1;
                    newline(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            ':' => out.push_str(if indent.is_some() { ": " } else { ":" }),
            c => out.push(c),
        }
    }
    out
}

/// One page of `select`: `limit … offset …`, or Oracle's `offset … rows fetch next …
/// rows only`. `select` ends with the table key in its order by, so pages are stable.
pub fn page_sql(select: &str, oracle: bool, limit: usize, offset: usize) -> String {
    if oracle {
        format!("{select}\noffset {offset} rows fetch next {limit} rows only")
    } else {
        format!("{select}\nlimit {limit} offset {offset}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, data_type: &str, column_type: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: data_type.into(),
            column_type: column_type.into(),
            ..ColumnInfo::default()
        }
    }

    fn orders() -> (TableRef, TableInfo) {
        let t = TableRef {
            database: "shop".into(),
            table: "orders".into(),
        };
        let info = TableInfo {
            columns: vec![
                ColumnInfo {
                    auto_increment: true,
                    ..col("id", "bigint", "bigint")
                },
                col("status", "varchar", "varchar(20)"),
                ColumnInfo {
                    nullable: true,
                    ..col("total", "decimal", "decimal(10,2)")
                },
            ],
            key: Some(vec![0]),
            backslash_escapes: true,
            ..TableInfo::default()
        };
        (t, info)
    }

    fn text(s: &str) -> Value {
        Value::Text(s.into())
    }

    #[test]
    fn identifiers_and_strings_are_quoted() {
        assert_eq!(quote_ident("a`b"), "`a``b`");
        assert_eq!(quote_str("it's", true), "'it''s'");
        assert_eq!(quote_str(r"C:\new", true), r"'C:\\new'");
        assert_eq!(quote_str(r"C:\new", false), r"'C:\new'");
        assert_eq!(quote_str("a\0b", true), r"'a\0b'");
        assert_eq!(quote_str("line\nbreak", true), "'line\nbreak'");
        assert_eq!(quote_str(r"\'", true), r"'\\'''");
    }

    #[test]
    fn literals_per_type() {
        let lit = |v: Value| literal(&v, true).unwrap();
        assert_eq!(lit(Value::Null), "NULL");
        assert_eq!(lit(Value::Int(-5)), "-5");
        assert_eq!(lit(Value::UInt(u64::MAX)), "18446744073709551615");
        assert_eq!(lit(Value::Float(0.1)), "0.1");
        assert_eq!(lit(Value::Float(1e300)), "1e300");
        assert_eq!(lit(Value::Float(-2.5e-9)), "-2.5e-9");
        assert_eq!(lit(Value::Decimal("325.20".into())), "325.20");
        assert_eq!(lit(Value::Bytes(vec![0x00, 0xAB])), "0x00AB");
        assert_eq!(lit(Value::Bytes(vec![])), "X''");
        assert_eq!(
            lit(Value::Date(NaiveDate::from_ymd_opt(2026, 11, 22).unwrap())),
            "'2026-11-22'"
        );
        let dt = NaiveDate::from_ymd_opt(2026, 11, 22)
            .unwrap()
            .and_hms_micro_opt(16, 5, 0, 120_000)
            .unwrap();
        assert_eq!(lit(Value::DateTime(dt)), "'2026-11-22 16:05:00.120'");
        assert_eq!(lit(Value::Time(-TimeDelta::seconds(3661))), "'-01:01:01'");
        assert!(literal(&Value::Float(f64::NAN), true).is_err());
        assert!(literal(&Value::Decimal("1; drop".into()), true).is_err());
    }

    #[test]
    fn update_sets_only_changed_columns_by_original_key() {
        let (t, info) = orders();
        let original = vec![Value::Int(48212), text("paid"), Value::Null];
        let set = BTreeMap::from([(1, Edit::Value(text("shipped")))]);
        assert_eq!(
            update_sql(&t, &info, &original, &set).unwrap(),
            "update `shop`.`orders` set `status` = 'shipped' where `id` = 48212"
        );
        // A primary-key edit sets the new key and finds the row by the old one.
        let set = BTreeMap::from([
            (0, Edit::Value(Value::Int(1))),
            (2, Edit::Value(Value::Null)),
        ]);
        assert_eq!(
            update_sql(&t, &info, &original, &set).unwrap(),
            "update `shop`.`orders` set `id` = 1, `total` = NULL where `id` = 48212"
        );
        let set = BTreeMap::from([(2, Edit::Default)]);
        assert!(
            update_sql(&t, &info, &original, &set)
                .unwrap()
                .contains("`total` = DEFAULT")
        );
        assert!(update_sql(&t, &info, &original, &BTreeMap::new()).is_err());
    }

    #[test]
    fn composite_and_quoted_keys() {
        let t = TableRef {
            database: "d`b".into(),
            table: "o'rders".into(),
        };
        let info = TableInfo {
            columns: vec![
                col("tenant", "varchar", "varchar(10)"),
                col("n", "int", "int"),
                col("note", "text", "text"),
            ],
            key: Some(vec![0, 1]),
            backslash_escapes: true,
            ..TableInfo::default()
        };
        let original = vec![text(r"a'\b"), Value::Int(7), text("x")];
        assert_eq!(
            delete_sql(&t, &info, &original).unwrap(),
            r"delete from `d``b`.`o'rders` where `tenant` = 'a''\\b' and `n` = 7"
        );
        // A NULL in the key cannot identify the row.
        let original = vec![Value::Null, Value::Int(7), text("x")];
        assert!(
            delete_sql(&t, &info, &original)
                .unwrap_err()
                .contains("NULL")
        );
        let no_key = TableInfo { key: None, ..info };
        assert!(delete_sql(&t, &no_key, &[text("a"), Value::Int(1), text("x")]).is_err());
    }

    #[test]
    fn inserts_leave_defaults_out() {
        let (t, info) = orders();
        let values = [
            Edit::Default,
            Edit::Value(text("paid")),
            Edit::Value(Value::Decimal("49.00".into())),
        ];
        assert_eq!(
            insert_sql(&t, &info, &values).unwrap(),
            "insert into `shop`.`orders` (`status`, `total`) values ('paid', 49.00)"
        );
        assert_eq!(
            insert_sql(&t, &info, &[Edit::Default, Edit::Default, Edit::Default]).unwrap(),
            "insert into `shop`.`orders` () values ()"
        );
    }

    #[test]
    fn keys_prefer_primary_then_smallest_not_null_unique() {
        let cols = vec![
            col("a", "int", "int"),
            ColumnInfo {
                nullable: true,
                ..col("b", "int", "int")
            },
            col("c", "int", "int"),
        ];
        let idx = |rows: &[(&str, bool, Option<&str>)]| -> Vec<(String, bool, Option<String>)> {
            rows.iter()
                .map(|(n, u, c)| ((*n).into(), *u, c.map(Into::into)))
                .collect()
        };
        let both = idx(&[("PRIMARY", false, Some("c")), ("u", false, Some("a"))]);
        assert_eq!(pick_key(&cols, &both), Some(vec![2]));
        let uniques = idx(&[
            ("nullable", false, Some("b")),
            ("pair", false, Some("a")),
            ("pair", false, Some("c")),
            ("plain", true, Some("a")),
        ]);
        assert_eq!(pick_key(&cols, &uniques), Some(vec![0, 2]));
        let functional = idx(&[("f", false, None), ("f", false, Some("a"))]);
        assert_eq!(pick_key(&cols, &functional), None);
        assert_eq!(
            pick_key(&cols, &idx(&[("nullable", false, Some("b"))])),
            None
        );
    }

    #[test]
    fn input_is_checked_against_the_type() {
        let ok = |c: &ColumnInfo, t: &str| parse_input(c, t).unwrap();
        let bad = |c: &ColumnInfo, t: &str| parse_input(c, t).is_err();
        let tiny = col("t", "tinyint", "tinyint");
        assert_eq!(ok(&tiny, "-128"), Value::Int(-128));
        assert!(bad(&tiny, "128") && bad(&tiny, "1.5") && bad(&tiny, "x"));
        let utiny = col("t", "tinyint", "tinyint unsigned");
        assert_eq!(ok(&utiny, "255"), Value::UInt(255));
        assert!(bad(&utiny, "-1"));
        let big = col("b", "bigint", "bigint unsigned");
        assert_eq!(ok(&big, "18446744073709551615"), Value::UInt(u64::MAX));
        let dec = col("d", "decimal", "decimal(10,2)");
        assert_eq!(ok(&dec, "325.20"), Value::Decimal("325.20".into()));
        assert_eq!(ok(&dec, "-0.5"), Value::Decimal("-0.5".into()));
        assert!(bad(&dec, "1.234") && bad(&dec, "123456789") && bad(&dec, "1e3"));
        assert_eq!(
            ok(&col("f", "double", "double"), "2e-3"),
            Value::Float(0.002)
        );
        assert!(bad(&col("f", "double", "double"), "inf"));
        let date = col("d", "date", "date");
        assert_eq!(
            ok(&date, "2026-11-22"),
            Value::Date(NaiveDate::from_ymd_opt(2026, 11, 22).unwrap())
        );
        assert!(bad(&date, "2026-02-30"));
        let dt = col("d", "datetime", "datetime(6)");
        assert!(matches!(
            ok(&dt, "2026-11-22 16:05:00.5"),
            Value::DateTime(_)
        ));
        assert!(matches!(ok(&dt, "2026-11-22T16:05:00"), Value::DateTime(_)));
        assert!(bad(&dt, "yesterday"));
        let time = col("t", "time", "time");
        assert_eq!(
            ok(&time, "-838:59:59"),
            Value::Time(-TimeDelta::seconds(838 * 3600 + 59 * 60 + 59))
        );
        assert!(bad(&time, "839:00:00") && bad(&time, "10:61:00"));
        let en = col("e", "enum", "enum('paid','it''s','a,b')");
        assert_eq!(ok(&en, "it's"), text("it's"));
        assert!(bad(&en, "PAID") && bad(&en, "nope"));
        let set = col("s", "set", "set('a','b')");
        assert_eq!(ok(&set, "a,b"), text("a,b"));
        assert!(bad(&set, "a,c"));
        let bit = col("b", "bit", "bit(3)");
        assert_eq!(ok(&bit, "101"), Value::Bytes(vec![5]));
        assert_eq!(ok(&bit, "0x07"), Value::Bytes(vec![7]));
        assert!(bad(&bit, "1111") && bad(&bit, "102"));
        let json = col("j", "json", "json");
        assert_eq!(ok(&json, r#"{"a": [1, "x"]}"#), text(r#"{"a": [1, "x"]}"#));
        assert!(bad(&json, "{a:1}"));
        let bin = col("b", "varbinary", "varbinary(2)");
        assert_eq!(ok(&bin, "0xABC"), Value::Bytes(vec![0x0A, 0xBC]));
        assert!(bad(&bin, "0x010203") && bad(&bin, "zz"));
        let vc = col("v", "varchar", "varchar(3)");
        assert_eq!(ok(&vc, "äöü"), text("äöü"));
        assert!(bad(&vc, "abcd"));
        assert_eq!(ok(&vc, ""), text(""));
        assert_eq!(ok(&col("y", "year", "year"), "2026"), Value::UInt(2026));
    }

    #[test]
    fn bits_edit_as_binary_digits() {
        let bit = col("b", "bit", "bit(8)");
        assert_eq!(edit_text(&bit, &Value::Bytes(vec![5])), "101");
        assert_eq!(edit_text(&bit, &Value::Bytes(vec![0])), "0");
        assert_eq!(edit_text(&bit, &Value::Null), "");
    }

    #[test]
    fn pending_changes_count_undo_and_generate() {
        let (t, info) = orders();
        let rows: Vec<Row> = (0..3)
            .map(|i| {
                vec![
                    Value::Int(10 + i),
                    text("paid"),
                    Value::Decimal("1.00".into()),
                ]
            })
            .collect();
        let mut p = Pending::default();
        p.set(
            RowId::Loaded(0),
            1,
            Edit::Value(text("shipped")),
            Some(&rows[0][1]),
        );
        p.set(
            RowId::Loaded(1),
            2,
            Edit::Value(Value::Decimal("2.00".into())),
            Some(&rows[1][2]),
        );
        p.delete(&[RowId::Loaded(2)]);
        let a = p.add(3);
        p.set(a, 1, Edit::Value(text("paid")), None);
        assert_eq!(p.count(), 4);
        assert_eq!(p.summary(), "Two updates, one delete and one insert.");
        assert_eq!(p.mark(RowId::Loaded(0)), Some('M'));
        assert_eq!(p.mark(RowId::Loaded(2)), Some('D'));
        assert_eq!(p.mark(a), Some('A'));
        let sql: Vec<String> = p
            .changes(&t, &info, &rows)
            .unwrap()
            .into_iter()
            .map(|c| c.sql)
            .collect();
        assert_eq!(
            sql,
            [
                "update `shop`.`orders` set `status` = 'shipped' where `id` = 10",
                "update `shop`.`orders` set `total` = 2.00 where `id` = 11",
                "delete from `shop`.`orders` where `id` = 12",
                "insert into `shop`.`orders` (`status`) values ('paid')",
            ]
        );
        // Setting a cell back to its original value drops the change.
        p.set(
            RowId::Loaded(1),
            2,
            Edit::Value(rows[1][2].clone()),
            Some(&rows[1][2]),
        );
        assert_eq!(p.mark(RowId::Loaded(1)), None);
        // Ctrl+Z goes back one step at a time.
        assert!(p.undo());
        assert_eq!(p.mark(RowId::Loaded(1)), Some('M'));
        assert!(p.undo());
        assert_eq!(p.cell(a, 1), Some(&Edit::Default));
        assert!(p.undo());
        assert_eq!(p.added_len(), 0);
        assert!(p.undo());
        assert!(!p.is_deleted(RowId::Loaded(2)));
        // Deleted rows ignore edits; a revert is one undo step.
        p.delete(&[RowId::Loaded(0)]);
        p.set(
            RowId::Loaded(0),
            1,
            Edit::Value(text("x")),
            Some(&rows[0][1]),
        );
        assert_eq!(
            p.cell(RowId::Loaded(0), 1),
            Some(&Edit::Value(text("shipped")))
        );
        p.revert(RowId::Loaded(0));
        assert_eq!(p.mark(RowId::Loaded(0)), None);
        assert!(p.undo());
        assert_eq!(p.mark(RowId::Loaded(0)), Some('D'));
        assert_eq!(p.summary(), "One update and one delete.");
    }

    #[test]
    fn selects_wrap_user_text_and_end_with_the_key() {
        let (t, info) = orders();
        assert_eq!(
            select_sql(&t, &info, "", "").unwrap(),
            "select `id`, `status`, `total` from `shop`.`orders`\norder by `id`"
        );
        let sql = select_sql(&t, &info, "status = 'paid' -- note", "total desc").unwrap();
        assert_eq!(
            sql,
            "select `id`, `status`, `total` from `shop`.`orders`\nwhere (\nstatus = 'paid' -- note\n)\norder by \ntotal desc\n, `id`"
        );
        // Several sort keys keep their order, and the table key still ends it.
        assert_eq!(
            select_sql(&t, &info, "", "`status` desc, `total`").unwrap(),
            "select `id`, `status`, `total` from `shop`.`orders`\norder by \n`status` desc, `total`\n, `id`"
        );
        assert!(select_sql(&t, &info, "1; delete from orders", "").is_err());
        assert!(count_sql(&t, &info, "1); drop table x; (1", None).is_err());
        // `\'` ends the literal only without backslash escapes, as the session reads it.
        let typed = "status = 'C:\\' or status = ';'";
        assert!(select_sql(&t, &info, typed, "").is_err());
        let plain = TableInfo {
            backslash_escapes: false,
            ..info.clone()
        };
        assert!(select_sql(&t, &plain, typed, "").is_ok());
        assert_eq!(
            count_sql(&t, &info, "id > 1", Some(5)).unwrap(),
            "select count(*) from (select 1 from `shop`.`orders`\nwhere (\nid > 1\n)\nlimit 5\n) x"
        );
        assert_eq!(
            count_sql(&t, &info, "", None).unwrap(),
            "select count(*) from `shop`.`orders`"
        );
        assert_eq!(
            page_sql("select 1 from t\norder by id", false, 500, 1000),
            "select 1 from t\norder by id\nlimit 500 offset 1000"
        );
        assert_eq!(
            page_sql("select 1 from t", true, 200, 0),
            "select 1 from t\noffset 0 rows fetch next 200 rows only"
        );
    }

    #[test]
    fn json_documents_reflow_without_changing_values() {
        assert!(is_json_doc(" {\"a\": 1}") && is_json_doc("[]"));
        assert!(!is_json_doc("12") && !is_json_doc("\"{}\"") && !is_json_doc("{bad"));
        assert!(!is_json_doc("null") && !is_json_doc("true"));
        let src = r#"{"z": [1.10, {}, "a, b: \"{ }\""], "a":{ "x" : 1e3 }, "e": [ ]}"#;
        let pretty = json_reflow(src, Some(2));
        assert_eq!(
            pretty,
            "{\n  \"z\": [\n    1.10,\n    {},\n    \"a, b: \\\"{ }\\\"\"\n  ],\n  \"a\": {\n    \"x\": 1e3\n  },\n  \"e\": []\n}"
        );
        assert_eq!(
            json_reflow(&pretty, None),
            r#"{"z":[1.10,{},"a, b: \"{ }\""],"a":{"x":1e3},"e":[]}"#
        );
    }

    #[test]
    fn oracle_sql_quotes_names_and_uses_rowid() {
        let t = TableRef {
            database: "APP".into(),
            table: "Or\"ders".into(),
        };
        let info = TableInfo {
            columns: vec![
                col("N", "number", "number"),
                col("D", "datetime", "date"),
                col("R", "varbinary", "raw(4)"),
                ColumnInfo {
                    pseudo: true,
                    ..col("ROWID", "rowid", "rowid")
                },
            ],
            key: Some(vec![3]),
            oracle: true,
            ..TableInfo::default()
        };
        assert!(info.key_note().is_some());
        let d = NaiveDate::from_ymd_opt(2026, 1, 2)
            .and_then(|d| d.and_hms_opt(3, 4, 5))
            .unwrap();
        let row = [
            Value::Decimal("1".into()),
            Value::DateTime(d),
            Value::Bytes(vec![0xAB]),
            text("AAAR3sAAEAAAACXAAA"),
        ];
        let set = BTreeMap::from([
            (0, Edit::Value(text("it's"))),
            (2, Edit::Value(Value::Bytes(vec![1, 2]))),
        ]);
        assert_eq!(
            update_sql(&t, &info, &row, &set).unwrap(),
            "update \"APP\".\"Or\"\"ders\" set \"N\" = 'it''s', \"R\" = HEXTORAW('0102') where ROWID = 'AAAR3sAAEAAAACXAAA'"
        );
        let values = [
            Edit::Default,
            Edit::Value(Value::DateTime(d)),
            Edit::Default,
            Edit::Default,
        ];
        assert_eq!(
            insert_sql(&t, &info, &values).unwrap(),
            "insert into \"APP\".\"Or\"\"ders\" (\"D\") values (TIMESTAMP '2026-01-02 03:04:05')"
        );
        assert!(
            insert_sql(&t, &info, &vec![Edit::Default; 4])
                .unwrap()
                .ends_with("(\"N\") values (DEFAULT)")
        );
        let sql = select_sql(&t, &info, "", "").unwrap();
        assert_eq!(
            sql,
            "select \"N\", \"D\", \"R\", ROWID from \"APP\".\"Or\"\"ders\"\norder by ROWID"
        );
        assert_eq!(oracle_literal(&Value::Bool(true)).unwrap(), "TRUE");
        assert_eq!(oracle_literal(&Value::Bytes(vec![])).unwrap(), "NULL");
        assert!(oracle_literal(&Value::Time(TimeDelta::zero())).is_err());
        assert_eq!(oracle_literal(&Value::Float(1e300)).unwrap(), "1e300d");
        assert_eq!(oracle_literal(&Value::Float(1e20)).unwrap(), "1e20");
        assert_eq!(
            parse_input(&col("N", "number", "number"), "1e-3"),
            Ok(Value::Decimal("1e-3".into()))
        );
        assert!(parse_input(&col("N", "number", "number"), "abc").is_err());
    }
}
