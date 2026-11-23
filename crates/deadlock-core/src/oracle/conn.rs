//! The one layer the two Oracle drivers differ in: running a statement and mapping
//! its values. Every query, parser and model in `oracle/` goes through [`Conn`], so
//! the Thin (`oracledb`, pure Rust) and Thick (`oracle`, Oracle Instant Client)
//! drivers share all of it.

use oracledb::{DbType, Metadata, OracleNumber, OracleTimestamp, ToDbValue};

use crate::driver::{Column, Result, Row, Value};

/// A positional bind value for the dictionary queries: text, or NULL. Oracle
/// converts it where the SQL compares it with a number.
pub(crate) trait Bind {
    fn text(&self) -> Option<String>;
}

impl Bind for &str {
    fn text(&self) -> Option<String> {
        Some((*self).to_owned())
    }
}

impl Bind for String {
    fn text(&self) -> Option<String> {
        Some(self.clone())
    }
}

impl Bind for Option<String> {
    fn text(&self) -> Option<String> {
        self.clone()
    }
}

/// An open Oracle session.
pub(crate) trait Conn: Send + Sync {
    /// Every row of `sql`, each column read as text. Dictionary queries convert dates
    /// and numbers in the SQL, so this is all they need.
    fn texts(&self, sql: &str, params: &[&dyn Bind]) -> Result<Vec<Vec<Option<String>>>>;

    /// Runs a statement that returns no rows; the rows it changed.
    fn execute(&self, sql: &str, params: &[&dyn Bind]) -> Result<u64>;

    fn commit(&self) -> Result<()>;

    fn rollback(&self) -> Result<()>;

    /// Runs a user's statement with its named binds (`None` binds NULL). A query hands
    /// its columns to `columns`, then each row's cells to `row` until either returns
    /// false, and gives `None`; anything else gives the rows it changed. An error
    /// after `columns` was called is a failed fetch.
    fn stream(
        &self,
        text: &str,
        binds: &[(String, Option<String>)],
        columns: &mut dyn FnMut(Vec<Column>) -> bool,
        row: &mut dyn FnMut(Row) -> bool,
    ) -> Result<Option<u64>>;

    /// The lines the last statement wrote with `DBMS_OUTPUT`.
    fn output(&self) -> Result<Vec<String>>;

    /// The warning the last statement left, as text (`success with compilation error`).
    fn last_warning(&self) -> Result<Option<String>>;

    /// Stops the running statement from another thread, where the driver can (Thick:
    /// `OCIBreak`); `None` when it cannot, and a side session cancels instead.
    fn break_execution(&self) -> Option<Result<()>> {
        None
    }
}

/// The PL/SQL that reads `DBMS_OUTPUT` into a cursor, as `:1` (`:c`).
// shortcut: lines over 4000 bytes are cut; the collection type holds no more.
pub(crate) const OUTPUT_SQL: &str = "DECLARE l VARCHAR2(32767); s INTEGER; \
    t SYS.ODCIVARCHAR2LIST := SYS.ODCIVARCHAR2LIST(); \
    BEGIN LOOP DBMS_OUTPUT.GET_LINE(l, s); EXIT WHEN s <> 0; t.EXTEND; \
    t(t.COUNT) := SUBSTRB(l, 1, 4000); END LOOP; \
    OPEN :c FOR SELECT COLUMN_VALUE FROM TABLE(t); END;";

/// `+1 02:03:04.000000000`: a day-to-second interval as both drivers show it.
pub(crate) fn interval_ds(days: i64, h: i64, m: i64, s: i64, ns: i64) -> Value {
    let neg = days < 0 || h < 0 || m < 0 || s < 0 || ns < 0;
    Value::Text(format!(
        "{}{} {:02}:{:02}:{:02}.{:09}",
        if neg { "-" } else { "+" },
        days.unsigned_abs(),
        h.unsigned_abs(),
        m.unsigned_abs(),
        s.unsigned_abs(),
        ns.unsigned_abs()
    ))
}

/// `+1-6`: a year-to-month interval.
pub(crate) fn interval_ym(years: i64, months: i64) -> Value {
    let neg = years < 0 || months < 0;
    Value::Text(format!(
        "{}{}-{}",
        if neg { "-" } else { "+" },
        years.unsigned_abs(),
        months.unsigned_abs()
    ))
}

/// `2026-01-02 02:04:05.123456 +00:00`: six fraction digits, nine when needed.
pub(crate) fn timestamp_text(ts: chrono::NaiveDateTime, zone: &str) -> String {
    use chrono::Timelike;
    let ns = ts.nanosecond();
    let base = ts.format("%Y-%m-%d %H:%M:%S");
    if ns.is_multiple_of(1000) {
        format!("{base}.{:06}{zone}", ns / 1000)
    } else {
        format!("{base}.{ns:09}{zone}")
    }
}

fn values(params: &[&dyn Bind]) -> Vec<Option<String>> {
    params.iter().map(|p| p.text()).collect()
}

/// The Thin driver: `oracledb`.
impl Conn for oracledb::Connection {
    fn texts(&self, sql: &str, params: &[&dyn Bind]) -> Result<Vec<Vec<Option<String>>>> {
        let vals = values(params);
        let refs: Vec<&dyn ToDbValue> = vals.iter().map(|v| v as &dyn ToDbValue).collect();
        let cursor = self.query(sql, &refs)?;
        let n = cursor.columns().len();
        let mut out = Vec::new();
        for row in cursor {
            let row = row?;
            out.push(
                (0..n)
                    .map(|i| row.get::<Option<String>>(i))
                    .collect::<Result<_, _>>()?,
            );
        }
        Ok(out)
    }

    fn execute(&self, sql: &str, params: &[&dyn Bind]) -> Result<u64> {
        let vals = values(params);
        let refs: Vec<&dyn ToDbValue> = vals.iter().map(|v| v as &dyn ToDbValue).collect();
        Ok(oracledb::Connection::execute(self, sql, &refs)?.rows_affected())
    }

    fn commit(&self) -> Result<()> {
        Ok(oracledb::Connection::commit(self)?)
    }

    fn rollback(&self) -> Result<()> {
        Ok(oracledb::Connection::rollback(self)?)
    }

    fn stream(
        &self,
        text: &str,
        binds: &[(String, Option<String>)],
        columns: &mut dyn FnMut(Vec<Column>) -> bool,
        row: &mut dyn FnMut(Row) -> bool,
    ) -> Result<Option<u64>> {
        let mut stmt = self
            .statement(text)
            .and_then(oracledb::StatementBuilder::build)?;
        let named: Vec<(&str, &dyn ToDbValue)> = binds
            .iter()
            .map(|(n, v)| (n.as_str(), v as &dyn ToDbValue))
            .collect();
        if !stmt.is_query() {
            let r = if named.is_empty() {
                stmt.execute(&[])
            } else {
                stmt.execute_named(&named)
            }?;
            return Ok(Some(r.rows_affected()));
        }
        let cursor = if named.is_empty() {
            stmt.query(&[])
        } else {
            stmt.query_named(&named)
        }?;
        let meta: Vec<Metadata> = cursor.columns().to_vec();
        let cols = meta
            .iter()
            .map(|m| Column {
                name: m.name().to_owned(),
                type_name: m.data_type(),
            })
            .collect();
        if !columns(cols) {
            return Ok(None);
        }
        for r in cursor {
            let r = r?;
            let cells = (0..meta.len())
                .map(|i| thin_cell(&r, i, meta[i].db_type()))
                .collect();
            if !row(cells) {
                break;
            }
        }
        Ok(None)
    }

    fn output(&self) -> Result<Vec<String>> {
        let mut r = oracledb::Connection::execute(self, OUTPUT_SQL, &[&oracledb::DB_TYPE_CURSOR])?;
        let cursor: oracledb::Cursor = r.out_bind_data().take(0)?;
        let mut lines = Vec::new();
        for row in cursor {
            lines.push(row?.get::<Option<String>>(0)?.unwrap_or_default());
        }
        Ok(lines)
    }

    fn last_warning(&self) -> Result<Option<String>> {
        Ok(oracledb::Connection::last_warning(self)?)
    }
}

/// Oracle's JSON as text. The driver keeps objects in a hash map, so their keys come
/// out sorted, not in the stored order.
fn json_text(j: oracledb::JsonValue) -> serde_json::Value {
    use oracledb::JsonValue as J;
    use serde_json::Value as S;
    match j {
        J::Null => S::Null,
        J::Boolean(b) => S::Bool(b),
        J::String(s) => S::String(s),
        J::Number(n) => serde_json::from_str(&n.to_string()).unwrap_or(S::String(n.to_string())),
        J::BinaryDouble(f) => serde_json::Number::from_f64(f).map_or(S::Null, S::Number),
        J::BinaryFloat(f) => serde_json::Number::from_f64(f.into()).map_or(S::Null, S::Number),
        J::JsonArray(v) => S::Array(v.into_iter().map(json_text).collect()),
        J::JsonObject(m) => {
            let mut v: Vec<_> = m.into_iter().collect();
            v.sort_by(|a, b| a.0.cmp(&b.0));
            S::Object(v.into_iter().map(|(k, v)| (k, json_text(v))).collect())
        }
        J::IntervalDS(v) => S::String(v.to_string()),
        J::IntervalYM(v) => S::String(v.to_string()),
        J::Timestamp(v) => S::String(v.to_string()),
        J::Raw(b) | J::JsonId(b) => S::String(b.iter().map(|x| format!("{x:02X}")).collect()),
        J::Vector(v) => S::String(format!("{v:?}")),
    }
}

/// Reads column `i` of `row` as the [`Value`] its type maps to. A type the driver
/// cannot convert shows its name instead of failing the whole result.
fn thin_cell(row: &oracledb::Row, i: usize, t: &DbType) -> Value {
    thin_typed(row, i, t).unwrap_or_else(|_| Value::Text(format!("<{}>", t.name())))
}

fn thin_typed(row: &oracledb::Row, i: usize, t: &DbType) -> Result<Value, oracledb::Error> {
    let value = if t == oracledb::DB_TYPE_NUMBER || t == oracledb::DB_TYPE_BINARY_INTEGER {
        row.get::<Option<OracleNumber>>(i)?
            .map(|n| Value::Decimal(n.to_string()))
    } else if t == oracledb::DB_TYPE_BINARY_DOUBLE {
        row.get::<Option<f64>>(i)?.map(Value::Float)
    } else if t == oracledb::DB_TYPE_BINARY_FLOAT {
        row.get::<Option<f32>>(i)?.map(|f| Value::Float(f.into()))
    } else if t == oracledb::DB_TYPE_BOOLEAN {
        row.get::<Option<bool>>(i)?.map(Value::Bool)
    } else if t == oracledb::DB_TYPE_DATE || t == oracledb::DB_TYPE_TIMESTAMP {
        row.get::<Option<OracleTimestamp>>(i)?.map(|ts| {
            chrono::NaiveDateTime::try_from(ts)
                .map_or_else(|_| Value::Text(ts.to_string()), Value::DateTime)
        })
    } else if t == oracledb::DB_TYPE_TIMESTAMP_TZ || t == oracledb::DB_TYPE_TIMESTAMP_LTZ {
        // The driver hands both over in UTC, without the original zone.
        row.get::<Option<OracleTimestamp>>(i)?.map(|ts| {
            chrono::NaiveDateTime::try_from(ts).map_or_else(
                |_| Value::Text(ts.to_string()),
                |d| Value::Text(timestamp_text(d, " +00:00")),
            )
        })
    } else if t == oracledb::DB_TYPE_INTERVAL_DS {
        row.get::<Option<oracledb::OracleIntervalDS>>(i)?.map(|v| {
            interval_ds(
                v.days().into(),
                v.hours().into(),
                v.minutes().into(),
                v.seconds().into(),
                v.nanoseconds().into(),
            )
        })
    } else if t == oracledb::DB_TYPE_INTERVAL_YM {
        row.get::<Option<oracledb::OracleIntervalYM>>(i)?
            .map(|v| interval_ym(v.years().into(), v.months().into()))
    } else if t.is_binary_type() {
        // RAW, LONG RAW and BLOB (which arrives as LONG RAW).
        row.get::<Option<Vec<u8>>>(i)?.map(Value::Bytes)
    } else if t == oracledb::DB_TYPE_JSON {
        row.get::<Option<oracledb::JsonValue>>(i)?
            .map(|j| Value::Text(json_text(j).to_string()))
    } else {
        // Strings, CLOBs (fetched as LONG), ROWIDs.
        row.get::<Option<String>>(i)?.map(Value::Text)
    };
    Ok(value.unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike as _;

    #[test]
    fn timestamps_keep_their_fraction() {
        let d = chrono::NaiveDate::from_ymd_opt(2026, 1, 2)
            .and_then(|d| d.and_hms_nano_opt(3, 4, 5, 123_456_789))
            .unwrap();
        assert_eq!(
            timestamp_text(d, " +00:00"),
            "2026-01-02 03:04:05.123456789 +00:00"
        );
        let d = d.with_nanosecond(500_000_000).unwrap();
        assert_eq!(timestamp_text(d, ""), "2026-01-02 03:04:05.500000");
        assert_eq!(
            interval_ds(1, 2, 3, 4, 0),
            Value::Text("+1 02:03:04.000000000".into())
        );
        assert_eq!(interval_ym(-1, -6), Value::Text("-1-6".into()));
    }
}
