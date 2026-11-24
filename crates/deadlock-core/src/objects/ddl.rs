//! Statements for each change: pure functions from the old definition and the
//! draft to a [`ChangePlan`]. The table "How each change is applied" in the design
//! is the spec: ALTER where the engine has it, `CREATE OR REPLACE` next, and on
//! MySQL routines and triggers check, drop and create with a restore on failure.

use super::parse::definer_parts;
use super::{
    ChangePlan, EventProps, EventStatus, Flavor, Grant, MatViewProps, ObjectDefinition,
    ObjectDraft, ObjectKind, ObjectRef, Param, ParamMode, Properties, RefreshMethod, RoutineProps,
    Schedule, Security, SequenceProps, SourceProps, Step, StepKind, SynonymProps, Timing,
    TriggerEvent, TriggerProps, ViewCheck, ViewProps,
};
use crate::edit::{quote_ident, quote_str};

type Res<T> = Result<T, String>;

/// Words Oracle reserves: names spelled like them are quoted.
const ORACLE_RESERVED: &[&str] = &[
    "ACCESS",
    "ADD",
    "ALL",
    "ALTER",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "AUDIT",
    "BETWEEN",
    "BY",
    "CHAR",
    "CHECK",
    "CLUSTER",
    "COLUMN",
    "COMMENT",
    "COMPRESS",
    "CONNECT",
    "CREATE",
    "CURRENT",
    "DATE",
    "DECIMAL",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "EXCLUSIVE",
    "EXISTS",
    "FILE",
    "FLOAT",
    "FOR",
    "FROM",
    "GRANT",
    "GROUP",
    "HAVING",
    "IDENTIFIED",
    "IMMEDIATE",
    "IN",
    "INCREMENT",
    "INDEX",
    "INITIAL",
    "INSERT",
    "INTEGER",
    "INTERSECT",
    "INTO",
    "IS",
    "LEVEL",
    "LIKE",
    "LOCK",
    "LONG",
    "MAXEXTENTS",
    "MINUS",
    "MLSLABEL",
    "MODE",
    "MODIFY",
    "NOAUDIT",
    "NOCOMPRESS",
    "NOT",
    "NOWAIT",
    "NULL",
    "NUMBER",
    "OF",
    "OFFLINE",
    "ON",
    "ONLINE",
    "OPTION",
    "OR",
    "ORDER",
    "PCTFREE",
    "PRIOR",
    "PUBLIC",
    "RAW",
    "RENAME",
    "RESOURCE",
    "REVOKE",
    "ROW",
    "ROWID",
    "ROWNUM",
    "ROWS",
    "SELECT",
    "SESSION",
    "SET",
    "SHARE",
    "SIZE",
    "SMALLINT",
    "START",
    "SUCCESSFUL",
    "SYNONYM",
    "SYSDATE",
    "TABLE",
    "THEN",
    "TO",
    "TRIGGER",
    "UID",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USER",
    "VALIDATE",
    "VALUES",
    "VARCHAR",
    "VARCHAR2",
    "VIEW",
    "WHENEVER",
    "WHERE",
    "WITH",
];

/// Characters of an Oracle name that needs no quotes (after the first letter).
fn plain(name: &str) -> bool {
    let mut c = name.chars();
    c.next().is_some_and(|f| f.is_ascii_alphabetic())
        && c.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '#'))
}

/// A name quoted for `flavor`: backticks on MySQL; on Oracle bare when it is an
/// upper-case plain name that is not reserved, else in double quotes (exact case).
pub fn quote_name(flavor: Flavor, name: &str) -> String {
    if flavor.is_mysql() {
        return quote_ident(name);
    }
    if plain(name) && name == name.to_ascii_uppercase() && !ORACLE_RESERVED.contains(&name) {
        name.to_owned()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// A name typed in the editor as the server will store it: on Oracle a plain name
/// is upper case (`calc_fee` → `CALC_FEE`), anything else stays exact. Applied to
/// new and changed names; names read from the server are never changed.
pub fn normalize_name(flavor: Flavor, typed: &str) -> String {
    let typed = typed.trim();
    if flavor == Flavor::Oracle && plain(typed) {
        typed.to_ascii_uppercase()
    } else {
        typed.to_owned()
    }
}

fn qualified(flavor: Flavor, schema: &str, name: &str) -> String {
    format!(
        "{}.{}",
        quote_name(flavor, schema),
        quote_name(flavor, name)
    )
}

fn qref(flavor: Flavor, o: &ObjectRef) -> String {
    if flavor == Flavor::Oracle && o.kind == ObjectKind::Synonym && o.schema == "PUBLIC" {
        return quote_name(flavor, &o.name);
    }
    qualified(flavor, &o.schema, &o.name)
}

/// A string literal: always escaping `\` on MySQL, which is safe in either
/// `NO_BACKSLASH_ESCAPES` mode (at worst a backslash doubles).
fn lit(flavor: Flavor, s: &str) -> String {
    quote_str(s, flavor.is_mysql())
}

/// A typed fragment (a data type, a default, an expression): trimmed, and refused
/// when it holds a `;` or a comment outside quotes, or unbalanced quotes or
/// parentheses, so it cannot end the statement it goes into.
fn fragment<'a>(flavor: Flavor, label: &str, text: &'a str) -> Res<&'a str> {
    let text = text.trim();
    let b = text.as_bytes();
    let (mut quote, mut depth) = (None::<u8>, 0i32);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match quote {
            Some(q) if c == q => {
                if b.get(i + 1) == Some(&q) {
                    i += 1;
                } else {
                    quote = None;
                }
            }
            Some(_) => {}
            None => match c {
                b'\'' | b'"' | b'`' => quote = Some(c),
                b'(' => depth += 1,
                b')' => depth -= 1,
                b';' => return Err(format!("{label} cannot hold a ;")),
                b'-' if b.get(i + 1) == Some(&b'-') => {
                    return Err(format!("{label} cannot hold a comment"));
                }
                b'/' if b.get(i + 1) == Some(&b'*') => {
                    return Err(format!("{label} cannot hold a comment"));
                }
                b'#' if flavor.is_mysql() => {
                    return Err(format!("{label} cannot hold a comment"));
                }
                _ => {}
            },
        }
        if depth < 0 {
            return Err(format!("{label} has a ) without its ("));
        }
        i += 1;
    }
    if quote.is_some() {
        return Err(format!("{label} has an unclosed quote"));
    }
    if depth != 0 {
        return Err(format!("{label} has a ( without its )"));
    }
    Ok(text)
}

/// A whole number, or `None` when empty.
fn integer<'a>(label: &str, text: &'a str) -> Res<Option<&'a str>> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let digits = t.strip_prefix('-').unwrap_or(t);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{label} must be a whole number"));
    }
    Ok(Some(t))
}

/// A body or query as it goes into the statement: trailing blanks off; MySQL and
/// plain SQL also lose a trailing `;`, PL/SQL a trailing `/`.
fn body_text(text: &str, plsql: bool) -> String {
    let mut t = text.trim_end();
    if plsql {
        if let Some(s) = t.strip_suffix('/')
            && (s.ends_with('\n') || s.is_empty())
        {
            t = s.trim_end();
        }
    } else {
        while let Some(s) = t.strip_suffix(';') {
            t = s.trim_end();
        }
    }
    t.to_owned()
}

/// MySQL `DEFINER=…` clause with a trailing space; empty for no definer.
fn definer(d: &str) -> String {
    let d = d.trim();
    if d.is_empty() {
        return String::new();
    }
    if d.eq_ignore_ascii_case("CURRENT_USER") || d.eq_ignore_ascii_case("CURRENT_USER()") {
        return "DEFINER=CURRENT_USER ".into();
    }
    let (user, host) = definer_parts(d);
    format!("DEFINER={}@{} ", quote_ident(user), quote_ident(host))
}

/// `_dl_check_<name>`, cut to MySQL's 64 characters.
fn check_name(name: &str) -> String {
    format!("_dl_check_{name}").chars().take(64).collect()
}

/// Labels of the fields that differ.
#[derive(Default)]
struct Changes(Vec<String>);

impl Changes {
    fn cmp<T: PartialEq + ?Sized>(&mut self, label: &str, a: &T, b: &T) {
        if a != b {
            self.0.push(label.to_owned());
        }
    }

    fn has(&self, label: &str) -> bool {
        self.0.iter().any(|c| c == label)
    }

    /// Every change is one of `labels`.
    fn only(&self, labels: &[&str]) -> bool {
        !self.0.is_empty() && self.0.iter().all(|c| labels.contains(&c.as_str()))
    }

    fn without(&self, labels: &[&str]) -> bool {
        self.0.iter().any(|c| !labels.contains(&c.as_str()))
    }
}

/// The grants as statements on `o`.
fn grant_sql(flavor: Flavor, o: &ObjectRef, grants: &[Grant]) -> Vec<String> {
    let what = match (flavor.is_mysql(), o.kind) {
        (true, ObjectKind::Procedure) => "PROCEDURE ",
        (true, ObjectKind::Function) => "FUNCTION ",
        _ => "",
    };
    grants
        .iter()
        .filter_map(|g| {
            let privs: Vec<&str> = g
                .privileges
                .iter()
                .map(String::as_str)
                .filter(|p| p.bytes().all(|b| b.is_ascii_alphabetic() || b == b' '))
                .collect();
            if privs.is_empty() {
                return None;
            }
            let to = if flavor.is_mysql() {
                format!("{}@{}", lit(flavor, &g.user), lit(flavor, &g.host))
            } else {
                quote_name(flavor, &g.user)
            };
            Some(format!(
                "GRANT {} ON {what}{} TO {to}{}",
                privs.join(", "),
                qref(flavor, o),
                if g.grant_option {
                    " WITH GRANT OPTION"
                } else {
                    ""
                }
            ))
        })
        .collect()
}

struct Builder {
    plan: ChangePlan,
}

impl Builder {
    fn new(flavor: Flavor, object: ObjectRef, previous: Option<ObjectRef>) -> Self {
        Self {
            plan: ChangePlan {
                flavor,
                object,
                previous,
                steps: Vec::new(),
                atomic: true,
                changes: Vec::new(),
                notices: Vec::new(),
                body: String::new(),
                source_lines: false,
            },
        }
    }

    fn step(&mut self, kind: StepKind, description: impl Into<String>, statements: Vec<String>) {
        if !statements.is_empty() {
            self.plan
                .steps
                .push(Step::new(kind, description, statements));
        }
    }

    fn done(mut self, changes: Changes) -> ChangePlan {
        let has = |k: StepKind| self.plan.steps.iter().any(|s| s.kind == k);
        self.plan.atomic = !has(StepKind::Swap) && !(has(StepKind::Create) && has(StepKind::Drop));
        self.plan.changes = changes.0;
        self.plan
    }
}

/// The plan that turns `old` into `new`, or creates `new` when `old` is `None`.
///
/// # Errors
/// The draft is incomplete (`To finish: name, table`) or holds something the
/// engine cannot do; the text says what, for the footer.
pub fn plan(flavor: Flavor, old: Option<&ObjectDefinition>, new: &ObjectDraft) -> Res<ChangePlan> {
    if let Some(o) = old
        && let Some(reason) = &o.read_only
    {
        return Err(reason.clone());
    }
    let missing = missing(flavor, new);
    if !missing.is_empty() {
        return Err(format!("To finish: {}.", missing.join(", ")));
    }
    let name = match old {
        Some(o) if o.object.name == new.object.name => new.object.name.clone(),
        _ => normalize_name(flavor, &new.object.name),
    };
    let mut target = ObjectRef {
        name,
        ..new.object.clone()
    };
    if let (Properties::Synonym(s), Flavor::Oracle) = (&new.props, flavor) {
        if s.public {
            target.schema = "PUBLIC".into();
        } else if target.schema == "PUBLIC" {
            return Err("pick the schema of a private synonym".into());
        }
    }
    let previous = old.map(|o| o.object.clone());
    match (&new.props, old.map(|o| &o.props)) {
        (Properties::View(n), None) => view(flavor, target, None, n),
        (Properties::View(n), Some(Properties::View(o))) => {
            view(flavor, target, Some((old.expect("old"), o)), n)
        }
        (Properties::MaterializedView(n), None) => matview(target, None, n),
        (Properties::MaterializedView(n), Some(Properties::MaterializedView(o))) => {
            matview(target, Some((old.expect("old"), o)), n)
        }
        (Properties::Routine(n), None) => routine(flavor, target, None, n),
        (Properties::Routine(n), Some(Properties::Routine(o))) => {
            routine(flavor, target, Some((old.expect("old"), o)), n)
        }
        (Properties::Trigger(n), None) => trigger(flavor, target, None, n),
        (Properties::Trigger(n), Some(Properties::Trigger(o))) => {
            trigger(flavor, target, Some((old.expect("old"), o)), n)
        }
        (Properties::Event(n), None) => event(flavor, target, None, n),
        (Properties::Event(n), Some(Properties::Event(o))) => {
            event(flavor, target, Some((old.expect("old"), o)), n)
        }
        (Properties::Sequence(n), None) => sequence(flavor, target, None, n),
        (Properties::Sequence(n), Some(Properties::Sequence(o))) => {
            sequence(flavor, target, Some((old.expect("old"), o)), n)
        }
        (Properties::Synonym(n), None) => synonym(target, None, n),
        (Properties::Synonym(n), Some(Properties::Synonym(o))) => {
            synonym(target, Some((old.expect("old"), o)), n)
        }
        (Properties::Source(n), None) => source(target, None, n),
        (Properties::Source(n), Some(Properties::Source(o))) => {
            source(target, Some((old.expect("old"), o)), n)
        }
        _ => Err("the draft's kind of properties does not match the object's".into()),
    }
    .map(|mut p| {
        if p.previous.is_none() {
            p.previous = previous;
        }
        p
    })
}

/// What the draft still needs, as the form names the fields.
fn missing(flavor: Flavor, d: &ObjectDraft) -> Vec<&'static str> {
    let mut out = Vec::new();
    let sourced = matches!(d.props, Properties::Source(_));
    if d.object.name.trim().is_empty() && !sourced {
        out.push("name");
    }
    match &d.props {
        Properties::View(v) if v.query.trim().is_empty() => out.push("query"),
        Properties::MaterializedView(v) if v.query.trim().is_empty() => out.push("query"),
        Properties::Routine(r) => {
            if d.object.kind == ObjectKind::Function && r.returns.trim().is_empty() {
                out.push("return type");
            }
            if r.params.iter().any(|p| p.name.trim().is_empty()) {
                out.push("parameter names");
            }
            if r.params.iter().any(|p| p.data_type.trim().is_empty()) {
                out.push("parameter types");
            }
            if r.body.trim().is_empty() {
                out.push("body");
            }
        }
        Properties::Trigger(t) => {
            if t.table.trim().is_empty() {
                out.push("pick a table");
            }
            if t.events.is_empty() {
                out.push("event");
            }
            if t.body.trim().is_empty() {
                out.push("body");
            }
        }
        Properties::Event(e) => {
            if e.body.trim().is_empty() {
                out.push("body");
            }
            match &e.schedule {
                Schedule::At(at) if at.trim().is_empty() => out.push("at"),
                Schedule::Every { value, .. } if value.trim().is_empty() => out.push("every"),
                _ => {}
            }
        }
        Properties::Synonym(s) if s.target.trim().is_empty() => out.push("object"),
        Properties::Source(s) if s.spec.trim().is_empty() => out.push("source"),
        _ => {}
    }
    let _ = flavor;
    out
}

// Views.

fn view_sql(flavor: Flavor, t: &ObjectRef, v: &ViewProps, or_replace: bool) -> Res<String> {
    let query = body_text(&v.query, false);
    let replace = if or_replace { "OR REPLACE " } else { "" };
    if flavor.is_mysql() {
        let check = match v.check {
            ViewCheck::None => "",
            ViewCheck::Cascaded => "\nWITH CASCADED CHECK OPTION",
            ViewCheck::Local => "\nWITH LOCAL CHECK OPTION",
            ViewCheck::ReadOnly => return Err("MySQL views have no READ ONLY option".into()),
        };
        return Ok(format!(
            "CREATE {replace}ALGORITHM={} {}SQL SECURITY {} VIEW {} AS\n{query}{check}",
            v.algorithm.sql(),
            definer(&v.definer),
            v.security.sql(),
            qref(flavor, t)
        ));
    }
    let check = match v.check {
        ViewCheck::None => "",
        ViewCheck::Cascaded => "\nWITH CHECK OPTION",
        ViewCheck::ReadOnly => "\nWITH READ ONLY",
        ViewCheck::Local => return Err("Oracle views have no LOCAL check option".into()),
    };
    let columns = fragment(flavor, "Columns", &v.columns)?;
    Ok(format!(
        "CREATE {replace}{}VIEW {}{} AS\n{query}{check}",
        if v.force { "FORCE " } else { "" },
        qref(flavor, t),
        if columns.is_empty() {
            String::new()
        } else {
            format!(" ({columns})")
        }
    ))
}

fn view(
    flavor: Flavor,
    t: ObjectRef,
    old: Option<(&ObjectDefinition, &ViewProps)>,
    n: &ViewProps,
) -> Res<ChangePlan> {
    let mut b = Builder::new(flavor, t.clone(), None);
    b.plan.body = n.query.clone();
    let Some((def, o)) = old else {
        b.step(StepKind::Create, "", vec![view_sql(flavor, &t, n, false)?]);
        return Ok(b.done(Changes::default()));
    };
    let mut ch = Changes::default();
    ch.cmp("Name", &def.object.name, &t.name);
    ch.cmp("Algorithm", &o.algorithm, &n.algorithm);
    ch.cmp("Definer", &o.definer, &n.definer);
    ch.cmp("SQL security", &o.security, &n.security);
    ch.cmp("Check option", &o.check, &n.check);
    ch.cmp("Force", &o.force, &n.force);
    ch.cmp("Columns", &o.columns, &n.columns);
    ch.cmp("Query", &o.query, &n.query);
    if ch.has("Name") {
        rename_steps(&mut b, def, &t)?;
    }
    if ch.without(&["Name"]) {
        b.step(
            StepKind::Replace,
            format!("Replace {} in one statement: grants are kept", t.name),
            vec![view_sql(flavor, &t, n, true)?],
        );
    }
    Ok(b.done(ch))
}

/// Steps that give `def` the name in `t`: a rename where the engine has one, else
/// create under the new name, then drop the old (and grant again).
fn rename_steps(b: &mut Builder, def: &ObjectDefinition, t: &ObjectRef) -> Res<()> {
    let flavor = b.plan.flavor;
    let o = &def.object;
    let renamable = o.kind.can_rename(flavor).is_ok()
        && (flavor != Flavor::Oracle || def.own_schema && o.schema != "PUBLIC");
    if !renamable {
        return Err(format!(
            "{} cannot be renamed here: {}",
            o.name,
            o.kind
                .can_rename(flavor)
                .err()
                .unwrap_or("Oracle renames only objects in your own schema")
        ));
    }
    b.step(
        StepKind::Rename,
        format!("Rename {} to {}", o.name, t.name),
        vec![rename_sql(flavor, o, &t.name)],
    );
    Ok(())
}

fn rename_sql(flavor: Flavor, o: &ObjectRef, to: &str) -> String {
    let to_q = qualified(flavor, &o.schema, to);
    match (flavor, o.kind) {
        (Flavor::Oracle, ObjectKind::Trigger) => {
            format!(
                "ALTER TRIGGER {} RENAME TO {}",
                qref(flavor, o),
                quote_name(flavor, to)
            )
        }
        (Flavor::Oracle, _) => format!(
            "RENAME {} TO {}",
            quote_name(flavor, &o.name),
            quote_name(flavor, to)
        ),
        (_, ObjectKind::Event) => format!("ALTER EVENT {} RENAME TO {to_q}", qref(flavor, o)),
        _ => format!("RENAME TABLE {} TO {to_q}", qref(flavor, o)),
    }
}

// Materialized views.

fn refresh_clause(v: &MatViewProps) -> Res<String> {
    if v.method == RefreshMethod::Never {
        return Ok("NEVER REFRESH".into());
    }
    let mut s = format!("REFRESH {} {}", v.method.sql(), v.mode.sql());
    let start = fragment(Flavor::Oracle, "Start with", &v.start_with)?;
    if !start.is_empty() {
        s.push_str(&format!(" START WITH {start}"));
    }
    let next = fragment(Flavor::Oracle, "Next", &v.next)?;
    if !next.is_empty() {
        s.push_str(&format!(" NEXT {next}"));
    }
    Ok(s)
}

fn matview_sql(t: &ObjectRef, v: &MatViewProps) -> Res<String> {
    Ok(format!(
        "CREATE MATERIALIZED VIEW {}\nBUILD {}\n{}\n{} QUERY REWRITE\nAS\n{}",
        qref(Flavor::Oracle, t),
        if v.build_deferred {
            "DEFERRED"
        } else {
            "IMMEDIATE"
        },
        refresh_clause(v)?,
        if v.query_rewrite { "ENABLE" } else { "DISABLE" },
        body_text(&v.query, false)
    ))
}

fn matview(
    t: ObjectRef,
    old: Option<(&ObjectDefinition, &MatViewProps)>,
    n: &MatViewProps,
) -> Res<ChangePlan> {
    let f = Flavor::Oracle;
    let mut b = Builder::new(f, t.clone(), None);
    b.plan.body = n.query.clone();
    let Some((def, o)) = old else {
        b.step(StepKind::Create, "", vec![matview_sql(&t, n)?]);
        return Ok(b.done(Changes::default()));
    };
    let mut ch = Changes::default();
    ch.cmp("Name", &def.object.name, &t.name);
    ch.cmp("Refresh method", &o.method, &n.method);
    ch.cmp("Refresh mode", &o.mode, &n.mode);
    ch.cmp("Start with", &o.start_with, &n.start_with);
    ch.cmp("Next", &o.next, &n.next);
    ch.cmp("Query rewrite", &o.query_rewrite, &n.query_rewrite);
    ch.cmp("Query", &o.query, &n.query);
    if ch.has("Name") {
        return Err("Oracle cannot rename a materialized view".into());
    }
    let q = qref(f, &t);
    if ch.has("Query") {
        b.step(
            StepKind::Swap,
            format!("Drop {} and create it again (rebuilds its data)", t.name),
            vec![format!("DROP MATERIALIZED VIEW {q}"), matview_sql(&t, n)?],
        );
        b.step(
            StepKind::Grants,
            format!("Grant again what {} had", t.name),
            grant_sql(f, &t, &def.grants),
        );
        let n_steps = b.plan.steps.len();
        b.step(
            StepKind::Restore,
            "Only if the CREATE fails: restore the original (held in the app until done)",
            def.restore.clone(),
        );
        if b.plan.steps.len() == n_steps {
            return Err("the original definition is not known, so it could not be restored".into());
        }
        b.step(
            StepKind::RestoreGrants,
            "After a restore: grant again",
            grant_sql(f, &def.object, &def.grants),
        );
        return Ok(b.done(ch));
    }
    if ch.has("Refresh method") || ch.has("Refresh mode") || ch.has("Start with") || ch.has("Next")
    {
        b.step(
            StepKind::Alter,
            "Change the refresh options",
            vec![format!(
                "ALTER MATERIALIZED VIEW {q}\n{}",
                refresh_clause(n)?
            )],
        );
    }
    if ch.has("Query rewrite") {
        b.step(
            StepKind::Alter,
            "Change query rewrite",
            vec![format!(
                "ALTER MATERIALIZED VIEW {q} {} QUERY REWRITE",
                if n.query_rewrite { "ENABLE" } else { "DISABLE" }
            )],
        );
    }
    Ok(b.done(ch))
}

// Procedures and functions.

fn mysql_routine_sql(
    flavor: Flavor,
    kind: ObjectKind,
    t: &ObjectRef,
    r: &RoutineProps,
    or_replace: bool,
) -> Res<String> {
    let function = kind == ObjectKind::Function;
    let mut params = Vec::new();
    for p in &r.params {
        if function && p.mode != ParamMode::In {
            return Err("function parameters are IN only".into());
        }
        let ty = fragment(flavor, "A parameter type", &p.data_type)?;
        let mode = if function {
            String::new()
        } else {
            format!("{} ", p.mode.sql())
        };
        params.push(format!("  {mode}{} {ty}", quote_ident(p.name.trim())));
    }
    let params = if params.is_empty() {
        "()".to_owned()
    } else {
        format!("(\n{}\n)", params.join(",\n"))
    };
    let mut s = format!(
        "CREATE {}{}{} {}{params}\n",
        if or_replace { "OR REPLACE " } else { "" },
        definer(&r.definer),
        kind.keyword(),
        qref(flavor, t)
    );
    if function {
        s.push_str(&format!(
            "RETURNS {}\n",
            fragment(flavor, "Returns", &r.returns)?
        ));
    }
    if r.deterministic {
        s.push_str("DETERMINISTIC\n");
    }
    if let Some(d) = r.data_access {
        s.push_str(d.sql());
        s.push('\n');
    }
    s.push_str(&format!("SQL SECURITY {}\n", r.security.sql()));
    if !r.comment.is_empty() {
        s.push_str(&format!("COMMENT {}\n", lit(flavor, &r.comment)));
    }
    s.push_str(&body_text(&r.body, false));
    Ok(s)
}

fn oracle_param(p: &Param) -> Res<String> {
    let f = Flavor::Oracle;
    let ty = fragment(f, "A parameter type", &p.data_type)?;
    let mode = match p.mode {
        ParamMode::In => "IN",
        ParamMode::Out => "OUT",
        ParamMode::InOut => "IN OUT",
    };
    let mut s = format!(
        "  {} {mode}{} {ty}",
        quote_name(f, &normalize_name(f, &p.name)),
        if p.nocopy && p.mode != ParamMode::In {
            " NOCOPY"
        } else {
            ""
        }
    );
    let d = fragment(f, "A default", &p.default)?;
    if !d.is_empty() {
        if p.mode != ParamMode::In {
            return Err("only IN parameters take a default".into());
        }
        s.push_str(&format!(" DEFAULT {d}"));
    }
    Ok(s)
}

fn oracle_routine_sql(
    kind: ObjectKind,
    t: &ObjectRef,
    r: &RoutineProps,
    or_replace: bool,
) -> Res<String> {
    let f = Flavor::Oracle;
    let params: Vec<String> = r.params.iter().map(oracle_param).collect::<Res<_>>()?;
    let mut s = format!(
        "CREATE {}{} {}",
        if or_replace { "OR REPLACE " } else { "" },
        kind.keyword(),
        qref(f, t)
    );
    if !params.is_empty() {
        s.push_str(&format!("(\n{}\n)", params.join(",\n")));
    }
    if kind == ObjectKind::Function {
        s.push_str(&format!("\nRETURN {}", fragment(f, "Returns", &r.returns)?));
    }
    s.push_str(match r.security {
        Security::Definer => " AUTHID DEFINER",
        Security::Invoker => " AUTHID CURRENT_USER",
    });
    if kind == ObjectKind::Function {
        for (on, word) in [
            (r.deterministic, " DETERMINISTIC"),
            (r.parallel_enable, " PARALLEL_ENABLE"),
            (r.result_cache, " RESULT_CACHE"),
        ] {
            if on {
                s.push_str(word);
            }
        }
    }
    s.push_str("\nIS\n");
    s.push_str(&body_text(&r.body, true));
    Ok(s)
}

fn routine(
    flavor: Flavor,
    t: ObjectRef,
    old: Option<(&ObjectDefinition, &RoutineProps)>,
    n: &RoutineProps,
) -> Res<ChangePlan> {
    let kind = t.kind;
    let sql = |t: &ObjectRef, or_replace: bool| {
        if flavor.is_mysql() {
            mysql_routine_sql(flavor, kind, t, n, or_replace)
        } else {
            oracle_routine_sql(kind, t, n, or_replace)
        }
    };
    let mut b = Builder::new(flavor, t.clone(), None);
    b.plan.body = n.body.clone();
    let Some((def, o)) = old else {
        b.step(StepKind::Create, "", vec![sql(&t, false)?]);
        return Ok(b.done(Changes::default()));
    };
    let mut ch = Changes::default();
    ch.cmp("Name", &def.object.name, &t.name);
    ch.cmp("Parameters", &o.params, &n.params);
    ch.cmp("Returns", &o.returns, &n.returns);
    ch.cmp("Deterministic", &o.deterministic, &n.deterministic);
    ch.cmp("Data access", &o.data_access, &n.data_access);
    ch.cmp("SQL security", &o.security, &n.security);
    ch.cmp("Comment", &o.comment, &n.comment);
    ch.cmp("Definer", &o.definer, &n.definer);
    ch.cmp("Result cache", &o.result_cache, &n.result_cache);
    ch.cmp("Parallel enable", &o.parallel_enable, &n.parallel_enable);
    ch.cmp("Body", &o.body, &n.body);
    if ch.0.is_empty() {
        return Ok(b.done(ch));
    }
    let q = qref(flavor, &t);
    if flavor.is_mysql() && ch.only(&["Data access", "SQL security", "Comment"]) {
        let mut s = format!("ALTER {} {q}", kind.keyword());
        if ch.has("Data access")
            && let Some(d) = n.data_access
        {
            s.push_str(&format!("\n{}", d.sql()));
        }
        if ch.has("SQL security") {
            s.push_str(&format!("\nSQL SECURITY {}", n.security.sql()));
        }
        if ch.has("Comment") {
            s.push_str(&format!("\nCOMMENT {}", lit(flavor, &n.comment)));
        }
        b.step(StepKind::Alter, "", vec![s]);
        return Ok(b.done(ch));
    }
    if !ch.has("Name") && flavor != Flavor::Mysql {
        b.step(
            StepKind::Replace,
            format!(
                "{} replaces the {} in one statement: grants are kept",
                flavor.label(),
                kind.label()
            ),
            vec![sql(&t, true)?],
        );
        return Ok(b.done(ch));
    }
    if flavor != Flavor::Mysql {
        // A new name: create it, then drop the old one.
        b.step(
            StepKind::Create,
            format!("Create {}", t.name),
            vec![sql(&t, false)?],
        );
        b.step(
            StepKind::Drop,
            format!("Drop {}", def.object.name),
            vec![format!(
                "DROP {} {}",
                kind.keyword(),
                qref(flavor, &def.object)
            )],
        );
        b.step(
            StepKind::Grants,
            format!("Grant again what {} had", def.object.name),
            grant_sql(flavor, &t, &def.grants),
        );
        return Ok(b.done(ch));
    }
    // MySQL: check under a temporary name, drop and create, restore on failure.
    let tmp = ObjectRef {
        name: check_name(&t.name),
        ..t.clone()
    };
    let mut check = Step::new(
        StepKind::Check,
        "Check the new definition under a temporary name",
        vec![sql(&tmp, false)?],
    );
    check.cleanup = vec![format!("DROP {} {}", kind.keyword(), qref(flavor, &tmp))];
    b.plan.steps.push(check);
    swap(&mut b, def, &t, sql(&t, false)?)?;
    b.plan.notices.extend(def.notices.iter().cloned());
    Ok(b.done(ch))
}

/// MySQL's replace: drop and create, grant again, and the restore steps.
fn swap(b: &mut Builder, def: &ObjectDefinition, t: &ObjectRef, create: String) -> Res<()> {
    let flavor = b.plan.flavor;
    let o = &def.object;
    let steps = b.plan.steps.len() + 1;
    let what = if o.name == t.name {
        format!("Drop {} and create it again", o.name)
    } else {
        format!("Drop {} and create {}", o.name, t.name)
    };
    b.step(
        StepKind::Swap,
        what,
        vec![
            format!("DROP {} {}", o.kind.keyword(), qref(flavor, o)),
            create,
        ],
    );
    b.step(
        StepKind::Grants,
        format!("Grant again what {} had (DROP removed it)", o.name),
        grant_sql(flavor, t, &def.grants),
    );
    if def.restore.is_empty() {
        return Err("the original definition is not known, so it could not be restored".into());
    }
    b.step(
        StepKind::Restore,
        format!(
            "Only if step {steps}'s CREATE fails: restore the original (held in the app until done)"
        ),
        def.restore.clone(),
    );
    b.step(
        StepKind::RestoreGrants,
        "After a restore: grant again",
        grant_sql(flavor, o, &def.grants),
    );
    Ok(())
}

// Triggers.

fn mysql_trigger_sql(
    flavor: Flavor,
    t: &ObjectRef,
    table: &str,
    tr: &TriggerProps,
    or_replace: bool,
    order: bool,
) -> Res<String> {
    let [event] = tr.events.as_slice() else {
        return Err("a MySQL trigger has exactly one event".into());
    };
    if tr.timing == Timing::InsteadOf {
        return Err("MySQL triggers fire BEFORE or AFTER".into());
    }
    let order = match (&tr.order, order) {
        (Some((kind, other)), true) if !other.trim().is_empty() => {
            format!(" {} {}", kind.sql(), quote_ident(other.trim()))
        }
        _ => String::new(),
    };
    Ok(format!(
        "CREATE {}{}TRIGGER {}\n{} {} ON {} FOR EACH ROW{order}\n{}",
        if or_replace { "OR REPLACE " } else { "" },
        definer(&tr.definer),
        qref(flavor, t),
        tr.timing.sql(),
        event.sql(),
        qualified(flavor, &t.schema, table),
        body_text(&tr.body, false)
    ))
}

fn oracle_trigger_sql(
    t: &ObjectRef,
    tr: &TriggerProps,
    old_columns: &[String],
    or_replace: bool,
) -> Res<String> {
    let f = Flavor::Oracle;
    let mut events = Vec::new();
    for e in &tr.events {
        if *e == TriggerEvent::Update && !tr.update_columns.is_empty() {
            let cols: Vec<String> = tr
                .update_columns
                .iter()
                .map(|c| {
                    if old_columns.contains(c) {
                        quote_name(f, c)
                    } else {
                        quote_name(f, &normalize_name(f, c))
                    }
                })
                .collect();
            events.push(format!("UPDATE OF {}", cols.join(", ")));
        } else {
            events.push(e.sql().to_owned());
        }
    }
    let table_schema = if tr.table_schema.is_empty() {
        &t.schema
    } else {
        &tr.table_schema
    };
    let table = qualified(f, table_schema, &normalize_name(f, &tr.table));
    let mut s = format!(
        "CREATE {}TRIGGER {}\n{} {} ON {table}\n",
        if or_replace { "OR REPLACE " } else { "" },
        qref(f, t),
        tr.timing.sql(),
        events.join(" OR ")
    );
    if tr.for_each_row || tr.timing == Timing::InsteadOf {
        s.push_str("FOR EACH ROW\n");
    }
    if let Some((kind, other)) = &tr.order
        && !other.trim().is_empty()
    {
        s.push_str(&format!(
            "{} {}\n",
            kind.sql(),
            qualified(f, &t.schema, &normalize_name(f, other))
        ));
    }
    if !tr.enabled {
        s.push_str("DISABLE\n");
    }
    let when = fragment(f, "When", &tr.when)?;
    if !when.is_empty() {
        if !tr.for_each_row {
            return Err("WHEN works only on row-level triggers (FOR EACH ROW)".into());
        }
        s.push_str(&format!("WHEN ({when})\n"));
    }
    s.push_str(&body_text(&tr.body, true));
    Ok(s)
}

fn trigger(
    flavor: Flavor,
    t: ObjectRef,
    old: Option<(&ObjectDefinition, &TriggerProps)>,
    n: &TriggerProps,
) -> Res<ChangePlan> {
    let mut b = Builder::new(flavor, t.clone(), None);
    b.plan.body = n.body.clone();
    let old_columns = old
        .map(|(_, o)| o.update_columns.clone())
        .unwrap_or_default();
    let sql = |t: &ObjectRef, or_replace: bool| {
        if flavor.is_mysql() {
            mysql_trigger_sql(flavor, t, n.table.trim(), n, or_replace, true)
        } else {
            oracle_trigger_sql(t, n, &old_columns, or_replace)
        }
    };
    let Some((def, o)) = old else {
        b.step(StepKind::Create, "", vec![sql(&t, false)?]);
        return Ok(b.done(Changes::default()));
    };
    let mut ch = Changes::default();
    ch.cmp("Name", &def.object.name, &t.name);
    ch.cmp(
        "Table",
        &(&o.table, &o.table_schema),
        &(&n.table, &n.table_schema),
    );
    ch.cmp("Timing", &o.timing, &n.timing);
    ch.cmp("Event", &o.events, &n.events);
    ch.cmp("Update of", &o.update_columns, &n.update_columns);
    ch.cmp("For each row", &o.for_each_row, &n.for_each_row);
    ch.cmp("When", &o.when, &n.when);
    ch.cmp("Order", &o.order, &n.order);
    ch.cmp("Definer", &o.definer, &n.definer);
    ch.cmp("Enabled", &o.enabled, &n.enabled);
    ch.cmp("Body", &o.body, &n.body);
    if ch.0.is_empty() {
        return Ok(b.done(ch));
    }
    match flavor {
        Flavor::Oracle => {
            if ch.has("Name") {
                rename_steps(&mut b, def, &t)?;
            }
            if ch.without(&["Name", "Enabled"]) {
                b.step(
                    StepKind::Replace,
                    "Oracle replaces the trigger in one statement: no moment without it",
                    vec![sql(&t, true)?],
                );
            } else if ch.has("Enabled") {
                b.step(
                    StepKind::Alter,
                    "",
                    vec![format!(
                        "ALTER TRIGGER {} {}",
                        qref(flavor, &t),
                        if n.enabled { "ENABLE" } else { "DISABLE" }
                    )],
                );
            }
        }
        Flavor::Mariadb if !ch.has("Name") => {
            b.step(
                StepKind::Replace,
                "MariaDB replaces the trigger in one statement: no moment without it",
                vec![sql(&t, true)?],
            );
        }
        Flavor::Mariadb => {
            b.step(
                StepKind::Create,
                format!("Create {}", t.name),
                vec![sql(&t, false)?],
            );
            b.step(
                StepKind::Drop,
                format!("Drop {}", def.object.name),
                vec![format!("DROP TRIGGER {}", qref(flavor, &def.object))],
            );
            b.plan.notices.push(format!(
                "Between the two statements both {} and {} exist and fire.",
                t.name, def.object.name
            ));
        }
        Flavor::Mysql => {
            // Check on an empty copy of the table, so the check never fires on data.
            let table = n.table.trim();
            let copy = check_name(table);
            let tmp = ObjectRef {
                name: check_name(&t.name),
                ..t.clone()
            };
            let mut check = Step::new(
                StepKind::Check,
                format!("Check the new definition on an empty copy of {table} (nothing fires)"),
                vec![
                    format!(
                        "CREATE TABLE {} LIKE {}",
                        qualified(flavor, &t.schema, &copy),
                        qualified(flavor, &t.schema, table)
                    ),
                    mysql_trigger_sql(flavor, &tmp, &copy, n, false, false)?,
                ],
            );
            check.cleanup = vec![format!(
                "DROP TABLE {}",
                qualified(flavor, &t.schema, &copy)
            )];
            check.skippable = true;
            b.plan.steps.push(check);
            swap(&mut b, def, &t, sql(&t, false)?)?;
        }
    }
    Ok(b.done(ch))
}

// Events.

fn timestamp(label: &str, s: &str) -> Res<String> {
    let s = fragment(Flavor::Mysql, label, s)?;
    Ok(quote_str(s, true))
}

fn schedule_sql(s: &Schedule) -> Res<String> {
    Ok(match s {
        Schedule::At(at) => format!("AT {}", timestamp("At", at)?),
        Schedule::Every {
            value,
            unit,
            starts,
            ends,
        } => {
            let v = fragment(Flavor::Mysql, "Every", value)?;
            let v = if v.bytes().all(|b| b.is_ascii_digit()) {
                v.to_owned()
            } else {
                quote_str(v, true)
            };
            let mut out = format!("EVERY {v} {}", unit.sql());
            if !starts.trim().is_empty() {
                out.push_str(&format!(" STARTS {}", timestamp("Starts", starts)?));
            }
            if !ends.trim().is_empty() {
                out.push_str(&format!(" ENDS {}", timestamp("Ends", ends)?));
            }
            out
        }
    })
}

fn status_sql(flavor: Flavor, s: EventStatus) -> &'static str {
    match (flavor, s) {
        (Flavor::Mariadb, EventStatus::DisableOnReplica) => "DISABLE ON SLAVE",
        (_, s) => s.sql(),
    }
}

fn completion(preserve: bool) -> &'static str {
    if preserve {
        "ON COMPLETION PRESERVE"
    } else {
        "ON COMPLETION NOT PRESERVE"
    }
}

fn event(
    flavor: Flavor,
    t: ObjectRef,
    old: Option<(&ObjectDefinition, &EventProps)>,
    n: &EventProps,
) -> Res<ChangePlan> {
    if !flavor.is_mysql() {
        return Err("events are MySQL and MariaDB objects".into());
    }
    let mut b = Builder::new(flavor, t.clone(), None);
    b.plan.body = n.body.clone();
    let body = body_text(&n.body, false);
    let Some((def, o)) = old else {
        let mut s = format!(
            "CREATE {}EVENT {}\nON SCHEDULE {}\n{}\n{}\n",
            definer(&n.definer),
            qref(flavor, &t),
            schedule_sql(&n.schedule)?,
            completion(n.preserve),
            status_sql(flavor, n.status)
        );
        if !n.comment.is_empty() {
            s.push_str(&format!("COMMENT {}\n", lit(flavor, &n.comment)));
        }
        s.push_str(&format!("DO {body}"));
        b.step(StepKind::Create, "", vec![s]);
        return Ok(b.done(Changes::default()));
    };
    let mut ch = Changes::default();
    ch.cmp("Name", &def.object.name, &t.name);
    ch.cmp("Schedule", &o.schedule, &n.schedule);
    ch.cmp("On completion", &o.preserve, &n.preserve);
    ch.cmp("Status", &o.status, &n.status);
    ch.cmp("Comment", &o.comment, &n.comment);
    ch.cmp("Definer", &o.definer, &n.definer);
    ch.cmp("Body", &o.body, &n.body);
    if ch.0.is_empty() {
        return Ok(b.done(ch));
    }
    let mut s = format!(
        "ALTER {}EVENT {}",
        if ch.has("Definer") {
            definer(&n.definer)
        } else {
            String::new()
        },
        qref(flavor, &def.object)
    );
    if ch.has("Schedule") {
        s.push_str(&format!("\nON SCHEDULE {}", schedule_sql(&n.schedule)?));
    }
    if ch.has("On completion") {
        s.push_str(&format!("\n{}", completion(n.preserve)));
    }
    if ch.has("Name") {
        s.push_str(&format!("\nRENAME TO {}", qref(flavor, &t)));
    }
    if ch.has("Status") {
        s.push_str(&format!("\n{}", status_sql(flavor, n.status)));
    }
    if ch.has("Comment") {
        s.push_str(&format!("\nCOMMENT {}", lit(flavor, &n.comment)));
    }
    if ch.has("Body") {
        s.push_str(&format!("\nDO {body}"));
    }
    b.step(StepKind::Alter, "", vec![s]);
    Ok(b.done(ch))
}

// Sequences.

fn sequence(
    flavor: Flavor,
    t: ObjectRef,
    old: Option<(&ObjectDefinition, &SequenceProps)>,
    n: &SequenceProps,
) -> Res<ChangePlan> {
    if flavor == Flavor::Mysql {
        return Err("MySQL has no sequences".into());
    }
    let oracle = flavor == Flavor::Oracle;
    let start = integer("Start", &n.start)?;
    let increment = integer("Increment", &n.increment)?;
    let min = integer("Min value", &n.min)?;
    let max = integer("Max value", &n.max)?;
    let cache = integer("Cache", &n.cache)?;
    let restart = integer("Restart with", &n.restart_with)?;
    let (no_min, no_max) = if oracle {
        ("NOMINVALUE", "NOMAXVALUE")
    } else {
        ("NO MINVALUE", "NO MAXVALUE")
    };
    let cache_sql = |c: Option<&str>| match c {
        None | Some("0") => "NOCACHE".to_owned(),
        Some(c) => format!("CACHE {c}"),
    };
    let mut b = Builder::new(flavor, t.clone(), None);
    let Some((def, o)) = old else {
        let mut s = format!("CREATE SEQUENCE {}", qref(flavor, &t));
        if let Some(v) = start {
            s.push_str(&format!(" START WITH {v}"));
        }
        if let Some(v) = increment {
            s.push_str(&format!(" INCREMENT BY {v}"));
        }
        if let Some(v) = min {
            s.push_str(&format!(" MINVALUE {v}"));
        }
        if let Some(v) = max {
            s.push_str(&format!(" MAXVALUE {v}"));
        }
        if cache.is_some() {
            s.push_str(&format!(" {}", cache_sql(cache)));
        }
        if n.cycle {
            s.push_str(" CYCLE");
        }
        if n.order && oracle {
            s.push_str(" ORDER");
        }
        b.step(StepKind::Create, "", vec![s]);
        return Ok(b.done(Changes::default()));
    };
    let mut ch = Changes::default();
    ch.cmp("Name", &def.object.name, &t.name);
    ch.cmp("Start", &o.start, &n.start);
    ch.cmp("Increment", &o.increment, &n.increment);
    ch.cmp("Min value", &o.min, &n.min);
    ch.cmp("Max value", &o.max, &n.max);
    ch.cmp("Cache", &o.cache, &n.cache);
    ch.cmp("Cycle", &o.cycle, &n.cycle);
    if oracle {
        ch.cmp("Order", &o.order, &n.order);
    }
    ch.cmp("Restart with", &o.restart_with, &n.restart_with);
    if ch.has("Start") && oracle {
        return Err("Start with is fixed after create on Oracle: use Restart with".into());
    }
    if ch.has("Name") {
        if oracle {
            rename_steps(&mut b, def, &t)?;
        } else {
            b.step(
                StepKind::Rename,
                format!("Rename {} to {}", def.object.name, t.name),
                vec![format!(
                    "RENAME TABLE {} TO {}",
                    qref(flavor, &def.object),
                    qref(flavor, &t)
                )],
            );
        }
    }
    let mut parts = Vec::new();
    if ch.has("Start")
        && let Some(v) = start
    {
        parts.push(format!("START WITH {v}"));
    }
    if ch.has("Increment") {
        parts.push(format!("INCREMENT BY {}", increment.unwrap_or("1")));
    }
    if ch.has("Min value") {
        parts.push(min.map_or(no_min.to_owned(), |v| format!("MINVALUE {v}")));
    }
    if ch.has("Max value") {
        parts.push(max.map_or(no_max.to_owned(), |v| format!("MAXVALUE {v}")));
    }
    if ch.has("Cache") {
        parts.push(cache_sql(cache));
    }
    if ch.has("Cycle") {
        parts.push(if n.cycle { "CYCLE" } else { "NOCYCLE" }.into());
    }
    if ch.has("Order") {
        parts.push(if n.order { "ORDER" } else { "NOORDER" }.into());
    }
    if let Some(v) = restart {
        parts.push(if oracle {
            format!("RESTART START WITH {v}")
        } else {
            format!("RESTART WITH {v}")
        });
    }
    if !parts.is_empty() {
        b.step(
            StepKind::Alter,
            "",
            vec![format!(
                "ALTER SEQUENCE {} {}",
                qref(flavor, &t),
                parts.join(" ")
            )],
        );
    }
    Ok(b.done(ch))
}

// Synonyms.

fn synonym_sql(t: &ObjectRef, s: &SynonymProps, or_replace: bool) -> Res<String> {
    let f = Flavor::Oracle;
    let target = normalize_name(f, &s.target);
    let mut to = if s.target_schema.trim().is_empty() {
        quote_name(f, &target)
    } else {
        qualified(f, &normalize_name(f, &s.target_schema), &target)
    };
    let link = s.db_link.trim();
    if !link.is_empty() {
        let ok = link.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && link
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '#' | '.' | '@'));
        if !ok {
            return Err("DB link must be a plain name like FIN_LINK or FIN.EXAMPLE.COM".into());
        }
        to.push('@');
        to.push_str(link);
    }
    Ok(format!(
        "CREATE {}{}SYNONYM {} FOR {to}",
        if or_replace { "OR REPLACE " } else { "" },
        if s.public { "PUBLIC " } else { "" },
        qref(f, t)
    ))
}

fn synonym(
    t: ObjectRef,
    old: Option<(&ObjectDefinition, &SynonymProps)>,
    n: &SynonymProps,
) -> Res<ChangePlan> {
    let f = Flavor::Oracle;
    let mut b = Builder::new(f, t.clone(), None);
    let Some((def, o)) = old else {
        b.step(StepKind::Create, "", vec![synonym_sql(&t, n, false)?]);
        return Ok(b.done(Changes::default()));
    };
    let mut ch = Changes::default();
    ch.cmp("Name", &def.object.name, &t.name);
    ch.cmp("Visibility", &o.public, &n.public);
    ch.cmp("Target schema", &o.target_schema, &n.target_schema);
    ch.cmp("Object", &o.target, &n.target);
    ch.cmp("DB link", &o.db_link, &n.db_link);
    if ch.0.is_empty() {
        return Ok(b.done(ch));
    }
    let renamable = !n.public && !o.public && def.own_schema;
    if ch.has("Visibility") || ch.has("Name") && !renamable {
        b.step(
            StepKind::Create,
            format!("Create {}", t.name),
            vec![synonym_sql(&t, n, false)?],
        );
        b.step(
            StepKind::Drop,
            format!("Drop {}", def.object.name),
            vec![drop_sql(f, &def.object)],
        );
        return Ok(b.done(ch));
    }
    if ch.has("Name") {
        rename_steps(&mut b, def, &t)?;
    }
    if ch.without(&["Name"]) {
        b.step(StepKind::Replace, "", vec![synonym_sql(&t, n, true)?]);
    }
    Ok(b.done(ch))
}

// Source-first objects.

/// The kind word a source-first text must start with, and the part's `ALL_ERRORS` type.
fn source_kind(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Package => "PACKAGE",
        ObjectKind::Type => "TYPE",
        ObjectKind::Trigger => "TRIGGER",
        ObjectKind::Procedure => "PROCEDURE",
        ObjectKind::Function => "FUNCTION",
        _ => "",
    }
}

fn source(
    t: ObjectRef,
    old: Option<(&ObjectDefinition, &SourceProps)>,
    n: &SourceProps,
) -> Res<ChangePlan> {
    let f = Flavor::Oracle;
    let want = source_kind(t.kind);
    if want.is_empty() {
        return Err(format!("a {} is not edited as source", t.kind.label()));
    }
    let spec = body_text(&n.spec, true);
    let body = body_text(&n.body, true);
    let (kind, is_body, name) = super::parse::source_name(&spec)
        .ok_or_else(|| format!("the source must start with {want} and a name"))?;
    if kind != want || is_body {
        return Err(format!("the source must start with {want} and a name"));
    }
    if !body.is_empty() {
        let has_body = matches!(t.kind, ObjectKind::Package | ObjectKind::Type);
        let ok = has_body
            && super::parse::source_name(&body)
                .is_some_and(|(k, b, nm)| k == want && b && nm == name);
        if !ok {
            return Err(format!("the body must start with {want} BODY {name}"));
        }
    }
    let target = ObjectRef { name, ..t };
    let mut b = Builder::new(f, target.clone(), None);
    b.plan.source_lines = true;
    b.plan.body = spec.clone();
    let q = qref(f, &target);
    let same = old.is_some_and(|(d, _)| d.object.name == target.name);
    let Some((def, o)) = old.filter(|_| same) else {
        if let Some((d, _)) = old {
            b.plan.notices.push(format!(
                "The source names {}: Apply creates {} and leaves {} as it is.",
                target.name, target.name, d.object.name
            ));
        }
        b.step(StepKind::Create, "", vec![format!("CREATE {spec}")]);
        if !body.is_empty() {
            b.step(StepKind::Create, "", vec![format!("CREATE {body}")]);
        }
        let mut ch = Changes::default();
        if old.is_some() {
            ch.0.push("Name".into());
        }
        return Ok(b.done(ch));
    };
    let mut ch = Changes::default();
    let spec_label = if matches!(target.kind, ObjectKind::Package | ObjectKind::Type) {
        "Spec"
    } else {
        "Source"
    };
    ch.cmp(spec_label, &body_text(&o.spec, true), &spec);
    ch.cmp("Body", &body_text(&o.body, true), &body);
    if ch.has(spec_label) {
        b.step(
            StepKind::Replace,
            "Oracle compiles on create: if it fails the new code is still saved, INVALID",
            vec![format!("CREATE OR REPLACE {spec}")],
        );
    }
    if ch.has("Body") {
        if body.is_empty() {
            b.step(
                StepKind::Drop,
                "Drop the body",
                vec![format!("DROP {want} BODY {q}")],
            );
        } else {
            b.step(
                StepKind::Replace,
                "",
                vec![format!("CREATE OR REPLACE {body}")],
            );
        }
    } else if ch.has(spec_label) && !body.is_empty() {
        b.step(
            StepKind::Compile,
            "Compile the body against the new spec",
            vec![format!("ALTER {want} {q} COMPILE BODY")],
        );
    }
    let _ = def;
    let mut plan = b.done(ch);
    // Spec and body never leave the object missing.
    plan.atomic = true;
    Ok(plan)
}

/// `DROP …` for `o`.
fn drop_sql(flavor: Flavor, o: &ObjectRef) -> String {
    if flavor == Flavor::Oracle && o.kind == ObjectKind::Synonym && o.schema == "PUBLIC" {
        return format!("DROP PUBLIC SYNONYM {}", quote_name(flavor, &o.name));
    }
    format!("DROP {} {}", o.kind.keyword(), qref(flavor, o))
}

/// Dropping `o` (a package goes with its body). Always ask first.
pub fn drop_plan(flavor: Flavor, o: &ObjectRef) -> ChangePlan {
    let mut b = Builder::new(flavor, o.clone(), Some(o.clone()));
    b.step(StepKind::Drop, "", vec![drop_sql(flavor, o)]);
    b.done(Changes::default())
}

/// The context menu's Rename of `def` to `to` (typed; normalized on Oracle).
///
/// # Errors
/// The engine cannot rename this object ([`ObjectKind::can_rename`]), or the name is empty.
pub fn rename_plan(def: &ObjectDefinition, to: &str) -> Res<ChangePlan> {
    let flavor = def.flavor;
    let to = normalize_name(flavor, to);
    if to.is_empty() {
        return Err("To finish: name.".into());
    }
    let t = ObjectRef {
        name: to,
        ..def.object.clone()
    };
    let mut b = Builder::new(flavor, t.clone(), Some(def.object.clone()));
    let mut ch = Changes::default();
    ch.cmp("Name", &def.object.name, &t.name);
    if !ch.0.is_empty() {
        rename_steps(&mut b, def, &t)?;
    }
    Ok(b.done(ch))
}

/// Oracle `ALTER … COMPILE`: the Recompile button.
///
/// # Errors
/// MySQL objects and Oracle sequences do not compile.
pub fn recompile_plan(flavor: Flavor, o: &ObjectRef) -> Res<ChangePlan> {
    if flavor != Flavor::Oracle || o.kind == ObjectKind::Sequence {
        return Err(format!(
            "{} {}s do not compile",
            flavor.label(),
            o.kind.label()
        ));
    }
    let sql = if o.kind == ObjectKind::Synonym && o.schema == "PUBLIC" {
        format!(
            "ALTER PUBLIC SYNONYM {} COMPILE",
            quote_name(flavor, &o.name)
        )
    } else {
        format!("ALTER {} {} COMPILE", o.kind.keyword(), qref(flavor, o))
    };
    let mut b = Builder::new(flavor, o.clone(), Some(o.clone()));
    b.step(StepKind::Compile, "", vec![sql]);
    Ok(b.done(Changes::default()))
}

/// Revert to last valid: puts `previous` (the definition held from before an Apply)
/// back over `current` (the definition read after it).
///
/// # Errors
/// As [`plan`].
pub fn revert_plan(current: &ObjectDefinition, previous: &ObjectDefinition) -> Res<ChangePlan> {
    let mut p = plan(previous.flavor, Some(current), &previous.draft())?;
    for s in &mut p.steps {
        if s.description.is_empty() || s.kind == StepKind::Replace {
            s.description = "Revert to the source held from before the last Apply".into();
        }
    }
    Ok(p)
}

/// A new object's starting draft in `schema`: the editor opens with it and Apply
/// reads Create. The name is empty (source-first kinds name it in the text).
pub fn template(flavor: Flavor, kind: ObjectKind, schema: &str) -> ObjectDraft {
    let oracle = flavor == Flavor::Oracle;
    let block = |inner: &str| {
        if oracle {
            format!("BEGIN\n  {inner}\nEND;")
        } else {
            format!("BEGIN\n  {inner}\nEND")
        }
    };
    let props = match kind {
        ObjectKind::View => Properties::View(ViewProps {
            query: if oracle {
                "SELECT 1 AS one FROM dual".into()
            } else {
                "SELECT 1 AS one".into()
            },
            ..ViewProps::default()
        }),
        ObjectKind::MaterializedView => Properties::MaterializedView(MatViewProps {
            query: "SELECT 1 AS one FROM dual".into(),
            ..MatViewProps::default()
        }),
        ObjectKind::Procedure => Properties::Routine(RoutineProps {
            body: block(if oracle { "NULL;" } else { "-- …" }),
            ..RoutineProps::default()
        }),
        ObjectKind::Function => Properties::Routine(RoutineProps {
            returns: if oracle { "NUMBER".into() } else { "INT".into() },
            deterministic: !oracle,
            data_access: (!oracle).then_some(super::DataAccess::NoSql),
            body: block("RETURN 0;"),
            ..RoutineProps::default()
        }),
        ObjectKind::Trigger => Properties::Trigger(TriggerProps {
            events: vec![TriggerEvent::Insert],
            for_each_row: true,
            enabled: true,
            body: block(if oracle {
                "NULL; -- :NEW.column := …;"
            } else {
                "-- SET NEW.column = …;"
            }),
            ..TriggerProps::default()
        }),
        ObjectKind::Event => Properties::Event(EventProps {
            body: block("-- …"),
            ..EventProps::default()
        }),
        ObjectKind::Sequence => Properties::Sequence(SequenceProps {
            start: "1".into(),
            increment: "1".into(),
            ..SequenceProps::default()
        }),
        ObjectKind::Synonym => Properties::Synonym(SynonymProps::default()),
        ObjectKind::Package => Properties::Source(SourceProps {
            spec: "PACKAGE NEW_PACKAGE AS\n  PROCEDURE run;\nEND NEW_PACKAGE;".into(),
            body: "PACKAGE BODY NEW_PACKAGE AS\n  PROCEDURE run IS\n  BEGIN\n    NULL;\n  END run;\nEND NEW_PACKAGE;".into(),
        }),
        ObjectKind::Type => Properties::Source(SourceProps {
            spec: "TYPE NEW_TYPE AS OBJECT (\n  ID NUMBER\n);".into(),
            body: String::new(),
        }),
    };
    ObjectDraft {
        object: ObjectRef::new(schema, "", kind),
        props,
    }
}
