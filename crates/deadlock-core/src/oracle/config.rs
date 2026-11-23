//! Server configuration on Oracle: `V$SYSTEM_PARAMETER` with the spfile's values from
//! `V$SPPARAMETER` and the allowed values from `V$PARAMETER_VALID_VALUES`; changes with
//! `ALTER SYSTEM SET … SCOPE =` and `ALTER SYSTEM RESET`.

use std::collections::HashMap;

use super::Connection;

use super::{col, ora_code, texts};
use crate::config::{
    Applied, Applies, ConfigChange, ConfigParam, Scope, ServerConfig, Source, Unit, ValueType,
    oracle_category, unit_of,
};
use crate::driver::{DriverError, Result};

const NO_ACCESS: &str = "Server configuration reads V$SYSTEM_PARAMETER and V$SPPARAMETER, \
    which this user may not: it needs SELECT ANY DICTIONARY or SELECT_CATALOG_ROLE.";

pub(super) fn read(conn: &Connection) -> Result<ServerConfig> {
    let rows = texts(
        conn,
        "SELECT p.NAME, TO_CHAR(p.TYPE), p.VALUE, p.DEFAULT_VALUE, p.ISDEFAULT, \
           p.ISSES_MODIFIABLE, p.ISSYS_MODIFIABLE, p.ISPDB_MODIFIABLE, p.ISMODIFIED, \
           p.DESCRIPTION, \
           (SELECT LISTAGG(s.VALUE, ', ') WITHIN GROUP (ORDER BY s.ORDINAL) FROM V$SPPARAMETER s \
            WHERE s.NAME = p.NAME AND s.ISSPECIFIED = 'TRUE') \
         FROM V$SYSTEM_PARAMETER p ORDER BY p.NAME",
        &[],
    )
    .map_err(|e| match ora_code(&e) {
        Some(942 | 1031) => DriverError::Invalid(NO_ACCESS.into()),
        _ => e,
    })?;
    let mut valid: HashMap<String, Vec<String>> = HashMap::new();
    for r in texts(
        conn,
        "SELECT NAME, VALUE FROM V$PARAMETER_VALID_VALUES ORDER BY NAME, ORDINAL",
        &[],
    )? {
        valid.entry(col(&r, 0)).or_default().push(col(&r, 1));
    }
    let context = texts(
        conn,
        "SELECT SYS_CONTEXT('USERENV', 'CON_NAME'), SYS_CONTEXT('USERENV', 'CDB_NAME'), \
           (SELECT 'Y' FROM SESSION_PRIVS WHERE PRIVILEGE = 'ALTER SYSTEM') FROM dual",
        &[],
    )?;
    let context = context.first().cloned().unwrap_or_default();
    let (con, cdb, can_alter) = (col(&context, 0), col(&context, 1), col(&context, 2) == "Y");
    let in_pdb = !cdb.is_empty() && con != "CDB$ROOT";
    let mut spfile = None;
    let mut params: Vec<ConfigParam> = Vec::with_capacity(rows.len());
    for r in &rows {
        let name = col(r, 0);
        if params.last().is_some_and(|p| p.name == name) {
            continue;
        }
        if name == "spfile" {
            spfile = r.get(2).cloned().flatten();
        }
        params.push(param(r, &valid, in_pdb));
    }
    let persist = spfile.is_some();
    for p in &mut params {
        if p.source == Source::ConfigFile(None) {
            p.source = Source::ConfigFile(spfile.clone());
        }
    }
    Ok(ServerConfig {
        params,
        config_file: spfile,
        persist,
        read_only: (!can_alter).then(|| {
            "Changing parameters needs the ALTER SYSTEM privilege, which this user does not \
             have: the values are shown only."
                .to_owned()
        }),
        restart_read_only: None,
        backslash_escapes: false,
        oracle: true,
        redis: false,
        container: in_pdb.then(|| {
            format!(
                "Changes apply to the pluggable database {con} only, not to the container \
                 database {cdb} or its other PDBs."
            )
        }),
    })
}

/// One `V$SYSTEM_PARAMETER` row as a [`ConfigParam`].
fn param(r: &[Option<String>], valid: &HashMap<String, Vec<String>>, in_pdb: bool) -> ConfigParam {
    let name = col(r, 0);
    let mut p = ConfigParam::new(&name, r.get(2).cloned().flatten());
    p.category = oracle_category(&p.name);
    p.default = r.get(3).cloned().flatten();
    p.value_type = match col(r, 1).as_str() {
        "1" => ValueType::Enum(vec!["TRUE".into(), "FALSE".into()]),
        "3" | "6" => ValueType::Integer,
        _ => match valid.get(&name) {
            Some(list) if !list.is_empty() => ValueType::Enum(list.clone()),
            _ => ValueType::Text,
        },
    };
    p.unit = match col(r, 1).as_str() {
        "6" => Some(Unit::Bytes),
        "3" => unit_of(&p.name),
        _ => None,
    };
    p.source = if col(r, 8) != "FALSE" {
        Source::Runtime { at: None, by: None }
    } else if col(r, 4) == "FALSE" {
        // Filled in with the spfile's path once it is known.
        Source::ConfigFile(None)
    } else {
        Source::Default
    };
    let system = col(r, 6);
    p.applies = if in_pdb && col(r, 7) != "TRUE" {
        Applies::Fixed
    } else if system == "FALSE" {
        Applies::Restart
    } else {
        Applies::Now
    };
    p.deferred = system == "DEFERRED";
    p.scope = Some(if col(r, 5) == "TRUE" {
        Scope::GlobalAndSession
    } else {
        Scope::GlobalOnly
    });
    p.description = r.get(9).cloned().flatten();
    p.persisted = r.get(10).cloned().flatten();
    p
}

/// Runs each change's statement in order and stops at the first error.
pub(super) fn apply(conn: &Connection, cfg: &ServerConfig, changes: &[ConfigChange]) -> Applied {
    for (done, c) in changes.iter().enumerate() {
        let outcome = cfg.statements(c).and_then(|sql| {
            sql.iter()
                .try_for_each(|s| conn.execute(s, &[]).map(drop).map_err(|e| e.to_string()))
        });
        if let Err(error) = outcome {
            return Applied {
                done,
                error: Some(format!("{}: {error}", c.name)),
            };
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
    use crate::config::{Mode, NewValue};

    fn row(v: &[&str]) -> Vec<Option<String>> {
        v.iter()
            .map(|s| (!s.is_empty()).then(|| (*s).to_owned()))
            .collect()
    }

    fn cfg(params: Vec<ConfigParam>) -> ServerConfig {
        ServerConfig {
            params,
            persist: true,
            oracle: true,
            ..ServerConfig::default()
        }
    }

    fn change(name: &str, value: NewValue, mode: Mode) -> ConfigChange {
        ConfigChange {
            name: name.into(),
            value,
            mode,
        }
    }

    #[test]
    fn parameters_map_and_change_with_the_right_scope() {
        let valid = HashMap::from([(
            "optimizer_mode".to_owned(),
            vec!["ALL_ROWS".to_owned(), "FIRST_ROWS".to_owned()],
        )]);
        let p = |v: &[&str]| param(&row(v), &valid, true);
        let cursors = p(&[
            "open_cursors",
            "3",
            "300",
            "50",
            "FALSE",
            "FALSE",
            "IMMEDIATE",
            "TRUE",
            "FALSE",
            "max # cursors per session",
            "",
        ]);
        assert_eq!(
            (cursors.applies, cursors.value_type.clone()),
            (Applies::Now, ValueType::Integer)
        );
        assert_eq!(cursors.source, Source::ConfigFile(None));
        let mode = p(&[
            "optimizer_mode",
            "2",
            "ALL_ROWS",
            "all_rows",
            "TRUE",
            "TRUE",
            "IMMEDIATE",
            "TRUE",
            "FALSE",
            "",
            "",
        ]);
        assert_eq!(mode.category, crate::config::Category::Optimizer);
        let limit = p(&[
            "resource_limit",
            "1",
            "TRUE",
            "TRUE",
            "TRUE",
            "FALSE",
            "IMMEDIATE",
            "TRUE",
            "FALSE",
            "",
            "",
        ]);
        let cached = p(&[
            "session_cached_cursors",
            "3",
            "50",
            "50",
            "TRUE",
            "TRUE",
            "DEFERRED",
            "TRUE",
            "FALSE",
            "",
            "",
        ]);
        let procs = p(&[
            "processes",
            "3",
            "200",
            "0",
            "FALSE",
            "FALSE",
            "IMMEDIATE",
            "FALSE",
            "FALSE",
            "",
            "",
        ]);
        let audit = param(
            &row(&[
                "audit_trail",
                "2",
                "NONE",
                "none",
                "TRUE",
                "FALSE",
                "FALSE",
                "FALSE",
                "FALSE",
                "",
                "DB",
            ]),
            &valid,
            false,
        );
        assert_eq!(procs.applies, Applies::Fixed);
        assert_eq!(
            (audit.applies, audit.persisted.as_deref()),
            (Applies::Restart, Some("DB"))
        );
        let c = cfg(vec![cursors, mode, limit, cached, procs, audit]);
        let sql =
            |n: &str, v: NewValue, m: Mode| c.statements(&change(n, v, m)).map(|s| s.join("; "));
        assert_eq!(
            sql("open_cursors", NewValue::Set("400".into()), Mode::Persist).unwrap(),
            "ALTER SYSTEM SET open_cursors = 400 SCOPE = BOTH"
        );
        assert_eq!(
            sql("open_cursors", NewValue::Set("400".into()), Mode::RunOnly).unwrap(),
            "ALTER SYSTEM SET open_cursors = 400 SCOPE = MEMORY"
        );
        assert_eq!(
            sql(
                "optimizer_mode",
                NewValue::Set("first_rows".into()),
                Mode::Persist
            )
            .unwrap(),
            "ALTER SYSTEM SET optimizer_mode = 'FIRST_ROWS' SCOPE = BOTH"
        );
        assert!(
            sql(
                "optimizer_mode",
                NewValue::Set("bogus".into()),
                Mode::Persist
            )
            .is_err()
        );
        assert_eq!(
            sql(
                "resource_limit",
                NewValue::Set("false".into()),
                Mode::RunOnly
            )
            .unwrap(),
            "ALTER SYSTEM SET resource_limit = FALSE SCOPE = MEMORY"
        );
        assert_eq!(
            sql(
                "session_cached_cursors",
                NewValue::Set("60".into()),
                Mode::RunOnly
            )
            .unwrap(),
            "ALTER SYSTEM SET session_cached_cursors = 60 DEFERRED SCOPE = MEMORY"
        );
        assert_eq!(
            sql("session_cached_cursors", NewValue::Default, Mode::Persist).unwrap(),
            "ALTER SYSTEM RESET session_cached_cursors DEFERRED SCOPE = MEMORY"
        );
        assert_eq!(
            sql("audit_trail", NewValue::Set("db".into()), Mode::Persist).unwrap(),
            "ALTER SYSTEM SET audit_trail = 'db' SCOPE = SPFILE"
        );
        assert_eq!(
            sql("audit_trail", NewValue::Default, Mode::Persist).unwrap(),
            "ALTER SYSTEM RESET audit_trail SCOPE = SPFILE"
        );
        assert!(
            sql("processes", NewValue::Set("300".into()), Mode::Persist)
                .unwrap_err()
                .contains("CDB$ROOT")
        );
        assert_eq!(c.who_can_set(&c.params[0]), Some("ALTER SYSTEM"));
    }
}
