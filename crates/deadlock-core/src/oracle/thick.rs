//! The Thick driver: the `oracle` crate on ODPI-C, which loads Oracle Instant Client
//! (`libclntsh`) at run time. Nothing links against it, so the app builds and starts
//! without it; only a Thick connection needs it.

use oracle::sql_type::{OracleType, RefCursor, Timestamp, ToSql};

use super::conn::{Bind, Conn, OUTPUT_SQL, interval_ds, interval_ym, timestamp_text};
use crate::driver::{Column, DriverError, Result, Row, Value};

/// Overrides where Instant Client is loaded from: only that folder is searched.
pub const CLIENT_DIR_ENV: &str = "DEADLOCK_ORACLE_CLIENT_DIR";

/// The library the Thick driver loads.
#[cfg(target_os = "macos")]
const LIBRARY: &str = "libclntsh.dylib";
#[cfg(windows)]
const LIBRARY: &str = "oci.dll";
#[cfg(not(any(target_os = "macos", windows)))]
const LIBRARY: &str = "libclntsh.so";

/// Connects through Instant Client. Its failure to load becomes a message that says
/// what was looked for and how to install it.
pub(super) fn open(user: &str, password: &str, connect_string: &str) -> Result<oracle::Connection> {
    let dir = std::env::var_os(CLIENT_DIR_ENV).filter(|d| !d.is_empty());
    if let Some(d) = &dir {
        let mut p = oracle::InitParams::new();
        p.oracle_client_lib_dir(d.clone())?;
        // Already initialised by an earlier connection: that client stays.
        p.init().map_err(|e| missing_client(&e, dir.as_deref()))?;
    }
    oracle::Connector::new(user, password, connect_string)
        .connect()
        .map_err(|e| missing_client(&e, dir.as_deref()))
}

/// `DPI-1047`, Instant Client not found, as a plain message; any other error as is.
fn missing_client(e: &oracle::Error, dir: Option<&std::ffi::OsStr>) -> DriverError {
    let text = e.to_string();
    if !text.contains("DPI-1047") {
        return DriverError::Invalid(text);
    }
    let place = dir.map_or_else(
        || "in the system library path".to_owned(),
        |d| format!("in {}", std::path::Path::new(d).display()),
    );
    DriverError::Invalid(format!(
        "The Thick driver needs Oracle Instant Client, and {LIBRARY} was not found {place}. \
         Install Instant Client (the Basic or Basic Light package from \
         https://www.oracle.com/database/technologies/instant-client.html), then make it \
         findable: on Linux run ldconfig on its folder or start Deadlock with \
         LD_LIBRARY_PATH set to it, or set {CLIENT_DIR_ENV} to the folder. Or switch the \
         connection back to the Thin driver. Details: {text}"
    ))
}

/// The `oracle` crate cannot fetch the native JSON and VECTOR types: say what to do.
fn unsupported(e: oracle::Error) -> DriverError {
    let text = e.to_string();
    match ["JSON", "VECTOR"]
        .into_iter()
        .find(|t| text.contains(&format!("unsupported Oracle type {t}")))
    {
        Some(t) => DriverError::Invalid(format!(
            "The Thick driver cannot read columns of Oracle's {t} type. Select them \
             converted (JSON_SERIALIZE(col) or VECTOR_SERIALIZE(col)), or use the Thin \
             driver for this connection."
        )),
        None => e.into(),
    }
}

fn values(params: &[&dyn Bind]) -> Vec<Option<String>> {
    params.iter().map(|p| p.text()).collect()
}

impl Conn for oracle::Connection {
    fn texts(&self, sql: &str, params: &[&dyn Bind]) -> Result<Vec<Vec<Option<String>>>> {
        let vals = values(params);
        let refs: Vec<&dyn ToSql> = vals.iter().map(|v| v as &dyn ToSql).collect();
        let mut out = Vec::new();
        for row in self.query(sql, &refs)? {
            let row = row?;
            out.push(
                row.sql_values()
                    .iter()
                    .map(|v| v.get::<Option<String>>())
                    .collect::<Result<_, _>>()?,
            );
        }
        Ok(out)
    }

    fn execute(&self, sql: &str, params: &[&dyn Bind]) -> Result<u64> {
        let vals = values(params);
        let refs: Vec<&dyn ToSql> = vals.iter().map(|v| v as &dyn ToSql).collect();
        Ok(oracle::Connection::execute(self, sql, &refs)?.row_count()?)
    }

    fn commit(&self) -> Result<()> {
        Ok(oracle::Connection::commit(self)?)
    }

    fn rollback(&self) -> Result<()> {
        Ok(oracle::Connection::rollback(self)?)
    }

    fn stream(
        &self,
        text: &str,
        binds: &[(String, Option<String>)],
        columns: &mut dyn FnMut(Vec<Column>) -> bool,
        row: &mut dyn FnMut(Row) -> bool,
    ) -> Result<Option<u64>> {
        let mut stmt = self.statement(text).build()?;
        let named: Vec<(&str, &dyn ToSql)> = binds
            .iter()
            .map(|(n, v)| (n.as_str(), v as &dyn ToSql))
            .collect();
        if !stmt.is_query() {
            if named.is_empty() {
                stmt.execute(&[])?;
            } else {
                stmt.execute_named(&named)?;
            }
            return Ok(Some(stmt.row_count()?));
        }
        let rows = if named.is_empty() {
            stmt.query(&[])
        } else {
            stmt.query_named(&named)
        }
        .map_err(unsupported)?;
        let cols = rows
            .column_info()
            .iter()
            .map(|c| Column {
                name: c.name().to_owned(),
                type_name: c.oracle_type().to_string(),
            })
            .collect();
        if !columns(cols) {
            return Ok(None);
        }
        for r in rows {
            let cells = r?.sql_values().iter().map(cell).collect();
            if !row(cells) {
                break;
            }
        }
        Ok(None)
    }

    fn output(&self) -> Result<Vec<String>> {
        let mut stmt = self.statement(OUTPUT_SQL).build()?;
        stmt.execute(&[&OracleType::RefCursor])?;
        let mut cursor: RefCursor = stmt.bind_value(1)?;
        let mut lines = Vec::new();
        for row in cursor.query()? {
            lines.push(row?.get::<_, Option<String>>(0)?.unwrap_or_default());
        }
        Ok(lines)
    }

    fn last_warning(&self) -> Result<Option<String>> {
        Ok(oracle::Connection::last_warning(self).map(|w| w.to_string()))
    }

    fn break_execution(&self) -> Option<Result<()>> {
        Some(oracle::Connection::break_execution(self).map_err(DriverError::from))
    }
}

/// A cell as the [`Value`] its type maps to; a type that cannot be read shows its
/// name instead of failing the result.
fn cell(v: &oracle::SqlValue) -> Value {
    typed(v).unwrap_or_else(|_| {
        let name = v
            .oracle_type()
            .map_or_else(|_| "?".into(), ToString::to_string);
        Value::Text(format!("<{name}>"))
    })
}

fn typed(v: &oracle::SqlValue) -> Result<Value, oracle::Error> {
    if v.is_null()? {
        return Ok(Value::Null);
    }
    Ok(match v.oracle_type()? {
        OracleType::Number(..) | OracleType::Float(_) | OracleType::Int64 | OracleType::UInt64 => {
            Value::Decimal(v.get::<String>()?)
        }
        OracleType::BinaryDouble => Value::Float(v.get::<f64>()?),
        OracleType::BinaryFloat => Value::Float(v.get::<f32>()?.into()),
        OracleType::Boolean => Value::Bool(v.get::<bool>()?),
        OracleType::Date | OracleType::Timestamp(_) => {
            Value::DateTime(v.get::<chrono::NaiveDateTime>()?)
        }
        // The zone offset as stored, unlike the Thin driver, which gives UTC.
        OracleType::TimestampTZ(_) | OracleType::TimestampLTZ(_) => {
            let t = v.get::<Timestamp>()?;
            let d = chrono::NaiveDate::from_ymd_opt(t.year(), t.month(), t.day())
                .and_then(|d| d.and_hms_nano_opt(t.hour(), t.minute(), t.second(), t.nanosecond()));
            let (h, m) = (t.tz_hour_offset(), t.tz_minute_offset());
            let zone = format!(
                " {}{:02}:{:02}",
                if h < 0 || m < 0 { '-' } else { '+' },
                h.unsigned_abs(),
                m.unsigned_abs()
            );
            d.map_or_else(
                || Value::Text(t.to_string()),
                |d| Value::Text(timestamp_text(d, &zone)),
            )
        }
        OracleType::IntervalDS(..) => {
            let i = v.get::<oracle::sql_type::IntervalDS>()?;
            interval_ds(
                i.days().into(),
                i.hours().into(),
                i.minutes().into(),
                i.seconds().into(),
                i.nanoseconds().into(),
            )
        }
        OracleType::IntervalYM(_) => {
            let i = v.get::<oracle::sql_type::IntervalYM>()?;
            interval_ym(i.years().into(), i.months().into())
        }
        OracleType::Raw(_) | OracleType::LongRaw | OracleType::BLOB => {
            Value::Bytes(v.get::<Vec<u8>>()?)
        }
        _ => Value::Text(v.get::<String>()?),
    })
}
