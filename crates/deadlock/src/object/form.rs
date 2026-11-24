//! What the object editor's Properties form shows for each kind of object, as plain
//! data: fields with a getter and a setter on the draft, so the form is a view of the
//! draft and the draft is what [`deadlock_core::objects::plan`] reads. Also the pure
//! rules the tab follows: dirty tracking, when Apply asks first, the confirmation's
//! step list and what an apply's outcome says.

use deadlock_core::connection::Environment;
use deadlock_core::objects::{
    ApplyOutcome, ChangePlan, DataAccess, EventStatus, FinalState, Flavor, IntervalUnit,
    ObjectDefinition, ObjectDraft, ObjectError, ObjectKind, ParamMode, Properties, RefreshMethod,
    RefreshMode, Schedule, Security, Timing, TriggerEvent, TriggerOrder, ViewAlgorithm, ViewCheck,
};

pub(crate) type Get = fn(&ObjectDraft) -> String;
pub(crate) type Set = fn(&mut ObjectDraft, &str);

/// How a field is edited. Values are always text: a check is `true` or `false`, a
/// choice is the option's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Widget {
    Text {
        mono: bool,
        placeholder: &'static str,
    },
    Select(Vec<&'static str>),
    Check,
    /// Shown, never edited (MariaDB's next value, Oracle's last number).
    Fixed,
}

#[derive(Clone)]
pub(crate) struct Field {
    /// Unique in the form: the input's id.
    pub key: &'static str,
    pub label: &'static str,
    pub widget: Widget,
    pub get: Get,
    pub set: Set,
}

impl Field {
    /// The field's value differs from the loaded object's: its label gets the dot.
    pub(crate) fn changed(&self, base: &ObjectDraft, draft: &ObjectDraft) -> bool {
        (self.get)(base) != (self.get)(draft)
    }
}

/// A titled group of field rows, with a hint under it.
pub(crate) struct Group {
    pub title: &'static str,
    pub rows: Vec<Vec<Field>>,
    pub hint: Option<&'static str>,
}

/// Getter and setter of a text property of one `Properties` variant.
macro_rules! text {
    ($v:ident . $f:ident) => {
        (
            |d: &ObjectDraft| match &d.props {
                Properties::$v(x) => x.$f.clone(),
                _ => String::new(),
            },
            |d: &mut ObjectDraft, s: &str| {
                if let Properties::$v(x) = &mut d.props {
                    s.clone_into(&mut x.$f);
                }
            },
        )
    };
}

macro_rules! flag {
    ($v:ident . $f:ident) => {
        (
            |d: &ObjectDraft| match &d.props {
                Properties::$v(x) => x.$f.to_string(),
                _ => String::new(),
            },
            |d: &mut ObjectDraft, s: &str| {
                if let Properties::$v(x) = &mut d.props {
                    x.$f = s == "true";
                }
            },
        )
    };
}

macro_rules! pick {
    ($v:ident . $f:ident : $e:ident) => {
        (
            |d: &ObjectDraft| match &d.props {
                Properties::$v(x) => x.$f.sql().to_owned(),
                _ => String::new(),
            },
            |d: &mut ObjectDraft, s: &str| {
                if let (Properties::$v(x), Some(k)) = (&mut d.props, $e::parse(s)) {
                    x.$f = k;
                }
            },
        )
    };
}

fn text(key: &'static str, label: &'static str, mono: bool, (get, set): (Get, Set)) -> Field {
    Field {
        key,
        label,
        widget: Widget::Text {
            mono,
            placeholder: "",
        },
        get,
        set,
    }
}

fn hinted(mut f: Field, placeholder: &'static str) -> Field {
    if let Widget::Text { placeholder: p, .. } = &mut f.widget {
        *p = placeholder;
    }
    f
}

fn select(
    key: &'static str,
    label: &'static str,
    options: Vec<&'static str>,
    (get, set): (Get, Set),
) -> Field {
    Field {
        key,
        label,
        widget: Widget::Select(options),
        get,
        set,
    }
}

fn check(key: &'static str, label: &'static str, (get, set): (Get, Set)) -> Field {
    Field {
        key,
        label,
        widget: Widget::Check,
        get,
        set,
    }
}

fn fixed(key: &'static str, label: &'static str, (get, set): (Get, Set)) -> Field {
    Field {
        key,
        label,
        widget: Widget::Fixed,
        get,
        set,
    }
}

fn sqls<T: Copy>(all: &[T], sql: fn(T) -> &'static str) -> Vec<&'static str> {
    all.iter().map(|k| sql(*k)).collect()
}

fn name() -> Field {
    text(
        "name",
        "Name",
        true,
        (
            |d| d.object.name.clone(),
            |d, s| s.clone_into(&mut d.object.name),
        ),
    )
}

/// The "Runs" choice of an event: which schedule fields show.
pub(crate) const RUNS: &str = "runs";
const EVERY: &str = "Recurring (EVERY)";
const AT: &str = "Once (AT)";
const NO_ORDER: &str = "none";
const NO_ACCESS: &str = "not set";

/// The Properties form of `draft` on `flavor`: groups of rows. The parameters grid and
/// the body editor are drawn by the tab ([`body_label`], [`has_params`]). `new`: the
/// object does not exist yet (Build deferred shows only then).
pub(crate) fn layout(flavor: Flavor, draft: &ObjectDraft, new: bool) -> Vec<Group> {
    let oracle = flavor == Flavor::Oracle;
    let kind = draft.object.kind;
    match &draft.props {
        Properties::View(_) if oracle => vec![Group {
            title: "Properties",
            rows: vec![
                vec![
                    name(),
                    hinted(
                        text("columns", "Columns", true, text!(View.columns)),
                        "none: the query's",
                    ),
                    select(
                        "check",
                        "Check option",
                        vec!["NONE", "CASCADED", "READ ONLY"],
                        pick!(View.check: ViewCheck),
                    ),
                ],
                vec![check(
                    "force",
                    "Force (create even if it does not compile)",
                    flag!(View.force),
                )],
            ],
            hint: Some("Check option: CASCADED is WITH CHECK OPTION, READ ONLY is WITH READ ONLY."),
        }],
        Properties::View(_) => vec![Group {
            title: "Properties",
            rows: vec![
                vec![
                    name(),
                    hinted(
                        text("definer", "Definer", true, text!(View.definer)),
                        "current user",
                    ),
                ],
                vec![
                    select(
                        "algorithm",
                        "Algorithm",
                        sqls(ViewAlgorithm::ALL, ViewAlgorithm::sql),
                        pick!(View.algorithm: ViewAlgorithm),
                    ),
                    select(
                        "security",
                        "SQL security",
                        sqls(Security::ALL, Security::sql),
                        pick!(View.security: Security),
                    ),
                    select(
                        "check",
                        "Check option",
                        vec!["NONE", "CASCADED", "LOCAL"],
                        pick!(View.check: ViewCheck),
                    ),
                ],
            ],
            hint: None,
        }],
        Properties::MaterializedView(_) => {
            let mut opts = vec![check(
                "rewrite",
                "Enable query rewrite",
                flag!(MaterializedView.query_rewrite),
            )];
            if new {
                opts.push(check(
                    "deferred",
                    "Build deferred",
                    flag!(MaterializedView.build_deferred),
                ));
            }
            vec![Group {
                title: "Properties",
                rows: vec![
                    vec![
                        name(),
                        select(
                            "method",
                            "Refresh method",
                            sqls(RefreshMethod::ALL, RefreshMethod::sql),
                            pick!(MaterializedView.method: RefreshMethod),
                        ),
                        select(
                            "mode",
                            "Refresh mode",
                            sqls(RefreshMode::ALL, RefreshMode::sql),
                            pick!(MaterializedView.mode: RefreshMode),
                        ),
                    ],
                    vec![
                        hinted(
                            text(
                                "start",
                                "Start with",
                                true,
                                text!(MaterializedView.start_with),
                            ),
                            "none",
                        ),
                        hinted(
                            text("next", "Next", true, text!(MaterializedView.next)),
                            "none",
                        ),
                    ],
                    opts,
                ],
                hint: Some("A change to the query drops and creates the materialized view."),
            }]
        }
        Properties::Routine(_) => routine(oracle, kind),
        Properties::Trigger(_) => trigger(oracle),
        Properties::Event(_) => event(draft),
        Properties::Sequence(_) => sequence(oracle, new),
        Properties::Synonym(_) => vec![Group {
            title: "Properties",
            rows: vec![
                vec![name(), check("public", "Public", flag!(Synonym.public))],
                vec![
                    text(
                        "target_schema",
                        "Target schema",
                        true,
                        text!(Synonym.target_schema),
                    ),
                    text("target", "Object", true, text!(Synonym.target)),
                    hinted(
                        text("db_link", "DB link", true, text!(Synonym.db_link)),
                        "local",
                    ),
                ],
            ],
            hint: Some("Public synonyms need CREATE PUBLIC SYNONYM."),
        }],
        Properties::Source(_) => Vec::new(),
    }
}

fn routine(oracle: bool, kind: ObjectKind) -> Vec<Group> {
    let function = kind == ObjectKind::Function;
    let mut head = vec![name()];
    if !oracle {
        head.push(hinted(
            text("definer", "Definer", true, text!(Routine.definer)),
            "current user",
        ));
        head.push(text("comment", "Comment", false, text!(Routine.comment)));
    }
    let mut chars = Vec::new();
    if function {
        chars.push(text("returns", "Returns", true, text!(Routine.returns)));
    }
    if !oracle {
        let mut access = vec![NO_ACCESS];
        access.extend(sqls(DataAccess::ALL, DataAccess::sql));
        chars.push(select(
            "access",
            "Data access",
            access,
            (
                |d| match &d.props {
                    Properties::Routine(r) => {
                        r.data_access.map_or(NO_ACCESS, DataAccess::sql).into()
                    }
                    _ => String::new(),
                },
                |d, s| {
                    if let Properties::Routine(r) = &mut d.props {
                        r.data_access = DataAccess::parse(s);
                    }
                },
            ),
        ));
    }
    let get: Get = if oracle {
        |d| match &d.props {
            Properties::Routine(r) if r.security == Security::Invoker => "CURRENT_USER".into(),
            Properties::Routine(r) => r.security.sql().into(),
            _ => String::new(),
        }
    } else {
        |d| match &d.props {
            Properties::Routine(r) => r.security.sql().into(),
            _ => String::new(),
        }
    };
    chars.push(select(
        "security",
        if oracle { "AUTHID" } else { "SQL security" },
        if oracle {
            vec!["DEFINER", "CURRENT_USER"]
        } else {
            sqls(Security::ALL, Security::sql)
        },
        (get, |d, s| {
            if let Properties::Routine(r) = &mut d.props {
                // Oracle's AUTHID CURRENT_USER is the invoker.
                let s = if s == "CURRENT_USER" { "INVOKER" } else { s };
                if let Some(k) = Security::parse(s) {
                    r.security = k;
                }
            }
        }),
    ));
    let mut checks = vec![check(
        "deterministic",
        "Deterministic",
        flag!(Routine.deterministic),
    )];
    if oracle && function {
        checks.push(check(
            "result_cache",
            "Result cache",
            flag!(Routine.result_cache),
        ));
        checks.push(check(
            "parallel",
            "Parallel enable",
            flag!(Routine.parallel_enable),
        ));
    }
    vec![
        Group {
            title: "Properties",
            rows: vec![head],
            hint: None,
        },
        Group {
            title: if function {
                "Returns and characteristics"
            } else {
                "Characteristics"
            },
            rows: vec![chars, checks],
            hint: None,
        },
    ]
}

fn trigger(oracle: bool) -> Vec<Group> {
    let order = select(
        "order",
        "Order",
        vec![NO_ORDER, "FOLLOWS", "PRECEDES"],
        (
            |d| match &d.props {
                Properties::Trigger(t) => {
                    t.order.as_ref().map_or(NO_ORDER, |(o, _)| o.sql()).into()
                }
                _ => String::new(),
            },
            |d, s| {
                if let Properties::Trigger(t) = &mut d.props {
                    let other = t.order.take().map(|(_, n)| n).unwrap_or_default();
                    t.order = TriggerOrder::parse(s).map(|o| (o, other));
                }
            },
        ),
    );
    let other = text(
        "other",
        "Other trigger",
        true,
        (
            |d| match &d.props {
                Properties::Trigger(t) => {
                    t.order.as_ref().map(|(_, n)| n.clone()).unwrap_or_default()
                }
                _ => String::new(),
            },
            |d, s| {
                if let Properties::Trigger(t) = &mut d.props {
                    match &mut t.order {
                        Some((_, n)) => s.clone_into(n),
                        None if !s.trim().is_empty() => {
                            t.order = Some((TriggerOrder::Follows, s.to_owned()));
                        }
                        None => {}
                    }
                }
            },
        ),
    );
    if !oracle {
        return vec![Group {
            title: "Properties",
            rows: vec![
                vec![
                    name(),
                    text("table", "Table", true, text!(Trigger.table)),
                    hinted(
                        text("definer", "Definer", true, text!(Trigger.definer)),
                        "current user",
                    ),
                ],
                vec![
                    select(
                        "timing",
                        "Timing",
                        vec!["BEFORE", "AFTER"],
                        pick!(Trigger.timing: Timing),
                    ),
                    select(
                        "event",
                        "Event",
                        sqls(TriggerEvent::ALL, TriggerEvent::sql),
                        (
                            |d| match &d.props {
                                Properties::Trigger(t) => {
                                    t.events.first().map_or("", |e| e.sql()).into()
                                }
                                _ => String::new(),
                            },
                            |d, s| {
                                if let (Properties::Trigger(t), Some(e)) =
                                    (&mut d.props, TriggerEvent::parse(s))
                                {
                                    t.events = vec![e];
                                }
                            },
                        ),
                    ),
                    order,
                    other,
                ],
            ],
            hint: Some(
                "MySQL triggers always run FOR EACH ROW and cannot be disabled. A new name \
                 runs DROP + CREATE: MySQL has no RENAME TRIGGER.",
            ),
        }];
    }
    macro_rules! event_check {
        ($key:literal, $e:ident) => {
            check(
                $key,
                TriggerEvent::$e.sql(),
                (
                    |d| match &d.props {
                        Properties::Trigger(t) => t.events.contains(&TriggerEvent::$e).to_string(),
                        _ => String::new(),
                    },
                    |d, s| {
                        if let Properties::Trigger(t) = &mut d.props {
                            t.events.retain(|e| *e != TriggerEvent::$e);
                            if s == "true" {
                                t.events.push(TriggerEvent::$e);
                                t.events.sort_by_key(|e| *e as u8);
                            }
                        }
                    },
                ),
            )
        };
    }
    vec![Group {
        title: "When it fires",
        rows: vec![
            vec![
                name(),
                text("table", "On", true, text!(Trigger.table)),
                hinted(
                    text(
                        "table_schema",
                        "Table schema",
                        true,
                        text!(Trigger.table_schema),
                    ),
                    "the trigger's",
                ),
            ],
            vec![
                select(
                    "timing",
                    "Timing",
                    sqls(Timing::ALL, Timing::sql),
                    pick!(Trigger.timing: Timing),
                ),
                hinted(
                    text(
                        "update_columns",
                        "Update of columns",
                        true,
                        (
                            |d| match &d.props {
                                Properties::Trigger(t) => t.update_columns.join(", "),
                                _ => String::new(),
                            },
                            |d, s| {
                                if let Properties::Trigger(t) = &mut d.props {
                                    t.update_columns = split_list(s);
                                }
                            },
                        ),
                    ),
                    "any column",
                ),
            ],
            vec![
                event_check!("insert", Insert),
                event_check!("update", Update),
                event_check!("delete", Delete),
                check("row", "For each row", flag!(Trigger.for_each_row)),
            ],
            vec![hinted(
                text(
                    "when",
                    "When (row level only, no colons)",
                    true,
                    text!(Trigger.when),
                ),
                "always",
            )],
            vec![
                order,
                other,
                check("enabled", "Enabled", flag!(Trigger.enabled)),
            ],
        ],
        hint: Some("INSTEAD OF works on views only and is always row level."),
    }]
}

/// `a, b ,c` → `[a, b, c]`.
fn split_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_owned)
        .collect()
}

fn event(draft: &ObjectDraft) -> Vec<Group> {
    let at = matches!(&draft.props, Properties::Event(e) if matches!(e.schedule, Schedule::At(_)));
    let runs = select(
        RUNS,
        "Runs",
        vec![EVERY, AT],
        (
            |d| match &d.props {
                Properties::Event(e) if matches!(e.schedule, Schedule::At(_)) => AT.into(),
                Properties::Event(_) => EVERY.into(),
                _ => String::new(),
            },
            |d, s| {
                if let Properties::Event(e) = &mut d.props {
                    let at = matches!(e.schedule, Schedule::At(_));
                    if s == AT && !at {
                        e.schedule = Schedule::At(String::new());
                    } else if s == EVERY && at {
                        e.schedule = Schedule::default();
                    }
                }
            },
        ),
    );
    macro_rules! every {
        ($key:literal, $label:literal, $f:ident, $ph:literal) => {
            hinted(
                text(
                    $key,
                    $label,
                    true,
                    (
                        |d| match &d.props {
                            Properties::Event(e) => match &e.schedule {
                                Schedule::Every { $f, .. } => $f.clone(),
                                Schedule::At(_) => String::new(),
                            },
                            _ => String::new(),
                        },
                        |d, s| {
                            if let Properties::Event(e) = &mut d.props
                                && let Schedule::Every { $f, .. } = &mut e.schedule
                            {
                                s.clone_into($f);
                            }
                        },
                    ),
                ),
                $ph,
            )
        };
    }
    let schedule = if at {
        vec![
            runs,
            hinted(
                text(
                    "at",
                    "At",
                    true,
                    (
                        |d| match &d.props {
                            Properties::Event(e) => match &e.schedule {
                                Schedule::At(t) => t.clone(),
                                Schedule::Every { .. } => String::new(),
                            },
                            _ => String::new(),
                        },
                        |d, s| {
                            if let Properties::Event(e) = &mut d.props
                                && let Schedule::At(t) = &mut e.schedule
                            {
                                s.clone_into(t);
                            }
                        },
                    ),
                ),
                "2026-11-25 02:00:00",
            ),
        ]
    } else {
        vec![
            runs,
            every!("every", "Every", value, "1"),
            select(
                "unit",
                "Unit",
                sqls(IntervalUnit::ALL, IntervalUnit::sql),
                (
                    |d| match &d.props {
                        Properties::Event(e) => match &e.schedule {
                            Schedule::Every { unit, .. } => unit.sql().into(),
                            Schedule::At(_) => String::new(),
                        },
                        _ => String::new(),
                    },
                    |d, s| {
                        if let Properties::Event(e) = &mut d.props
                            && let Schedule::Every { unit, .. } = &mut e.schedule
                            && let Some(u) = IntervalUnit::parse(s)
                        {
                            *unit = u;
                        }
                    },
                ),
            ),
        ]
    };
    let mut rows = vec![schedule];
    if !at {
        rows.push(vec![
            every!("starts", "Starts", starts, "now"),
            every!("ends", "Ends", ends, "never"),
        ]);
    }
    rows.push(vec![
        select(
            "status",
            "Status",
            sqls(EventStatus::ALL, EventStatus::sql),
            pick!(Event.status: EventStatus),
        ),
        text("comment", "Comment", false, text!(Event.comment)),
        hinted(
            text("definer", "Definer", true, text!(Event.definer)),
            "current user",
        ),
    ]);
    rows.push(vec![check(
        "preserve",
        "Keep after the last run (ON COMPLETION PRESERVE)",
        flag!(Event.preserve),
    )]);
    vec![
        Group {
            title: "Name",
            rows: vec![vec![name()]],
            hint: None,
        },
        Group {
            title: "Schedule",
            rows,
            hint: Some(
                "Times are in the session time zone. The event is stored but never runs \
                 while event_scheduler is OFF.",
            ),
        },
    ]
}

fn sequence(oracle: bool, new: bool) -> Vec<Group> {
    let mut first = vec![name()];
    // Oracle fixes Start with after create: Restart with covers it.
    if new || !oracle {
        first.push(text("start", "Start", true, text!(Sequence.start)));
    }
    first.push(text(
        "increment",
        if oracle { "Increment by" } else { "Increment" },
        true,
        text!(Sequence.increment),
    ));
    let mut checks = vec![check("cycle", "Cycle", flag!(Sequence.cycle))];
    if oracle {
        checks.push(check("order", "Order", flag!(Sequence.order)));
    }
    let mut rows = vec![
        first,
        vec![
            hinted(
                text("min", "Min value", true, text!(Sequence.min)),
                "no minimum",
            ),
            hinted(
                text("max", "Max value", true, text!(Sequence.max)),
                "no maximum",
            ),
            hinted(
                text("cache", "Cache", true, text!(Sequence.cache)),
                "no cache",
            ),
        ],
    ];
    if !new {
        rows.push(vec![
            hinted(
                text(
                    "restart",
                    if oracle {
                        "Restart with (18c+)"
                    } else {
                        "Restart with"
                    },
                    true,
                    text!(Sequence.restart_with),
                ),
                "leave empty to keep",
            ),
            fixed(
                "current",
                if oracle {
                    "Last number (cache high-water)"
                } else {
                    "Next uncached value"
                },
                text!(Sequence.current),
            ),
        ]);
    }
    rows.push(checks);
    vec![Group {
        title: "Values",
        rows,
        hint: None,
    }]
}

/// Routines have the parameters grid.
pub(crate) fn has_params(draft: &ObjectDraft) -> bool {
    matches!(draft.props, Properties::Routine(_))
}

/// The parameter modes a routine offers; MySQL functions take IN only (`None`).
pub(crate) fn param_modes(flavor: Flavor, kind: ObjectKind) -> Option<Vec<&'static str>> {
    match (flavor, kind) {
        (Flavor::Oracle, _) => Some(vec!["IN", "OUT", "IN OUT"]),
        (_, ObjectKind::Function) => None,
        _ => Some(sqls(ParamMode::ALL, ParamMode::sql)),
    }
}

/// A mode as the grid shows it on `flavor`.
pub(crate) fn mode_label(flavor: Flavor, mode: ParamMode) -> &'static str {
    match (flavor, mode) {
        (Flavor::Oracle, ParamMode::InOut) => "IN OUT",
        (_, m) => m.sql(),
    }
}

/// The mode a grid label stands for.
pub(crate) fn parse_mode(label: &str) -> Option<ParamMode> {
    ParamMode::parse(&label.replace("IN OUT", "INOUT"))
}

/// The body editor's title, getter and setter; `None` for kinds without one.
pub(crate) fn body(draft: &ObjectDraft) -> Option<(&'static str, Get, Set)> {
    Some(match &draft.props {
        Properties::View(_) => {
            let (g, s) = text!(View.query);
            ("Query", g, s)
        }
        Properties::MaterializedView(_) => {
            let (g, s) = text!(MaterializedView.query);
            ("Query", g, s)
        }
        Properties::Routine(_) => {
            let (g, s) = text!(Routine.body);
            ("Body", g, s)
        }
        Properties::Trigger(_) => {
            let (g, s) = text!(Trigger.body);
            ("Body", g, s)
        }
        Properties::Event(_) => {
            let (g, s) = text!(Event.body);
            ("Body (DO)", g, s)
        }
        Properties::Source(_) => {
            let (g, s) = text!(Source.spec);
            (
                if source_has_body(draft.object.kind) {
                    "Spec"
                } else {
                    "Source"
                },
                g,
                s,
            )
        }
        Properties::Sequence(_) | Properties::Synonym(_) => return None,
    })
}

/// The second editor of packages and types.
pub(crate) fn source_body() -> (Get, Set) {
    text!(Source.body)
}

/// Packages and types have a body part next to the spec.
pub(crate) fn source_has_body(kind: ObjectKind) -> bool {
    matches!(kind, ObjectKind::Package | ObjectKind::Type)
}

/// Whether Apply asks first: always on Production; on Stage when the plan is not
/// atomic (it may leave the object missing or half changed in between).
pub(crate) fn needs_confirm(env: Environment, plan: &ChangePlan) -> bool {
    match env {
        Environment::Production => true,
        Environment::Staging => plan.non_atomic_warning().is_some(),
        Environment::Development => false,
    }
}

/// The confirmation's step list: `1. Check … (3 statements)`, restore steps marked.
pub(crate) fn steps_text(plan: &ChangePlan) -> String {
    let plural = |n: usize| {
        if n == 1 {
            "1 statement".to_owned()
        } else {
            format!("{n} statements")
        }
    };
    plan.steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let what = if s.description.is_empty() {
                s.statements
                    .first()
                    .map_or("", |s| s.lines().next().unwrap_or(""))
            } else {
                &s.description
            };
            let only = if s.conditional() {
                "Only if needed: "
            } else {
                ""
            };
            format!(
                "{}. {only}{what} ({})",
                i + 1,
                plural(s.statements.len() + s.cleanup.len())
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `5 statements, 1 more only to restore`.
pub(crate) fn count_label(plan: &ChangePlan) -> String {
    let n = plan.statement_count();
    let mut out = if n == 1 {
        "1 statement".to_owned()
    } else {
        format!("{n} statements")
    };
    let r = plan.restore_count();
    if r > 0 {
        out.push_str(&format!(", {r} more only to restore"));
    }
    out
}

/// `3 changes: Comment, Parameters, Body`.
pub(crate) fn changes_label(plan: &ChangePlan) -> Option<String> {
    match plan.changes.len() {
        0 => None,
        1 => Some(format!("1 change: {}", plan.changes[0])),
        n => Some(format!("{n} changes: {}", plan.changes.join(", "))),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Info,
    Warn,
    Error,
}

/// What the tab says after an apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Banner {
    pub tone: Tone,
    pub text: String,
    pub errors: Vec<ObjectError>,
    /// Missing: the original's statements, and the file they went to.
    pub backup: Option<(String, Option<std::path::PathBuf>)>,
    /// Offer Revert to last valid.
    pub revert: bool,
}

impl Banner {
    pub(crate) fn error(text: impl Into<String>) -> Self {
        Self {
            tone: Tone::Error,
            text: text.into(),
            errors: Vec::new(),
            backup: None,
            revert: false,
        }
    }
}

/// The banner for `o`; `name` is the object's, `conn` the connection's, `verb` what
/// happened when it worked (`Replaced`, `Dropped`).
pub(crate) fn outcome_banner(o: &ApplyOutcome, name: &str, conn: &str, verb: &str) -> Banner {
    let mut b = Banner::error(String::new());
    b.errors.clone_from(&o.errors);
    let first = o
        .errors
        .first()
        .map(|e| {
            if e.code.is_empty() {
                e.message.clone()
            } else {
                format!("{} {}", e.code, e.message)
            }
        })
        .unwrap_or_default();
    match &o.state {
        FinalState::Changed => {
            b.tone = Tone::Info;
            b.text = format!("{verb} {name}.");
            b.errors.clear();
        }
        FinalState::Invalid => {
            let n = o.errors.iter().filter(|e| !e.warning).count();
            b.text = format!(
                "{name} was saved with {n} {} and is now INVALID. Callers fail until it compiles.",
                if n == 1 { "error" } else { "errors" }
            );
            b.revert = true;
        }
        FinalState::Unchanged => {
            b.text = format!("The change did not go through, so {name} was not touched. {first}");
        }
        FinalState::Partial => {
            b.text =
                format!("Some statements ran before one failed: {name} is partly changed. {first}");
        }
        FinalState::Restored => {
            b.tone = Tone::Warn;
            b.text = format!(
                "The new {name} could not be created, so the original was put back. The server \
                 has the old one; your edits are still here. {first}"
            );
        }
        FinalState::Missing { backup, path } => {
            b.text = match path {
                Some(p) => format!(
                    "{name} is missing on {conn}. The new definition and the restore both \
                     failed. The original is saved in {}. {first}",
                    p.display()
                ),
                None => format!(
                    "{name} is missing on {conn}. The new definition and the restore both \
                     failed, and the original could not be written to disk: copy it now. {first}"
                ),
            };
            b.backup = Some((backup.clone(), path.clone()));
        }
    }
    b.text = b.text.trim_end().to_owned();
    b
}

/// The draft differs from what the editor started from.
pub(crate) fn is_dirty(base: &ObjectDraft, draft: &ObjectDraft) -> bool {
    base != draft
}

/// `SPEC VALID`, `BODY INVALID`, or plain `VALID` for a one-part object.
pub(crate) fn status_chips(def: &ObjectDefinition) -> Vec<(String, bool)> {
    let one = def.status.len() == 1;
    def.status
        .iter()
        .map(|(part, ok)| {
            let state = if *ok { "VALID" } else { "INVALID" };
            if one {
                (state.to_owned(), *ok)
            } else {
                let p = if part.ends_with("BODY") {
                    "BODY"
                } else {
                    "SPEC"
                };
                (format!("{p} {state}"), *ok)
            }
        })
        .collect()
}

/// ENABLED or DISABLED for triggers and events, `None` for the rest. MySQL triggers
/// are always enabled and show none.
pub(crate) fn enabled_chip(flavor: Flavor, props: &Properties) -> Option<bool> {
    match props {
        Properties::Trigger(t) if flavor == Flavor::Oracle => Some(t.enabled),
        Properties::Event(e) => Some(e.status == EventStatus::Enable),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadlock_core::objects::{
        ObjectRef, StepResult, StepStatus, TriggerProps, drop_plan, template,
    };

    fn fields(flavor: Flavor, d: &ObjectDraft) -> Vec<Field> {
        layout(flavor, d, false)
            .into_iter()
            .flat_map(|g| g.rows.into_iter().flatten())
            .collect()
    }

    fn field(flavor: Flavor, d: &ObjectDraft, key: &str) -> Field {
        fields(flavor, d)
            .into_iter()
            .find(|f| f.key == key)
            .unwrap_or_else(|| panic!("no field {key}"))
    }

    #[test]
    fn every_field_round_trips_through_the_draft() {
        for flavor in [Flavor::Mysql, Flavor::Mariadb, Flavor::Oracle] {
            for kind in ObjectKind::managed(flavor) {
                let d = template(flavor, *kind, "s");
                let mut keys: Vec<&str> = fields(flavor, &d).iter().map(|f| f.key).collect();
                let n = keys.len();
                keys.sort_unstable();
                keys.dedup();
                assert_eq!(keys.len(), n, "{flavor:?} {kind:?}: keys are unique");
                for f in fields(flavor, &d) {
                    let mut copy = d.clone();
                    (f.set)(&mut copy, &(f.get)(&d));
                    assert_eq!(copy, d, "{flavor:?} {kind:?} {}", f.key);
                    if let Widget::Select(options) = &f.widget {
                        assert!(
                            options.contains(&(f.get)(&d).as_str()),
                            "{flavor:?} {kind:?} {}: {} is an option",
                            f.key,
                            (f.get)(&d)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn setting_a_field_changes_the_draft_and_marks_it() {
        let base = template(Flavor::Mysql, ObjectKind::Trigger, "shop");
        let mut d = base.clone();
        let table = field(Flavor::Mysql, &d, "table");
        (table.set)(&mut d, "orders");
        assert!(table.changed(&base, &d));
        assert!(is_dirty(&base, &d));
        let event = field(Flavor::Mysql, &d, "event");
        (event.set)(&mut d, "UPDATE");
        let Properties::Trigger(TriggerProps { table, events, .. }) = &d.props else {
            panic!("trigger")
        };
        assert_eq!(
            (table.as_str(), events.as_slice()),
            ("orders", &[TriggerEvent::Update][..])
        );
        assert!(!field(Flavor::Mysql, &d, "timing").changed(&base, &d));
    }

    #[test]
    fn oracle_trigger_events_are_checks() {
        let mut d = template(Flavor::Oracle, ObjectKind::Trigger, "S");
        (field(Flavor::Oracle, &d, "delete").set)(&mut d, "true");
        (field(Flavor::Oracle, &d, "insert").set)(&mut d, "false");
        let Properties::Trigger(t) = &d.props else {
            panic!()
        };
        assert_eq!(t.events, [TriggerEvent::Delete]);
        (field(Flavor::Oracle, &d, "update_columns").set)(&mut d, " a, ,b ");
        let Properties::Trigger(t) = &d.props else {
            panic!()
        };
        assert_eq!(t.update_columns, ["a", "b"]);
    }

    #[test]
    fn event_runs_switches_the_schedule_fields() {
        let mut d = template(Flavor::Mysql, ObjectKind::Event, "s");
        assert!(fields(Flavor::Mysql, &d).iter().any(|f| f.key == "every"));
        (field(Flavor::Mysql, &d, RUNS).set)(&mut d, AT);
        let keys: Vec<_> = fields(Flavor::Mysql, &d).iter().map(|f| f.key).collect();
        assert!(keys.contains(&"at") && !keys.contains(&"every"));
        (field(Flavor::Mysql, &d, "at").set)(&mut d, "2026-12-01 00:00:00");
        let Properties::Event(e) = &d.props else {
            panic!()
        };
        assert_eq!(e.schedule, Schedule::At("2026-12-01 00:00:00".into()));
    }

    #[test]
    fn routine_choices_map_to_keywords() {
        let mut d = template(Flavor::Oracle, ObjectKind::Function, "S");
        (field(Flavor::Oracle, &d, "security").set)(&mut d, "CURRENT_USER");
        let Properties::Routine(r) = &d.props else {
            panic!()
        };
        assert_eq!(r.security, Security::Invoker);
        let mut d = template(Flavor::Mysql, ObjectKind::Procedure, "s");
        (field(Flavor::Mysql, &d, "access").set)(&mut d, "READS SQL DATA");
        let Properties::Routine(r) = &d.props else {
            panic!()
        };
        assert_eq!(r.data_access, Some(DataAccess::ReadsSqlData));
        (field(Flavor::Mysql, &d, "access").set)(&mut d, NO_ACCESS);
        let Properties::Routine(r) = &d.props else {
            panic!()
        };
        assert_eq!(r.data_access, None);
        assert_eq!(parse_mode("IN OUT"), Some(ParamMode::InOut));
        assert_eq!(mode_label(Flavor::Oracle, ParamMode::InOut), "IN OUT");
        assert!(param_modes(Flavor::Mysql, ObjectKind::Function).is_none());
    }

    #[test]
    fn production_always_asks_stage_only_when_not_atomic() {
        let o = ObjectRef::new("s", "t", ObjectKind::Trigger);
        let mut plan = drop_plan(Flavor::Mysql, &o);
        plan.atomic = true;
        assert!(needs_confirm(Environment::Production, &plan));
        assert!(!needs_confirm(Environment::Staging, &plan));
        plan.atomic = false;
        assert!(needs_confirm(Environment::Staging, &plan));
        assert!(!needs_confirm(Environment::Development, &plan));
        assert_eq!(steps_text(&plan), "1. DROP TRIGGER `s`.`t` (1 statement)");
        assert_eq!(count_label(&plan), "1 statement");
    }

    #[test]
    fn outcomes_map_to_banners() {
        let err = ObjectError {
            code: "1064 (42000)".into(),
            message: "syntax".into(),
            ..ObjectError::default()
        };
        let mut o = ApplyOutcome {
            steps: vec![StepResult {
                kind: deadlock_core::objects::StepKind::Replace,
                description: String::new(),
                status: StepStatus::Failed,
                elapsed: std::time::Duration::ZERO,
                error: Some(err.clone()),
            }],
            state: FinalState::Unchanged,
            notices: Vec::new(),
            errors: vec![err],
            refresh: Vec::new(),
        };
        let b = outcome_banner(&o, "t", "wh", "Replaced");
        assert_eq!(b.tone, Tone::Error);
        assert!(b.text.contains("not touched") && b.text.ends_with("1064 (42000) syntax"));
        o.state = FinalState::Restored;
        assert_eq!(outcome_banner(&o, "t", "wh", "Replaced").tone, Tone::Warn);
        o.state = FinalState::Invalid;
        assert!(outcome_banner(&o, "t", "wh", "Replaced").revert);
        o.state = FinalState::Missing {
            backup: "CREATE …;".into(),
            path: None,
        };
        let b = outcome_banner(&o, "t", "wh", "Replaced");
        assert!(b.text.contains("missing on wh") && b.backup.is_some());
        o.state = FinalState::Changed;
        let b = outcome_banner(&o, "t", "wh", "Dropped");
        assert_eq!((b.tone, b.text.as_str()), (Tone::Info, "Dropped t."));
        assert!(b.errors.is_empty());
    }
}
