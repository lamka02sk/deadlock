//! CSV import into a table: an RFC 4180 reader that decodes the file's encoding as it
//! streams, separator and encoding detection, the check of every value against the
//! column's type, and the import itself in one transaction, with the INSERT
//! statements and batching of the SQL export. The file is never held in memory.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use encoding_rs::Encoding;
use encoding_rs_io::{DecodeReaderBytes, DecodeReaderBytesBuilder};
use tokio::sync::{mpsc, oneshot};

use crate::driver::{Column, Driver, Row, Value};
use crate::edit::{TableInfo, TableRef, parse_input};
use crate::export::{Dialect, Format, Spec, inserts};

/// The separators offered, with their labels.
pub const SEPARATORS: [(u8, &str); 3] = [(b',', "Comma"), (b';', "Semicolon"), (b'\t', "Tab")];

/// The encodings offered: UTF-8 and the single-byte ones of Central and Western
/// Europe, plus UTF-16, which a byte-order mark also selects.
pub const ENCODINGS: [&Encoding; 6] = [
    encoding_rs::UTF_8,
    encoding_rs::WINDOWS_1250,
    encoding_rs::WINDOWS_1252,
    encoding_rs::ISO_8859_2,
    encoding_rs::UTF_16LE,
    encoding_rs::UTF_16BE,
];

/// Rows per INSERT batch sent to the server.
const BATCH: usize = 500;

/// One field of a record. `quoted`: written in quotes, so `""` is an empty text,
/// never NULL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub text: String,
    pub quoted: bool,
}

/// Reads RFC 4180 records: fields split at the separator, `"…"` quoting with `""`
/// for a quote, separators and line breaks inside quotes, CRLF or LF line ends. A
/// byte-order mark picks UTF-8 or UTF-16 over `encoding`. Lenient where the RFC is
/// strict: a quote inside an unquoted field is kept, text after a closing quote
/// joins the field. Blank lines are skipped.
pub struct Reader<R: Read> {
    input: BufReader<DecodeReaderBytes<R, Vec<u8>>>,
    separator: u8,
    field: Vec<u8>,
    /// The line break was `\r`: a `\n` right after it belongs to it.
    after_cr: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Start,
    Plain,
    Quoted,
    /// A quote inside a quoted field: the end, or the first of `""`.
    QuoteInQuoted,
}

impl<R: Read> Reader<R> {
    pub fn new(input: R, separator: u8, encoding: &'static Encoding) -> Self {
        let decoded = DecodeReaderBytesBuilder::new()
            .encoding(Some(encoding))
            .bom_override(true)
            .strip_bom(true)
            .build(input);
        Self {
            input: BufReader::with_capacity(256 * 1024, decoded),
            separator,
            field: Vec::new(),
            after_cr: false,
        }
    }

    /// The next record into `out` (cleared first); `false` at the end of the file.
    ///
    /// # Errors
    /// The file could not be read.
    pub fn record(&mut self, out: &mut Vec<Field>) -> io::Result<bool> {
        let Self {
            input,
            separator,
            field,
            after_cr,
        } = self;
        out.clear();
        field.clear();
        let mut state = State::Start;
        loop {
            let buf = input.fill_buf()?;
            if buf.is_empty() {
                // End of file: a record without its line break still counts.
                if state != State::Start || !out.is_empty() {
                    push(field, out, state);
                }
                return Ok(!out.is_empty());
            }
            let mut used = 0;
            let mut done = false;
            for &b in buf {
                used += 1;
                if std::mem::take(after_cr) && b == b'\n' && state == State::Start {
                    continue;
                }
                match (state, b) {
                    (State::Quoted, b'"') => state = State::QuoteInQuoted,
                    (State::Quoted, b) => field.push(b),
                    (State::QuoteInQuoted, b'"') => {
                        field.push(b'"');
                        state = State::Quoted;
                    }
                    (State::Start, b'"') => state = State::Quoted,
                    (_, b) if b == *separator => {
                        push(field, out, state);
                        state = State::Start;
                    }
                    (_, b'\r' | b'\n') => {
                        *after_cr = b == b'\r';
                        // A blank line is no record.
                        if state == State::Start && out.is_empty() {
                            continue;
                        }
                        push(field, out, state);
                        done = true;
                        break;
                    }
                    (_, b) => {
                        field.push(b);
                        state = State::Plain;
                    }
                }
            }
            input.consume(used);
            if done {
                return Ok(true);
            }
        }
    }
}

fn push(field: &mut Vec<u8>, out: &mut Vec<Field>, state: State) {
    // The decoder hands over UTF-8; a quote or separator never splits a character.
    let text = String::from_utf8_lossy(field).into_owned();
    field.clear();
    out.push(Field {
        text,
        quoted: matches!(state, State::Quoted | State::QuoteInQuoted),
    });
}

/// The separator and encoding a file most likely uses, from its first bytes: a
/// byte-order mark, else UTF-8 when the bytes are valid UTF-8, else Windows-1250.
/// The separator is the one that splits the first lines into the same number of
/// fields, more than one.
pub fn detect(head: &[u8]) -> (u8, &'static Encoding) {
    let encoding = match Encoding::for_bom(head) {
        Some((e, _)) => e,
        None => match std::str::from_utf8(head) {
            Ok(_) => encoding_rs::UTF_8,
            // A character cut at the end of the sample is still UTF-8.
            Err(e) if e.error_len().is_none() => encoding_rs::UTF_8,
            Err(_) => encoding_rs::WINDOWS_1250,
        },
    };
    let mut best = (b',', 0usize);
    for (sep, _) in SEPARATORS {
        let mut r = Reader::new(head, sep, encoding);
        let mut rec = Vec::new();
        let mut counts = Vec::new();
        // The last record of a sample may be cut short.
        while counts.len() < 21 && r.record(&mut rec).unwrap_or(false) {
            counts.push(rec.len());
        }
        if counts.len() > 1 {
            counts.pop();
        }
        let width = counts.first().copied().unwrap_or(0);
        let score = if width > 1 && counts.iter().all(|&c| c == width) {
            counts.len() * width
        } else {
            0
        };
        if score > best.1 {
            best = (sep, score);
        }
    }
    (best.0, encoding)
}

/// How the file is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub separator: u8,
    pub encoding: &'static Encoding,
    /// The first record holds column names.
    pub header: bool,
    /// An empty unquoted field is NULL; otherwise empty text.
    pub empty_is_null: bool,
}

impl Options {
    /// Opens `path` for reading with these options.
    ///
    /// # Errors
    /// The file cannot be opened.
    pub fn open(&self, path: &Path) -> Result<Reader<File>, String> {
        let f = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Reader::new(f, self.separator, self.encoding))
    }
}

/// Where each file column goes: `targets[i]` is the table column (an index into
/// [`TableInfo::columns`]) for file column `i`, or `None` to skip it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    pub names: Vec<String>,
    pub targets: Vec<Option<usize>>,
}

impl Mapping {
    /// Matches file columns to table columns by name, ignoring case, spaces and
    /// underscores; without names, by position. Columns the server fills (generated,
    /// Oracle's ROWID) are never targets.
    pub fn auto(names: Vec<String>, info: &TableInfo, by_name: bool) -> Self {
        let key = |s: &str| {
            s.chars()
                .filter(|c| !c.is_whitespace() && *c != '_')
                .flat_map(char::to_lowercase)
                .collect::<String>()
        };
        let open: Vec<usize> = (0..info.columns.len())
            .filter(|&i| writable(info, i))
            .collect();
        let mut used = Vec::new();
        let targets = names
            .iter()
            .enumerate()
            .map(|(n, name)| {
                let t = if by_name {
                    open.iter()
                        .copied()
                        .find(|&i| !used.contains(&i) && key(&info.columns[i].name) == key(name))
                } else {
                    open.get(n).copied()
                };
                used.extend(t);
                t
            })
            .collect();
        Self { names, targets }
    }

    /// The table columns written, in file order.
    pub fn columns(&self) -> Vec<usize> {
        self.targets.iter().flatten().copied().collect()
    }
}

/// Whether the import may write column `i`.
pub fn writable(info: &TableInfo, i: usize) -> bool {
    info.columns
        .get(i)
        .is_some_and(|c| !c.pseudo && !c.generated)
}

/// A record as the values of the mapped table columns, checked against their types.
///
/// # Errors
/// What is wrong with the record, for the user: `price "12,50": expected a number`.
pub fn convert(
    info: &TableInfo,
    map: &Mapping,
    options: &Options,
    record: &[Field],
) -> Result<Row, String> {
    if record.len() != map.names.len() {
        return Err(format!(
            "has {} fields instead of {}",
            record.len(),
            map.names.len()
        ));
    }
    let mut row = Vec::with_capacity(map.targets.len());
    for ((field, target), name) in record.iter().zip(&map.targets).zip(&map.names) {
        let Some(col) = target.and_then(|t| info.columns.get(t)) else {
            continue;
        };
        let value = if field.text.is_empty() && !field.quoted && options.empty_is_null {
            if !col.nullable && !col.auto_increment {
                return Err(format!("{name} is empty, and {} cannot be NULL", col.name));
            }
            Value::Null
        } else {
            parse_input(col, &field.text).map_err(|e| {
                let shown: String = field.text.chars().take(40).collect();
                let cut = if shown.len() < field.text.len() {
                    "…"
                } else {
                    ""
                };
                format!("{name} \"{shown}{cut}\": {e}")
            })?
        };
        row.push(value);
    }
    Ok(row)
}

/// A row that cannot be imported. `row` counts data rows from 1, the header not
/// included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub row: u64,
    pub text: String,
}

/// The result of [`check`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Checked {
    pub rows: u64,
    pub bad: u64,
    /// The first few problems.
    pub first: Vec<Problem>,
}

/// Problems kept by [`check`].
const FIRST: usize = 3;

/// Reads the whole file and converts every value, writing nothing. `progress` counts
/// the rows read; `stop` ends the check early (`None`).
///
/// # Errors
/// The file cannot be read.
pub fn check(
    path: &Path,
    options: &Options,
    info: &TableInfo,
    map: &Mapping,
    progress: &AtomicU64,
    stop: &AtomicBool,
) -> Result<Option<Checked>, String> {
    let mut r = options.open(path)?;
    let mut rec = Vec::new();
    let mut out = Checked::default();
    let read = |r: &mut Reader<File>, rec: &mut Vec<Field>| {
        r.record(rec)
            .map_err(|e| format!("{}: {e}", path.display()))
    };
    if options.header {
        read(&mut r, &mut rec)?;
    }
    while read(&mut r, &mut rec)? {
        if stop.load(Ordering::Relaxed) {
            return Ok(None);
        }
        out.rows += 1;
        if let Err(text) = convert(info, map, options, &rec) {
            out.bad += 1;
            if out.first.len() < FIRST {
                out.first.push(Problem {
                    row: out.rows,
                    text,
                });
            }
        }
        progress.store(out.rows, Ordering::Relaxed);
    }
    Ok(Some(out))
}

/// What [`run`] imports.
#[derive(Debug, Clone)]
pub struct Job {
    pub path: PathBuf,
    pub options: Options,
    pub table: TableRef,
    pub info: TableInfo,
    pub map: Mapping,
    /// Rows that cannot be converted are left out; otherwise the first stops the
    /// import.
    pub skip_bad: bool,
}

/// Imports the file into the table on `driver`, a session of its own, in one
/// transaction: rows go in batches of INSERT statements; any failure rolls everything
/// back and names the row. `progress` counts the rows sent. A message on `cancel`
/// (or dropping its sender) stops and rolls back. Returns the rows imported.
///
/// # Errors
/// The row that failed and why, `"cancelled"`, or why the file or session failed.
pub async fn run(
    driver: Arc<dyn Driver>,
    job: Job,
    progress: Arc<AtomicU64>,
    mut cancel: oneshot::Receiver<()>,
) -> Result<u64, String> {
    let info = &job.info;
    let dialect = if info.oracle {
        Dialect::Oracle
    } else {
        Dialect::Mysql {
            backslash_escapes: info.backslash_escapes,
        }
    };
    // The INSERTs name the table alone: the session moves to its database.
    let use_db = if info.oracle {
        format!(
            "ALTER SESSION SET CURRENT_SCHEMA = {}",
            info.ident(&job.table.database)
        )
    } else {
        format!("USE {}", info.ident(&job.table.database))
    };
    exec(&*driver, &use_db).await?;
    driver
        .set_auto_commit(false)
        .await
        .map_err(|e| e.to_string())?;
    let spec = Spec {
        format: Format::SqlInsert,
        dialect,
        table: job.table.table.clone(),
        keep: None,
    };
    let columns: Vec<Column> = job
        .map
        .columns()
        .into_iter()
        .map(|i| Column {
            name: info.columns[i].name.clone(),
            type_name: String::new(),
        })
        .collect();

    // The file is read on the blocking pool, a batch ahead of the server.
    let (tx, mut rx) = mpsc::channel::<Result<Vec<(u64, Row)>, String>>(2);
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let (job, stop) = (job.clone(), Arc::clone(&stop));
        tokio::task::spawn_blocking(move || read_batches(&job, &tx, &stop))
    };
    let outcome = async {
        let mut total = 0u64;
        while let Some(batch) = rx.recv().await {
            let batch = batch?;
            if !matches!(cancel.try_recv(), Err(oneshot::error::TryRecvError::Empty)) {
                return Err("cancelled".to_owned());
            }
            let rows: Vec<Row> = batch.iter().map(|(_, r)| r.clone()).collect();
            let statements = inserts(&spec, &columns, &rows)?;
            let sent = match dialect {
                // One round trip for the batch: Oracle runs a block of single-row
                // INSERTs, which is all or nothing like MySQL's multi-row one.
                Dialect::Oracle => {
                    exec(
                        &*driver,
                        &format!("BEGIN\n{};\nEND;", statements.join(";\n")),
                    )
                    .await
                }
                Dialect::Mysql { .. } => {
                    let mut r = Ok(());
                    for s in &statements {
                        r = exec(&*driver, s).await;
                        if r.is_err() {
                            break;
                        }
                    }
                    r
                }
            };
            if let Err(e) = sent {
                // Find the row: each one again on its own until one fails.
                for (n, row) in &batch {
                    let one = inserts(&spec, &columns, std::slice::from_ref(row))?;
                    for s in one {
                        if let Err(e) = exec(&*driver, &s).await {
                            return Err(format!("row {n}: {e}"));
                        }
                    }
                }
                let (first, last) = (batch[0].0, batch[batch.len() - 1].0);
                return Err(format!("rows {first} to {last}: {e}"));
            }
            total += batch.len() as u64;
            progress.store(total, Ordering::Relaxed);
        }
        Ok(total)
    }
    .await;
    stop.store(true, Ordering::Relaxed);
    drop(rx);
    let read = reader.await.unwrap_or(Ok(()));
    let outcome = outcome.and_then(|n| read.map(|()| n));
    let ended = driver.end_transaction(outcome.is_ok()).await;
    match (outcome, ended) {
        (Ok(n), Ok(())) => Ok(n),
        (Ok(_), Err(e)) => Err(format!("the commit failed: {e}")),
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(r)) => Err(format!("{e} Rolling back failed too: {r}")),
    }
}

/// Runs a statement and reads its answer to the end.
async fn exec(driver: &dyn Driver, sql: &str) -> Result<(), String> {
    let mut stream = driver.execute(sql).await.map_err(|e| e.to_string())?;
    while let Some(page) = stream.next_page().await {
        page.map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The reading side of [`run`]: converted rows in batches, numbered from 1.
fn read_batches(
    job: &Job,
    tx: &mpsc::Sender<Result<Vec<(u64, Row)>, String>>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let mut r = job.options.open(&job.path)?;
    let mut rec = Vec::new();
    let read = |r: &mut Reader<File>, rec: &mut Vec<Field>| {
        r.record(rec)
            .map_err(|e| format!("{}: {e}", job.path.display()))
    };
    if job.options.header {
        read(&mut r, &mut rec)?;
    }
    let mut n = 0u64;
    let mut batch = Vec::with_capacity(BATCH);
    while read(&mut r, &mut rec)? {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        n += 1;
        match convert(&job.info, &job.map, &job.options, &rec) {
            Ok(row) => batch.push((n, row)),
            Err(_) if job.skip_bad => {}
            Err(e) => {
                let _ = tx.blocking_send(Err(format!("row {n}: {e}")));
                return Ok(());
            }
        }
        if batch.len() == BATCH
            && tx
                .blocking_send(Ok(std::mem::replace(&mut batch, Vec::with_capacity(BATCH))))
                .is_err()
        {
            return Ok(());
        }
    }
    if !batch.is_empty() {
        let _ = tx.blocking_send(Ok(batch));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::ColumnInfo;

    fn records(bytes: &[u8], sep: u8, enc: &'static Encoding) -> Vec<Vec<(String, bool)>> {
        let mut r = Reader::new(bytes, sep, enc);
        let mut rec = Vec::new();
        let mut out = Vec::new();
        while r.record(&mut rec).unwrap() {
            out.push(rec.iter().map(|f| (f.text.clone(), f.quoted)).collect());
        }
        out
    }

    fn texts(bytes: &[u8], sep: u8) -> Vec<Vec<String>> {
        records(bytes, sep, encoding_rs::UTF_8)
            .into_iter()
            .map(|r| r.into_iter().map(|(t, _)| t).collect())
            .collect()
    }

    #[test]
    fn reads_rfc_4180() {
        let csv = b"\xEF\xBB\xBFid,name,note\r\n1,\"a, b\",\"say \"\"hi\"\"\"\r\n2,\"two\nlines\",\r\n\r\n3,,\"\"\n4,x,y";
        assert_eq!(
            texts(csv, b','),
            [
                vec!["id", "name", "note"],
                vec!["1", "a, b", "say \"hi\""],
                vec!["2", "two\nlines", ""],
                vec!["3", "", ""],
                vec!["4", "x", "y"],
            ]
        );
        // An unquoted empty field and `""` stay apart.
        let r = records(b"3,,\"\"\n", b',', encoding_rs::UTF_8);
        assert_eq!(
            r[0],
            [
                ("3".into(), false),
                (String::new(), false),
                (String::new(), true)
            ]
        );
        // A quote inside an unquoted field is kept; CR alone ends a line.
        assert_eq!(
            texts(b"a\"b;c\rd;e", b';'),
            [vec!["a\"b", "c"], vec!["d", "e"]]
        );
        assert_eq!(texts(b"", b','), Vec::<Vec<String>>::new());
        assert_eq!(texts(b"\n\n", b','), Vec::<Vec<String>>::new());
    }

    #[test]
    fn decodes_the_chosen_encoding_and_boms() {
        // "Žluťoučký" in Windows-1250.
        let cp1250 = b"\x8Elu\x9Dou\xE8k\xFD;k\xF9\xF2\n";
        let r = records(cp1250, b';', encoding_rs::WINDOWS_1250);
        assert_eq!(r[0][0].0, "Žluťoučký");
        let utf16: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("a,é\n".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(texts(&utf16, b','), [vec!["a".to_owned(), "é".to_owned()]]);
    }

    #[test]
    fn detects_separator_and_encoding() {
        assert_eq!(detect(b"a,b,c\n1,2,3\n4,5,6\n"), (b',', encoding_rs::UTF_8));
        assert_eq!(detect(b"a;b\n1,5;2\n3;4\n").0, b';');
        assert_eq!(detect(b"a\tb\n1\t2\n").0, b'\t');
        // Commas inside quotes do not count.
        assert_eq!(detect(b"a;b\n\"x,y,z\";2\n\"p,q\";3\n").0, b';');
        assert_eq!(detect(b"n\xE1zev;cena\n").1, encoding_rs::WINDOWS_1250);
        assert_eq!(detect("název;cena\n".as_bytes()).1, encoding_rs::UTF_8);
    }

    fn info() -> TableInfo {
        let col = |name: &str, ty: &str, column_type: &str, nullable: bool| ColumnInfo {
            name: name.into(),
            data_type: ty.into(),
            column_type: column_type.into(),
            nullable,
            ..ColumnInfo::default()
        };
        TableInfo {
            columns: vec![
                ColumnInfo {
                    auto_increment: true,
                    ..col("id", "int", "int", false)
                },
                col("sku", "varchar", "varchar(4)", false),
                col("Unit_Price", "decimal", "decimal(5,2)", true),
                ColumnInfo {
                    generated: true,
                    ..col("total", "decimal", "decimal(5,2)", true)
                },
            ],
            ..TableInfo::default()
        }
    }

    #[test]
    fn maps_by_name_or_position() {
        let i = info();
        let names = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let m = Mapping::auto(names(&["SKU", "unit price", "notes", "total"]), &i, true);
        assert_eq!(m.targets, [Some(1), Some(2), None, None]);
        let m = Mapping::auto(names(&["Column 1", "Column 2"]), &i, false);
        assert_eq!(m.targets, [Some(0), Some(1)]);
    }

    #[test]
    fn converts_and_explains() {
        let i = info();
        let map = Mapping {
            names: vec!["sku".into(), "price".into()],
            targets: vec![Some(1), Some(2)],
        };
        let opts = Options {
            separator: b',',
            encoding: encoding_rs::UTF_8,
            header: true,
            empty_is_null: true,
        };
        let f = |t: &str, quoted| Field {
            text: t.into(),
            quoted,
        };
        assert_eq!(
            convert(&i, &map, &opts, &[f("KB", false), f("12.5", false)]).unwrap(),
            [Value::Text("KB".into()), Value::Decimal("12.5".into())]
        );
        assert_eq!(
            convert(&i, &map, &opts, &[f("KB", false), f("", false)]).unwrap()[1],
            Value::Null
        );
        let e = convert(&i, &map, &opts, &[f("KB", false), f("12,50", false)]).unwrap_err();
        assert!(e.starts_with("price \"12,50\": expected a number"), "{e}");
        let e = convert(&i, &map, &opts, &[f("", false), f("1", false)]).unwrap_err();
        assert!(e.contains("cannot be NULL"), "{e}");
        // `""` is empty text, also when empty means NULL.
        assert_eq!(
            convert(&i, &map, &opts, &[f("", true), f("1", false)]).unwrap()[0],
            Value::Text(String::new())
        );
        let e = convert(&i, &map, &opts, &[f("KB", false)]).unwrap_err();
        assert_eq!(e, "has 1 fields instead of 2");
        let e = convert(&i, &map, &opts, &[f("TOOLONG", false), f("1", false)]).unwrap_err();
        assert!(e.contains("at most 4 characters"), "{e}");
    }

    #[test]
    fn check_counts_rows_and_keeps_the_first_problems() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.csv");
        let mut text = String::from("sku,price\n");
        for n in 1..=10 {
            text.push_str(if n % 3 == 0 {
                "TOOLONG,1\n"
            } else {
                "AB,1.5\n"
            });
        }
        std::fs::write(&path, text).unwrap();
        let i = info();
        let map = Mapping::auto(vec!["sku".into(), "price".into()], &i, true);
        let opts = Options {
            separator: b',',
            encoding: encoding_rs::UTF_8,
            header: true,
            empty_is_null: true,
        };
        let progress = AtomicU64::new(0);
        let c = check(&path, &opts, &i, &map, &progress, &AtomicBool::new(false))
            .unwrap()
            .unwrap();
        assert_eq!((c.rows, c.bad), (10, 3));
        assert_eq!(c.first.iter().map(|p| p.row).collect::<Vec<_>>(), [3, 6, 9]);
        assert!(
            check(&path, &opts, &i, &map, &progress, &AtomicBool::new(true))
                .unwrap()
                .is_none()
        );
    }
}
