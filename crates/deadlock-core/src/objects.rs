//! Database objects other than tables: views, materialized views, procedures,
//! functions, packages, triggers, events, sequences, synonyms and types.
//!
//! The flow the object editor follows:
//!
//! 1. [`crate::driver::Driver::object`] loads an [`ObjectDefinition`]: typed
//!    [`Properties`], the server's source, status, grants and compile errors.
//!    New objects start from [`template`] instead.
//! 2. The editor changes a copy of [`ObjectDefinition::draft`]. On every keystroke
//!    [`plan`] turns the old definition and the draft into a [`ChangePlan`]: the
//!    ordered statements Apply runs, grouped in [`Step`]s with a description each,
//!    plus whether it is atomic. [`ChangePlan::preview`] is the footer text.
//!    [`drop_plan`], [`rename_plan`], [`recompile_plan`] and [`revert_plan`] build
//!    the other plans; every plan is pure, so the preview is exactly what runs.
//! 3. [`crate::driver::Driver::apply_object`] runs a plan and returns an
//!    [`ApplyOutcome`]: each step's status, the final [`FinalState`], notices,
//!    errors with lines, and the tree nodes to reload ([`ChangePlan::refresh`]).
//!
//! Names are always quoted ([`quote_name`]); data types, defaults and expressions
//! typed in the form are checked for statement separators and comments; bodies and
//! queries are the user's SQL and go in as written.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::driver::{BoxFuture, DriverError, ObjectPath};

mod ddl;
pub(crate) mod parse;

pub use ddl::{
    drop_plan, normalize_name, plan, quote_name, recompile_plan, rename_plan, revert_plan, template,
};

/// The SQL dialect objects are managed in. MySQL and MariaDB share a driver but not
/// every statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flavor {
    Mysql,
    Mariadb,
    Oracle,
}

impl Flavor {
    /// MySQL or MariaDB.
    pub fn is_mysql(self) -> bool {
        matches!(self, Self::Mysql | Self::Mariadb)
    }

    /// `MySQL`, `MariaDB`, `Oracle`: for messages.
    pub fn label(self) -> &'static str {
        match self {
            Self::Mysql => "MySQL",
            Self::Mariadb => "MariaDB",
            Self::Oracle => "Oracle",
        }
    }
}

/// A kind of object the editor manages. Tables and indexes stay in Structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    View,
    /// Oracle.
    MaterializedView,
    Procedure,
    Function,
    /// Oracle: spec and body, edited as source.
    Package,
    Trigger,
    /// MySQL and MariaDB.
    Event,
    /// MariaDB and Oracle.
    Sequence,
    /// Oracle.
    Synonym,
    /// Oracle object and collection types: spec and body, edited as source.
    Type,
}

impl ObjectKind {
    pub const ALL: [Self; 10] = [
        Self::View,
        Self::MaterializedView,
        Self::Procedure,
        Self::Function,
        Self::Package,
        Self::Trigger,
        Self::Event,
        Self::Sequence,
        Self::Synonym,
        Self::Type,
    ];

    /// The kinds `flavor` has, in the tree's order (after Tables).
    pub fn managed(flavor: Flavor) -> &'static [Self] {
        match flavor {
            Flavor::Mysql => &[
                Self::View,
                Self::Procedure,
                Self::Function,
                Self::Trigger,
                Self::Event,
            ],
            Flavor::Mariadb => &[
                Self::View,
                Self::Procedure,
                Self::Function,
                Self::Trigger,
                Self::Event,
                Self::Sequence,
            ],
            Flavor::Oracle => &[
                Self::View,
                Self::MaterializedView,
                Self::Sequence,
                Self::Procedure,
                Self::Function,
                Self::Package,
                Self::Trigger,
                Self::Synonym,
                Self::Type,
            ],
        }
    }

    /// The tree group listing this kind: `Views`, `Materialized views`, `Procedures`…
    pub fn group(self) -> &'static str {
        match self {
            Self::View => "Views",
            Self::MaterializedView => "Materialized views",
            Self::Procedure => "Procedures",
            Self::Function => "Functions",
            Self::Package => "Packages",
            Self::Trigger => "Triggers",
            Self::Event => "Events",
            Self::Sequence => "Sequences",
            Self::Synonym => "Synonyms",
            Self::Type => "Types",
        }
    }

    /// The kind a tree group lists; `None` for Tables and other groups.
    pub fn from_group(group: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.group() == group)
    }

    /// Lower case, as [`crate::search::Hit::kind`] and [`crate::driver::Driver::definition`]
    /// name it: `view`, `materialized view`, `procedure`…
    pub fn label(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::MaterializedView => "materialized view",
            Self::Procedure => "procedure",
            Self::Function => "function",
            Self::Package => "package",
            Self::Trigger => "trigger",
            Self::Event => "event",
            Self::Sequence => "sequence",
            Self::Synonym => "synonym",
            Self::Type => "type",
        }
    }

    /// The kind for a [`Self::label`].
    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.label() == label)
    }

    /// The SQL keyword: `VIEW`, `MATERIALIZED VIEW`, `PROCEDURE`…
    pub fn keyword(self) -> &'static str {
        match self {
            Self::View => "VIEW",
            Self::MaterializedView => "MATERIALIZED VIEW",
            Self::Procedure => "PROCEDURE",
            Self::Function => "FUNCTION",
            Self::Package => "PACKAGE",
            Self::Trigger => "TRIGGER",
            Self::Event => "EVENT",
            Self::Sequence => "SEQUENCE",
            Self::Synonym => "SYNONYM",
            Self::Type => "TYPE",
        }
    }

    /// `Ok` when the context menu's Rename works for this kind on `flavor`; otherwise
    /// the reason to show next to the disabled item. Oracle also needs the object in
    /// the session's own schema ([`ObjectDefinition::own_schema`]); public synonyms
    /// cannot be renamed. A name changed in Properties works everywhere (on kinds
    /// without a rename it drops and creates).
    pub fn can_rename(self, flavor: Flavor) -> Result<(), &'static str> {
        match (flavor, self) {
            (Flavor::Mysql | Flavor::Mariadb, Self::View | Self::Event) => Ok(()),
            (Flavor::Mysql | Flavor::Mariadb, Self::Trigger) => Err(
                "MySQL has no RENAME TRIGGER: change the name in Properties, which drops \
                 and creates the trigger",
            ),
            (Flavor::Mysql | Flavor::Mariadb, _) => Err(
                "MySQL cannot rename this kind of object: change the name in Properties, \
                 which creates it under the new name",
            ),
            (Flavor::Oracle, Self::Trigger | Self::View | Self::Sequence | Self::Synonym) => Ok(()),
            (Flavor::Oracle, _) => Err(
                "Oracle cannot rename this kind of object: change the name in its source, \
                 which creates it under the new name",
            ),
        }
    }
}

/// One object: where it lives and what it is. On Oracle a public synonym's schema is
/// `PUBLIC`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ObjectRef {
    pub schema: String,
    pub name: String,
    pub kind: ObjectKind,
}

impl ObjectRef {
    pub fn new(schema: impl Into<String>, name: impl Into<String>, kind: ObjectKind) -> Self {
        Self {
            schema: schema.into(),
            name: name.into(),
            kind,
        }
    }

    /// The object a sidebar row stands for, from its [`crate::schema::NodeId`]:
    /// `[connection, schema, group, name]`, or a trigger under its table,
    /// `[connection, schema, "Tables", table, "Triggers", name]`. `None` for any
    /// other row (tables, columns, groups).
    pub fn from_node(id: &[String]) -> Option<Self> {
        match id {
            [_, schema, group, name] => {
                Some(Self::new(schema, name, ObjectKind::from_group(group)?))
            }
            [_, schema, tables, _, triggers, name]
                if tables == "Tables" && triggers == "Triggers" =>
            {
                Some(Self::new(schema, name, ObjectKind::Trigger))
            }
            _ => None,
        }
    }
}

/// Fixed keyword choices: each variant writes one SQL keyword.
macro_rules! keywords {
    ($(#[$m:meta])* $name:ident { $($(#[$vm:meta])* $v:ident => $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name { $($(#[$vm])* $v),+ }

        impl $name {
            /// Every choice, in the order a Select lists them; the first is the default.
            pub const ALL: &'static [Self] = &[$(Self::$v),+];

            /// The SQL keyword(s), as the Select shows them.
            pub fn sql(self) -> &'static str {
                match self { $(Self::$v => $s),+ }
            }

            /// The choice whose [`Self::sql`] is `s`, ignoring case and spaces around.
            pub fn parse(s: &str) -> Option<Self> {
                let s = s.trim();
                Self::ALL.iter().copied().find(|k| k.sql().eq_ignore_ascii_case(s))
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::ALL[0]
            }
        }
    };
}

keywords!(
    /// MySQL view algorithm.
    ViewAlgorithm { Undefined => "UNDEFINED", Merge => "MERGE", Temptable => "TEMPTABLE" }
);
keywords!(
    /// MySQL `SQL SECURITY`; Oracle `AUTHID` (where `Invoker` is `CURRENT_USER`).
    Security { Definer => "DEFINER", Invoker => "INVOKER" }
);
keywords!(
    /// A view's check option. MySQL: `Cascaded`, `Local`. Oracle: `Cascaded` is
    /// `WITH CHECK OPTION`, `ReadOnly` is `WITH READ ONLY`.
    ViewCheck { None => "NONE", Cascaded => "CASCADED", Local => "LOCAL", ReadOnly => "READ ONLY" }
);
keywords!(
    /// A parameter's direction. Oracle writes `InOut` as `IN OUT`.
    ParamMode { In => "IN", Out => "OUT", InOut => "INOUT" }
);
keywords!(
    /// MySQL routine data access.
    DataAccess {
        ContainsSql => "CONTAINS SQL",
        NoSql => "NO SQL",
        ReadsSqlData => "READS SQL DATA",
        ModifiesSqlData => "MODIFIES SQL DATA",
    }
);
keywords!(
    /// When a trigger fires. `InsteadOf`: Oracle, on views only.
    Timing { Before => "BEFORE", After => "AFTER", InsteadOf => "INSTEAD OF" }
);
keywords!(
    /// A DML trigger event. MySQL triggers have exactly one.
    TriggerEvent { Insert => "INSERT", Update => "UPDATE", Delete => "DELETE" }
);
keywords!(
    /// `FOLLOWS` or `PRECEDES` another trigger.
    TriggerOrder { Follows => "FOLLOWS", Precedes => "PRECEDES" }
);
keywords!(
    /// MySQL event interval unit.
    IntervalUnit {
        Day => "DAY",
        Hour => "HOUR",
        Minute => "MINUTE",
        Second => "SECOND",
        Week => "WEEK",
        Month => "MONTH",
        Quarter => "QUARTER",
        Year => "YEAR",
        YearMonth => "YEAR_MONTH",
        DayHour => "DAY_HOUR",
        DayMinute => "DAY_MINUTE",
        DaySecond => "DAY_SECOND",
        HourMinute => "HOUR_MINUTE",
        HourSecond => "HOUR_SECOND",
        MinuteSecond => "MINUTE_SECOND",
    }
);
keywords!(
    /// MySQL event status. MariaDB writes `DisableOnReplica` as `DISABLE ON SLAVE`.
    EventStatus { Enable => "ENABLE", Disable => "DISABLE", DisableOnReplica => "DISABLE ON REPLICA" }
);
keywords!(
    /// Oracle materialized view refresh method.
    RefreshMethod { Force => "FORCE", Complete => "COMPLETE", Fast => "FAST", Never => "NEVER" }
);
keywords!(
    /// Oracle materialized view refresh mode.
    RefreshMode { OnDemand => "ON DEMAND", OnCommit => "ON COMMIT" }
);

/// A view. MySQL uses algorithm, definer and security; Oracle uses `force` and
/// `columns`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewProps {
    pub algorithm: ViewAlgorithm,
    /// MySQL `user@host`; empty: the server's default (the current user).
    pub definer: String,
    pub security: Security,
    pub check: ViewCheck,
    /// Oracle `FORCE`: create even when the query does not compile yet.
    pub force: bool,
    /// Oracle: the column list as written between the parentheses; empty for none.
    pub columns: String,
    pub query: String,
}

/// An Oracle materialized view.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MatViewProps {
    pub method: RefreshMethod,
    pub mode: RefreshMode,
    /// `START WITH` expression, e.g. `SYSDATE`; empty for none.
    pub start_with: String,
    /// `NEXT` expression, e.g. `TRUNC(SYSDATE) + 1`; empty for none.
    pub next: String,
    pub query_rewrite: bool,
    /// `BUILD DEFERRED`: only when creating.
    pub build_deferred: bool,
    pub query: String,
}

/// A routine parameter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Param {
    /// MySQL functions take `In` only, and write no mode.
    pub mode: ParamMode,
    pub name: String,
    /// As written: `VARCHAR(64)`, `BIGINT UNSIGNED`, Oracle `NUMBER` (no length).
    pub data_type: String,
    /// Oracle, `In` only: the default expression; empty for none.
    pub default: String,
    /// Oracle `NOCOPY` (for `Out` and `InOut`).
    pub nocopy: bool,
}

/// A procedure or a function ([`ObjectRef::kind`] says which).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutineProps {
    pub params: Vec<Param>,
    /// Functions: the return type.
    pub returns: String,
    pub deterministic: bool,
    /// MySQL.
    pub data_access: Option<DataAccess>,
    /// MySQL `SQL SECURITY`; Oracle `AUTHID`.
    pub security: Security,
    /// MySQL.
    pub comment: String,
    /// MySQL `user@host`; empty: the current user.
    pub definer: String,
    /// Oracle functions.
    pub result_cache: bool,
    /// Oracle functions.
    pub parallel_enable: bool,
    /// MySQL: the routine body (`BEGIN … END`). Oracle: everything after `IS`,
    /// declarations included.
    pub body: String,
}

/// A DML trigger. Oracle compound, DDL and database-event triggers are
/// [`Properties::Source`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TriggerProps {
    pub table: String,
    /// Oracle: the table's schema when it is not the trigger's; empty otherwise.
    pub table_schema: String,
    pub timing: Timing,
    /// MySQL: exactly one.
    pub events: Vec<TriggerEvent>,
    /// Oracle `UPDATE OF` columns.
    pub update_columns: Vec<String>,
    /// MySQL: always true.
    pub for_each_row: bool,
    /// Oracle `WHEN` condition, without the parentheses (row level only).
    pub when: String,
    pub order: Option<(TriggerOrder, String)>,
    /// MySQL `user@host`; empty: the current user.
    pub definer: String,
    /// Oracle; MySQL triggers are always enabled.
    pub enabled: bool,
    /// MySQL: the statement (`BEGIN … END`). Oracle: the PL/SQL block.
    pub body: String,
}

/// When a MySQL event runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    /// Once, at a timestamp (`2026-11-25 02:00:00`), in the session time zone.
    At(String),
    /// Repeatedly. `starts` and `ends` are timestamps, empty for none.
    Every {
        value: String,
        unit: IntervalUnit,
        starts: String,
        ends: String,
    },
}

impl Default for Schedule {
    fn default() -> Self {
        Self::Every {
            value: "1".into(),
            unit: IntervalUnit::Day,
            starts: String::new(),
            ends: String::new(),
        }
    }
}

/// A MySQL event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventProps {
    pub schedule: Schedule,
    /// `ON COMPLETION PRESERVE`.
    pub preserve: bool,
    pub status: EventStatus,
    pub comment: String,
    /// `user@host`; empty: the current user.
    pub definer: String,
    /// The `DO` statement.
    pub body: String,
}

/// A sequence. Numbers are integer text; empty means the default (`NO MINVALUE`,
/// `NOCACHE`…).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SequenceProps {
    /// Fixed after create on Oracle (use `restart_with`).
    pub start: String,
    pub increment: String,
    pub min: String,
    pub max: String,
    /// Empty or `0`: `NOCACHE`.
    pub cache: String,
    pub cycle: bool,
    /// Oracle `ORDER`.
    pub order: bool,
    /// `RESTART WITH` (Oracle 18c+: `RESTART START WITH`); empty for none.
    pub restart_with: String,
    /// Read only: MariaDB's next uncached value, Oracle's `LAST_NUMBER`.
    pub current: String,
}

/// An Oracle synonym. Public: [`ObjectRef::schema`] is `PUBLIC`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SynonymProps {
    pub public: bool,
    pub target_schema: String,
    pub target: String,
    /// Empty for a local target.
    pub db_link: String,
}

/// Source-first objects: Oracle packages and types, Oracle compound, DDL and
/// database-event triggers, and routines whose header the form cannot represent.
/// Each part is the full source without `CREATE OR REPLACE` (`PACKAGE x AS …`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceProps {
    pub spec: String,
    /// Package or type body; empty for none.
    pub body: String,
}

/// An object's typed properties: what the Properties form edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Properties {
    View(ViewProps),
    MaterializedView(MatViewProps),
    /// Procedure or function.
    Routine(RoutineProps),
    Trigger(TriggerProps),
    Event(EventProps),
    Sequence(SequenceProps),
    Synonym(SynonymProps),
    Source(SourceProps),
}

/// A privilege on the object held by someone else, read before a change that drops
/// the object (MySQL routines, Oracle materialized views and renames) so the change
/// can grant it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// MySQL user name or Oracle grantee.
    pub user: String,
    /// MySQL host; empty on Oracle.
    pub host: String,
    /// `EXECUTE`, `ALTER ROUTINE`, `SELECT`…
    pub privileges: Vec<String>,
    pub grant_option: bool,
}

/// A loaded object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectDefinition {
    pub object: ObjectRef,
    pub flavor: Flavor,
    pub props: Properties,
    /// The CREATE statement(s) as the server reports them: the Source section and
    /// Copy CREATE statement.
    pub source: String,
    /// Statements that recreate the object exactly as loaded: what a failed replace
    /// restores. Keep the definition from before an Apply for Revert to last valid.
    pub restore: Vec<String>,
    /// Why it cannot be changed from here (no body visible, read-only connection,
    /// wrapped PL/SQL): properties show read-only, Apply and Drop are off.
    pub read_only: Option<String>,
    /// Oracle: `(part, valid)` from ALL_OBJECTS, e.g. `("PACKAGE", true),
    /// ("PACKAGE BODY", false)`. Empty on MySQL.
    pub status: Vec<(String, bool)>,
    /// Oracle: the compile errors in ALL_ERRORS now.
    pub errors: Vec<ObjectError>,
    pub grants: Vec<Grant>,
    /// Header facts and read-only summary rows, as `(label, value)`: definer,
    /// created, modified, `resolves to`, last refresh, type attributes and methods.
    pub facts: Vec<(String, String)>,
    /// Triggers: the triggers on the same table with the same timing and event, in
    /// firing order, this one included.
    pub siblings: Vec<String>,
    /// Things to tell (event scheduler off, grants not readable).
    pub notices: Vec<String>,
    /// Oracle: the object is in the session's own schema (RENAME works only there).
    pub own_schema: bool,
}

impl ObjectDefinition {
    /// The editable copy: what the form starts from.
    pub fn draft(&self) -> ObjectDraft {
        ObjectDraft {
            object: self.object.clone(),
            props: self.props.clone(),
        }
    }

    /// Oracle: whether every part is valid; `None` on MySQL.
    pub fn valid(&self) -> Option<bool> {
        (!self.status.is_empty()).then(|| self.status.iter().all(|(_, v)| *v))
    }
}

/// What the editor holds: the object's name (in `object`) and its properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectDraft {
    pub object: ObjectRef,
    pub props: Properties,
}

impl ObjectDraft {
    /// Whether this is a trigger on table `table`.
    pub fn is_on_table(&self, table: &crate::edit::TableRef) -> bool {
        let Properties::Trigger(t) = &self.props else {
            return false;
        };
        let schema = if t.table_schema.is_empty() {
            &self.object.schema
        } else {
            &t.table_schema
        };
        t.table == table.table && *schema == table.database
    }

    /// Table `from` was renamed to `to`: a trigger on it is on `to` now. Returns
    /// whether this is such a trigger.
    pub fn follow_table_rename(&mut self, from: &crate::edit::TableRef, to: &str) -> bool {
        let on = self.is_on_table(from);
        if on && let Properties::Trigger(t) = &mut self.props {
            to.clone_into(&mut t.table);
        }
        on
    }
}

/// What a step does; decides how [`crate::driver::Driver::apply_object`] treats a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    /// Validates the new definition on a throwaway copy, then drops it
    /// ([`Step::cleanup`]). A failure leaves everything as it was.
    Check,
    Alter,
    /// `CREATE OR REPLACE`.
    Replace,
    Create,
    Drop,
    Rename,
    /// `DROP` then `CREATE`: the object is missing between the first statement and
    /// the last. A failure after the drop runs the [`StepKind::Restore`] steps.
    Swap,
    /// Grants read before the change, run again. A failure is a notice.
    Grants,
    /// Only when a step after a drop fails: recreates the original.
    Restore,
    /// Only after a [`StepKind::Restore`] that worked: its grants. A failure is a notice.
    RestoreGrants,
    /// `ALTER … COMPILE`.
    Compile,
}

/// A group of statements with one description, as the preview and the confirmation
/// list them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub kind: StepKind,
    pub description: String,
    pub statements: Vec<String>,
    /// [`StepKind::Check`]: run after `statements` whether they worked or not, but
    /// only when the first one worked (it created what cleanup drops).
    pub cleanup: Vec<String>,
    /// [`StepKind::Check`]: when the first statement fails for lack of a privilege,
    /// skip the check with a notice instead of stopping.
    pub skippable: bool,
}

impl Step {
    fn new(kind: StepKind, description: impl Into<String>, statements: Vec<String>) -> Self {
        Self {
            kind,
            description: description.into(),
            statements,
            cleanup: Vec::new(),
            skippable: false,
        }
    }

    /// Runs only after a failure.
    pub fn conditional(&self) -> bool {
        matches!(self.kind, StepKind::Restore | StepKind::RestoreGrants)
    }
}

/// Everything one Apply (or Drop, Rename, Recompile) runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangePlan {
    pub flavor: Flavor,
    /// The object after the change (for a drop: the object dropped).
    pub object: ObjectRef,
    /// The object before, when it had another name or is being created (`None`).
    pub previous: Option<ObjectRef>,
    pub steps: Vec<Step>,
    /// One statement, or statements that never leave the object missing or half
    /// changed. A non-atomic plan should be confirmed on Production and Stage.
    pub atomic: bool,
    /// The fields that changed, as the form labels them: `Body`, `Comment`.
    pub changes: Vec<String>,
    /// Things to tell before applying (a check that will be skipped, grants that
    /// cannot be re-run).
    pub notices: Vec<String>,
    /// The body as written in the editor, for mapping error lines onto it.
    pub body: String,
    /// Error lines are already relative to the editor (source-first objects).
    pub source_lines: bool,
}

impl ChangePlan {
    /// Nothing to run: the footer reads "No changes".
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Statements that run when nothing fails (checks' cleanup included).
    pub fn statement_count(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| !s.conditional())
            .map(|s| s.statements.len() + s.cleanup.len())
            .sum()
    }

    /// Statements that run only to restore after a failure.
    pub fn restore_count(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| s.conditional())
            .map(|s| s.statements.len())
            .sum()
    }

    /// The chip next to the count: `ALTER`, `CREATE OR REPLACE`, `DROP + CREATE`…
    pub fn verb(&self) -> &'static str {
        let main = self.steps.iter().find(|s| {
            !matches!(
                s.kind,
                StepKind::Check | StepKind::Grants | StepKind::Restore | StepKind::RestoreGrants
            )
        });
        match main.map(|s| s.kind) {
            Some(StepKind::Alter) => "ALTER",
            Some(StepKind::Replace) => "CREATE OR REPLACE",
            Some(StepKind::Create) => "CREATE",
            Some(StepKind::Drop) => "DROP",
            Some(StepKind::Rename) => "RENAME",
            Some(StepKind::Swap) => "DROP + CREATE",
            Some(StepKind::Compile) => "COMPILE",
            _ => "",
        }
    }

    /// The sentence a confirmation shows for a non-atomic plan; `None` when atomic.
    pub fn non_atomic_warning(&self) -> Option<String> {
        if self.atomic {
            return None;
        }
        let swap = self.steps.iter().any(|s| s.kind == StepKind::Swap);
        Some(match (self.flavor, swap) {
            (Flavor::Oracle, true) => "Not atomic: Oracle commits DDL immediately. The object is \
                 dropped and created again, so it is missing in between; the original is \
                 restored automatically if the new definition fails."
                .into(),
            (_, true) => format!(
                "Not atomic: {} commits DDL immediately; the original is restored \
                 automatically if the new definition fails. Between the DROP and the CREATE \
                 the object is missing for a moment.",
                self.flavor.label()
            ),
            _ => format!(
                "Not atomic: {} commits each statement immediately. If a later statement \
                 fails, the earlier ones stay applied.",
                self.flavor.label()
            ),
        })
    }

    /// The tree nodes to reload after this plan ran, relative to the connection
    /// (`[schema]`: the schema's groups load whole).
    pub fn refresh(&self) -> Vec<ObjectPath> {
        let mut out = vec![ObjectPath(vec![self.object.schema.clone()])];
        if let Some(p) = &self.previous
            && p.schema != self.object.schema
        {
            out.push(ObjectPath(vec![p.schema.clone()]));
        }
        out
    }

    /// The footer's text: every statement in order with step comments, restore steps
    /// commented out. Exactly what Apply sends, plus terminators.
    pub fn preview(&self) -> String {
        if self.steps.is_empty() {
            return "-- No changes".into();
        }
        let mut out = String::new();
        if let Some(w) = self.non_atomic_warning() {
            for line in wrap(&w, 80) {
                out.push_str("-- ");
                out.push_str(&line);
                out.push('\n');
            }
        }
        let numbered = self.steps.len() > 1;
        for (i, step) in self.steps.iter().enumerate() {
            if numbered || !step.description.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                if numbered {
                    out.push_str(&format!("-- {}. {}\n", i + 1, step.description));
                } else {
                    out.push_str(&format!("-- {}\n", step.description));
                }
            }
            for s in step.statements.iter().chain(&step.cleanup) {
                let text = terminated(self.flavor, s);
                if step.conditional() {
                    for line in text.lines() {
                        out.push_str("-- ");
                        out.push_str(line);
                        out.push('\n');
                    }
                } else {
                    out.push_str(&text);
                    out.push('\n');
                }
            }
        }
        out
    }
}

/// `s` as a script would hold it: `;` after SQL, `/` after Oracle PL/SQL.
fn terminated(flavor: Flavor, s: &str) -> String {
    let s = s.trim_end();
    if flavor == Flavor::Oracle && s.ends_with(';') {
        format!("{s}\n/")
    } else {
        format!("{s};")
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    for w in text.split_whitespace() {
        let last = lines.last_mut().expect("one line");
        if !last.is_empty() && last.len() + w.len() + 1 > width {
            lines.push(w.to_owned());
        } else {
            if !last.is_empty() {
                last.push(' ');
            }
            last.push_str(w);
        }
    }
    lines
}

/// An error from the server, with its place when it has one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectError {
    /// `1064 (42000)` on MySQL; `PLS-00201`, `ORA-00942` on Oracle; empty when unknown.
    pub code: String,
    pub message: String,
    /// The line in the statement that failed (Oracle: in the stored source).
    pub line: Option<u32>,
    /// Oracle: the column.
    pub position: Option<u32>,
    /// The line in the editor's body (1-based), when the error falls inside it.
    pub body_line: Option<u32>,
    /// Oracle: the part the error is in (`PACKAGE BODY`, `TRIGGER`); empty otherwise.
    pub part: String,
    /// Oracle: a compiler warning, not an error.
    pub warning: bool,
}

impl ObjectError {
    /// A failed statement's error; `body` is the editor's body, to give the line in it.
    pub fn from_driver(e: &DriverError, statement: &str, body: &str) -> Self {
        let mut out = match e {
            DriverError::Mysql(mysql_async::Error::Server(s)) => Self {
                code: format!("{} ({})", s.code, s.state),
                message: s.message.clone(),
                line: parse::mysql_error_line(&s.message),
                ..Self::default()
            },
            other => {
                let text = other.to_string();
                Self {
                    code: parse::oracle_code(&text).unwrap_or_default(),
                    message: text,
                    ..Self::default()
                }
            }
        };
        out.body_line = out.line.and_then(|l| body_line(statement, body, l));
        out
    }
}

/// `line` of `statement` as a line of `body`, when the body is in the statement and
/// the line falls inside it.
pub(crate) fn body_line(statement: &str, body: &str, line: u32) -> Option<u32> {
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    let at = statement.find(body)?;
    let first = u32::try_from(statement[..at].matches('\n').count()).ok()? + 1;
    let last = first + u32::try_from(body.matches('\n').count()).ok()?;
    (first..=last).contains(&line).then(|| line - first + 1)
}

/// How a step went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Done,
    Failed,
    /// A check skipped for lack of a privilege.
    Skipped,
    /// Not run: an earlier step failed, or a restore step that was not needed.
    NotRun,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepResult {
    pub kind: StepKind,
    pub description: String,
    pub status: StepStatus,
    pub elapsed: Duration,
    pub error: Option<ObjectError>,
}

/// Where the object ended up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalState {
    /// Everything ran.
    Changed,
    /// Oracle: everything ran, but the object compiled with errors and is INVALID.
    /// [`ApplyOutcome::errors`] lists them; offer Revert to last valid.
    Invalid,
    /// Nothing changed: the check or the first statement failed.
    Unchanged,
    /// Some statements ran before one failed (non-atomic plans).
    Partial,
    /// The new definition failed after the drop; the original is back.
    Restored,
    /// The object is gone: the new definition and the restore both failed. `backup`
    /// holds the original's statements; `path` is where they were written, when the
    /// apply had a [`Backup`] and the write worked.
    Missing {
        backup: String,
        path: Option<PathBuf>,
    },
}

/// What one apply did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOutcome {
    pub steps: Vec<StepResult>,
    pub state: FinalState,
    pub notices: Vec<String>,
    /// The error that stopped it, and on Oracle the compile errors after it.
    pub errors: Vec<ObjectError>,
    /// The tree nodes to reload ([`ChangePlan::refresh`]).
    pub refresh: Vec<ObjectPath>,
}

/// Where [`crate::driver::Driver::apply_object`] writes the original when an object
/// ends up missing: `<root>/.deadlock/backups/<connection>-<schema>-<type>-<name>-<timestamp>.sql`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backup {
    /// The project folder.
    pub root: PathBuf,
    /// The connection id.
    pub connection: String,
}

impl Backup {
    /// Writes `text` for `object`; the file's path.
    ///
    /// # Errors
    /// The folder or the file cannot be written.
    pub fn write(&self, object: &ObjectRef, text: &str) -> crate::Result<PathBuf> {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let stamp = chrono::DateTime::from_timestamp(i64::try_from(secs).unwrap_or(0), 0)
            .map(|t| t.format("%Y%m%d-%H%M%S").to_string())
            .unwrap_or_default();
        let path = backup_path(&self.root, &self.connection, object, &stamp);
        let dir = path.parent().unwrap_or(&self.root);
        std::fs::create_dir_all(dir).map_err(crate::io_err(dir))?;
        crate::write_atomic(&path, text.as_bytes())?;
        Ok(path)
    }
}

/// The backup file for `object` taken at `stamp` (`20261124-104200`, UTC).
pub fn backup_path(root: &Path, connection: &str, object: &ObjectRef, stamp: &str) -> PathBuf {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_alphanumeric() || matches!(c, '_' | '.' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let name = format!(
        "{}-{}-{}-{}-{stamp}.sql",
        clean(connection),
        clean(&object.schema),
        clean(&object.kind.label().replace(' ', "_")),
        clean(&object.name)
    );
    root.join(".deadlock").join("backups").join(name)
}

/// Runs statements for [`execute`]: one session, in order.
pub(crate) trait Runner: Send {
    fn run<'a>(&'a mut self, sql: &'a str) -> BoxFuture<'a, Result<(), DriverError>>;

    /// The error says the account lacks a privilege: a skippable check is skipped.
    fn denied(&self, e: &DriverError) -> bool;
}

/// Runs `plan` step by step, as [`StepKind`] describes: stops at the first failure,
/// restores after a failed swap, writes the backup when the restore fails too.
pub(crate) async fn execute(
    plan: &ChangePlan,
    runner: &mut dyn Runner,
    backup: Option<&Backup>,
) -> ApplyOutcome {
    let mut out = ApplyOutcome {
        steps: Vec::with_capacity(plan.steps.len()),
        state: FinalState::Changed,
        notices: Vec::new(),
        errors: Vec::new(),
        refresh: plan.refresh(),
    };
    let (mut dropped, mut changed, mut failed, mut missing) = (false, false, false, false);
    for step in &plan.steps {
        let started = Instant::now();
        let mut result = StepResult {
            kind: step.kind,
            description: step.description.clone(),
            status: StepStatus::NotRun,
            elapsed: Duration::ZERO,
            error: None,
        };
        if failed || step.conditional() {
            out.steps.push(result);
            continue;
        }
        match step.kind {
            StepKind::Grants => {
                result.status = StepStatus::Done;
                for s in &step.statements {
                    if let Err(e) = runner.run(s).await {
                        result.status = StepStatus::Failed;
                        out.notices.push(format!("Granting again failed: {s}: {e}"));
                    }
                }
            }
            kind => {
                let (first_ok, r) = run_all(runner, &step.statements).await;
                if kind == StepKind::Check && first_ok {
                    for c in &step.cleanup {
                        if let Err(e) = runner.run(c).await {
                            out.notices.push(format!(
                                "Cleaning up after the check failed ({e}); run it by hand: {c}"
                            ));
                        }
                    }
                }
                match r {
                    Ok(()) => {
                        result.status = StepStatus::Done;
                        match kind {
                            StepKind::Check => {}
                            StepKind::Drop => {
                                dropped = true;
                                changed = true;
                            }
                            StepKind::Swap => {
                                dropped = false;
                                changed = true;
                            }
                            _ => changed = true,
                        }
                    }
                    Err((0, e))
                        if kind == StepKind::Check && step.skippable && runner.denied(&e) =>
                    {
                        result.status = StepStatus::Skipped;
                        out.notices.push(format!(
                            "The check was skipped: this account may not create the copy it \
                             needs ({e}). The change was applied without it."
                        ));
                    }
                    Err((i, e)) => {
                        result.status = StepStatus::Failed;
                        let statement = &step.statements[i];
                        let err = if plan.source_lines {
                            ObjectError {
                                body_line: None,
                                ..ObjectError::from_driver(&e, statement, "")
                            }
                        } else {
                            ObjectError::from_driver(&e, statement, &plan.body)
                        };
                        result.error = Some(err.clone());
                        out.errors.push(err);
                        failed = true;
                        // A swap's first statement is the drop.
                        missing = dropped || kind == StepKind::Swap && i > 0;
                        out.state = if changed || missing {
                            FinalState::Partial
                        } else {
                            FinalState::Unchanged
                        };
                    }
                }
            }
        }
        result.elapsed = started.elapsed();
        out.steps.push(result);
    }
    if missing {
        out.state = restore(plan, runner, &mut out, backup).await;
    }
    out
}

/// Runs `statements` in order; whether the first worked, and the first failure with
/// its index.
async fn run_all(
    runner: &mut dyn Runner,
    statements: &[String],
) -> (bool, Result<(), (usize, DriverError)>) {
    for (i, s) in statements.iter().enumerate() {
        if let Err(e) = runner.run(s).await {
            return (i > 0, Err((i, e)));
        }
    }
    (!statements.is_empty(), Ok(()))
}

/// After a failure that left the object missing: the restore steps, then their
/// grants. Updates the restore steps' results in `out`.
async fn restore(
    plan: &ChangePlan,
    runner: &mut dyn Runner,
    out: &mut ApplyOutcome,
    backup: Option<&Backup>,
) -> FinalState {
    let text: String = plan
        .steps
        .iter()
        .filter(|s| s.kind == StepKind::Restore)
        .flat_map(|s| &s.statements)
        .map(|s| terminated(plan.flavor, s) + "\n")
        .collect();
    let mut ok = !text.is_empty();
    for (at, step) in plan.steps.iter().enumerate() {
        if step.kind != StepKind::Restore || !ok {
            continue;
        }
        let started = Instant::now();
        let (_, r) = run_all(runner, &step.statements).await;
        let result = &mut out.steps[at];
        result.status = StepStatus::Done;
        if let Err((i, e)) = r {
            ok = false;
            result.status = StepStatus::Failed;
            let err = ObjectError::from_driver(&e, &step.statements[i], "");
            result.error = Some(err.clone());
            out.errors.push(err);
        }
        result.elapsed = started.elapsed();
    }
    if !ok {
        let previous = plan.previous.as_ref().unwrap_or(&plan.object);
        let path = backup.and_then(|b| match b.write(previous, &text) {
            Ok(p) => Some(p),
            Err(e) => {
                out.notices
                    .push(format!("Writing the backup of the original failed: {e}"));
                None
            }
        });
        return FinalState::Missing { backup: text, path };
    }
    for (at, step) in plan.steps.iter().enumerate() {
        if step.kind != StepKind::RestoreGrants {
            continue;
        }
        let started = Instant::now();
        let mut status = StepStatus::Done;
        for s in &step.statements {
            if let Err(e) = runner.run(s).await {
                status = StepStatus::Failed;
                out.notices.push(format!("Granting again failed: {s}: {e}"));
            }
        }
        out.steps[at].status = status;
        out.steps[at].elapsed = started.elapsed();
    }
    FinalState::Restored
}

#[cfg(test)]
mod tests;
