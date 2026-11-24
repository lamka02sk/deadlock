//! The object editor on MySQL and MariaDB: loading definitions from
//! `information_schema` and running change plans on the session.

use mysql_async::Conn;
use mysql_async::prelude::Queryable;

use crate::driver::{BoxFuture, DriverError, Result};
use crate::edit::quote_ident;
use crate::objects::{
    self, ApplyOutcome, Backup, ChangePlan, DataAccess, EventProps, EventStatus, Flavor, Grant,
    IntervalUnit, ObjectDefinition, ObjectKind, ObjectRef, Param, ParamMode, Properties,
    RoutineProps, Schedule, Security, SequenceProps, Timing, TriggerEvent, TriggerOrder,
    TriggerProps, ViewCheck, ViewProps, parse,
};

fn missing(o: &ObjectRef) -> DriverError {
    DriverError::Invalid(format!(
        "{} {}.{} does not exist, or this account cannot see it",
        o.kind.label(),
        o.schema,
        o.name
    ))
}

pub(super) async fn load(
    conn: &mut Conn,
    mariadb: bool,
    read_only: bool,
    o: &ObjectRef,
) -> Result<ObjectDefinition> {
    let flavor = if mariadb {
        Flavor::Mariadb
    } else {
        Flavor::Mysql
    };
    let args = (&o.schema, &o.name);
    let mut def = ObjectDefinition {
        object: o.clone(),
        flavor,
        props: Properties::Source(objects::SourceProps::default()),
        source: String::new(),
        restore: Vec::new(),
        read_only: read_only.then(|| "The connection is read-only.".to_owned()),
        status: Vec::new(),
        errors: Vec::new(),
        grants: Vec::new(),
        facts: Vec::new(),
        siblings: Vec::new(),
        notices: Vec::new(),
        own_schema: true,
    };
    let source = super::search::definition(conn, &o.schema, &o.name, o.kind.label()).await;
    match o.kind {
        ObjectKind::View => {
            let row: Option<(Option<String>, String, String, String)> = conn
                .exec_first(
                    "SELECT VIEW_DEFINITION, CHECK_OPTION, DEFINER, SECURITY_TYPE \
                     FROM information_schema.VIEWS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
                    args,
                )
                .await?;
            let (query, check, definer, security) = row.ok_or_else(|| missing(o))?;
            let source = source.unwrap_or_default();
            def.props = Properties::View(ViewProps {
                algorithm: parse::mysql_view_algorithm(&source),
                definer: definer.clone(),
                security: Security::parse(&security).unwrap_or_default(),
                check: ViewCheck::parse(&check).unwrap_or_default(),
                query: query.clone().unwrap_or_default(),
                ..ViewProps::default()
            });
            def.facts.push(("definer".into(), definer));
            if query.is_none_or(|q| q.is_empty()) {
                def.read_only.get_or_insert_with(|| {
                    format!(
                        "MySQL returns no query for {}: this account lacks SHOW VIEW.",
                        o.name
                    )
                });
            }
            def.restore = vec![source.clone()];
            def.source = source;
        }
        ObjectKind::Procedure | ObjectKind::Function => {
            let kind = o.kind.keyword();
            type Row = (
                Option<String>,
                Option<String>,
                String,
                String,
                String,
                String,
                String,
                Option<String>,
                Option<String>,
            );
            let row: Option<Row> = conn
                .exec_first(
                    "SELECT ROUTINE_DEFINITION, DTD_IDENTIFIER, IS_DETERMINISTIC, SQL_DATA_ACCESS, \
                     SECURITY_TYPE, ROUTINE_COMMENT, DEFINER, CAST(CREATED AS CHAR), \
                     CAST(LAST_ALTERED AS CHAR) FROM information_schema.ROUTINES \
                     WHERE ROUTINE_SCHEMA = ? AND ROUTINE_NAME = ? AND ROUTINE_TYPE = ?",
                    (&o.schema, &o.name, kind),
                )
                .await?;
            let (
                body,
                returns,
                deterministic,
                access,
                security,
                comment,
                definer,
                created,
                altered,
            ) = row.ok_or_else(|| missing(o))?;
            let params: Vec<(Option<String>, Option<String>, Option<String>)> = conn
                .exec(
                    "SELECT PARAMETER_MODE, PARAMETER_NAME, DTD_IDENTIFIER \
                     FROM information_schema.PARAMETERS WHERE SPECIFIC_SCHEMA = ? \
                     AND SPECIFIC_NAME = ? AND ROUTINE_TYPE = ? AND ORDINAL_POSITION > 0 \
                     ORDER BY ORDINAL_POSITION",
                    (&o.schema, &o.name, kind),
                )
                .await?;
            def.props = Properties::Routine(RoutineProps {
                params: params
                    .into_iter()
                    .map(|(mode, name, ty)| Param {
                        mode: mode
                            .as_deref()
                            .and_then(ParamMode::parse)
                            .unwrap_or_default(),
                        name: name.unwrap_or_default(),
                        data_type: ty.unwrap_or_default(),
                        ..Param::default()
                    })
                    .collect(),
                returns: if o.kind == ObjectKind::Function {
                    returns.unwrap_or_default()
                } else {
                    String::new()
                },
                deterministic: deterministic == "YES",
                data_access: DataAccess::parse(&access),
                security: Security::parse(&security).unwrap_or_default(),
                comment,
                definer: definer.clone(),
                body: body.clone().unwrap_or_default(),
                ..RoutineProps::default()
            });
            def.facts.push(("definer".into(), definer));
            def.facts.extend(created.map(|c| ("created".into(), c)));
            def.facts.extend(altered.map(|c| ("modified".into(), c)));
            if body.is_none() {
                def.read_only.get_or_insert_with(|| {
                    format!(
                        "MySQL returns no body for {}: you are not its definer and lack \
                         SHOW_ROUTINE.",
                        o.name
                    )
                });
            }
            let grants: std::result::Result<Vec<(String, String, String)>, _> = conn
                .exec(
                    "SELECT User, Host, CAST(Proc_priv AS CHAR) FROM mysql.procs_priv \
                     WHERE Db = ? AND Routine_name = ? AND Routine_type = ?",
                    (&o.schema, &o.name, kind),
                )
                .await;
            match grants {
                Ok(rows) => {
                    def.grants = rows
                        .into_iter()
                        .filter_map(|(user, host, set)| {
                            let (privileges, grant_option) = parse::proc_privs(&set);
                            (!privileges.is_empty()).then_some(Grant {
                                user,
                                host,
                                privileges,
                                grant_option,
                            })
                        })
                        .collect();
                }
                Err(e) => def.notices.push(format!(
                    "Grants on {} could not be read ({e}): replacing it drops them, and they \
                     will not be granted again.",
                    o.name
                )),
            }
            let source = source.unwrap_or_default();
            def.restore = vec![source.clone()]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect();
            def.source = source;
        }
        ObjectKind::Trigger => {
            type Row = (String, String, String, String, String, Option<String>);
            let row: Option<Row> = conn
                .exec_first(
                    "SELECT EVENT_MANIPULATION, EVENT_OBJECT_TABLE, ACTION_STATEMENT, \
                     ACTION_TIMING, DEFINER, CAST(CREATED AS CHAR) FROM information_schema.TRIGGERS \
                     WHERE TRIGGER_SCHEMA = ? AND TRIGGER_NAME = ?",
                    args,
                )
                .await?;
            let (event, table, body, timing, definer, created) = row.ok_or_else(|| missing(o))?;
            def.siblings = conn
                .exec(
                    "SELECT TRIGGER_NAME FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = ? \
                     AND EVENT_OBJECT_TABLE = ? AND ACTION_TIMING = ? AND EVENT_MANIPULATION = ? \
                     ORDER BY ACTION_ORDER",
                    (&o.schema, &table, &timing, &event),
                )
                .await?;
            let at = def.siblings.iter().position(|n| n == &o.name);
            let order = at
                .filter(|&i| i > 0)
                .map(|i| (TriggerOrder::Follows, def.siblings[i - 1].clone()));
            def.props = Properties::Trigger(TriggerProps {
                table: table.clone(),
                timing: Timing::parse(&timing).unwrap_or_default(),
                events: TriggerEvent::parse(&event).into_iter().collect(),
                for_each_row: true,
                order,
                definer: definer.clone(),
                enabled: true,
                body,
                ..TriggerProps::default()
            });
            def.facts.push(("table".into(), table));
            def.facts.push(("definer".into(), definer));
            def.facts.extend(created.map(|c| ("created".into(), c)));
            let source = source?;
            def.restore = vec![source.clone()];
            def.source = source;
        }
        ObjectKind::Event => {
            type Row = (
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
                String,
                String,
                String,
                String,
                String,
            );
            let row: Option<Row> = conn
                .exec_first(
                    "SELECT EVENT_TYPE, CAST(EXECUTE_AT AS CHAR), INTERVAL_VALUE, INTERVAL_FIELD, \
                     CAST(STARTS AS CHAR), CAST(ENDS AS CHAR), ON_COMPLETION, STATUS, \
                     EVENT_COMMENT, EVENT_DEFINITION, DEFINER FROM information_schema.EVENTS \
                     WHERE EVENT_SCHEMA = ? AND EVENT_NAME = ?",
                    args,
                )
                .await?;
            let (ty, at, value, field, starts, ends, completion, status, comment, body, definer) =
                row.ok_or_else(|| missing(o))?;
            let schedule = if ty.eq_ignore_ascii_case("ONE TIME") {
                Schedule::At(at.unwrap_or_default())
            } else {
                Schedule::Every {
                    value: value.unwrap_or_default().trim_matches('\'').to_owned(),
                    unit: field
                        .as_deref()
                        .and_then(IntervalUnit::parse)
                        .unwrap_or_default(),
                    starts: starts.unwrap_or_default(),
                    ends: ends.unwrap_or_default(),
                }
            };
            let status = match status.to_ascii_uppercase().as_str() {
                "ENABLED" => EventStatus::Enable,
                "DISABLED" => EventStatus::Disable,
                _ => EventStatus::DisableOnReplica,
            };
            def.props = Properties::Event(EventProps {
                schedule,
                preserve: completion.eq_ignore_ascii_case("PRESERVE"),
                status,
                comment,
                definer: definer.clone(),
                body,
            });
            def.facts.push(("definer".into(), definer));
            let scheduler: Option<String> = conn.query_first("SELECT @@event_scheduler").await?;
            if scheduler.is_some_and(|s| !s.eq_ignore_ascii_case("ON")) {
                def.notices.push(
                    "The event scheduler is off (event_scheduler): the event is stored but \
                     never runs."
                        .into(),
                );
            }
            let source = source?;
            def.restore = vec![source.clone()];
            def.source = source;
        }
        ObjectKind::Sequence if mariadb => {
            type Row = (String, String, String, String, String, String, String);
            let row: Option<Row> = conn
                .query_first(format!(
                    "SELECT CAST(start_value AS CHAR), CAST(minimum_value AS CHAR), \
                     CAST(maximum_value AS CHAR), CAST(increment AS CHAR), \
                     CAST(cycle_option AS CHAR), CAST(cache_size AS CHAR), \
                     CAST(next_not_cached_value AS CHAR) FROM {}.{}",
                    quote_ident(&o.schema),
                    quote_ident(&o.name)
                ))
                .await?;
            let (start, min, max, increment, cycle, cache, next) = row.ok_or_else(|| missing(o))?;
            def.props = Properties::Sequence(SequenceProps {
                start,
                increment,
                min,
                max,
                cache,
                cycle: cycle == "1",
                current: next,
                ..SequenceProps::default()
            });
            let source = source?;
            def.restore = vec![source.clone()];
            def.source = source;
        }
        _ => {
            return Err(DriverError::Invalid(format!(
                "{} has no {}s",
                flavor.label(),
                o.kind.label()
            )));
        }
    }
    Ok(def)
}

/// Runs statements on the session for [`objects::execute`].
struct Runner<'c>(&'c mut Conn);

impl objects::Runner for Runner<'_> {
    fn run<'a>(&'a mut self, sql: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { Ok(self.0.query_drop(sql).await?) })
    }

    /// `CREATE command denied` (1142), no access to the database (1044), a missing
    /// privilege (1227).
    fn denied(&self, e: &DriverError) -> bool {
        matches!(e, DriverError::Mysql(mysql_async::Error::Server(s)) if matches!(s.code, 1142 | 1044 | 1227))
    }
}

/// Runs `plan` with the object's schema as the session's database (the original
/// definitions name their tables without it), then switches back.
pub(super) async fn apply(
    conn: &mut Conn,
    plan: &ChangePlan,
    backup: Option<&Backup>,
) -> Result<ApplyOutcome> {
    let current: Option<Option<String>> = conn.query_first("SELECT DATABASE()").await?;
    let current = current.flatten();
    let schema = &plan.previous.as_ref().unwrap_or(&plan.object).schema;
    let switch = current.as_deref() != Some(schema.as_str());
    if switch {
        conn.query_drop(format!("USE {}", quote_ident(schema)))
            .await?;
    }
    let mut out = objects::execute(plan, &mut Runner(conn), backup).await;
    if switch
        && let Some(db) = current
        && let Err(e) = conn.query_drop(format!("USE {}", quote_ident(&db))).await
    {
        out.notices
            .push(format!("Switching back to database {db} failed: {e}"));
    }
    Ok(out)
}
