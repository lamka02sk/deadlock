//! The object editor on Oracle: definitions from the `ALL_*` views and
//! `DBMS_METADATA`, change plans run on the session, compile errors read after.

use super::structure::ddl;
use super::{Connection, col, texts};
use crate::driver::{BoxFuture, DriverError, Result};
use crate::edit::TableRef;
use crate::objects::{
    self, ApplyOutcome, Backup, ChangePlan, FinalState, Flavor, Grant, MatViewProps,
    ObjectDefinition, ObjectError, ObjectKind, ObjectRef, Properties, RefreshMethod, RefreshMode,
    SequenceProps, SourceProps, SynonymProps, Timing, TriggerEvent, TriggerOrder, TriggerProps,
    parse,
};

fn missing(o: &ObjectRef) -> DriverError {
    DriverError::NotFound(format!(
        "{} {}.{} does not exist, or this account cannot see it",
        o.kind.label(),
        o.schema,
        o.name
    ))
}

/// The stored source of one part (`PACKAGE BODY`…), lines joined; empty when none.
fn source_text(conn: &Connection, o: &ObjectRef, ty: &str) -> Result<String> {
    let rows = texts(
        conn,
        "SELECT TEXT FROM ALL_SOURCE WHERE OWNER = :1 AND NAME = :2 AND TYPE = :3 ORDER BY LINE",
        &[&o.schema, &o.name, &ty],
    )?;
    Ok(rows
        .iter()
        .map(|r| col(r, 0))
        .collect::<String>()
        // Oracle ends a trigger's source with a NUL once it rewrote it (after a rename
        // of its table); left in, the body differs from the one read before.
        .trim_end_matches(|c: char| c.is_whitespace() || c == '\0')
        .to_owned())
}

/// `DBMS_METADATA`'s DDL without its terminator.
fn metadata(conn: &Connection, kind: &str, o: &ObjectRef) -> Result<String> {
    let t = TableRef {
        database: o.schema.clone(),
        table: o.name.clone(),
    };
    let d = ddl(conn, kind, &t)?;
    Ok(d.trim_end().trim_end_matches(';').trim_end().to_owned())
}

/// The object's compile errors and warnings now.
pub(super) fn errors(conn: &Connection, o: &ObjectRef) -> Result<Vec<ObjectError>> {
    let rows = texts(
        conn,
        "SELECT TYPE, TO_CHAR(LINE), TO_CHAR(POSITION), TEXT, ATTRIBUTE FROM ALL_ERRORS \
         WHERE OWNER = :1 AND NAME = :2 ORDER BY TYPE, SEQUENCE",
        &[&o.schema, &o.name],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let text = col(r, 3).trim_end().to_owned();
            ObjectError {
                code: parse::oracle_code(&text).unwrap_or_default(),
                line: col(r, 1).parse().ok(),
                position: col(r, 2).parse().ok(),
                body_line: None,
                part: col(r, 0),
                warning: col(r, 4) == "WARNING",
                message: text,
            }
        })
        .collect())
}

pub(super) fn load(conn: &Connection, read_only: bool, o: &ObjectRef) -> Result<ObjectDefinition> {
    let kw = o.kind.keyword();
    let body_kw = format!("{kw} BODY");
    let status = texts(
        conn,
        "SELECT OBJECT_TYPE, STATUS, TO_CHAR(CREATED, 'YYYY-MM-DD HH24:MI:SS'), \
         TO_CHAR(LAST_DDL_TIME, 'YYYY-MM-DD HH24:MI:SS') FROM ALL_OBJECTS \
         WHERE OWNER = :1 AND OBJECT_NAME = :2 AND OBJECT_TYPE IN (:3, :4) ORDER BY OBJECT_TYPE",
        &[&o.schema, &o.name, &kw, &body_kw],
    )?;
    let Some(first) = status.first() else {
        return Err(missing(o));
    };
    let user = texts(conn, "SELECT USER FROM dual", &[])?;
    let mut def = ObjectDefinition {
        object: o.clone(),
        flavor: Flavor::Oracle,
        props: Properties::Source(SourceProps::default()),
        source: String::new(),
        restore: Vec::new(),
        read_only: read_only.then(|| "The connection is read-only.".to_owned()),
        status: status
            .iter()
            .map(|r| (col(r, 0), col(r, 1) == "VALID"))
            .collect(),
        errors: errors(conn, o)?,
        grants: Vec::new(),
        facts: vec![
            ("created".into(), col(first, 2)),
            ("last compiled".into(), col(first, 3)),
        ],
        siblings: Vec::new(),
        notices: Vec::new(),
        own_schema: user.first().is_some_and(|r| col(r, 0) == o.schema),
    };
    if o.kind != ObjectKind::Synonym && o.kind != ObjectKind::Trigger {
        let rows = texts(
            conn,
            "SELECT GRANTEE, PRIVILEGE, GRANTABLE FROM ALL_TAB_PRIVS \
             WHERE TABLE_SCHEMA = :1 AND TABLE_NAME = :2 ORDER BY GRANTEE, GRANTABLE, PRIVILEGE",
            &[&o.schema, &o.name],
        )?;
        for r in &rows {
            let (grantee, option) = (col(r, 0), col(r, 2) == "YES");
            match def.grants.last_mut() {
                Some(g) if g.user == grantee && g.grant_option == option => {
                    g.privileges.push(col(r, 1));
                }
                _ => def.grants.push(Grant {
                    user: grantee,
                    host: String::new(),
                    privileges: vec![col(r, 1)],
                    grant_option: option,
                }),
            }
        }
    }
    let create = |s: &str| format!("CREATE OR REPLACE {s}");
    match o.kind {
        ObjectKind::View => {
            let d = metadata(conn, "VIEW", o)?;
            def.props = Properties::View(parse::oracle_view(&d).ok_or_else(|| {
                DriverError::Invalid(format!("the DDL of view {} could not be read", o.name))
            })?);
            def.restore = vec![d.clone()];
            def.source = d;
        }
        ObjectKind::MaterializedView => {
            let rows = texts(
                conn,
                "SELECT REFRESH_METHOD, REFRESH_MODE, REWRITE_ENABLED, STALENESS, \
                 TO_CHAR(LAST_REFRESH_DATE, 'YYYY-MM-DD HH24:MI:SS'), LAST_REFRESH_TYPE \
                 FROM ALL_MVIEWS WHERE OWNER = :1 AND MVIEW_NAME = :2",
                &[&o.schema, &o.name],
            )?;
            let r = rows.first().ok_or_else(|| missing(o))?;
            let d = metadata(conn, "MATERIALIZED_VIEW", o)?;
            let (query, start_with, next) = parse::oracle_matview(&d).unwrap_or_default();
            def.props = Properties::MaterializedView(MatViewProps {
                method: RefreshMethod::parse(&col(r, 0)).unwrap_or_default(),
                mode: if col(r, 1) == "COMMIT" {
                    RefreshMode::OnCommit
                } else {
                    RefreshMode::OnDemand
                },
                start_with,
                next,
                query_rewrite: col(r, 2) == "Y",
                build_deferred: false,
                query,
            });
            def.facts
                .push(("staleness".into(), col(r, 3).to_ascii_lowercase()));
            def.facts.push(("last refresh".into(), col(r, 4)));
            def.facts.push(("last refresh type".into(), col(r, 5)));
            def.restore = vec![d.clone()];
            def.source = d;
        }
        ObjectKind::Procedure | ObjectKind::Function => {
            let src = source_text(conn, o, kw)?;
            if src.is_empty() {
                def.read_only.get_or_insert_with(|| {
                    format!(
                        "The source of {} is not visible to this account (ALL_SOURCE is empty).",
                        o.name
                    )
                });
            }
            def.props = match parse::oracle_routine(&src) {
                Some(p) => Properties::Routine(p),
                None => Properties::Source(SourceProps {
                    spec: src.clone(),
                    body: String::new(),
                }),
            };
            wrapped(&mut def, &src);
            def.restore = vec![create(&src)];
            def.source = create(&src);
        }
        ObjectKind::Package | ObjectKind::Type => {
            let spec = source_text(conn, o, kw)?;
            let body = source_text(conn, o, &body_kw)?;
            if spec.is_empty() {
                def.read_only.get_or_insert_with(|| {
                    format!(
                        "The source of {} is not visible to this account (ALL_SOURCE is empty).",
                        o.name
                    )
                });
            }
            wrapped(&mut def, &spec);
            wrapped(&mut def, &body);
            def.restore = [&spec, &body]
                .into_iter()
                .filter(|s| !s.is_empty())
                .map(|s| create(s))
                .collect();
            def.source = def
                .restore
                .iter()
                .map(|s| format!("{s}\n/\n"))
                .collect::<String>();
            if o.kind == ObjectKind::Type {
                type_summary(conn, o, &mut def)?;
            }
            def.props = Properties::Source(SourceProps { spec, body });
        }
        ObjectKind::Trigger => {
            let rows = texts(
                conn,
                "SELECT TRIGGER_TYPE, TRIGGERING_EVENT, TABLE_OWNER, BASE_OBJECT_TYPE, \
                 TABLE_NAME, STATUS, ACTION_TYPE FROM ALL_TRIGGERS \
                 WHERE OWNER = :1 AND TRIGGER_NAME = :2",
                &[&o.schema, &o.name],
            )?;
            let r = rows.first().ok_or_else(|| missing(o))?;
            let src = source_text(conn, o, "TRIGGER")?;
            let (ty, base) = (col(r, 0), col(r, 3));
            let enabled = col(r, 5) == "ENABLED";
            let table_owner = col(r, 2);
            def.facts.push((
                "on".into(),
                format!("{} ({})", col(r, 4), base.to_ascii_lowercase()),
            ));
            def.facts.push(("kind".into(), ty.to_ascii_lowercase()));
            let dml = matches!(base.as_str(), "TABLE" | "VIEW")
                && col(r, 6).trim() == "PL/SQL"
                && [
                    "BEFORE STATEMENT",
                    "BEFORE EACH ROW",
                    "AFTER STATEMENT",
                    "AFTER EACH ROW",
                    "INSTEAD OF",
                ]
                .contains(&ty.as_str());
            def.props = match parse::oracle_trigger(&src).filter(|_| dml) {
                Some((when, body)) => {
                    let columns = texts(
                        conn,
                        "SELECT COLUMN_NAME FROM ALL_TRIGGER_COLS WHERE TRIGGER_OWNER = :1 \
                         AND TRIGGER_NAME = :2 AND COLUMN_LIST = 'YES' ORDER BY COLUMN_NAME",
                        &[&o.schema, &o.name],
                    )?;
                    let order = texts(
                        conn,
                        "SELECT ORDERING_TYPE, REFERENCED_TRIGGER_NAME FROM ALL_TRIGGER_ORDERING \
                         WHERE TRIGGER_OWNER = :1 AND TRIGGER_NAME = :2",
                        &[&o.schema, &o.name],
                    )?;
                    Properties::Trigger(TriggerProps {
                        table: col(r, 4),
                        table_schema: if table_owner == o.schema {
                            String::new()
                        } else {
                            table_owner
                        },
                        timing: [Timing::InsteadOf, Timing::Before, Timing::After]
                            .into_iter()
                            .find(|t| ty.starts_with(t.sql()))
                            .unwrap_or_default(),
                        events: col(r, 1)
                            .split(" OR ")
                            .filter_map(TriggerEvent::parse)
                            .collect(),
                        update_columns: columns.iter().map(|c| col(c, 0)).collect(),
                        for_each_row: ty.contains("EACH ROW") || ty == "INSTEAD OF",
                        when,
                        order: order
                            .first()
                            .and_then(|r| Some((TriggerOrder::parse(&col(r, 0))?, col(r, 1)))),
                        definer: String::new(),
                        enabled,
                        body,
                    })
                }
                None => Properties::Source(SourceProps {
                    spec: src.clone(),
                    body: String::new(),
                }),
            };
            def.restore = vec![create(&src)];
            def.source = create(&src);
        }
        ObjectKind::Sequence => {
            let rows = texts(
                conn,
                "SELECT TO_CHAR(MIN_VALUE), TO_CHAR(MAX_VALUE), TO_CHAR(INCREMENT_BY), CYCLE_FLAG, \
                 ORDER_FLAG, TO_CHAR(CACHE_SIZE), TO_CHAR(LAST_NUMBER) FROM ALL_SEQUENCES \
                 WHERE SEQUENCE_OWNER = :1 AND SEQUENCE_NAME = :2",
                &[&o.schema, &o.name],
            )?;
            let r = rows.first().ok_or_else(|| missing(o))?;
            def.props = Properties::Sequence(SequenceProps {
                start: String::new(),
                increment: col(r, 2),
                min: col(r, 0),
                max: col(r, 1),
                cache: col(r, 5),
                cycle: col(r, 3) == "Y",
                order: col(r, 4) == "Y",
                restart_with: String::new(),
                current: col(r, 6),
            });
            let d = metadata(conn, "SEQUENCE", o)?;
            def.restore = vec![d.clone()];
            def.source = d;
        }
        ObjectKind::Synonym => {
            let rows = texts(
                conn,
                "SELECT TABLE_OWNER, TABLE_NAME, DB_LINK FROM ALL_SYNONYMS \
                 WHERE OWNER = :1 AND SYNONYM_NAME = :2",
                &[&o.schema, &o.name],
            )?;
            let r = rows.first().ok_or_else(|| missing(o))?;
            let s = SynonymProps {
                public: o.schema == "PUBLIC",
                target_schema: col(r, 0),
                target: col(r, 1),
                db_link: col(r, 2),
            };
            let resolves = if s.db_link.is_empty() {
                texts(
                    conn,
                    "SELECT OBJECT_TYPE FROM ALL_OBJECTS WHERE OWNER = :1 AND OBJECT_NAME = :2 \
                     AND SUBOBJECT_NAME IS NULL ORDER BY OBJECT_TYPE FETCH FIRST 1 ROWS ONLY",
                    &[&s.target_schema, &s.target],
                )?
                .first()
                .map_or_else(
                    || "nothing: the synonym is dangling".to_owned(),
                    |t| format!("{} {}.{}", col(t, 0), s.target_schema, s.target),
                )
            } else {
                format!("{}.{}@{}", s.target_schema, s.target, s.db_link)
            };
            def.facts.push(("resolves to".into(), resolves));
            def.props = Properties::Synonym(s);
            let d = metadata(conn, "SYNONYM", o)?;
            def.restore = vec![d.clone()];
            def.source = d;
        }
        ObjectKind::Event => {
            return Err(DriverError::Invalid(
                "Oracle has no events (DBMS_SCHEDULER jobs are not edited here)".into(),
            ));
        }
    }
    // Lines in the editor: source-first parts hold the whole source, a trigger's
    // errors count from its block, a routine's body starts after `IS`.
    let stored = def.restore.first().cloned().unwrap_or_default();
    for e in &mut def.errors {
        e.body_line = match &def.props {
            Properties::Source(_) | Properties::Trigger(_) => e.line,
            Properties::Routine(r) => e.line.and_then(|l| objects::body_line(&stored, &r.body, l)),
            _ => None,
        };
    }
    Ok(def)
}

/// Wrapped PL/SQL shows only as the obfuscated text: read only.
fn wrapped(def: &mut ObjectDefinition, src: &str) {
    let head = src.lines().next().unwrap_or_default().to_ascii_lowercase();
    if head.split_whitespace().any(|w| w == "wrapped") {
        def.read_only.get_or_insert_with(|| {
            format!(
                "{} is wrapped PL/SQL: its source cannot be shown.",
                def.object.name
            )
        });
    }
}

/// An object type's attributes and methods, as facts.
fn type_summary(conn: &Connection, o: &ObjectRef, def: &mut ObjectDefinition) -> Result<()> {
    let t = texts(
        conn,
        "SELECT FINAL, INSTANTIABLE, TYPECODE FROM ALL_TYPES WHERE OWNER = :1 AND TYPE_NAME = :2",
        &[&o.schema, &o.name],
    )?;
    if let Some(r) = t.first() {
        let flags = [
            if col(r, 0) == "YES" {
                "FINAL"
            } else {
                "NOT FINAL"
            },
            if col(r, 1) == "YES" {
                "INSTANTIABLE"
            } else {
                "NOT INSTANTIABLE"
            },
        ];
        def.facts.push(("typecode".into(), col(r, 2)));
        def.facts.push(("flags".into(), flags.join(", ")));
    }
    let attrs = texts(
        conn,
        "SELECT ATTR_NAME, ATTR_TYPE_NAME, TO_CHAR(LENGTH), TO_CHAR(PRECISION), TO_CHAR(SCALE) \
         FROM ALL_TYPE_ATTRS WHERE OWNER = :1 AND TYPE_NAME = :2 ORDER BY ATTR_NO",
        &[&o.schema, &o.name],
    )?;
    for r in &attrs {
        let (ty, len, p, s) = (col(r, 1), col(r, 2), col(r, 3), col(r, 4));
        let label = match (len.as_str(), p.as_str(), s.as_str()) {
            (l, "", _) if !l.is_empty() => format!("{ty}({l})"),
            (_, p, "" | "0") if !p.is_empty() => format!("{ty}({p})"),
            (_, p, s) if !p.is_empty() => format!("{ty}({p},{s})"),
            _ => ty,
        };
        def.facts.push((format!("attribute {}", col(r, 0)), label));
    }
    let methods = texts(
        conn,
        "SELECT METHOD_NAME, METHOD_TYPE FROM ALL_TYPE_METHODS WHERE OWNER = :1 AND TYPE_NAME = :2 \
         ORDER BY METHOD_NO",
        &[&o.schema, &o.name],
    )?;
    for r in &methods {
        def.facts.push((
            format!("method {}", col(r, 0)),
            col(r, 1).to_ascii_lowercase(),
        ));
    }
    Ok(())
}

/// Runs statements for [`objects::execute`]; compile errors do not fail a
/// statement (they are read after).
struct Runner<'c>(&'c Connection);

impl objects::Runner for Runner<'_> {
    fn run<'a>(&'a mut self, sql: &'a str) -> BoxFuture<'a, Result<()>> {
        let r = self.0.execute(sql, &[]).map(|_| ());
        Box::pin(std::future::ready(r))
    }

    /// ORA-01031 insufficient privileges.
    fn denied(&self, e: &DriverError) -> bool {
        super::ora_code(e) == Some(1031)
    }
}

/// Runs `plan`, then reads the compile errors of what it created. Call on a
/// blocking thread of the runtime.
pub(super) fn apply(
    conn: &Connection,
    plan: &ChangePlan,
    backup: Option<&Backup>,
) -> Result<ApplyOutcome> {
    let handle = tokio::runtime::Handle::current();
    let mut out = handle.block_on(objects::execute(plan, &mut Runner(conn), backup));
    if out.errors.iter().any(|e| e.code == "ORA-02303") {
        out.notices.push(
            "The type has dependents, so CREATE OR REPLACE TYPE cannot change it. Use ALTER \
             TYPE … ADD or DROP ATTRIBUTE … CASCADE for attributes; other spec changes need \
             the dependents dropped first."
                .into(),
        );
    }
    let ran = !matches!(
        out.state,
        FinalState::Unchanged | FinalState::Missing { .. }
    );
    let compiles = !matches!(plan.object.kind, ObjectKind::Sequence | ObjectKind::Synonym);
    let dropped = plan.steps.iter().all(|s| s.kind == objects::StepKind::Drop);
    if ran && compiles && !dropped {
        let statement = plan
            .steps
            .iter()
            .flat_map(|s| &s.statements)
            .find(|s| !plan.body.trim().is_empty() && s.contains(plan.body.trim()))
            .cloned()
            .unwrap_or_default();
        let mut found = errors(conn, &plan.object)?;
        // Trigger errors count lines from the trigger's PL/SQL block: the body.
        let block = plan.object.kind == ObjectKind::Trigger && !plan.source_lines;
        for e in &mut found {
            e.body_line = if plan.source_lines || block {
                e.line
            } else {
                e.line
                    .and_then(|l| objects::body_line(&statement, &plan.body, l))
            };
        }
        if found.iter().any(|e| !e.warning) && out.state == FinalState::Changed {
            out.state = FinalState::Invalid;
        }
        out.errors.extend(found);
    }
    Ok(out)
}
