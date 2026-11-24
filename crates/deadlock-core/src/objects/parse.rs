//! Reading what servers report back: Oracle source headers and DDL into typed
//! properties, error texts into codes and lines, MySQL grant sets.

use std::ops::Range;

use super::{Param, ParamMode, RoutineProps, Security, ViewAlgorithm, ViewCheck, ViewProps};
use crate::connection::Engine;

/// One token of Oracle text: its text and byte range.
#[derive(Debug, Clone, Copy)]
struct Tok<'a> {
    text: &'a str,
    at: usize,
    end: usize,
    /// Parenthesis depth the token is at (a `(` is at the outer depth).
    depth: usize,
}

impl Tok<'_> {
    fn is(&self, word: &str) -> bool {
        self.text.eq_ignore_ascii_case(word)
    }
}

fn toks(text: &str) -> Vec<Tok<'_>> {
    let mut depth = 0usize;
    crate::split::tokens(text, Engine::Oracle)
        .into_iter()
        .map(|Range { start, end }| {
            let t = &text[start..end];
            if t == ")" {
                depth = depth.saturating_sub(1);
            }
            let tok = Tok {
                text: t,
                at: start,
                end,
                depth,
            };
            if t == "(" {
                depth += 1;
            }
            tok
        })
        .collect()
}

/// The index of the `)` closing the `(` at `open`.
fn closing(t: &[Tok<'_>], open: usize) -> Option<usize> {
    let d = t.get(open)?.depth;
    (open + 1..t.len()).find(|&i| t[i].text == ")" && t[i].depth == d)
}

/// An Oracle name token as the dictionary stores it: quoted exact, else upper case.
fn name_of(tok: &str) -> String {
    match tok.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Some(inner) => inner.replace("\"\"", "\""),
        None => tok.to_ascii_uppercase(),
    }
}

/// `at line 4` in a MySQL error message.
pub(crate) fn mysql_error_line(message: &str) -> Option<u32> {
    let rest = &message[message.rfind("at line ")? + 8..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The first `ORA-00942`, `PLS-00201`-style code in `text`.
pub(crate) fn oracle_code(text: &str) -> Option<String> {
    let b = text.as_bytes();
    (0..b.len().saturating_sub(8)).find_map(|i| {
        let w = &b[i..i + 9];
        (w[..3].iter().all(u8::is_ascii_uppercase)
            && w[3] == b'-'
            && w[4..].iter().all(u8::is_ascii_digit)
            && (i == 0 || !b[i - 1].is_ascii_alphanumeric()))
        .then(|| text[i..i + 9].to_owned())
    })
}

/// `ALGORITHM=MERGE` in a MySQL `SHOW CREATE VIEW`.
pub(crate) fn mysql_view_algorithm(create: &str) -> ViewAlgorithm {
    let upper = create.to_ascii_uppercase();
    upper
        .find("ALGORITHM=")
        .and_then(|i| {
            let rest = &upper[i + 10..];
            let word: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphabetic())
                .collect();
            ViewAlgorithm::parse(&word)
        })
        .unwrap_or_default()
}

/// A `mysql.procs_priv` privilege set (`Execute,Alter Routine,Grant`) as privileges
/// and the grant option.
pub(crate) fn proc_privs(set: &str) -> (Vec<String>, bool) {
    let mut grant = false;
    let privs = set
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .filter(|p| {
            let g = p.eq_ignore_ascii_case("grant");
            grant |= g;
            !g
        })
        .map(str::to_ascii_uppercase)
        .collect();
    (privs, grant)
}

/// A MySQL `user@host` definer as its parts; `@` splits at the last one.
pub(crate) fn definer_parts(definer: &str) -> (&str, &str) {
    definer.rsplit_once('@').unwrap_or((definer, "%"))
}

/// The kind word, `BODY` flag and name a source-first text creates: `PACKAGE BODY
/// "Pk"` → `("PACKAGE", true, "Pk")`. Skips `EDITIONABLE` and a schema.
pub(crate) fn source_name(text: &str) -> Option<(String, bool, String)> {
    let t = toks(text);
    let mut i = 0;
    while t
        .get(i)
        .is_some_and(|w| w.is("EDITIONABLE") || w.is("NONEDITIONABLE"))
    {
        i += 1;
    }
    let kind = t.get(i)?.text.to_ascii_uppercase();
    i += 1;
    let body = t.get(i).is_some_and(|w| w.is("BODY"));
    if body {
        i += 1;
    }
    let mut name = t.get(i)?.text;
    if t.get(i + 1).is_some_and(|w| w.text == ".") {
        name = t.get(i + 2)?.text;
    }
    Some((kind, body, name_of(name)))
}

/// An Oracle procedure's or function's source (`ALL_SOURCE`, starting at
/// `PROCEDURE`/`FUNCTION`) as properties; `None` when the header has anything the
/// form does not hold (pipelined, external, `ACCESSIBLE BY`, `RELIES_ON`…).
pub(crate) fn oracle_routine(source: &str) -> Option<RoutineProps> {
    let t = toks(source);
    let function = t.first()?.is("FUNCTION");
    if !function && !t.first()?.is("PROCEDURE") {
        return None;
    }
    let mut i = 2;
    if t.get(i).is_some_and(|w| w.text == ".") {
        i += 2;
    }
    let mut props = RoutineProps::default();
    if t.get(i).is_some_and(|w| w.text == "(") {
        let close = closing(&t, i)?;
        let mut start = i + 1;
        for j in i + 1..=close {
            if j == close || t[j].text == "," && t[j].depth == t[i].depth + 1 {
                props.params.push(oracle_param(source, &t[start..j])?);
                start = j + 1;
            }
        }
        i = close + 1;
    }
    let stop = |w: &Tok<'_>| {
        w.depth == 0
            && [
                "IS",
                "AS",
                "AUTHID",
                "DETERMINISTIC",
                "PARALLEL_ENABLE",
                "RESULT_CACHE",
                "PIPELINED",
                "AGGREGATE",
                "ACCESSIBLE",
                "SHARING",
                "DEFAULT",
                "USING",
            ]
            .iter()
            .any(|k| w.is(k))
    };
    loop {
        let w = t.get(i)?;
        if w.is("IS") || w.is("AS") {
            props.body = source[w.end..]
                .trim_start_matches([' ', '\t'])
                .trim_start_matches(['\r', '\n'])
                .trim_end()
                .to_owned();
            if props.body.is_empty() {
                return None;
            }
            return Some(props);
        } else if w.is("RETURN") && function {
            let from = i + 1;
            let mut j = from;
            while t.get(j).is_some_and(|w| !stop(w)) {
                j += 1;
            }
            let (a, b) = (t.get(from)?.at, t.get(j - 1)?.end);
            props.returns = source[a..b].to_owned();
            i = j;
        } else if w.is("AUTHID") {
            let who = t.get(i + 1)?;
            props.security = if who.is("CURRENT_USER") {
                Security::Invoker
            } else if who.is("DEFINER") {
                Security::Definer
            } else {
                return None;
            };
            i += 2;
        } else if w.is("DETERMINISTIC") {
            props.deterministic = true;
            i += 1;
        } else if w.is("PARALLEL_ENABLE") && t.get(i + 1).is_none_or(|n| n.text != "(") {
            props.parallel_enable = true;
            i += 1;
        } else if w.is("RESULT_CACHE") && t.get(i + 1).is_none_or(|n| !n.is("RELIES_ON")) {
            props.result_cache = true;
            i += 1;
        } else {
            return None;
        }
    }
}

/// `p_as_of IN DATE DEFAULT SYSDATE`.
fn oracle_param(source: &str, t: &[Tok<'_>]) -> Option<Param> {
    let mut p = Param {
        name: name_of(t.first()?.text),
        ..Param::default()
    };
    let mut i = 1;
    if t.get(i).is_some_and(|w| w.is("IN")) {
        i += 1;
        if t.get(i).is_some_and(|w| w.is("OUT")) {
            p.mode = ParamMode::InOut;
            i += 1;
        }
    } else if t.get(i).is_some_and(|w| w.is("OUT")) {
        p.mode = ParamMode::Out;
        i += 1;
    }
    if t.get(i).is_some_and(|w| w.is("NOCOPY")) {
        p.nocopy = true;
        i += 1;
    }
    let d = t[i..]
        .iter()
        .position(|w| w.is("DEFAULT") || w.text == ":")
        .map_or(t.len(), |d| d + i);
    p.data_type = source[t.get(i)?.at..t.get(d - 1)?.end].to_owned();
    if d < t.len() {
        let from = if t[d].text == ":" { d + 2 } else { d + 1 };
        p.default = source[t.get(from)?.at..t.last()?.end].to_owned();
    }
    Some(p)
}

/// An Oracle simple DML trigger's `WHEN` condition (without parentheses) and body
/// from its source; `None` when the header has `REFERENCING`, is compound or a call.
pub(crate) fn oracle_trigger(source: &str) -> Option<(String, String)> {
    let t = toks(source);
    let mut when = String::new();
    let mut i = 0;
    while let Some(w) = t.get(i) {
        if w.depth == 0 {
            if w.is("REFERENCING") || w.is("COMPOUND") || w.is("CALL") {
                return None;
            }
            if w.is("WHEN") {
                let open = i + 1;
                let close = closing(&t, open)?;
                when = source[t[open].end..t[close].at].trim().to_owned();
                i = close + 1;
                continue;
            }
            if w.is("DECLARE") || w.is("BEGIN") {
                return Some((when, source[w.at..].trim_end().to_owned()));
            }
        }
        i += 1;
    }
    None
}

/// An Oracle view's DDL (`DBMS_METADATA`) as properties: column list, query, check
/// option and `FORCE`.
pub(crate) fn oracle_view(ddl: &str) -> Option<ViewProps> {
    let t = toks(ddl);
    let v = t.iter().position(|w| w.is("VIEW"))?;
    let force = t[..v].iter().any(|w| w.is("FORCE"));
    let mut i = v + 2;
    if t.get(i).is_some_and(|w| w.text == ".") {
        i += 2;
    }
    let mut columns = String::new();
    if t.get(i).is_some_and(|w| w.text == "(") {
        let close = closing(&t, i)?;
        columns = ddl[t[i].end..t[close].at].trim().to_owned();
        i = close + 1;
    }
    let as_ = (i..t.len()).find(|&j| t[j].depth == 0 && t[j].is("AS"))?;
    let mut end = t.len();
    while end > as_ + 1 && t[end - 1].text == ";" {
        end -= 1;
    }
    let mut check = ViewCheck::None;
    // `WITH READ ONLY` / `WITH CHECK OPTION`, maybe `CONSTRAINT name` after.
    if let Some(w) = (as_ + 1..end).rev().find(|&j| {
        t[j].depth == 0
            && t[j].is("WITH")
            && (t.get(j + 1).is_some_and(|n| n.is("READ"))
                || t.get(j + 1).is_some_and(|n| n.is("CHECK")))
    }) {
        check = if t[w + 1].is("READ") {
            ViewCheck::ReadOnly
        } else {
            ViewCheck::Cascaded
        };
        end = w;
    }
    let query = ddl[t.get(as_ + 1)?.at..t.get(end - 1)?.end]
        .trim()
        .to_owned();
    Some(ViewProps {
        force,
        columns,
        check,
        query,
        ..ViewProps::default()
    })
}

/// An Oracle materialized view's DDL: its query, `START WITH` and `NEXT`.
pub(crate) fn oracle_matview(ddl: &str) -> Option<(String, String, String)> {
    let t = toks(ddl);
    let v = t.iter().position(|w| w.is("VIEW"))?;
    let as_ = (v..t.len()).find(|&j| t[j].depth == 0 && t[j].is("AS"))?;
    let mut end = t.len();
    while end > as_ + 1 && t[end - 1].text == ";" {
        end -= 1;
    }
    let query = ddl[t.get(as_ + 1)?.at..t.get(end - 1)?.end]
        .trim()
        .to_owned();
    let clause = |from: usize| -> String {
        let stops = [
            "NEXT", "WITH", "USING", "ENABLE", "DISABLE", "AS", "FOR", "ON", "BUILD", "REFRESH",
        ];
        let mut j = from;
        while j < as_ && !(t[j].depth == 0 && stops.iter().any(|s| t[j].is(s))) {
            j += 1;
        }
        if j == from {
            return String::new();
        }
        ddl[t[from].at..t[j - 1].end].trim().to_owned()
    };
    let mut start_with = String::new();
    let mut next = String::new();
    for j in v..as_ {
        if t[j].depth != 0 {
            continue;
        }
        if t[j].is("START") && t.get(j + 1).is_some_and(|w| w.is("WITH")) {
            start_with = clause(j + 2);
        } else if t[j].is("NEXT") {
            next = clause(j + 1);
        }
    }
    Some((query, start_with, next))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_texts_give_codes_and_lines() {
        assert_eq!(
            mysql_error_line("You have an error … near 'THN' at line 4"),
            Some(4)
        );
        assert_eq!(mysql_error_line("Lock wait timeout exceeded"), None);
        assert_eq!(
            oracle_code("PLS-00201: identifier 'X' must be declared").as_deref(),
            Some("PLS-00201")
        );
        assert_eq!(
            oracle_code("error: ORA-00942: table or view does not exist").as_deref(),
            Some("ORA-00942")
        );
        assert_eq!(oracle_code("nothing here"), None);
    }

    #[test]
    fn grant_sets_split_off_the_grant_option() {
        assert_eq!(
            proc_privs("Execute,Alter Routine,Grant"),
            (vec!["EXECUTE".into(), "ALTER ROUTINE".into()], true)
        );
        assert_eq!(proc_privs("Execute"), (vec!["EXECUTE".into()], false));
        assert_eq!(definer_parts("shop@admin@%"), ("shop@admin", "%"));
    }

    #[test]
    fn oracle_routine_headers_become_properties() {
        let src = "FUNCTION calc_late_fee(\n  p_invoice_id IN NUMBER,\n  p_as_of IN DATE DEFAULT SYSDATE,\n  p_days OUT NOCOPY PLS_INTEGER,\n  p_x IN OUT VARCHAR2 := 'a,b'\n) RETURN NUMBER AUTHID CURRENT_USER DETERMINISTIC RESULT_CACHE\nIS\n  v NUMBER;\nBEGIN\n  RETURN 1;\nEND;";
        let p = oracle_routine(src).unwrap();
        assert_eq!(p.params.len(), 4);
        assert_eq!(p.params[0].name, "P_INVOICE_ID");
        assert_eq!(p.params[0].data_type, "NUMBER");
        assert_eq!(p.params[1].default, "SYSDATE");
        assert_eq!(p.params[2].mode, ParamMode::Out);
        assert!(p.params[2].nocopy);
        assert_eq!(p.params[2].data_type, "PLS_INTEGER");
        assert_eq!(p.params[3].mode, ParamMode::InOut);
        assert_eq!(p.params[3].default, "'a,b'");
        assert_eq!(p.returns, "NUMBER");
        assert_eq!(p.security, Security::Invoker);
        assert!(p.deterministic && p.result_cache && !p.parallel_enable);
        assert!(p.body.starts_with("  v NUMBER;") && p.body.ends_with("END;"));
        let proc = oracle_routine("PROCEDURE p AS BEGIN NULL; END;").unwrap();
        assert!(proc.params.is_empty() && proc.body == "BEGIN NULL; END;");
        assert!(oracle_routine("FUNCTION f RETURN t PIPELINED IS BEGIN NULL; END;").is_none());
    }

    #[test]
    fn oracle_triggers_split_when_and_body() {
        let src = "TRIGGER t BEFORE UPDATE OF a ON x FOR EACH ROW\nWHEN (new.a > (old.a + 1))\nDECLARE v NUMBER; BEGIN NULL; END;";
        let (when, body) = oracle_trigger(src).unwrap();
        assert_eq!(when, "new.a > (old.a + 1)");
        assert_eq!(body, "DECLARE v NUMBER; BEGIN NULL; END;");
        assert!(oracle_trigger("TRIGGER t FOR INSERT ON x COMPOUND TRIGGER END;").is_none());
    }

    #[test]
    fn oracle_view_ddl_becomes_properties() {
        let ddl = "CREATE OR REPLACE FORCE EDITIONABLE VIEW \"S\".\"V\" (\"A\", \"B\") DEFAULT COLLATION \"USING_NLS_COMP\" AS \n  select a, b as x from t\n  WITH READ ONLY;";
        let v = oracle_view(ddl).unwrap();
        assert!(v.force);
        assert_eq!(v.columns, "\"A\", \"B\"");
        assert_eq!(v.query, "select a, b as x from t");
        assert_eq!(v.check, ViewCheck::ReadOnly);
        let mv = "CREATE MATERIALIZED VIEW \"S\".\"M\" (\"N\") BUILD IMMEDIATE REFRESH FORCE ON DEMAND START WITH sysdate+0 NEXT trunc(sysdate) + 1 USING DEFAULT LOCAL ROLLBACK SEGMENT DISABLE QUERY REWRITE AS select count(*) n from t;";
        let (q, start, next) = oracle_matview(mv).unwrap();
        assert_eq!(q, "select count(*) n from t");
        assert_eq!(start, "sysdate+0");
        assert_eq!(next, "trunc(sysdate) + 1");
    }

    #[test]
    fn source_names_are_read_like_the_dictionary() {
        assert_eq!(
            source_name("PACKAGE BODY \"Pk\" AS END;"),
            Some(("PACKAGE".into(), true, "Pk".into()))
        );
        assert_eq!(
            source_name("editionable type app.addr_t as object (x number);"),
            Some(("TYPE".into(), false, "ADDR_T".into()))
        );
        assert_eq!(
            mysql_view_algorithm("CREATE ALGORITHM=MERGE DEFINER=`a`@`%` VIEW"),
            ViewAlgorithm::Merge
        );
    }
}
