//! Redis's Clients and slow log screen and its Server configuration screen.

use crate::config::{Applied, ConfigChange, ServerConfig};
use crate::driver::{DriverError, Result};
use crate::redis::{self as rk, Command};
use crate::redis_driver::Redis;
use crate::sessions::{Session, SessionId, SlowEntry, Snapshot, State};

/// Slow-log entries the screen shows.
const SLOW_ENTRIES: usize = 128;

fn text(v: &redis::Value) -> String {
    match v {
        redis::Value::BulkString(b) => String::from_utf8_lossy(b).into_owned(),
        redis::Value::SimpleString(s) | redis::Value::VerbatimString { text: s, .. } => s.clone(),
        redis::Value::Int(i) => i.to_string(),
        _ => String::new(),
    }
}

fn int(v: Option<&redis::Value>) -> i64 {
    match v {
        Some(redis::Value::Int(i)) => *i,
        Some(other) => text(other).parse().unwrap_or(0),
        None => 0,
    }
}

/// `CLIENT LIST` as sessions, and the slow log.
pub(crate) async fn sessions(r: &Redis) -> Result<Snapshot> {
    let mut conn = r.conn();
    let list: String = redis::cmd("CLIENT")
        .arg("LIST")
        .query_async(&mut conn)
        .await?;
    let own = r.client_id();
    let sessions = rk::client_list(&list)
        .into_iter()
        .map(|c| Session {
            id: c.id,
            user: c.user.clone(),
            host: c.addr.clone(),
            application: (!c.name.is_empty())
                .then(|| c.name.clone())
                .or_else(|| (!c.lib_name.is_empty()).then(|| c.lib_name.clone())),
            database: Some(format!("db{}", c.db)),
            state: if c.blocked() {
                State::Blocked
            } else if c.id == own || c.idle == 0 {
                State::Active
            } else {
                State::Idle
            },
            detail: c.flag_words(),
            seconds: Some(c.idle),
            statement: (!c.cmd.is_empty() && c.cmd != "NULL").then(|| c.cmd.replace('|', " ")),
            running: c.blocked(),
            transaction: None,
            holds: Vec::new(),
            own: c.id == own,
            deadlock: c.lib_name == "deadlock",
            age: Some(c.age),
        })
        .collect();
    let mut notes = Vec::new();
    let slow_log = match redis::cmd("SLOWLOG")
        .arg("GET")
        .arg(SLOW_ENTRIES)
        .query_async::<redis::Value>(&mut conn)
        .await
    {
        Ok(v) => Some(slow_entries(v)),
        Err(e) => {
            notes.push(format!("The slow log could not be read: {e}"));
            None
        }
    };
    Ok(Snapshot {
        sessions,
        waits: Vec::new(),
        own_only: None,
        notes,
        slow_log,
    })
}

/// `SLOWLOG GET` entries: id, time, microseconds, arguments, client address, name.
fn slow_entries(v: redis::Value) -> Vec<SlowEntry> {
    let redis::Value::Array(entries) = v else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|e| {
            let redis::Value::Array(f) = e else {
                return None;
            };
            let args = match f.get(3) {
                Some(redis::Value::Array(a)) => a
                    .iter()
                    .map(|x| match x {
                        redis::Value::BulkString(b) => rk::quote(b),
                        other => text(other),
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => String::new(),
            };
            let client = [f.get(4), f.get(5)]
                .into_iter()
                .flatten()
                .map(text)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            Some(SlowEntry {
                id: u64::try_from(int(f.first())).unwrap_or(0),
                at: int(f.get(1)),
                micros: u64::try_from(int(f.get(2))).unwrap_or(0),
                command: args,
                client,
            })
        })
        .collect()
}

/// `CLIENT KILL ID` or `CLIENT UNBLOCK`; never this screen's own client.
pub(crate) async fn client_command(r: &Redis, id: SessionId, what: &'static str) -> Result<()> {
    if id == r.client_id() {
        return Err(DriverError::Invalid(
            "That is this screen's own connection.".into(),
        ));
    }
    let c = match what {
        "KILL" => Command::new("CLIENT")
            .arg("KILL")
            .arg("ID")
            .arg(id.to_string()),
        _ => Command::new("CLIENT").arg("UNBLOCK").arg(id.to_string()),
    };
    let reply = r.run(&[c]).await?;
    match reply.first() {
        Some(redis::Value::Int(0)) => Err(DriverError::Invalid(format!(
            "Client {id} is gone or was not blocked."
        ))),
        _ => Ok(()),
    }
}

/// `CONFIG GET *`, and whether `CONFIG REWRITE` has a file to write.
pub(crate) async fn config(r: &Redis) -> Result<ServerConfig> {
    let mut conn = r.conn();
    let reply: redis::Value = redis::cmd("CONFIG")
        .arg("GET")
        .arg("*")
        .query_async(&mut conn)
        .await?;
    let pairs: Vec<(String, String)> = match reply {
        redis::Value::Map(m) => m.iter().map(|(k, v)| (text(k), text(v))).collect(),
        redis::Value::Array(a) => a
            .chunks(2)
            .map(|p| (text(&p[0]), p.get(1).map(text).unwrap_or_default()))
            .collect(),
        _ => Vec::new(),
    };
    let server: String = redis::cmd("INFO")
        .arg("server")
        .query_async(&mut conn)
        .await?;
    let config_file = server
        .lines()
        .find_map(|l| l.strip_prefix("config_file:"))
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(str::to_owned);
    Ok(ServerConfig {
        params: rk::config_params(pairs),
        persist: config_file.is_some(),
        config_file,
        read_only: r
            .is_read_only()
            .then(|| "Read-only connection: settings cannot be changed.".to_owned()),
        redis: true,
        ..ServerConfig::default()
    })
}

/// Runs each change's `CONFIG SET`, then `CONFIG REWRITE` when one asks to persist.
pub(crate) async fn apply_config(
    r: &Redis,
    config: &ServerConfig,
    changes: &[ConfigChange],
) -> Result<Applied> {
    let mut done = 0;
    for c in changes {
        let statements = match config.statements(c) {
            Ok(s) => s,
            Err(e) => {
                return Ok(Applied {
                    done,
                    error: Some(e),
                });
            }
        };
        for s in statements {
            let args = rk::split_args(&s).map_err(DriverError::Invalid)?;
            let mut cmd = redis::cmd("CONFIG");
            for a in &args[1..] {
                cmd.arg(a);
            }
            let mut conn = r.conn();
            if let Err(e) = cmd.query_async::<()>(&mut conn).await {
                return Ok(Applied {
                    done,
                    error: Some(format!("{}: {e}", c.name)),
                });
            }
        }
        done += 1;
    }
    if config.trailer(changes).is_some() {
        let mut conn = r.conn();
        if let Err(e) = redis::cmd("CONFIG")
            .arg("REWRITE")
            .query_async::<()>(&mut conn)
            .await
        {
            return Ok(Applied {
                done,
                error: Some(format!(
                    "The changes apply now, but CONFIG REWRITE failed, so they are lost at \
                     the next restart: {e}"
                )),
            });
        }
    }
    Ok(Applied { done, error: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_log_entries_parse() {
        use redis::Value as R;
        let v = R::Array(vec![R::Array(vec![
            R::Int(7),
            R::Int(1_700_000_000),
            R::Int(15_000),
            R::Array(vec![
                R::BulkString(b"SET".to_vec()),
                R::BulkString(b"a b".to_vec()),
            ]),
            R::BulkString(b"127.0.0.1:5000".to_vec()),
            R::BulkString(b"worker".to_vec()),
        ])]);
        let e = slow_entries(v);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].id, 7);
        assert_eq!(e[0].micros, 15_000);
        assert_eq!(e[0].command, r#"SET "a b""#);
        assert_eq!(e[0].client, "127.0.0.1:5000 worker");
    }
}
