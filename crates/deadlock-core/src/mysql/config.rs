//! Server configuration on MySQL 8.4 (`performance_schema`) and MariaDB 11
//! (`information_schema.SYSTEM_VARIABLES`).

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use mysql_async::Conn;
use mysql_async::prelude::Queryable;

use crate::config::{
    Applied, Applies, ConfigChange, ConfigParam, Scope, ServerConfig, Source, ValueType, unit_of,
};
use crate::driver::Result;

/// MySQL 8.4's variables that `SET GLOBAL` refuses, probed on 8.4.11: the server
/// reports no dynamic flag.
const MYSQL_STATIC: &str = include_str!("../config/mysql-8.4.tsv");
/// Every global variable of MySQL 8.4.11. Others (plugins, components) are not known
/// to apply at once.
const MYSQL_KNOWN: &str = include_str!("../../tests/fixtures/config/mysql-8.4.11.txt");

fn mysql_tables() -> &'static (HashMap<&'static str, Applies>, HashSet<&'static str>) {
    static T: OnceLock<(HashMap<&'static str, Applies>, HashSet<&'static str>)> = OnceLock::new();
    T.get_or_init(|| {
        let fixed = MYSQL_STATIC
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| l.split_once('\t'))
            .map(|(name, kind)| {
                let applies = if kind == "restart" {
                    Applies::Restart
                } else {
                    // `fixed`, and `session`: no global value to set.
                    Applies::Fixed
                };
                (name, applies)
            })
            .collect();
        (fixed, MYSQL_KNOWN.lines().collect())
    })
}

fn text(r: &mysql_async::Row, i: usize) -> Option<String> {
    r.get_opt::<Option<String>, usize>(i)
        .and_then(std::result::Result::ok)
        .flatten()
}

/// The option file most variables came from.
fn main_file(params: &[ConfigParam]) -> Option<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for p in params {
        if let Source::ConfigFile(Some(path)) = &p.source {
            *counts.entry(path.as_str()).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0)))
        .map(|(p, _)| p.to_owned())
}

/// The current user's global privileges, and whether it has roles enabled (whose
/// privileges `USER_PRIVILEGES` does not list).
async fn privileges(conn: &mut Conn) -> Result<(String, HashSet<String>, bool)> {
    let user: Option<String> = conn.query_first("SELECT CURRENT_USER()").await?;
    let privileges: Vec<String> = conn
        .query(
            "SELECT PRIVILEGE_TYPE FROM information_schema.USER_PRIVILEGES WHERE GRANTEE = \
             CONCAT('''', SUBSTRING_INDEX(CURRENT_USER(), '@', 1), '''@''', \
             SUBSTRING_INDEX(CURRENT_USER(), '@', -1), '''')",
        )
        .await?;
    let roles: Option<u64> = conn
        .query_first(
            "SELECT COUNT(*) FROM information_schema.ENABLED_ROLES WHERE ROLE_NAME IS NOT NULL",
        )
        .await
        .unwrap_or_default();
    Ok((
        user.unwrap_or_default(),
        privileges.into_iter().collect(),
        roles.unwrap_or(0) > 0,
    ))
}

pub(super) async fn read(conn: &mut Conn, mariadb: bool) -> Result<ServerConfig> {
    let mode: Option<String> = conn.query_first("SELECT @@SESSION.sql_mode").await?;
    let backslash_escapes = !mode.unwrap_or_default().contains("NO_BACKSLASH_ESCAPES");
    let (user, privileges, roles) = privileges(conn).await?;
    let has = |p: &str| privileges.contains(p);
    let mut cfg = if mariadb {
        let mut cfg = mariadb_params(conn).await?;
        if !has("SUPER") && !roles {
            cfg.read_only = Some(format!(
                "Read-only: changing server variables needs the SUPER privilege, which {user} \
                 does not have."
            ));
        }
        cfg
    } else {
        let mut cfg = mysql_params(conn).await?;
        let admin = has("SYSTEM_VARIABLES_ADMIN") || has("SUPER");
        if !admin && !roles {
            cfg.read_only = Some(format!(
                "Read-only: changing server variables needs the SYSTEM_VARIABLES_ADMIN \
                 privilege, which {user} does not have."
            ));
        } else if !has("PERSIST_RO_VARIABLES_ADMIN") && !roles {
            cfg.restart_read_only = Some(format!(
                "Changing a restart-only variable needs the PERSIST_RO_VARIABLES_ADMIN \
                 privilege as well, which {user} does not have."
            ));
        }
        cfg
    };
    cfg.backslash_escapes = backslash_escapes;
    cfg.config_file = main_file(&cfg.params);
    Ok(cfg)
}

async fn mysql_params(conn: &mut Conn) -> Result<ServerConfig> {
    let rows: Vec<mysql_async::Row> = conn
        .query(
            "SELECT g.VARIABLE_NAME, g.VARIABLE_VALUE, i.VARIABLE_SOURCE, i.VARIABLE_PATH, \
             i.MIN_VALUE, i.MAX_VALUE, CAST(i.SET_TIME AS CHAR(26)), i.SET_USER, i.SET_HOST, \
             p.VARIABLE_VALUE, IF(s.VARIABLE_NAME IS NULL, 'N', 'Y') \
             FROM performance_schema.global_variables g \
             LEFT JOIN performance_schema.variables_info i USING (VARIABLE_NAME) \
             LEFT JOIN performance_schema.persisted_variables p USING (VARIABLE_NAME) \
             LEFT JOIN performance_schema.session_variables s USING (VARIABLE_NAME) \
             ORDER BY g.VARIABLE_NAME",
        )
        .await?;
    let (fixed, known) = mysql_tables();
    let params = rows
        .iter()
        .filter_map(|r| {
            let name = text(r, 0)?.to_ascii_lowercase();
            let value = text(r, 1);
            let (min, max) = (text(r, 4), text(r, 5));
            let ranged = min.as_deref().is_some_and(|m| m != "0")
                || max.as_deref().is_some_and(|m| m != "0");
            let decimal = value
                .as_deref()
                .is_some_and(|v| v.contains('.') && v.parse::<f64>().is_ok());
            // The server reports no types; a reported range means a number. Decimal
            // ranges come as raw float bits, so they are left out.
            let (value_type, min, max) = match (ranged, decimal) {
                (true, false) => (ValueType::Integer, min, max),
                (true, true) => (ValueType::Float, None, None),
                (false, _) => (ValueType::Text, None, None),
            };
            let source = match text(r, 2).as_deref() {
                Some("COMPILED") | None => Source::Default,
                Some("COMMAND_LINE") => Source::CommandLine,
                Some("PERSISTED") => Source::Persisted,
                Some("DYNAMIC") => Source::Runtime {
                    at: text(r, 6),
                    by: text(r, 7).filter(|u| !u.is_empty()).map(|u| {
                        match text(r, 8).filter(|h| !h.is_empty()) {
                            Some(h) => format!("{u}@{h}"),
                            None => u,
                        }
                    }),
                },
                Some(_) => Source::ConfigFile(text(r, 3).filter(|p| !p.is_empty())),
            };
            let applies = match fixed.get(name.as_str()) {
                Some(a) => *a,
                None if known.contains(name.as_str()) => Applies::Now,
                None => Applies::Unknown,
            };
            Some(ConfigParam {
                // The running value of a compiled-in variable is its default.
                default: (source == Source::Default).then(|| value.clone()).flatten(),
                unit: (value_type == ValueType::Integer)
                    .then(|| unit_of(&name))
                    .flatten(),
                value_type,
                min,
                max,
                source,
                applies,
                scope: Some(if text(r, 10).as_deref() == Some("Y") {
                    Scope::GlobalAndSession
                } else {
                    Scope::GlobalOnly
                }),
                persisted: text(r, 9),
                ..ConfigParam::new(&name, value)
            })
        })
        .collect();
    Ok(ServerConfig {
        params,
        persist: true,
        ..ServerConfig::default()
    })
}

async fn mariadb_params(conn: &mut Conn) -> Result<ServerConfig> {
    let rows: Vec<mysql_async::Row> = conn
        .query(
            "SELECT VARIABLE_NAME, GLOBAL_VALUE, GLOBAL_VALUE_ORIGIN, DEFAULT_VALUE, \
             VARIABLE_SCOPE, VARIABLE_TYPE, VARIABLE_COMMENT, NUMERIC_MIN_VALUE, \
             NUMERIC_MAX_VALUE, NUMERIC_BLOCK_SIZE, ENUM_VALUE_LIST, READ_ONLY, \
             GLOBAL_VALUE_PATH FROM information_schema.SYSTEM_VARIABLES \
             WHERE VARIABLE_SCOPE <> 'SESSION ONLY' ORDER BY VARIABLE_NAME",
        )
        .await?;
    let params = rows
        .iter()
        .filter_map(|r| {
            let name = text(r, 0)?.to_ascii_lowercase();
            let list = || {
                text(r, 10)
                    .unwrap_or_default()
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            };
            let ty = text(r, 5).unwrap_or_default();
            let value_type = match ty.as_str() {
                "BOOLEAN" => ValueType::Boolean,
                "DOUBLE" => ValueType::Float,
                "ENUM" => ValueType::Enum(list()),
                "SET" => ValueType::Set(list()),
                t if t.contains("INT") => ValueType::Integer,
                // VARCHAR, and FLAGSET (`optimizer_switch`), are edited as text.
                _ => ValueType::Text,
            };
            let numeric = matches!(value_type, ValueType::Integer | ValueType::Float);
            Some(ConfigParam {
                default: text(r, 3),
                unit: (value_type == ValueType::Integer)
                    .then(|| unit_of(&name))
                    .flatten(),
                min: text(r, 7).filter(|_| numeric),
                max: text(r, 8).filter(|_| numeric),
                block: text(r, 9).and_then(|b| b.parse().ok()),
                value_type,
                source: match text(r, 2).as_deref() {
                    Some("COMPILE-TIME") | None => Source::Default,
                    Some("AUTO") => Source::Auto,
                    Some("CONFIG") => Source::ConfigFile(text(r, 12).filter(|p| !p.is_empty())),
                    Some("COMMAND-LINE") => Source::CommandLine,
                    Some("SQL") => Source::Runtime { at: None, by: None },
                    Some(other) => Source::Other(other.to_owned()),
                },
                applies: if text(r, 11).as_deref() == Some("YES") {
                    Applies::Restart
                } else {
                    Applies::Now
                },
                scope: Some(if text(r, 4).as_deref() == Some("GLOBAL") {
                    Scope::GlobalOnly
                } else {
                    Scope::GlobalAndSession
                }),
                description: text(r, 6).filter(|c| !c.is_empty()),
                ..ConfigParam::new(&name, text(r, 1))
            })
        })
        .collect();
    Ok(ServerConfig {
        params,
        persist: false,
        ..ServerConfig::default()
    })
}

/// Runs each change's statements in order and stops at the first error.
pub(super) async fn apply(
    conn: &mut Conn,
    cfg: &ServerConfig,
    changes: &[ConfigChange],
) -> Applied {
    for (i, c) in changes.iter().enumerate() {
        let statements = match cfg.statements(c) {
            Ok(s) => s,
            Err(e) => {
                return Applied {
                    done: i,
                    error: Some(e),
                };
            }
        };
        for s in statements {
            if let Err(e) = conn.query_drop(s).await {
                return Applied {
                    done: i,
                    error: Some(e.to_string()),
                };
            }
        }
    }
    Applied {
        done: changes.len(),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_static_table_covers_known_names_only() {
        let (fixed, known) = mysql_tables();
        assert_eq!(fixed.len(), 201);
        assert_eq!(fixed.get("max_connections"), None);
        assert_eq!(
            fixed.get("innodb_buffer_pool_instances"),
            Some(&Applies::Restart)
        );
        assert_eq!(fixed.get("datadir"), Some(&Applies::Fixed));
        assert!(
            fixed.keys().all(|n| known.contains(n)),
            "a probed name is unknown"
        );
    }
}
