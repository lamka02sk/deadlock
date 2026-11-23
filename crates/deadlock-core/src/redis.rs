//! Redis without the network: key names and how commands quote them, the key
//! browser's namespaces, the commands a key tab's pending changes become, expiry
//! input, and parsers for `INFO keyspace`, `CLIENT LIST` and `CONFIG GET`.
//!
//! Keys and values are bytes. The sidebar and the grids hold text, so a key that is
//! not plain UTF-8 is written the way `redis-cli` quotes it, and read back from that.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::ops::Bound;

use crate::driver::{NodeKind, ObjectNode, Row, Value};
use crate::edit::{ColumnInfo, Edit, Pending, TableInfo};

/// Splits key names into namespaces.
pub const DELIMITER: u8 = b':';

/// The name of a "Load more" node in the sidebar tree. No key label holds a NUL: it
/// would be quoted.
pub const MORE: &str = "\0more";

/// Keys shown under a namespace before its "Load more" row.
pub const NAMESPACE_PAGE: usize = 200;

/// What `TYPE` says a key holds.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum KeyType {
    String,
    Hash,
    List,
    Set,
    ZSet,
    Stream,
    /// RedisJSON's `ReJSON-RL`.
    Json,
    /// A module's type, by the name `TYPE` reports.
    Other(String),
}

impl KeyType {
    pub fn from_type(t: &str) -> Self {
        match t {
            "string" => Self::String,
            "hash" => Self::Hash,
            "list" => Self::List,
            "set" => Self::Set,
            "zset" => Self::ZSet,
            "stream" => Self::Stream,
            "ReJSON-RL" => Self::Json,
            other => Self::Other(other.to_owned()),
        }
    }

    /// The sidebar's type label.
    pub fn label(&self) -> &str {
        match self {
            Self::String => "string",
            Self::Hash => "hash",
            Self::List => "list",
            Self::Set => "set",
            Self::ZSet => "zset",
            Self::Stream => "stream",
            Self::Json => "json",
            Self::Other(s) => s,
        }
    }

    /// The grid's columns; empty for types edited as text or not at all.
    pub fn columns(&self) -> &'static [&'static str] {
        match self {
            Self::Hash => &["Field", "Value"],
            Self::List => &["Index", "Value"],
            Self::Set => &["Member"],
            Self::ZSet => &["Member", "Score"],
            Self::Stream => &["ID", "Fields"],
            Self::String | Self::Json | Self::Other(_) => &[],
        }
    }

    /// What the element count counts: `Fields`, `Members`.
    pub fn noun(&self) -> &'static str {
        match self {
            Self::Hash => "Fields",
            Self::List => "Elements",
            Self::Set | Self::ZSet => "Members",
            Self::Stream => "Entries",
            Self::String | Self::Json | Self::Other(_) => "Length",
        }
    }

    /// The O(1) command that counts the elements.
    pub fn len_command(&self) -> Option<&'static str> {
        Some(match self {
            Self::Hash => "HLEN",
            Self::List => "LLEN",
            Self::Set => "SCARD",
            Self::ZSet => "ZCARD",
            Self::Stream => "XLEN",
            Self::String => "STRLEN",
            Self::Json | Self::Other(_) => return None,
        })
    }

    /// The grid's model of the type: its columns, how rows are told apart and what is
    /// locked.
    pub fn table_info(&self) -> TableInfo {
        let columns = self
            .columns()
            .iter()
            .map(|&name| ColumnInfo {
                name: name.into(),
                data_type: if name == "Score" { "double" } else { "varchar" }.into(),
                // The list index and the stream ID say where the element is: fixed.
                pseudo: name == "Index" || name == "ID",
                ..ColumnInfo::default()
            })
            .collect();
        TableInfo {
            columns,
            key: Some(vec![0]),
            ..TableInfo::default()
        }
    }
}

/// Whether `arg` reads back from a command line without quotes.
fn bare(arg: &[u8]) -> bool {
    !arg.is_empty()
        && std::str::from_utf8(arg).is_ok_and(|s| {
            s.chars()
                .all(|c| !c.is_whitespace() && !c.is_control() && !matches!(c, '"' | '\'' | '\\'))
        })
}

/// `arg` in double quotes as `redis-cli` reads it: `\"`, `\\`, `\n`, `\r`, `\t`, and
/// `\xNN` for any other control character or byte that is not UTF-8.
pub fn quoted(arg: &[u8]) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for chunk in arg.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if c.is_control() => {
                    let mut buf = [0; 4];
                    for b in c.encode_utf8(&mut buf).bytes() {
                        let _ = write!(out, "\\x{b:02x}");
                    }
                }
                c => out.push(c),
            }
        }
        for b in chunk.invalid() {
            let _ = write!(out, "\\x{b:02x}");
        }
    }
    out.push('"');
    out
}

/// An argument for a command line: bare when it can be, else [`quoted`].
pub fn quote(arg: &[u8]) -> String {
    if bare(arg) {
        String::from_utf8_lossy(arg).into_owned()
    } else {
        quoted(arg)
    }
}

/// A key as the sidebar and tab titles show it: as it is when it is UTF-8 without
/// control characters, else [`quoted`]. [`label_key`] reads it back.
pub fn key_label(key: &[u8]) -> String {
    match std::str::from_utf8(key) {
        Ok(s) if !s.starts_with('"') && !s.chars().any(char::is_control) && !s.is_empty() => {
            s.to_owned()
        }
        _ => quoted(key),
    }
}

/// The key a [`key_label`] stands for.
pub fn label_key(label: &str) -> Vec<u8> {
    if label.starts_with('"')
        && let Ok(mut args) = split_args(label)
        && args.len() == 1
    {
        return args.remove(0);
    }
    label.as_bytes().to_vec()
}

/// Splits a command line into arguments like `redis-cli`: double and single quotes
/// group words; inside double quotes `\"`, `\\`, `\n`, `\r`, `\t`, `\b`, `\a` and
/// `\xNN` are escapes, inside single quotes only `\'`.
///
/// # Errors
/// An unclosed quote or a bad `\x` escape.
pub fn split_args(line: &str) -> Result<Vec<Vec<u8>>, String> {
    let mut args = Vec::new();
    let mut chars = line.chars().peekable();
    let unclosed = || "unclosed quote".to_owned();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.peek().is_none() {
            return Ok(args);
        }
        let mut arg = Vec::new();
        let push = |arg: &mut Vec<u8>, c: char| {
            let mut buf = [0; 4];
            arg.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        };
        while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
            match c {
                '"' => loop {
                    match chars.next().ok_or_else(unclosed)? {
                        '"' => break,
                        '\\' => match chars.next().ok_or_else(unclosed)? {
                            'n' => arg.push(b'\n'),
                            'r' => arg.push(b'\r'),
                            't' => arg.push(b'\t'),
                            'b' => arg.push(8),
                            'a' => arg.push(7),
                            'x' => {
                                let hex: String = (0..2).filter_map(|_| chars.next()).collect();
                                let b = u8::from_str_radix(&hex, 16)
                                    .map_err(|_| format!("bad escape \\x{hex}"))?;
                                arg.push(b);
                            }
                            e => push(&mut arg, e),
                        },
                        x => push(&mut arg, x),
                    }
                },
                '\'' => loop {
                    match chars.next().ok_or_else(unclosed)? {
                        '\'' => break,
                        '\\' if chars.peek() == Some(&'\'') => {
                            chars.next();
                            arg.push(b'\'');
                        }
                        x => push(&mut arg, x),
                    }
                },
                _ => push(&mut arg, c),
            }
        }
        args.push(arg);
    }
}

/// One command a key tab or an action sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub name: &'static str,
    args: Vec<(Vec<u8>, bool)>,
}

/// A piece of a command's preview, for highlighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Name,
    Arg,
    /// A value the user typed: always in quotes.
    Value,
}

impl Command {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            args: Vec::new(),
        }
    }

    /// A key, field or option: quoted only when it must be.
    #[must_use]
    pub fn arg(mut self, a: impl AsRef<[u8]>) -> Self {
        self.args.push((a.as_ref().to_vec(), false));
        self
    }

    /// A value: always quoted in the preview.
    #[must_use]
    pub fn value(mut self, v: impl AsRef<[u8]>) -> Self {
        self.args.push((v.as_ref().to_vec(), true));
        self
    }

    /// The arguments after the name.
    pub fn args(&self) -> impl Iterator<Item = &[u8]> {
        self.args.iter().map(|(a, _)| a.as_slice())
    }

    /// The preview in pieces: `HSET`, ` user:1 plan`, ` "pro"`.
    pub fn parts(&self) -> Vec<(String, Part)> {
        let mut out = vec![(self.name.to_owned(), Part::Name)];
        for (a, value) in &self.args {
            if *value {
                out.push((format!(" {}", quoted(a)), Part::Value));
            } else {
                out.push((format!(" {}", quote(a)), Part::Arg));
            }
        }
        out
    }

    /// `HSET user:1002 plan "pro"`: a line `redis-cli` and the console run as is.
    pub fn text(&self) -> String {
        self.parts().into_iter().map(|(s, _)| s).collect()
    }
}

/// A cell's value as the bytes Redis stores.
fn bytes_of(v: &Value) -> Vec<u8> {
    match v {
        Value::Bytes(b) => b.clone(),
        Value::Text(s) | Value::Decimal(s) => s.as_bytes().to_vec(),
        other => other.to_string().into_bytes(),
    }
}

/// A sorted-set score as Redis reads it.
fn score_of(v: &Value) -> Result<String, String> {
    match v {
        Value::Float(f) if f.is_nan() => Err("a score cannot be NaN".into()),
        Value::Float(f) if f.is_infinite() => Ok(if *f > 0. { "+inf" } else { "-inf" }.into()),
        Value::Float(f) => Ok(f.to_string()),
        Value::Int(i) => Ok(i.to_string()),
        other => Err(format!("{other} is not a score")),
    }
}

/// The value the user typed into a key tab's cell. Text is kept exactly, spaces
/// included; a cell that held binary takes hex (`0x…`) again; scores take numbers and
/// `inf`.
///
/// # Errors
/// Bad hex or a bad score, with a message for the user.
pub fn parse_cell(col: &ColumnInfo, text: &str, original: Option<&Value>) -> Result<Value, String> {
    if col.data_type == "double" {
        let t = text.trim();
        let f: f64 = match t.to_ascii_lowercase().as_str() {
            "inf" | "+inf" => f64::INFINITY,
            "-inf" => f64::NEG_INFINITY,
            _ => t
                .parse()
                .map_err(|_| "Enter a number, or inf.".to_owned())?,
        };
        return if f.is_nan() {
            Err("Enter a number, or inf.".into())
        } else {
            Ok(Value::Float(f))
        };
    }
    if matches!(original, Some(Value::Bytes(_))) {
        let t = text.trim();
        let hex = t
            .strip_prefix("0x")
            .or_else(|| t.strip_prefix("0X"))
            .ok_or("Binary value: enter hex, like 0x89504E47.")?;
        if hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("Enter an even number of hex digits after 0x.".into());
        }
        return (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| e.to_string()))
            .collect::<Result<Vec<u8>, _>>()
            .map(Value::Bytes);
    }
    Ok(Value::Text(text.to_owned()))
}

/// Where an added list element goes, as its Index cell holds it.
pub const HEAD: &str = "head";
pub const TAIL: &str = "tail";

/// The commands that make a key tab's pending changes, in order: updates, then
/// deletes, then additions, as the panel lists them. They run in one `MULTI`.
/// `tombstone` marks list elements to delete (`LSET` then one `LREM`): pass a value
/// no element holds.
///
/// # Errors
/// A change the type cannot take (an edited stream entry, an added field without a
/// name), or a row that is gone; the message names the row.
pub fn commands(
    key: &[u8],
    kind: &KeyType,
    pending: &Pending,
    rows: &[Row],
    tombstone: &[u8],
) -> Result<Vec<Command>, String> {
    let row = |r: usize| rows.get(r).ok_or_else(|| format!("row {} is gone", r + 1));
    let cell = |r: &Row, c: usize| r.get(c).cloned().unwrap_or(Value::Null);
    let new_or = |set: &BTreeMap<usize, Edit>, c: usize, old: Value| match set.get(&c) {
        Some(Edit::Value(v)) => v.clone(),
        _ => old,
    };
    let given = |values: &[Edit], c: usize, what: &str, n: usize| match values.get(c) {
        Some(Edit::Value(v)) => Ok(v.clone()),
        _ => Err(format!("added row {n}: enter the {what}")),
    };
    let mut out = Vec::new();
    // Updates.
    let mut set_names: Vec<Vec<u8>> = Vec::new();
    for (r, set) in pending.updated() {
        let old = row(r)?;
        let at = |e: String| format!("row {}: {e}", r + 1);
        match kind {
            KeyType::Hash => {
                let (f0, v0) = (bytes_of(&cell(old, 0)), cell(old, 1));
                let f1 = bytes_of(&new_or(set, 0, cell(old, 0)));
                let v1 = new_or(set, 1, v0);
                if f1 != f0 {
                    out.push(Command::new("HDEL").arg(key).arg(&f0));
                }
                out.push(Command::new("HSET").arg(key).arg(&f1).value(bytes_of(&v1)));
                set_names.push(f1);
            }
            KeyType::List => {
                let index = cell(old, 0).to_string();
                let v1 = new_or(set, 1, cell(old, 1));
                out.push(
                    Command::new("LSET")
                        .arg(key)
                        .arg(index)
                        .value(bytes_of(&v1)),
                );
            }
            KeyType::Set => {
                let (m0, m1) = (
                    bytes_of(&cell(old, 0)),
                    bytes_of(&new_or(set, 0, cell(old, 0))),
                );
                out.push(Command::new("SREM").arg(key).value(&m0));
                out.push(Command::new("SADD").arg(key).value(&m1));
                set_names.push(m1);
            }
            KeyType::ZSet => {
                let (m0, m1) = (
                    bytes_of(&cell(old, 0)),
                    bytes_of(&new_or(set, 0, cell(old, 0))),
                );
                let score = score_of(&new_or(set, 1, cell(old, 1))).map_err(at)?;
                if m1 != m0 {
                    out.push(Command::new("ZREM").arg(key).value(&m0));
                }
                out.push(Command::new("ZADD").arg(key).arg(score).value(&m1));
                set_names.push(m1);
            }
            KeyType::Stream => {
                return Err(at(
                    "streams are append-only: entries cannot be edited, only added or deleted"
                        .into(),
                ));
            }
            KeyType::String | KeyType::Json | KeyType::Other(_) => {
                return Err("this type has no rows".into());
            }
        }
    }
    // Deletes.
    let mut tombstoned = false;
    for r in pending.deleted() {
        let old = row(r)?;
        let name = bytes_of(&cell(old, 0));
        if set_names.contains(&name) {
            return Err(format!(
                "{} is both deleted and set: undo one of the two",
                quote(&name)
            ));
        }
        out.push(match kind {
            KeyType::Hash => Command::new("HDEL").arg(key).arg(&name),
            KeyType::List => {
                tombstoned = true;
                Command::new("LSET")
                    .arg(key)
                    .arg(cell(old, 0).to_string())
                    .value(tombstone)
            }
            KeyType::Set => Command::new("SREM").arg(key).value(&name),
            KeyType::ZSet => Command::new("ZREM").arg(key).value(&name),
            KeyType::Stream => Command::new("XDEL").arg(key).arg(&name),
            KeyType::String | KeyType::Json | KeyType::Other(_) => {
                return Err("this type has no rows".into());
            }
        });
    }
    if tombstoned {
        out.push(Command::new("LREM").arg(key).arg("0").value(tombstone));
    }
    // Additions.
    for (n, (_, values)) in pending.added().enumerate() {
        let n = n + 1;
        out.push(match kind {
            KeyType::Hash => {
                let f = bytes_of(&given(values, 0, "field name", n)?);
                let v = bytes_of(&given(values, 1, "value", n)?);
                Command::new("HSET").arg(key).arg(f).value(v)
            }
            KeyType::List => {
                let end = given(values, 0, "end", n)?.to_string();
                let v = bytes_of(&given(values, 1, "value", n)?);
                let name = if end == HEAD { "LPUSH" } else { "RPUSH" };
                Command::new(name).arg(key).value(v)
            }
            KeyType::Set => Command::new("SADD")
                .arg(key)
                .value(bytes_of(&given(values, 0, "member", n)?)),
            KeyType::ZSet => {
                let m = bytes_of(&given(values, 0, "member", n)?);
                let s = score_of(&given(values, 1, "score", n)?)
                    .map_err(|e| format!("added row {n}: {e}"))?;
                Command::new("ZADD").arg(key).arg(s).value(m)
            }
            KeyType::Stream => {
                let id = match values.first() {
                    Some(Edit::Value(v)) => v.to_string(),
                    _ => "*".into(),
                };
                let fields = given(values, 1, "fields", n)?.to_string();
                let pairs = stream_fields(&fields).map_err(|e| format!("added row {n}: {e}"))?;
                let mut c = Command::new("XADD").arg(key).arg(id);
                for (f, v) in pairs {
                    c = c.arg(f).value(v);
                }
                c
            }
            KeyType::String | KeyType::Json | KeyType::Other(_) => {
                return Err("this type has no rows".into());
            }
        });
    }
    Ok(out)
}

/// A stream entry's fields as the grid shows them: a JSON object of strings.
pub fn stream_text(pairs: &[(Vec<u8>, Vec<u8>)]) -> String {
    let map: serde_json::Map<String, serde_json::Value> = pairs
        .iter()
        .map(|(f, v)| {
            (
                String::from_utf8_lossy(f).into_owned(),
                serde_json::Value::String(String::from_utf8_lossy(v).into_owned()),
            )
        })
        .collect();
    serde_json::Value::Object(map).to_string()
}

/// The fields of a new stream entry, typed as a JSON object.
///
/// # Errors
/// Not a JSON object, or an empty one.
pub fn stream_fields(text: &str) -> Result<Vec<(String, String)>, String> {
    let serde_json::Value::Object(map) = serde_json::from_str(text)
        .map_err(|_| r#"enter the fields as a JSON object, like {"field": "value"}"#.to_owned())?
    else {
        return Err(r#"enter the fields as a JSON object, like {"field": "value"}"#.into());
    };
    if map.is_empty() {
        return Err("an entry needs at least one field".into());
    }
    Ok(map
        .into_iter()
        .map(|(k, v)| match v {
            serde_json::Value::String(s) => (k, s),
            other => (k, other.to_string()),
        })
        .collect())
}

/// The command that writes a string or JSON key's edited text. `SET` keeps the
/// key's expiry (`KEEPTTL`).
pub fn text_command(key: &[u8], kind: &KeyType, text: &str) -> Option<Command> {
    match kind {
        KeyType::String => Some(Command::new("SET").arg(key).value(text).arg("KEEPTTL")),
        KeyType::Json => Some(Command::new("JSON.SET").arg(key).arg("$").value(text)),
        _ => None,
    }
}

/// A new key's first value, as the New key dialog takes it.
///
/// # Errors
/// What the type needs that is missing, for the dialog.
pub fn new_key(key: &[u8], kind: &KeyType, a: &str, b: &str) -> Result<Command, String> {
    let need = |s: &str, what: &str| {
        if s.is_empty() {
            Err(format!("Enter the {what}."))
        } else {
            Ok(())
        }
    };
    Ok(match kind {
        KeyType::String => Command::new("SET").arg(key).value(a).arg("NX"),
        KeyType::Hash => {
            need(a, "first field")?;
            Command::new("HSET").arg(key).arg(a).value(b)
        }
        KeyType::List => Command::new("RPUSH").arg(key).value(a),
        KeyType::Set => Command::new("SADD").arg(key).value(a),
        KeyType::ZSet => {
            let score = parse_cell(
                &ColumnInfo {
                    data_type: "double".into(),
                    ..ColumnInfo::default()
                },
                b,
                None,
            )?;
            Command::new("ZADD")
                .arg(key)
                .arg(score_of(&score)?)
                .value(a)
        }
        KeyType::Stream => {
            let mut c = Command::new("XADD").arg(key).arg("*");
            for (f, v) in stream_fields(a)? {
                c = c.arg(f).value(v);
            }
            c
        }
        KeyType::Json => {
            serde_json::from_str::<serde_json::Value>(a)
                .map_err(|e| format!("Not valid JSON: {e}."))?;
            Command::new("JSON.SET")
                .arg(key)
                .arg("$")
                .value(a)
                .arg("NX")
        }
        KeyType::Other(t) => return Err(format!("Deadlock cannot create {t} keys.")),
    })
}

/// When a key should expire, as typed into Set expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expiry {
    Never,
    /// Seconds from now.
    In(i64),
    /// A Unix time in seconds.
    At(i64),
}

/// Reads `never`, a duration (`90`, `90s`, `15 min`, `2h30m`, `1d`) or a local date
/// and time (`2026-12-31`, `2026-12-31 18:00`). `now` is the current local time.
///
/// # Errors
/// Text that is none of those, or a time not in the future.
pub fn parse_expiry(text: &str, now: chrono::NaiveDateTime) -> Result<Expiry, String> {
    let t = text.trim().to_ascii_lowercase();
    if matches!(t.as_str(), "never" | "none" | "persist") {
        return Ok(Expiry::Never);
    }
    let local = |d: chrono::NaiveDateTime| {
        use chrono::TimeZone as _;
        chrono::Local
            .from_local_datetime(&d)
            .earliest()
            .map(|d| d.timestamp())
            .ok_or_else(|| "That local time does not exist.".to_owned())
    };
    for f in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M"] {
        if let Ok(d) = chrono::NaiveDateTime::parse_from_str(&t, f) {
            if d <= now {
                return Err("That time has passed.".into());
            }
            return local(d).map(Expiry::At);
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(&t, "%Y-%m-%d") {
        let d = d.and_time(chrono::NaiveTime::MIN);
        if d <= now {
            return Err("That date has passed.".into());
        }
        return local(d).map(Expiry::At);
    }
    let bad = || "Enter a duration like 90s, 15m, 2h or 1d, a date like 2026-12-31, or never.";
    let mut total: i64 = 0;
    let mut rest = t.as_str();
    while !rest.trim_start().is_empty() {
        rest = rest.trim_start();
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let n: i64 = rest[..digits].parse().map_err(|_| bad())?;
        rest = rest[digits..].trim_start();
        let unit_len = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        let unit = match &rest[..unit_len] {
            "" | "s" | "sec" | "secs" | "second" | "seconds" => 1,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "h" | "hr" | "hour" | "hours" => 3600,
            "d" | "day" | "days" => 86_400,
            "w" | "week" | "weeks" => 604_800,
            _ => return Err(bad().into()),
        };
        rest = &rest[unit_len..];
        total = n
            .checked_mul(unit)
            .and_then(|s| total.checked_add(s))
            .ok_or("That is too long.")?;
    }
    if total <= 0 {
        return Err("Enter a duration longer than zero, or never.".into());
    }
    Ok(Expiry::In(total))
}

/// The command for an expiry.
pub fn expiry_command(key: &[u8], e: Expiry) -> Command {
    match e {
        Expiry::Never => Command::new("PERSIST").arg(key),
        Expiry::In(s) => Command::new("EXPIRE").arg(key).arg(s.to_string()),
        Expiry::At(t) => Command::new("EXPIREAT").arg(key).arg(t.to_string()),
    }
}

/// `in 23 h 41 min`, from milliseconds left.
pub fn expires_in(ms: i64) -> String {
    let s = (ms.max(0) + 999) / 1000;
    let (d, h, m) = (s / 86_400, s / 3600 % 24, s / 60 % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("in {s} s"),
        (0, 0, m) => format!("in {m} min {} s", s % 60),
        (0, h, m) => format!("in {h} h {m} min"),
        (d, h, _) => format!("in {d} d {h} h"),
    }
}

/// `312 B`, `1.5 KB`, `12 MB`.
pub fn bytes_size(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    #[allow(clippy::cast_precision_loss, reason = "a size for people")]
    let mut v = n as f64 / 1024.;
    let mut unit = UNITS[0];
    for u in &UNITS[1..] {
        if v < 1024. {
            break;
        }
        v /= 1024.;
        unit = u;
    }
    if v < 10. {
        format!("{v:.1} {unit}")
    } else {
        format!("{v:.0} {unit}")
    }
}

/// `12 480`, as the design groups digits.
pub fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// The `SCAN MATCH` pattern for a sidebar search: text with a glob character (`*`,
/// `?`, `[`) is the pattern itself; plain text matches anywhere, as `*text*`.
pub fn search_pattern(text: &str) -> String {
    let t = text.trim();
    if t.contains(['*', '?', '[']) {
        t.to_owned()
    } else {
        format!("*{}*", t.replace('\\', "\\\\"))
    }
}

/// `prefix` with the glob characters escaped, then `*`: the pattern for every key
/// under a namespace.
pub fn prefix_pattern(prefix: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(prefix.len() + 1);
    for &b in prefix {
        if matches!(b, b'*' | b'?' | b'[' | b']' | b'\\') {
            out.push(b'\\');
        }
        out.push(b);
    }
    out.push(b'*');
    out
}

/// Byte ranges of `label` that a search for `text` highlights: the plain text,
/// ignoring case; nothing for glob patterns.
pub fn search_hits(label: &str, text: &str) -> Vec<std::ops::Range<usize>> {
    let t = text.trim();
    if t.is_empty() || t.contains(['*', '?', '[']) || !label.is_ascii() || !t.is_ascii() {
        return Vec::new();
    }
    let (l, q) = (label.to_ascii_lowercase(), t.to_ascii_lowercase());
    l.match_indices(&q).map(|(i, m)| i..i + m.len()).collect()
}

/// The databases in `INFO keyspace` and their key counts.
pub fn keyspace(info: &str) -> Vec<(u32, u64)> {
    info.lines()
        .filter_map(|l| {
            let (db, rest) = l.strip_prefix("db")?.split_once(':')?;
            let keys = rest.split(',').find_map(|kv| kv.strip_prefix("keys="))?;
            Some((db.parse().ok()?, keys.trim().parse().ok()?))
        })
        .collect()
}

/// The keys a database's browser has loaded so far, by name, with their types.
pub type Loaded = BTreeMap<Vec<u8>, KeyType>;

/// How far a database's scan has gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanState {
    pub loaded: usize,
    /// `DBSIZE` when the scan started.
    pub total: u64,
    pub complete: bool,
}

/// The sidebar nodes under `prefix` (empty for the database itself): its namespaces
/// with the number of loaded keys in each, then up to `limit` of its own keys with
/// their types, then "Load more" rows: one for this level's keys when more are
/// loaded than shown, and at the database level one that scans further.
pub fn level(keys: &Loaded, prefix: &[u8], limit: usize, scan: ScanState) -> Vec<ObjectNode> {
    let mut spaces: Vec<(Vec<u8>, u64)> = Vec::new();
    let mut own: Vec<ObjectNode> = Vec::new();
    let mut own_count = 0usize;
    let range = keys.range::<[u8], _>((Bound::Included(prefix), Bound::Unbounded));
    for (key, kind) in range.take_while(|(k, _)| k.starts_with(prefix)) {
        let rest = &key[prefix.len()..];
        match rest.iter().position(|&b| b == DELIMITER) {
            Some(i) => {
                let ns = &key[..prefix.len() + i + 1];
                match spaces.last_mut() {
                    Some((last, n)) if last.as_slice() == ns => *n += 1,
                    _ => spaces.push((ns.to_vec(), 1)),
                }
            }
            None => {
                own_count += 1;
                if own.len() < limit {
                    let mut n = ObjectNode::new(key_label(key), NodeKind::Key);
                    n.detail = kind.label().to_owned();
                    own.push(n);
                }
            }
        }
    }
    // Keys sort bytewise, so a namespace's keys can sit apart (`a:` and `a:b` around
    // `a:\x01`): merge repeats.
    spaces.sort();
    spaces.dedup_by(|b, a| {
        if a.0 == b.0 {
            a.1 += b.1;
            true
        } else {
            false
        }
    });
    let incomplete = if scan.complete { "" } else { " loaded" };
    let mut out: Vec<ObjectNode> = spaces
        .into_iter()
        .map(|(ns, n)| {
            let mut node = ObjectNode::new(key_label(&ns), NodeKind::Namespace);
            node.detail = format!("{}{incomplete}", grouped(n));
            node
        })
        .collect();
    out.extend(own);
    if own_count > limit {
        let mut more = ObjectNode::new(MORE, NodeKind::More);
        more.detail = format!("{} of {}", grouped(limit as u64), grouped(own_count as u64));
        more.system = false;
        out.push(more);
    }
    if prefix.is_empty() && !scan.complete {
        let mut more = ObjectNode::new(format!("{MORE}keys"), NodeKind::More);
        more.detail = format!(
            "{} of {}",
            grouped(scan.loaded as u64),
            grouped(scan.total.max(scan.loaded as u64))
        );
        out.push(more);
    }
    out
}

/// One client in `CLIENT LIST`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Client {
    pub id: u64,
    pub addr: String,
    pub name: String,
    pub user: String,
    pub db: u32,
    pub age: u64,
    pub idle: u64,
    pub flags: String,
    /// The last command, `client|list` style.
    pub cmd: String,
    pub lib_name: String,
}

impl Client {
    /// Blocked on a list, stream or sorted-set pop, or `WAIT`.
    pub fn blocked(&self) -> bool {
        self.flags.contains('b')
    }

    /// The flags in words: `blocked`, `in MULTI`, `replica`, `pubsub`.
    pub fn flag_words(&self) -> String {
        let words: Vec<&str> = self
            .flags
            .chars()
            .filter_map(|f| {
                Some(match f {
                    'b' => "blocked",
                    'x' => "in MULTI",
                    'd' => "watched key changed",
                    'S' => "replica",
                    'M' => "master",
                    'P' => "pubsub",
                    'O' => "monitor",
                    'c' => "closing",
                    'A' => "closing asap",
                    't' => "tracking",
                    'r' => "readonly (cluster)",
                    'u' => "unblocked",
                    'U' => "unix socket",
                    'T' => "no-touch",
                    'e' => "no-evict",
                    _ => return None,
                })
            })
            .collect();
        if words.is_empty() {
            "normal".into()
        } else {
            words.join(", ")
        }
    }
}

/// Parses `CLIENT LIST`: one client per line of `name=value` fields.
pub fn client_list(text: &str) -> Vec<Client> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut c = Client::default();
            for field in line.split(' ') {
                let Some((k, v)) = field.split_once('=') else {
                    continue;
                };
                let n = || v.parse().unwrap_or(0);
                match k {
                    "id" => c.id = n(),
                    "addr" => c.addr = v.into(),
                    "name" => c.name = v.into(),
                    "user" => c.user = v.into(),
                    "db" => c.db = v.parse().unwrap_or(0),
                    "age" => c.age = n(),
                    "idle" => c.idle = n(),
                    "flags" => c.flags = v.into(),
                    "cmd" => c.cmd = v.into(),
                    "lib-name" => c.lib_name = v.into(),
                    _ => {}
                }
            }
            c
        })
        .collect()
}

/// Parameters `CONFIG SET` refuses while the server runs: probed on `redis:8`
/// (8.10.2, with its bundled modules) by setting each of the 303 parameters to its own
/// value and keeping those answered `can't set immutable config`. Protected ones
/// (`dir`, `dbfilename`) depend on `enable-protected-configs` and are not listed: a
/// parameter the server refuses shows the server's error at apply time.
pub const IMMUTABLE: &[&str] = &[
    "aclfile",
    "always-show-logo",
    "aof-rewrite-cpulist",
    "aof_rewrite_cpulist",
    "appenddirname",
    "appendfilename",
    "backupdirname",
    "bgsave-cpulist",
    "bgsave_cpulist",
    "bio-cpulist",
    "bio_cpulist",
    "cluster-config-file",
    "cluster-enabled",
    "cluster-port",
    "daemonize",
    "databases",
    "disable-thp",
    "enable-debug-command",
    "enable-module-command",
    "enable-protected-configs",
    "io-threads",
    "logfile",
    "pidfile",
    "preload-file",
    "rdbchecksum",
    "replicaof",
    "search-_simulate-in-flex",
    "search-bg-index-sleep-gap",
    "search-disk-drop-read-cache",
    "search-disk-use-direct-reads",
    "search-ext-load",
    "search-friso-ini",
    "search-gc-scan-size",
    "search-max-doctablesize",
    "search-multi-text-slop",
    "search-no-gc",
    "search-no-mem-pools",
    "search-partial-indexed-docs",
    "search-raw-docid-encoding",
    "search-tiered-hnsw-buffer-limit",
    "search-workers-priority-bias-threshold",
    "server-cpulist",
    "server_cpulist",
    "set-proc-title",
    "slaveof",
    "socket-mark-id",
    "supervised",
    "syslog-enabled",
    "syslog-facility",
    "syslog-ident",
    "tcp-backlog",
    "ts-libmr-protocol",
    "ts-topology-events",
    "unixsocket",
    "unixsocketperm",
];

/// Parameters whose values are secrets: shown masked and never changed here.
pub const SECRETS: &[&str] = &[
    "requirepass",
    "masterauth",
    "tls-key-file-pass",
    "tls-client-key-file-pass",
];

/// What a masked secret shows.
pub const MASKED: &str = "(hidden)";

/// A Redis parameter's category, from its name.
pub fn config_category(name: &str) -> crate::config::Category {
    use crate::config::Category as C;
    let starts = |p: &[&str]| p.iter().any(|x| name.starts_with(x));
    if starts(&[
        "maxmemory",
        "lazyfree",
        "active-defrag",
        "activedefrag",
        "lfu-",
        "hash-max",
        "list-max",
        "set-max",
        "zset-max",
        "stream-node",
        "hll-",
        "proto-max",
        "client-query-buffer",
        "client-output-buffer",
        "jemalloc",
    ]) {
        C::Memory
    } else if starts(&[
        "repl-",
        "replica",
        "slave",
        "master",
        "min-replicas",
        "min-slaves",
    ]) {
        C::Replication
    } else if starts(&[
        "appendonly",
        "appendfsync",
        "appendfilename",
        "appenddirname",
        "aof-",
        "auto-aof",
        "save",
        "rdb",
        "dbfilename",
        "dir",
        "stop-writes",
    ]) {
        C::Persistence
    } else if starts(&[
        "port",
        "bind",
        "tcp-",
        "timeout",
        "maxclients",
        "unixsocket",
        "protected-mode",
        "io-threads",
        "busy-reply",
    ]) {
        C::Connections
    } else if starts(&[
        "loglevel",
        "logfile",
        "syslog",
        "slowlog",
        "latency",
        "crash-log",
    ]) {
        C::Logging
    } else if starts(&[
        "tls-",
        "requirepass",
        "masterauth",
        "acl",
        "enable-protected",
        "enable-debug",
        "enable-module",
    ]) {
        C::Security
    } else {
        C::Other
    }
}

/// `CONFIG GET *` pairs as the configuration model. Redis reports no types,
/// defaults or descriptions, so none are given.
pub fn config_params(pairs: Vec<(String, String)>) -> Vec<crate::config::ConfigParam> {
    use crate::config::{Applies, ConfigParam, Scope, Source};
    let mut params: Vec<ConfigParam> = pairs
        .into_iter()
        .map(|(name, value)| {
            let secret = SECRETS.contains(&name.as_str());
            let mut p = ConfigParam::new(
                &name,
                Some(if secret && !value.is_empty() {
                    MASKED.to_owned()
                } else {
                    value
                }),
            );
            p.category = config_category(&p.name);
            p.applies = if IMMUTABLE.contains(&p.name.as_str()) {
                Applies::Fixed
            } else {
                Applies::Now
            };
            p.scope = Some(Scope::GlobalOnly);
            p.source = Source::Unknown;
            p
        })
        .collect();
    params.sort_by(|a, b| a.name.cmp(&b.name));
    params
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_quote_like_redis_cli_and_read_back() {
        assert_eq!(quote(b"user:1002"), "user:1002");
        assert_eq!(quote(b"a b"), "\"a b\"");
        assert_eq!(quote(b""), "\"\"");
        assert_eq!(quote(b"say \"hi\""), r#""say \"hi\"""#);
        assert_eq!(quote(b"back\\slash"), r#""back\\slash""#);
        assert_eq!(quote(b"it's"), r#""it's""#);
        assert_eq!(quote(b"\x00\xff\n"), r#""\x00\xff\n""#);
        assert_eq!(quote("žluťoučký kůň".as_bytes()), "\"žluťoučký kůň\"");
        assert_eq!(quote("日本".as_bytes()), "日本");
        for arg in [
            &b"plain"[..],
            b"a b",
            b"",
            b"\"q\"",
            b"\\",
            b"\x00\x01\xfe\xff",
            "émoji 🎉".as_bytes(),
            b"tab\there\r\n",
            b"'single'",
        ] {
            let line = format!("SET {} {}", quote(arg), quoted(arg));
            let args = split_args(&line).unwrap();
            assert_eq!(
                args,
                vec![b"SET".to_vec(), arg.to_vec(), arg.to_vec()],
                "{line}"
            );
        }
    }

    #[test]
    fn command_lines_split_like_redis_cli() {
        let s = |l: &str| split_args(l).unwrap();
        assert_eq!(s("  GET foo "), [b"GET".to_vec(), b"foo".to_vec()]);
        assert_eq!(
            s(r#"SET k "a \"b\" c" 'x y'"#),
            [
                b"SET".to_vec(),
                b"k".to_vec(),
                b"a \"b\" c".to_vec(),
                b"x y".to_vec()
            ]
        );
        assert_eq!(s(r"SET k 'it\'s'")[2], b"it's");
        assert_eq!(s(r#"SET k "\x00\xFF""#)[2], [0, 255]);
        assert!(s("").is_empty());
        assert!(split_args("GET \"foo").is_err());
        assert!(split_args(r#"GET "\xZZ""#).is_err());
    }

    #[test]
    fn key_labels_round_trip() {
        for key in [
            &b"user:1002"[..],
            b"has space",
            b"\"starts with quote",
            b"",
            b"\xff\xfe",
            b"line\nbreak",
            "ünï:cødé".as_bytes(),
        ] {
            assert_eq!(label_key(&key_label(key)), key, "{:?}", key_label(key));
        }
        assert_eq!(key_label(b"has space"), "has space");
        assert_eq!(key_label(b"\xff"), "\"\\xff\"");
    }

    fn hash_rows() -> Vec<Row> {
        ["email", "plan", "trial_ends", "prefs"]
            .iter()
            .map(|f| vec![Value::Text((*f).into()), Value::Text(format!("v-{f}"))])
            .collect()
    }

    #[test]
    fn hash_changes_become_the_designs_commands() {
        let rows = hash_rows();
        let mut p = Pending::default();
        p.set(
            crate::edit::RowId::Loaded(1),
            1,
            Edit::Value(Value::Text("pro".into())),
            Some(&rows[1][1]),
        );
        p.delete(&[crate::edit::RowId::Loaded(2)]);
        let c = commands(b"user:1002", &KeyType::Hash, &p, &rows, b"x").unwrap();
        let text: Vec<String> = c.iter().map(Command::text).collect();
        assert_eq!(
            text,
            [r#"HSET user:1002 plan "pro""#, "HDEL user:1002 trial_ends"]
        );
        assert_eq!(c[0].parts()[0], ("HSET".into(), Part::Name));
        assert_eq!(c[0].parts()[3], (" \"pro\"".into(), Part::Value));
    }

    #[test]
    fn renamed_fields_and_conflicts() {
        let rows = hash_rows();
        let mut p = Pending::default();
        let r0 = crate::edit::RowId::Loaded(0);
        p.set(
            r0,
            0,
            Edit::Value(Value::Text("mail".into())),
            Some(&rows[0][0]),
        );
        let c = commands(b"k", &KeyType::Hash, &p, &rows, b"x").unwrap();
        let text: Vec<String> = c.iter().map(Command::text).collect();
        assert_eq!(text, ["HDEL k email", r#"HSET k mail "v-email""#]);
        // Renaming onto a field that is deleted in the same submit is refused.
        p.set(
            r0,
            0,
            Edit::Value(Value::Text("plan".into())),
            Some(&rows[0][0]),
        );
        p.delete(&[crate::edit::RowId::Loaded(1)]);
        assert!(commands(b"k", &KeyType::Hash, &p, &rows, b"x").is_err());
    }

    #[test]
    fn added_rows_need_their_values() {
        let mut p = Pending::default();
        let row = p.add(2);
        assert!(commands(b"k", &KeyType::Hash, &p, &[], b"x").is_err());
        p.set(row, 0, Edit::Value(Value::Text("f".into())), None);
        p.set(row, 1, Edit::Value(Value::Bytes(vec![0, 0xff])), None);
        let c = commands(b"k", &KeyType::Hash, &p, &[], b"x").unwrap();
        assert_eq!(c[0].text(), r#"HSET k f "\x00\xff""#);
        let args: Vec<&[u8]> = c[0].args().collect();
        assert_eq!(args, [&b"k"[..], b"f", &[0, 0xff]]);
    }

    #[test]
    fn list_changes_set_by_index_remove_by_tombstone_and_push() {
        let rows: Vec<Row> = (0..4)
            .map(|i| vec![Value::Int(i), Value::Text(format!("e{i}"))])
            .collect();
        let mut p = Pending::default();
        p.set(
            crate::edit::RowId::Loaded(0),
            1,
            Edit::Value(Value::Text("new".into())),
            Some(&rows[0][1]),
        );
        p.delete(&[crate::edit::RowId::Loaded(1), crate::edit::RowId::Loaded(3)]);
        let head = p.add(2);
        p.set(head, 0, Edit::Value(Value::Text(HEAD.into())), None);
        p.set(head, 1, Edit::Value(Value::Text("first".into())), None);
        let tail = p.add(2);
        p.set(tail, 0, Edit::Value(Value::Text(TAIL.into())), None);
        p.set(tail, 1, Edit::Value(Value::Text("last".into())), None);
        let text: Vec<String> = commands(b"q", &KeyType::List, &p, &rows, b"\0gone")
            .unwrap()
            .iter()
            .map(Command::text)
            .collect();
        assert_eq!(
            text,
            [
                r#"LSET q 0 "new""#,
                r#"LSET q 1 "\x00gone""#,
                r#"LSET q 3 "\x00gone""#,
                r#"LREM q 0 "\x00gone""#,
                r#"LPUSH q "first""#,
                r#"RPUSH q "last""#,
            ]
        );
    }

    #[test]
    fn set_zset_and_stream_changes() {
        let rows = vec![vec![Value::Text("a".into())], vec![Value::Text("b".into())]];
        let mut p = Pending::default();
        p.set(
            crate::edit::RowId::Loaded(0),
            0,
            Edit::Value(Value::Text("A".into())),
            Some(&rows[0][0]),
        );
        p.delete(&[crate::edit::RowId::Loaded(1)]);
        let text: Vec<String> = commands(b"s", &KeyType::Set, &p, &rows, b"x")
            .unwrap()
            .iter()
            .map(Command::text)
            .collect();
        assert_eq!(text, [r#"SREM s "a""#, r#"SADD s "A""#, r#"SREM s "b""#]);

        let rows = vec![vec![Value::Text("m".into()), Value::Float(1.5)]];
        let mut p = Pending::default();
        p.set(
            crate::edit::RowId::Loaded(0),
            1,
            Edit::Value(Value::Float(f64::INFINITY)),
            Some(&rows[0][1]),
        );
        let add = p.add(2);
        p.set(add, 0, Edit::Value(Value::Text("n".into())), None);
        p.set(add, 1, Edit::Value(Value::Float(-2.25)), None);
        let text: Vec<String> = commands(b"z", &KeyType::ZSet, &p, &rows, b"x")
            .unwrap()
            .iter()
            .map(Command::text)
            .collect();
        assert_eq!(text, [r#"ZADD z +inf "m""#, r#"ZADD z -2.25 "n""#]);

        let rows = vec![vec![
            Value::Text("1-0".into()),
            Value::Text(r#"{"a":"1"}"#.into()),
        ]];
        let mut p = Pending::default();
        p.set(
            crate::edit::RowId::Loaded(0),
            1,
            Edit::Value(Value::Text("{}".into())),
            Some(&rows[0][1]),
        );
        assert!(
            commands(b"x", &KeyType::Stream, &p, &rows, b"x")
                .unwrap_err()
                .contains("append-only")
        );
        let mut p = Pending::default();
        p.delete(&[crate::edit::RowId::Loaded(0)]);
        let add = p.add(2);
        p.set(
            add,
            1,
            Edit::Value(Value::Text(r#"{"t":"21 °C","n":3}"#.into())),
            None,
        );
        let text: Vec<String> = commands(b"x", &KeyType::Stream, &p, &rows, b"x")
            .unwrap()
            .iter()
            .map(Command::text)
            .collect();
        assert_eq!(text, ["XDEL x 1-0", r#"XADD x * t "21 °C" n "3""#]);
    }

    #[test]
    fn cells_parse_text_hex_and_scores() {
        let text = ColumnInfo {
            data_type: "varchar".into(),
            ..ColumnInfo::default()
        };
        let score = ColumnInfo {
            data_type: "double".into(),
            ..ColumnInfo::default()
        };
        assert_eq!(
            parse_cell(&text, "  spaced  ", None),
            Ok(Value::Text("  spaced  ".into()))
        );
        let bin = Value::Bytes(vec![1]);
        assert_eq!(
            parse_cell(&text, "0x00ff", Some(&bin)),
            Ok(Value::Bytes(vec![0, 255]))
        );
        assert!(parse_cell(&text, "0x0", Some(&bin)).is_err());
        assert!(parse_cell(&text, "zz", Some(&bin)).is_err());
        assert_eq!(parse_cell(&score, " 2.5 ", None), Ok(Value::Float(2.5)));
        assert_eq!(
            parse_cell(&score, "-inf", None),
            Ok(Value::Float(f64::NEG_INFINITY))
        );
        assert!(parse_cell(&score, "nan", None).is_err());
        assert!(parse_cell(&score, "abc", None).is_err());
    }

    #[test]
    fn text_and_new_key_commands() {
        assert_eq!(
            text_command(b"k", &KeyType::String, "a b").unwrap().text(),
            r#"SET k "a b" KEEPTTL"#
        );
        assert_eq!(
            text_command(b"k", &KeyType::Json, r#"{"a":1}"#)
                .unwrap()
                .text(),
            r#"JSON.SET k $ "{\"a\":1}""#
        );
        assert!(text_command(b"k", &KeyType::Hash, "").is_none());
        assert_eq!(
            new_key(b"n", &KeyType::Hash, "f", "v").unwrap().text(),
            r#"HSET n f "v""#
        );
        assert!(new_key(b"n", &KeyType::Hash, "", "v").is_err());
        assert_eq!(
            new_key(b"n", &KeyType::ZSet, "m", "3").unwrap().text(),
            r#"ZADD n 3 "m""#
        );
        assert!(new_key(b"n", &KeyType::Json, "{", "").is_err());
        assert!(new_key(b"n", &KeyType::Other("x".into()), "", "").is_err());
    }

    #[test]
    fn expiry_input() {
        let now = chrono::NaiveDate::from_ymd_opt(2026, 11, 23)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let e = |t: &str| parse_expiry(t, now);
        assert_eq!(e("never"), Ok(Expiry::Never));
        assert_eq!(e("90"), Ok(Expiry::In(90)));
        assert_eq!(e("90s"), Ok(Expiry::In(90)));
        assert_eq!(e("15 min"), Ok(Expiry::In(900)));
        assert_eq!(e("2h30m"), Ok(Expiry::In(9000)));
        assert_eq!(e("1d"), Ok(Expiry::In(86_400)));
        assert!(matches!(e("2026-12-31"), Ok(Expiry::At(_))));
        assert!(matches!(e("2026-12-31 18:00"), Ok(Expiry::At(_))));
        assert!(e("2020-01-01").is_err());
        assert!(e("0").is_err());
        assert!(e("soon").is_err());
        assert!(e("5 parsecs").is_err());
        assert_eq!(expiry_command(b"k", Expiry::In(60)).text(), "EXPIRE k 60");
        assert_eq!(expiry_command(b"k", Expiry::Never).text(), "PERSIST k");
        assert_eq!(expires_in(85_260_000), "in 23 h 41 min");
        assert_eq!(expires_in(45_000), "in 45 s");
        assert_eq!(expires_in(3 * 86_400_000 + 4 * 3_600_000), "in 3 d 4 h");
        assert_eq!(bytes_size(312), "312 B");
        assert_eq!(bytes_size(1536), "1.5 KB");
        assert_eq!(bytes_size(12 * 1024 * 1024), "12 MB");
        assert_eq!(grouped(12_480), "12 480");
    }

    #[test]
    fn search_patterns_wrap_plain_text() {
        assert_eq!(search_pattern("user"), "*user*");
        assert_eq!(search_pattern("user:*"), "user:*");
        assert_eq!(search_pattern("a?c"), "a?c");
        assert_eq!(search_pattern("x[12]"), "x[12]");
        assert_eq!(search_pattern(r"a\b"), r"*a\\b*");
        assert_eq!(search_hits("User:1002", "user"), vec![0..4]);
        assert_eq!(prefix_pattern(b"a*b:"), br"a\*b:*");
        assert!(search_hits("user:1", "user:*").is_empty());
    }

    #[test]
    fn keyspace_and_clients_parse() {
        let info = "# Keyspace\r\ndb0:keys=12480,expires=3,avg_ttl=0,subexpiry=0\r\ndb1:keys=96,expires=0,avg_ttl=0\r\n";
        assert_eq!(keyspace(info), [(0, 12_480), (1, 96)]);
        let list = "id=3 addr=127.0.0.1:53712 laddr=127.0.0.1:6379 fd=8 name=worker age=12 idle=0 flags=b db=1 sub=0 user=default lib-name=deadlock cmd=blpop\nid=4 addr=[::1]:1 name= age=1 idle=1 flags=N db=0 user=app cmd=client|list\n";
        let c = client_list(list);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].id, 3);
        assert_eq!(c[0].name, "worker");
        assert_eq!(c[0].db, 1);
        assert!(c[0].blocked());
        assert_eq!(c[0].flag_words(), "blocked");
        assert_eq!(c[0].lib_name, "deadlock");
        assert_eq!(c[1].flag_words(), "normal");
        assert_eq!(c[1].cmd, "client|list");
    }

    #[test]
    fn namespaces_group_loaded_keys() {
        let mut keys = Loaded::new();
        for k in [
            "user:1",
            "user:2",
            "user:2:cart",
            "session:a",
            "plain",
            "zed",
        ] {
            keys.insert(k.as_bytes().to_vec(), KeyType::Hash);
        }
        keys.insert(b"user:2:roles".to_vec(), KeyType::Set);
        let scan = ScanState {
            loaded: 7,
            total: 10,
            complete: false,
        };
        let top = level(&keys, b"", 1, scan);
        let names: Vec<(&str, &str)> = top
            .iter()
            .map(|n| (n.name.as_str(), n.detail.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                ("session:", "1 loaded"),
                ("user:", "4 loaded"),
                ("plain", "hash"),
                (MORE, "1 of 2"),
                ("\0morekeys", "7 of 10"),
            ]
        );
        let done = ScanState {
            complete: true,
            ..scan
        };
        let user = level(&keys, b"user:", 10, done);
        let names: Vec<(&str, NodeKind)> = user.iter().map(|n| (n.name.as_str(), n.kind)).collect();
        assert_eq!(
            names,
            [
                ("user:2:", NodeKind::Namespace),
                ("user:1", NodeKind::Key),
                ("user:2", NodeKind::Key),
            ]
        );
        assert_eq!(user[0].detail, "2");
    }

    #[test]
    fn config_pairs_become_params() {
        let p = config_params(vec![
            ("maxmemory".into(), "0".into()),
            ("requirepass".into(), "secret".into()),
        ]);
        assert_eq!(p[0].name, "maxmemory");
        assert_eq!(p[0].category, crate::config::Category::Memory);
        assert_eq!(p[1].value.as_deref(), Some(MASKED));
    }
}
