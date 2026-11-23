//! Export of a result or a table to a file: CSV, TSV, JSON, JSON Lines or SQL INSERT
//! statements. Rows are written as they arrive, page by page; the whole file is never
//! held in memory.
//!
//! Conventions, the same in every format unless said otherwise:
//! - UTF-8, no byte-order mark.
//! - NULL is an empty, unquoted CSV / TSV field; an empty string is `""`, so the two
//!   stay apart. JSON writes `null`.
//! - Binary values are hex, upper case, without a `0x` prefix (SQL uses the engine's
//!   own binary literal).
//! - Dates and times are ISO 8601: `2024-05-01`, `2024-05-01T13:45:00.25`; a MySQL
//!   `TIME` is `HH:MM:SS[.ffffff]`, signed and past 24 hours when the value is.
//! - JSON numbers keep their precision: integers beyond ±2^53 and decimals with more
//!   than 15 significant digits are written as strings.

use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{mpsc, oneshot};

use crate::driver::{Column, Driver, Row, Value};
use crate::edit::{TableInfo, literal, oracle_literal, quote_ident};

/// The file formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Comma-separated, RFC 4180: CRLF line ends, fields quoted when needed.
    Csv,
    /// Tab-separated, quoted by the same rules as CSV.
    Tsv,
    /// One JSON array of objects, one object per row, keyed by column name.
    Json,
    /// One JSON object per line.
    JsonLines,
    /// `insert` statements: multi-row for MySQL and MariaDB, one per row for Oracle.
    SqlInsert,
}

impl Format {
    pub const ALL: [Self; 5] = [
        Self::Csv,
        Self::Tsv,
        Self::Json,
        Self::JsonLines,
        Self::SqlInsert,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Csv => "CSV",
            Self::Tsv => "TSV",
            Self::Json => "JSON",
            Self::JsonLines => "JSON Lines",
            Self::SqlInsert => "SQL INSERT",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Tsv => "tsv",
            Self::Json => "json",
            Self::JsonLines => "jsonl",
            Self::SqlInsert => "sql",
        }
    }
}

/// How SQL literals and names are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// MySQL and MariaDB; `backslash_escapes` unless the session has
    /// `NO_BACKSLASH_ESCAPES`.
    Mysql {
        backslash_escapes: bool,
    },
    Oracle,
}

/// What to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub format: Format,
    pub dialect: Dialect,
    /// The table the INSERT statements name, unqualified.
    pub table: String,
    /// The columns written, by index into the result's columns; `None` writes all.
    pub keep: Option<Vec<usize>>,
}

impl Spec {
    /// The columns of a table worth exporting: never Oracle's `ROWID`, and for INSERT
    /// statements no generated column, which the server refuses a value for.
    pub fn table_columns(info: &TableInfo, format: Format) -> Vec<usize> {
        info.columns
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.pseudo && !(format == Format::SqlInsert && c.generated))
            .map(|(i, _)| i)
            .collect()
    }
}

/// MySQL INSERT statements hold up to this many rows…
const BATCH_ROWS: usize = 500;
/// …or about this many bytes, well under the default `max_allowed_packet`.
const BATCH_BYTES: usize = 1 << 20;

/// Writes rows in one format. Call [`Writer::row`] per row, then [`Writer::finish`].
pub struct Writer<W: Write> {
    out: W,
    format: Format,
    dialect: Dialect,
    keep: Vec<usize>,
    /// Per kept column: the JSON key with quotes, or the quoted SQL name.
    names: Vec<String>,
    /// SQL: `insert into t (a, b) values`.
    prefix: String,
    line: String,
    rows: u64,
    /// MySQL: rows and bytes in the INSERT being written.
    batch: (usize, usize),
    /// Bytes written so far, and where each SQL statement starts: [`inserts`] splits
    /// the output there.
    written: usize,
    starts: Vec<usize>,
}

/// A failure while writing; the text is shown to the user.
pub type Error = String;

fn io(e: io::Error) -> Error {
    e.to_string()
}

impl<W: Write> Writer<W> {
    /// Writes the header: the CSV / TSV column names, JSON's `[`.
    ///
    /// # Errors
    /// The output refused the write.
    pub fn new(out: W, spec: &Spec, columns: &[Column]) -> Result<Self, Error> {
        let keep = spec
            .keep
            .clone()
            .unwrap_or_else(|| (0..columns.len()).collect());
        let kept: Vec<&str> = keep
            .iter()
            .filter_map(|&i| columns.get(i).map(|c| c.name.as_str()))
            .collect();
        let names: Vec<String> = match spec.format {
            Format::Csv | Format::Tsv => Vec::new(),
            Format::Json | Format::JsonLines => {
                unique(&kept).iter().map(|n| json_string(n)).collect()
            }
            Format::SqlInsert => kept.iter().map(|n| sql_ident(spec.dialect, n)).collect(),
        };
        let prefix = format!(
            "insert into {} ({}) values",
            sql_ident(spec.dialect, &spec.table),
            names.join(", ")
        );
        let mut w = Self {
            out,
            format: spec.format,
            dialect: spec.dialect,
            keep,
            names,
            prefix,
            line: String::new(),
            rows: 0,
            batch: (0, 0),
            written: 0,
            starts: Vec::new(),
        };
        match w.format {
            Format::Csv | Format::Tsv => {
                let sep = w.separator();
                for (i, n) in kept.iter().enumerate() {
                    if i > 0 {
                        w.line.push(sep);
                    }
                    delimited(&mut w.line, n, sep);
                }
                w.line.push_str("\r\n");
                w.flush_line()?;
            }
            Format::Json => w.out.write_all(b"[").map_err(io)?,
            Format::JsonLines | Format::SqlInsert => {}
        }
        Ok(w)
    }

    fn separator(&self) -> char {
        if self.format == Format::Tsv {
            '\t'
        } else {
            ','
        }
    }

    fn flush_line(&mut self) -> Result<(), Error> {
        self.out.write_all(self.line.as_bytes()).map_err(io)?;
        self.written += self.line.len();
        self.line.clear();
        Ok(())
    }

    /// Writes one row.
    ///
    /// # Errors
    /// The output refused the write, or a value has no SQL literal (a non-finite
    /// float, an Oracle time of day).
    pub fn row(&mut self, row: &Row) -> Result<(), Error> {
        const NULL: &Value = &Value::Null;
        let cell = |i: usize| row.get(i).unwrap_or(NULL);
        match self.format {
            Format::Csv | Format::Tsv => {
                let sep = self.separator();
                for (n, &i) in self.keep.iter().enumerate() {
                    if n > 0 {
                        self.line.push(sep);
                    }
                    match cell(i) {
                        Value::Null => {}
                        Value::Text(s) if s.is_empty() => self.line.push_str("\"\""),
                        v => delimited(&mut self.line, &plain(v), sep),
                    }
                }
                self.line.push_str("\r\n");
            }
            Format::Json | Format::JsonLines => {
                if self.format == Format::Json {
                    self.line
                        .push_str(if self.rows == 0 { "\n" } else { ",\n" });
                }
                self.line.push('{');
                for (n, &i) in self.keep.iter().enumerate() {
                    if n > 0 {
                        self.line.push(',');
                    }
                    self.line.push_str(&self.names[n]);
                    self.line.push(':');
                    json_value(&mut self.line, cell(i));
                }
                self.line.push('}');
                if self.format == Format::JsonLines {
                    self.line.push('\n');
                }
            }
            Format::SqlInsert => {
                let mut values = String::from("(");
                for (n, &i) in self.keep.iter().enumerate() {
                    if n > 0 {
                        values.push_str(", ");
                    }
                    let lit = match self.dialect {
                        Dialect::Mysql { backslash_escapes } => literal(cell(i), backslash_escapes),
                        Dialect::Oracle => oracle_literal(cell(i)),
                    };
                    values.push_str(&lit.map_err(|e| format!("row {}: {e}", self.rows + 1))?);
                }
                values.push(')');
                match self.dialect {
                    // INSERT ALL is avoided: one plain statement per row.
                    Dialect::Oracle => {
                        self.starts.push(self.written + self.line.len());
                        let _ = writeln!(self.line, "{} {values};", self.prefix);
                    }
                    Dialect::Mysql { .. } => {
                        if self.batch.0 > 0
                            && (self.batch.0 >= BATCH_ROWS
                                || self.batch.1 + values.len() > BATCH_BYTES)
                        {
                            self.line.push_str(";\n");
                            self.batch = (0, 0);
                        }
                        if self.batch.0 == 0 {
                            self.starts.push(self.written + self.line.len());
                            self.line.push_str(&self.prefix);
                            self.line.push('\n');
                        } else {
                            self.line.push_str(",\n");
                        }
                        self.batch.0 += 1;
                        self.batch.1 += values.len();
                        self.line.push_str(&values);
                    }
                }
            }
        }
        self.rows += 1;
        self.flush_line()
    }

    /// Rows written so far.
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// Writes the end (JSON's `]`, the last INSERT's `;`) and flushes.
    ///
    /// # Errors
    /// The output refused the write.
    pub fn finish(mut self) -> Result<W, Error> {
        match self.format {
            Format::Json => self
                .line
                .push_str(if self.rows == 0 { "]\n" } else { "\n]\n" }),
            Format::SqlInsert if self.batch.0 > 0 => self.line.push_str(";\n"),
            _ => {}
        }
        self.flush_line()?;
        self.out.flush().map_err(io)?;
        Ok(self.out)
    }
}

/// The INSERT statements a SQL export writes for `rows`, each without its `;`: MySQL
/// rows batched as the export batches them, one statement per row for Oracle. For
/// import, which runs them.
///
/// # Errors
/// A value has no SQL literal.
pub fn inserts(spec: &Spec, columns: &[Column], rows: &[Row]) -> Result<Vec<String>, Error> {
    let spec = Spec {
        format: Format::SqlInsert,
        ..spec.clone()
    };
    let mut w = Writer::new(Vec::new(), &spec, columns)?;
    for r in rows {
        w.row(r)?;
    }
    let starts = std::mem::take(&mut w.starts);
    let text = String::from_utf8(w.finish()?).map_err(|e| e.to_string())?;
    Ok(starts
        .iter()
        .enumerate()
        .map(|(i, &start)| {
            let end = starts.get(i + 1).copied().unwrap_or(text.len());
            let s = text[start..end].trim_end();
            s.strip_suffix(';').unwrap_or(s).to_owned()
        })
        .collect())
}

/// Column names made unique for JSON keys: a second `id` becomes `id_2`.
fn unique(names: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(names.len());
    for n in names {
        let mut name = (*n).to_owned();
        let mut k = 2;
        while out.contains(&name) {
            name = format!("{n}_{k}");
            k += 1;
        }
        out.push(name);
    }
    out
}

fn sql_ident(dialect: Dialect, name: &str) -> String {
    match dialect {
        Dialect::Mysql { .. } => quote_ident(name),
        Dialect::Oracle => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

/// A field for CSV / TSV: quoted when it holds the separator, a quote, CR or LF, or
/// starts or ends with a space; quotes inside are doubled.
fn delimited(out: &mut String, s: &str, sep: char) {
    let quote = s.is_empty()
        || s.starts_with(' ')
        || s.ends_with(' ')
        || s.contains([sep, '"', '\r', '\n']);
    if !quote {
        out.push_str(s);
        return;
    }
    out.push('"');
    for c in s.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
}

fn hex(out: &mut String, b: &[u8]) {
    out.reserve(b.len() * 2);
    for x in b {
        let _ = write!(out, "{x:02X}");
    }
}

/// A value as plain text: what CSV writes and JSON quotes.
fn plain(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bytes(b) => {
            let mut s = String::new();
            hex(&mut s, b);
            s
        }
        Value::DateTime(d) => d.format("%Y-%m-%dT%H:%M:%S%.f").to_string(),
        v => v.to_string(),
    }
}

fn json_string(s: &str) -> String {
    // Serialising a str cannot fail.
    serde_json::to_string(s).unwrap_or_default()
}

/// Integers a double holds exactly.
const MAX_SAFE: u64 = 1 << 53;

fn json_value(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) if i.unsigned_abs() <= MAX_SAFE => {
            let _ = write!(out, "{i}");
        }
        Value::UInt(u) if *u <= MAX_SAFE => {
            let _ = write!(out, "{u}");
        }
        Value::Float(f) if f.is_finite() => {
            let _ = write!(out, "{}", serde_json::Value::from(*f));
        }
        Value::Decimal(d) if json_number(d) => out.push_str(d),
        v => out.push_str(&json_string(&plain(v))),
    }
}

/// Decimal text that is a JSON number and that a double holds without losing digits
/// (at most 15 significant ones).
fn json_number(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i - start
    };
    let int_start = i;
    let int = digits(&mut i);
    if int == 0 || (int > 1 && b[int_start] == b'0') {
        return false;
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        if digits(&mut i) == 0 {
            return false;
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if digits(&mut i) == 0 {
            return false;
        }
    }
    let mantissa = s.split(['e', 'E']).next().unwrap_or(s);
    let significant = mantissa
        .bytes()
        .filter(u8::is_ascii_digit)
        .skip_while(|&d| d == b'0')
        .count();
    i == b.len() && significant <= 15
}

/// `<path>.part`: where an export writes until it is complete.
fn part_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".part");
    PathBuf::from(p)
}

fn create(path: &Path) -> Result<io::BufWriter<std::fs::File>, Error> {
    std::fs::File::create(path)
        .map(|f| io::BufWriter::with_capacity(256 * 1024, f))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Moves the finished file into place, or removes the partial one.
fn settle(part: &Path, path: &Path, written: Result<u64, Error>) -> Result<u64, Error> {
    match written {
        Ok(n) => std::fs::rename(part, path)
            .map(|()| n)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) => {
            let _ = std::fs::remove_file(part);
            Err(e)
        }
    }
}

/// Writes rows already in memory (the grid's loaded rows) to `path`. Blocking.
///
/// # Errors
/// The file could not be written, or a value has no SQL literal. Nothing is left at
/// `path` then.
pub fn write_rows(
    columns: &[Column],
    rows: &[Row],
    spec: &Spec,
    path: &Path,
) -> Result<u64, Error> {
    let part = part_path(path);
    let written = (|| {
        let mut w = Writer::new(create(&part)?, spec, columns)?;
        for r in rows {
            w.row(r)?;
        }
        let n = w.rows();
        w.finish()?;
        Ok(n)
    })();
    settle(&part, path, written)
}

/// Runs `sql` on `driver` and writes every row of its first result set to `path` as
/// the pages arrive. `progress` counts the rows written. A message on `cancel` (or
/// dropping its sender) stops the export: the statement is cancelled on the server.
///
/// # Errors
/// The statement failed, the file could not be written, a value has no SQL literal,
/// or the export was cancelled (`"cancelled"`). Nothing is left at `path` then.
pub async fn run(
    driver: Arc<dyn Driver>,
    sql: String,
    binds: Vec<(String, Option<String>)>,
    spec: Spec,
    path: PathBuf,
    progress: Arc<AtomicU64>,
    cancel: oneshot::Receiver<()>,
) -> Result<u64, Error> {
    let d = Arc::clone(&driver);
    let rows: crate::driver::BoxFuture<
        'static,
        crate::driver::Result<crate::driver::ResultStream>,
    > = Box::pin(async move { d.execute_bound(&sql, &binds).await });
    run_from(driver, rows, spec, path, progress, cancel).await
}

/// [`run`] for rows from another source than a statement: a Redis key's elements.
///
/// # Errors
/// As [`run`].
pub async fn run_from(
    driver: Arc<dyn Driver>,
    rows: crate::driver::BoxFuture<'static, crate::driver::Result<crate::driver::ResultStream>>,
    spec: Spec,
    path: PathBuf,
    progress: Arc<AtomicU64>,
    cancel: oneshot::Receiver<()>,
) -> Result<u64, Error> {
    let part = part_path(&path);
    let target = part.clone();
    let work = async move {
        let mut stream = rows.await.map_err(|e| e.to_string())?;
        if stream.columns.is_empty() {
            return Err("the statement returned no rows to export".to_owned());
        }
        let columns = std::mem::take(&mut stream.columns);
        let (tx, mut rx) = mpsc::channel::<Vec<Row>>(2);
        // The file is written on the blocking pool, so a slow disk never holds up the
        // runtime's other sessions.
        let writer = tokio::task::spawn_blocking(move || {
            let mut w = Writer::new(create(&target)?, &spec, &columns)?;
            while let Some(page) = rx.blocking_recv() {
                for r in &page {
                    w.row(r)?;
                }
                progress.fetch_add(page.len() as u64, Ordering::Relaxed);
            }
            let n = w.rows();
            w.finish()?;
            Ok::<_, Error>(n)
        });
        let mut fetch = Ok(());
        while let Some(page) = stream.next_page().await {
            match page {
                Ok(rows) => {
                    if tx.send(rows).await.is_err() {
                        break; // the writer failed: its error is the answer
                    }
                }
                Err(e) => {
                    fetch = Err(e.to_string());
                    break;
                }
            }
        }
        drop(tx);
        let written = writer
            .await
            .map_err(|_| "the export's writer stopped".to_owned())?;
        fetch.and(written)
    };
    let written = tokio::select! {
        r = work => r,
        _ = cancel => {
            // The dropped stream would otherwise read the rest of the result.
            let _ = driver.cancel().await;
            Err("cancelled".to_owned())
        }
    };
    settle(&part, &path, written)
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, TimeDelta};

    use super::*;

    fn cols(names: &[&str]) -> Vec<Column> {
        names
            .iter()
            .map(|n| Column {
                name: (*n).into(),
                type_name: String::new(),
            })
            .collect()
    }

    fn spec(format: Format) -> Spec {
        Spec {
            format,
            dialect: Dialect::Mysql {
                backslash_escapes: true,
            },
            table: "t".into(),
            keep: None,
        }
    }

    fn write(spec: &Spec, columns: &[Column], rows: &[Row]) -> String {
        let mut w = Writer::new(Vec::new(), spec, columns).unwrap();
        for r in rows {
            w.row(r).unwrap();
        }
        String::from_utf8(w.finish().unwrap()).unwrap()
    }

    fn text(s: &str) -> Value {
        Value::Text(s.into())
    }

    #[test]
    fn csv_quotes_per_rfc_4180() {
        let rows = vec![
            vec![text("plain"), text("a,b"), text("say \"hi\"")],
            vec![text("two\nlines"), text("cr\r"), text(" padded")],
            vec![Value::Null, text(""), Value::Int(-3)],
        ];
        let out = write(&spec(Format::Csv), &cols(&["a", "b,c", "d"]), &rows);
        assert_eq!(
            out,
            "a,\"b,c\",d\r\n\
             plain,\"a,b\",\"say \"\"hi\"\"\"\r\n\
             \"two\nlines\",\"cr\r\",\" padded\"\r\n\
             ,\"\",-3\r\n"
        );
        assert!(!out.starts_with('\u{feff}'));
    }

    #[test]
    fn tsv_quotes_tabs_not_commas() {
        let rows = vec![vec![text("a,b"), text("tab\there")]];
        let out = write(&spec(Format::Tsv), &cols(&["x", "y"]), &rows);
        assert_eq!(out, "x\ty\r\na,b\t\"tab\there\"\r\n");
    }

    #[test]
    fn binary_dates_and_times_in_text_formats() {
        let d = NaiveDate::from_ymd_opt(2024, 5, 1).unwrap();
        let dt = d.and_hms_milli_opt(13, 45, 0, 250).unwrap();
        let t = -(TimeDelta::hours(838) + TimeDelta::seconds(59));
        let rows = vec![vec![
            Value::Bytes(vec![0xDE, 0xAD]),
            Value::Date(d),
            Value::DateTime(dt),
            Value::Time(t),
        ]];
        let out = write(&spec(Format::Csv), &cols(&["b", "d", "dt", "t"]), &rows);
        assert_eq!(
            out.lines().nth(1).unwrap(),
            "DEAD,2024-05-01,2024-05-01T13:45:00.250,-838:00:59"
        );
    }

    #[test]
    fn json_keeps_number_precision() {
        let rows = vec![vec![
            Value::Int(42),
            Value::Int(i64::MAX),
            Value::UInt(u64::MAX),
            Value::Decimal("12.50".into()),
            Value::Decimal("12345678901234567.89".into()),
            Value::Float(0.1),
            Value::Float(f64::NAN),
            Value::Null,
            Value::Bool(true),
            text("q\"\n"),
        ]];
        let names = [
            "i", "big", "ubig", "dec", "bigdec", "f", "nan", "n", "b", "s",
        ];
        let out = write(&spec(Format::JsonLines), &cols(&names), &rows);
        assert_eq!(
            out,
            "{\"i\":42,\"big\":\"9223372036854775807\",\"ubig\":\"18446744073709551615\",\
             \"dec\":12.50,\"bigdec\":\"12345678901234567.89\",\"f\":0.1,\"nan\":\"NaN\",\
             \"n\":null,\"b\":true,\"s\":\"q\\\"\\n\"}\n"
        );
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["big"], "9223372036854775807");
    }

    #[test]
    fn json_is_an_array_with_unique_keys() {
        let rows = vec![
            vec![Value::Int(1), Value::Int(2)],
            vec![Value::Int(3), Value::Null],
        ];
        let out = write(&spec(Format::Json), &cols(&["id", "id"]), &rows);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v,
            serde_json::json!([{"id": 1, "id_2": 2}, {"id": 3, "id_2": null}])
        );
        assert_eq!(write(&spec(Format::Json), &cols(&["id"]), &[]), "[]\n");
    }

    #[test]
    fn json_number_grammar() {
        for ok in ["0", "-1.5", "1e10", "1.25E-3", "123456789012345"] {
            assert!(json_number(ok), "{ok}");
        }
        for bad in [".5", "5.", "+1", "007", "1234567890123456", "", "-", "1e"] {
            assert!(!json_number(bad), "{bad}");
        }
    }

    #[test]
    fn mysql_inserts_are_batched() {
        let rows: Vec<Row> = (0..BATCH_ROWS + 2)
            .map(|i| vec![Value::Int(i as i64), text("it's \\")])
            .collect();
        let out = write(&spec(Format::SqlInsert), &cols(&["id", "name"]), &rows);
        assert_eq!(
            out.matches("insert into `t` (`id`, `name`) values\n")
                .count(),
            2
        );
        assert!(out.starts_with("insert into `t` (`id`, `name`) values\n(0, 'it''s \\\\'),\n(1,"));
        assert!(out.ends_with("(501, 'it''s \\\\');\n"));
        assert_eq!(out.matches(";\n").count(), 2);
    }

    #[test]
    fn inserts_split_into_statements() {
        let rows: Vec<Row> = (0..BATCH_ROWS + 1)
            .map(|i| vec![Value::Int(i as i64), text("a;\nb")])
            .collect();
        let my = inserts(&spec(Format::Csv), &cols(&["id", "name"]), &rows).unwrap();
        assert_eq!(my.len(), 2);
        assert!(my[0].starts_with("insert into `t` (`id`, `name`) values\n(0, 'a;\nb'),"));
        assert!(my[0].ends_with("(499, 'a;\nb')"));
        assert_eq!(
            my[1],
            "insert into `t` (`id`, `name`) values\n(500, 'a;\nb')"
        );
        let s = Spec {
            dialect: Dialect::Oracle,
            table: "T".into(),
            ..spec(Format::Csv)
        };
        let ora = inserts(&s, &cols(&["A"]), &[vec![text("x;")], vec![Value::Null]]).unwrap();
        assert_eq!(
            ora,
            [
                "insert into \"T\" (\"A\") values ('x;')",
                "insert into \"T\" (\"A\") values (NULL)"
            ]
        );
        assert!(
            inserts(&spec(Format::Csv), &cols(&["a"]), &[])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn oracle_inserts_one_statement_per_row() {
        let s = Spec {
            dialect: Dialect::Oracle,
            table: "T".into(),
            keep: Some(vec![1]),
            ..spec(Format::SqlInsert)
        };
        let d = NaiveDate::from_ymd_opt(2024, 5, 1).unwrap();
        let rows = vec![
            vec![text("AAAx"), Value::Date(d)],
            vec![text("AAAy"), Value::Null],
        ];
        let out = write(&s, &cols(&["ROWID", "D"]), &rows);
        assert_eq!(
            out,
            "insert into \"T\" (\"D\") values (DATE '2024-05-01');\n\
             insert into \"T\" (\"D\") values (NULL);\n"
        );
        assert!(!out.contains("insert all"));
    }

    #[test]
    fn sql_refuses_values_without_a_literal() {
        let mut w = Writer::new(Vec::new(), &spec(Format::SqlInsert), &cols(&["f"])).unwrap();
        assert!(
            w.row(&vec![Value::Float(f64::INFINITY)])
                .unwrap_err()
                .contains("row 1")
        );
    }

    #[test]
    fn write_rows_leaves_no_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.sql");
        let bad = vec![vec![Value::Int(1)], vec![Value::Float(f64::NAN)]];
        assert!(write_rows(&cols(&["x"]), &bad, &spec(Format::SqlInsert), &path).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        let good = vec![vec![Value::Int(1)]];
        assert_eq!(
            write_rows(&cols(&["x"]), &good, &spec(Format::Csv), &path),
            Ok(1)
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\r\n1\r\n");
    }
}
