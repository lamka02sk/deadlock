//! The schema a session's unqualified names resolve in: the connection's default, the
//! statement that switches it, and reading a typed switch back out of a statement.

use crate::connection::{ConnectionConfig, Engine};
use crate::objects::{Flavor, quote_name};

/// The schema a new session of `config` starts in: MySQL / MariaDB `database`, Oracle
/// the signed-in user, Redis `db<n>`. `None` when MySQL has no default database.
pub fn default_schema(config: &ConnectionConfig) -> Option<String> {
    match config.engine {
        Engine::Mysql | Engine::Mariadb => {
            Some(config.database.trim().to_owned()).filter(|d| !d.is_empty())
        }
        // `sys as sysdba` signs in as SYS.
        Engine::Oracle => config.user.split_whitespace().next().map(|u| {
            match u.strip_prefix('"').and_then(|u| u.strip_suffix('"')) {
                Some(exact) => exact.to_owned(),
                None => u.to_ascii_uppercase(),
            }
        }),
        Engine::Redis => Some(format!(
            "db{}",
            config.database.trim().parse::<u32>().unwrap_or(0)
        )),
    }
}

/// The statement that makes `schema` the session's: `USE`, `ALTER SESSION SET
/// CURRENT_SCHEMA` or Redis `SELECT n` (for a `db<n>` name; `None` otherwise).
pub fn switch_sql(engine: Engine, schema: &str) -> Option<String> {
    Some(match engine {
        Engine::Mysql | Engine::Mariadb => format!("USE {}", quote_name(Flavor::Mysql, schema)),
        Engine::Oracle => format!(
            "ALTER SESSION SET CURRENT_SCHEMA = {}",
            quote_name(Flavor::Oracle, schema)
        ),
        Engine::Redis => format!("SELECT {}", redis_db(schema)?),
    })
}

/// The statement word the schema menu names: what a pick runs.
pub fn switch_verb(engine: Engine) -> &'static str {
    match engine {
        Engine::Mysql | Engine::Mariadb => "USE",
        Engine::Oracle => "ALTER SESSION SET CURRENT_SCHEMA",
        Engine::Redis => "SELECT",
    }
}

fn redis_db(schema: &str) -> Option<u32> {
    schema.strip_prefix("db")?.trim().parse().ok()
}

/// The schema `text` switches the session to, when it is a `USE`, an `ALTER SESSION
/// SET CURRENT_SCHEMA` or a Redis `SELECT n`, as the tree names it (Oracle: a bare
/// name in upper case; Redis: `db<n>`).
pub fn switched_to(engine: Engine, text: &str) -> Option<String> {
    let mysql = matches!(engine, Engine::Mysql | Engine::Mariadb);
    let rest = skip_comments(text, mysql);
    let rest = rest.trim_end().trim_end_matches(';').trim_end();
    match engine {
        Engine::Mysql | Engine::Mariadb => name(word(rest, "USE")?, '`', false),
        Engine::Oracle => {
            let rest = word(rest, "ALTER")
                .and_then(|r| word(r, "SESSION"))
                .and_then(|r| word(r, "SET"))
                .and_then(|r| word(r, "CURRENT_SCHEMA"))?;
            name(rest.trim_start().strip_prefix('=')?, '"', true)
        }
        Engine::Redis => {
            let n: u32 = word(rest, "SELECT")?.trim().parse().ok()?;
            Some(format!("db{n}"))
        }
    }
}

/// `text` after leading white space and comments.
fn skip_comments(mut text: &str, mysql: bool) -> &str {
    loop {
        text = text.trim_start();
        if let Some(r) = text.strip_prefix("/*") {
            text = r.find("*/").map_or("", |e| &r[e + 2..]);
        } else if text.starts_with("--") || (mysql && text.starts_with('#')) {
            text = text.find('\n').map_or("", |e| &text[e + 1..]);
        } else {
            return text;
        }
    }
}

/// What follows the keyword `kw` (any case) at the start of `text`, or `None`.
fn word<'a>(text: &'a str, kw: &str) -> Option<&'a str> {
    let text = text.trim_start();
    let head = text.get(..kw.len())?;
    let rest = &text[kw.len()..];
    let ends = rest
        .chars()
        .next()
        .is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '_' | '$' | '#')));
    (head.eq_ignore_ascii_case(kw) && ends).then_some(rest)
}

/// The one name `text` holds: in `quote` (doubled inside), or bare up to the end. A bare
/// name is upper case when `fold`. `None` for anything after the name.
fn name(text: &str, quote: char, fold: bool) -> Option<String> {
    let text = text.trim();
    if let Some(inner) = text.strip_prefix(quote) {
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == quote {
                match chars.next() {
                    Some(c2) if c2 == quote => out.push(quote),
                    None => return (!out.is_empty()).then_some(out),
                    Some(_) => return None,
                }
            } else {
                out.push(c);
            }
        }
        return None;
    }
    if text.is_empty() || text.contains(char::is_whitespace) {
        return None;
    }
    Some(if fold {
        text.to_ascii_uppercase()
    } else {
        text.to_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(engine: Engine, database: &str, user: &str) -> ConnectionConfig {
        ConnectionConfig {
            engine,
            database: database.into(),
            user: user.into(),
            ..ConnectionConfig::new(engine)
        }
    }

    #[test]
    fn default_schema_per_engine() {
        assert_eq!(
            default_schema(&config(Engine::Mysql, "shop", "u")).as_deref(),
            Some("shop")
        );
        assert_eq!(default_schema(&config(Engine::Mariadb, " ", "u")), None);
        assert_eq!(
            default_schema(&config(Engine::Oracle, "FREEPDB1", "hr")).as_deref(),
            Some("HR")
        );
        assert_eq!(
            default_schema(&config(Engine::Oracle, "FREE", "sys as sysdba")).as_deref(),
            Some("SYS")
        );
        assert_eq!(default_schema(&config(Engine::Oracle, "FREE", "")), None);
        assert_eq!(
            default_schema(&config(Engine::Redis, "3", "")).as_deref(),
            Some("db3")
        );
        assert_eq!(
            default_schema(&config(Engine::Redis, "", "")).as_deref(),
            Some("db0")
        );
    }

    #[test]
    fn switch_statements_quote_the_name() {
        assert_eq!(
            switch_sql(Engine::Mysql, "a`b").as_deref(),
            Some("USE `a``b`")
        );
        assert_eq!(
            switch_sql(Engine::Oracle, "HR").as_deref(),
            Some("ALTER SESSION SET CURRENT_SCHEMA = HR")
        );
        assert_eq!(
            switch_sql(Engine::Oracle, "app").as_deref(),
            Some("ALTER SESSION SET CURRENT_SCHEMA = \"app\"")
        );
        assert_eq!(
            switch_sql(Engine::Redis, "db4").as_deref(),
            Some("SELECT 4")
        );
        assert_eq!(switch_sql(Engine::Redis, "keys"), None);
    }

    #[test]
    fn typed_switches_are_read_back() {
        let m = |t| switched_to(Engine::Mysql, t);
        assert_eq!(m("USE shop").as_deref(), Some("shop"));
        assert_eq!(m("  use `my db`;").as_deref(), Some("my db"));
        assert_eq!(m("-- pick\n/* x */ USE `a``b`").as_deref(), Some("a`b"));
        assert_eq!(m("# c\nuse x ;").as_deref(), Some("x"));
        assert_eq!(m("USER()"), None);
        assert_eq!(m("SELECT 1"), None);
        assert_eq!(m("USE"), None);
        assert_eq!(m("USE a b"), None);
        let o = |t| switched_to(Engine::Oracle, t);
        assert_eq!(
            o("alter session set current_schema = hr").as_deref(),
            Some("HR")
        );
        assert_eq!(
            o("ALTER SESSION SET CURRENT_SCHEMA=\"app\"").as_deref(),
            Some("app")
        );
        assert_eq!(o("ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY'"), None);
        assert_eq!(o("ALTER SESSION SET CURRENT_SCHEMAS = x"), None);
        let r = |t| switched_to(Engine::Redis, t);
        assert_eq!(r("select 2").as_deref(), Some("db2"));
        assert_eq!(r("SELECT x"), None);
        assert_eq!(r("GET k"), None);
        // What a switch writes reads back as the same schema.
        for (e, s) in [
            (Engine::Mysql, "we`ird"),
            (Engine::Oracle, "HR"),
            (Engine::Oracle, "mixed Case"),
            (Engine::Redis, "db7"),
        ] {
            let sql = switch_sql(e, s).expect("a switch");
            assert_eq!(switched_to(e, &sql).as_deref(), Some(s), "{sql}");
        }
    }
}
