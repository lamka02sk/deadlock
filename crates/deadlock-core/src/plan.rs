//! Explain plans: an engine-neutral tree of steps, and the parsers that build it from
//! what MySQL and MariaDB report.
//!
//! - MySQL and MariaDB `EXPLAIN FORMAT=JSON`, and MariaDB `ANALYZE FORMAT=JSON`: [`json`].
//! - MySQL `EXPLAIN ANALYZE` (the tree format): [`mysql_tree`].
//!
//! Every value is optional: a step carries only what the server reported. Keys and
//! steps the parsers do not know become generic steps or facts, never errors.

use std::fmt::Write as _;
use std::ops::Range;

use serde_json::{Map, Value as Json};

use crate::split::{block_comment_end, line_end, quoted_end};

/// What a step does, as far as the plain-language description needs to know.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Op {
    /// Reads every row of a table.
    FullScan,
    /// Reads a whole index.
    IndexScan,
    /// Reads the part of an index within a range.
    RangeScan,
    /// Finds rows through an index, by value.
    Lookup,
    /// Worked out once, before the query runs.
    Const,
    Join,
    Filter,
    Sort,
    Group,
    /// Stores rows in a temporary table: derived tables, subqueries, unions.
    Materialize,
    Union,
    Window,
    Limit,
    /// One `SELECT` of the statement.
    Query,
    /// Changes the rows its children find: `UPDATE`, `DELETE`.
    Write,
    #[default]
    Other,
}

/// One step of a plan.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanNode {
    pub op: Op,
    /// `Table scan`, `Nested loop inner join`.
    pub title: String,
    /// `on customers`, a condition, a sort key.
    pub detail: String,
    /// The table as the server names it: the alias when the statement gives one.
    pub table: Option<String>,
    pub index: Option<String>,
    /// Rows one run of the step returns or reads, as the optimizer expects.
    pub estimated_rows: Option<f64>,
    pub cost: Option<f64>,
    /// Rows one run returned or read, averaged over the runs.
    pub actual_rows: Option<f64>,
    /// How many times the step ran.
    pub loops: Option<f64>,
    /// Time spent in this step itself, over all runs.
    pub own_ms: Option<f64>,
    /// Time spent in this step and the steps below it, over all runs, where the server
    /// reports it.
    pub total_ms: Option<f64>,
    /// Everything else the server reported, as label and value.
    pub facts: Vec<(String, String)>,
    pub children: Vec<PlanNode>,
}

/// A whole plan.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub steps: Vec<PlanNode>,
    /// The statement ran: times and actual rows may be present.
    pub measured: bool,
    pub execution_ms: Option<f64>,
    pub planning_ms: Option<f64>,
    pub rows_returned: Option<f64>,
    /// The optimizer's cost for the whole statement.
    pub cost: Option<f64>,
    /// The plan as the server wrote it.
    pub raw: String,
    /// The session's default database when the plan was made.
    pub database: Option<String>,
}

impl PlanNode {
    /// `Table scan on customers`.
    pub fn label(&self) -> String {
        if self.detail.is_empty() {
            self.title.clone()
        } else {
            format!("{} {}", self.title, self.detail)
        }
    }

    /// Whether the estimate and the actual rows differ by more than 10×. Values below
    /// one row count as one, so 0.2 rows against 1 expected is not flagged.
    pub fn misestimated(&self) -> bool {
        match (self.estimated_rows, self.actual_rows) {
            (Some(e), Some(a)) => {
                let (e, a) = (e.max(1.), a.max(1.));
                e / a > 10. || a / e > 10.
            }
            _ => false,
        }
    }

    /// A sentence saying what the step does. `table` is the table's real name when it
    /// is known; otherwise the name the server used.
    pub fn describe(&self, table: Option<&str>) -> String {
        let t = table.or(self.table.as_deref());
        let internal = t.is_none_or(|t| t.starts_with('<'));
        let t = t.unwrap_or("a table");
        let index = self.index.as_deref();
        let lower = self.title.to_ascii_lowercase();
        let mut s = match self.op {
            Op::FullScan if internal => {
                "Reads back the rows of an internal temporary table.".into()
            }
            Op::FullScan => format!("Reads every row of {t}."),
            Op::IndexScan => match index {
                Some(i) => format!("Reads all of index {i} of {t}, in index order."),
                None => format!("Reads a whole index of {t}, in index order."),
            },
            Op::RangeScan => match index {
                Some(i) => format!("Reads the part of index {i} of {t} that falls within a range."),
                None => format!("Reads the part of an index of {t} that falls within a range."),
            },
            Op::Lookup if internal => {
                "Looks up rows in a temporary table that an earlier step built.".into()
            }
            Op::Lookup => {
                let one = lower.contains("single-row") || lower.contains("unique");
                let what = if one {
                    "at most one row"
                } else {
                    "the matching rows"
                };
                match index {
                    Some(i) => format!("Looks up {what} of {t} through index {i}."),
                    None => format!("Looks up {what} of {t} through an index."),
                }
            }
            Op::Const => match self.table.as_deref() {
                Some(_) if !internal => {
                    format!("Reads at most one row of {t}, once, before the query runs.")
                }
                _ => "The server worked this out before running the query.".into(),
            },
            Op::Join if lower.contains("hash") => {
                "Joins its inputs with a hash table built from one of them.".into()
            }
            Op::Join => {
                "Joins its inputs: for each row of the first, it looks for matches in the next."
                    .into()
            }
            Op::Filter => "Keeps only the rows that match its condition.".into(),
            Op::Sort => {
                "Sorts the rows, which needs all of them before it returns the first.".into()
            }
            Op::Group if lower.contains("temporary") || self.has_fact("Temporary table") => {
                "Groups the rows and computes the aggregates in a temporary table.".into()
            }
            Op::Group => "Groups the rows and computes the aggregates.".into(),
            Op::Materialize => {
                "Stores the rows of a subquery, derived table or union in a temporary table, \
                 so they can be read again."
                    .into()
            }
            Op::Union => "Combines the rows of the SELECTs below it.".into(),
            Op::Window => "Computes window functions over the rows.".into(),
            Op::Limit => "Stops once it has the rows the query asks for.".into(),
            Op::Query => "One SELECT of the statement; its steps are below it.".into(),
            Op::Write if internal => "Changes the rows that the steps below it find.".into(),
            Op::Write => format!("Changes the rows of {t} that the steps below it find."),
            Op::Other => format!("A step the server reports as “{}”.", self.title),
        };
        if let Some(n) = self.loops.filter(|n| *n > 1.) {
            let _ = write!(s, " It ran {} times.", rows(n));
        }
        s
    }

    fn has_fact(&self, label: &str) -> bool {
        self.facts.iter().any(|(l, v)| l == label && v != "no")
    }

    /// This step's time and its children's, from what the server reported.
    pub fn inclusive_ms(&self) -> Option<f64> {
        self.total_ms.or_else(|| {
            let children = sum(self.children.iter().map(Self::inclusive_ms));
            match (self.own_ms, children) {
                (None, None) => None,
                (o, c) => Some(o.unwrap_or(0.) + c.unwrap_or(0.)),
            }
        })
    }
}

fn sum(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    values
        .flatten()
        .fold(None, |acc, v| Some(acc.unwrap_or(0.) + v))
}

impl Plan {
    /// Every step in display order, with its depth.
    pub fn walk(&self) -> Vec<(usize, &PlanNode)> {
        fn go<'a>(n: &'a PlanNode, depth: usize, out: &mut Vec<(usize, &'a PlanNode)>) {
            out.push((depth, n));
            for c in &n.children {
                go(c, depth + 1, out);
            }
        }
        let mut out = Vec::new();
        for s in &self.steps {
            go(s, 0, &mut out);
        }
        out
    }

    /// The sum of every step's own time: the base of each step's share.
    pub fn own_total(&self) -> f64 {
        self.walk().iter().filter_map(|(_, n)| n.own_ms).sum()
    }

    /// The position in [`Plan::walk`] of the step with the most own time, if any took time.
    pub fn slowest(&self) -> Option<usize> {
        self.walk()
            .iter()
            .enumerate()
            .filter_map(|(i, (_, n))| Some((i, n.own_ms.filter(|t| *t > 0.)?)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    }

    /// Fills the own times the server did not report and the plan's execution time.
    fn finish(mut self) -> Self {
        for s in &mut self.steps {
            derive_own(s);
        }
        if self.measured && self.execution_ms.is_none() {
            self.execution_ms = self.steps.first().and_then(PlanNode::inclusive_ms);
        }
        self
    }
}

/// Fills `own_ms` from inclusive times, depth first.
///
/// The servers report how long a step took including the steps below it: MySQL's
/// `actual time=first..last` is the time to a run's last row, averaged over its runs,
/// so a step's inclusive time is `last × loops`; MariaDB's `query_block` carries an
/// inclusive `r_total_time_ms`. A step's own time is its inclusive time minus the
/// inclusive times of its children. Where the server already reports a step's own time
/// (MariaDB's `r_table_time_ms + r_other_time_ms`) that is kept, and its inclusive time
/// is its own plus its children's.
///
/// The result is floored at zero: averaging over runs, and children that are counted
/// in more than one parent (a materialized subquery read on every run, a dependent
/// subquery listed beside the step that calls it), can make the difference negative.
fn derive_own(n: &mut PlanNode) {
    for c in &mut n.children {
        derive_own(c);
    }
    if let (Some(total), None) = (n.total_ms, n.own_ms) {
        let below = sum(n.children.iter().map(PlanNode::inclusive_ms)).unwrap_or(0.);
        n.own_ms = Some((total - below).max(0.));
    }
}

// ---------------------------------------------------------------------------
// JSON: MySQL and MariaDB `EXPLAIN FORMAT=JSON`, MariaDB `ANALYZE FORMAT=JSON`.

/// Keys that are steps of their own.
const STEPS: &[&str] = &[
    "query_block",
    "table",
    "nested_loop",
    "ordering_operation",
    "grouping_operation",
    "duplicates_removal",
    "duplicate_removal",
    "windowing",
    "window_functions_computation",
    "union_result",
    "buffer_result",
    "materialized_from_subquery",
    "materialized",
    "filesort",
    "temporary_table",
    "read_sorted_file",
    "subquery_cache",
    "expression_cache",
    "block-nl-join",
];

/// Keys whose contents are steps, but which are not steps themselves.
const WRAPPERS: &[&str] = &[
    "query_specifications",
    "select_list_subqueries",
    "attached_subqueries",
    "optimized_away_subqueries",
    "having_subqueries",
    "order_by_subqueries",
    "group_by_subqueries",
    "update_value_subqueries",
    "subqueries",
    "sorts",
];

fn is_plan_key(k: &str) -> bool {
    STEPS.contains(&k) || WRAPPERS.contains(&k)
}

/// Whether `v` holds a step anywhere inside it.
fn holds_steps(v: &Json) -> bool {
    match v {
        Json::Object(o) => o.iter().any(|(k, v)| is_plan_key(k) || holds_steps(v)),
        Json::Array(a) => a.iter().any(holds_steps),
        _ => false,
    }
}

/// Parses a JSON plan. `measured`: it came from MariaDB's `ANALYZE`.
///
/// # Errors
/// The text is not JSON.
pub fn json(raw: &str, measured: bool) -> Result<Plan, String> {
    let root: Json =
        serde_json::from_str(raw).map_err(|e| format!("the server's plan is not JSON: {e}"))?;
    let planning_ms = root
        .pointer("/query_optimization/r_total_time_ms")
        .and_then(number);
    let mut steps: Vec<PlanNode> = match &root {
        Json::Object(o) => o
            .iter()
            .filter(|(k, _)| *k != "query_optimization")
            .flat_map(|(k, v)| steps_of(k, v))
            .collect(),
        _ => Vec::new(),
    };
    // A shape nobody told us about (MySQL's `explain_json_format_version = 2`, say):
    // one generic step with everything as facts; the Text view has the rest.
    if steps.is_empty()
        && let Json::Object(o) = &root
    {
        steps.push(step("plan", o));
    }
    let cost = steps
        .first()
        .filter(|s| s.op == Op::Query)
        .and_then(|s| s.cost);
    Ok(Plan {
        steps,
        measured,
        planning_ms,
        cost,
        raw: raw.to_owned(),
        ..Plan::default()
    }
    .finish())
}

/// The steps under key `k`.
fn steps_of(k: &str, v: &Json) -> Vec<PlanNode> {
    match v {
        Json::Array(a) if k == "nested_loop" => {
            let mut inputs: Vec<PlanNode> = a.iter().flat_map(|e| steps_of("", e)).collect();
            if inputs.len() == 1 {
                return inputs;
            }
            let mut join = PlanNode {
                op: Op::Join,
                title: "Nested loop join".into(),
                ..PlanNode::default()
            };
            join.children.append(&mut inputs);
            vec![join]
        }
        Json::Array(a) => a.iter().flat_map(|e| steps_of(k, e)).collect(),
        Json::Object(o) if STEPS.contains(&k) => vec![step(k, o)],
        // A wrapper or an array element: its steps, with its own values on the first.
        Json::Object(o) if WRAPPERS.contains(&k) || k.is_empty() => {
            let mut facts = Vec::new();
            let mut out = Vec::new();
            for (k, v) in o {
                if holds_steps(v) || is_plan_key(k) {
                    out.extend(steps_of(k, v));
                } else {
                    flatten(k, v, &mut facts);
                }
            }
            if let Some(first) = out.first_mut() {
                facts.append(&mut first.facts);
                first.facts = facts;
            }
            out
        }
        // A key nobody told us about that holds steps: a generic step.
        Json::Object(o) if holds_steps(v) => vec![step(k, o)],
        _ => Vec::new(),
    }
}

/// One step from its key and object.
fn step(k: &str, o: &Map<String, Json>) -> PlanNode {
    let mut n = PlanNode::default();
    let mut facts = Vec::new();
    for (key, v) in o {
        if holds_steps(v) || is_plan_key(key) {
            n.children.extend(steps_of(key, v));
        } else {
            flatten(key, v, &mut facts);
        }
    }
    let text = |key: &str| o.get(key).and_then(Json::as_str).map(str::to_owned);
    let num = |key: &str| o.get(key).and_then(number);
    let flag = |key: &str| o.get(key).is_some_and(truthy);
    let cost_info = |key: &str| o.get("cost_info").and_then(|c| c.get(key)).and_then(number);

    n.table = text("table_name");
    n.index = text("key");
    n.estimated_rows = num("rows_examined_per_scan").or_else(|| num("rows"));
    n.actual_rows = num("r_rows").or_else(|| num("r_output_rows"));
    n.loops = num("r_loops");
    n.cost = num("cost")
        .or_else(|| cost_info("query_cost"))
        .or_else(|| Some(cost_info("read_cost")? + cost_info("eval_cost")?));
    let engine = sum([num("r_table_time_ms"), num("r_other_time_ms")].into_iter());
    if k == "query_block" {
        n.total_ms = num("r_total_time_ms");
    } else {
        n.own_ms = engine.or_else(|| num("r_total_time_ms"));
    }

    let table = n.table.clone().unwrap_or_default();
    let on = |what: &str| match &n.index {
        Some(i) if !table.is_empty() => format!("{what}{table} using {i}"),
        _ => format!("{what}{table}"),
    };
    let (op, title, detail) = match k {
        "query_block" => {
            let title = match num("select_id") {
                Some(id) => format!("Select #{id}"),
                None => "Query".into(),
            };
            let detail = text("operation")
                .or_else(|| text("message"))
                .unwrap_or_default()
                .to_ascii_lowercase();
            (Op::Query, title, detail)
        }
        "table" => {
            let (op, title) = access(text("access_type").as_deref());
            let write = ["update", "delete", "insert", "replace"]
                .into_iter()
                .find(|w| flag(w));
            match write {
                Some(w) => {
                    let verb = format!("{}{}", w[..1].to_ascii_uppercase(), &w[1..]);
                    let how = title.to_ascii_lowercase();
                    let detail = match &n.index {
                        Some(i) => format!("{table} via {how} using {i}"),
                        None => format!("{table} via {how}"),
                    };
                    (Op::Write, verb, detail)
                }
                None => (op, title.to_owned(), on("on ")),
            }
        }
        "ordering_operation" if flag("using_filesort") => (Op::Sort, "Sort".into(), String::new()),
        "ordering_operation" => (Op::Sort, "Order".into(), "read in index order".into()),
        "grouping_operation" => {
            let how = if flag("using_temporary_table") {
                "using a temporary table"
            } else if flag("using_filesort") {
                "using a sort"
            } else {
                ""
            };
            (Op::Group, "Group".into(), how.into())
        }
        "duplicates_removal" | "duplicate_removal" => {
            (Op::Other, "Remove duplicates".into(), String::new())
        }
        "windowing" | "window_functions_computation" => {
            (Op::Window, "Window functions".into(), String::new())
        }
        "union_result" => {
            let title = if flag("using_temporary_table") || table.starts_with("<union") {
                "Union"
            } else {
                "Union all"
            };
            (Op::Union, title.into(), table.clone())
        }
        "buffer_result" => (Op::Materialize, "Buffer result".into(), String::new()),
        "materialized_from_subquery" | "materialized" => {
            (Op::Materialize, "Materialize".into(), String::new())
        }
        "filesort" => (
            Op::Sort,
            "Sort".into(),
            text("sort_key").unwrap_or_default(),
        ),
        "temporary_table" => (Op::Materialize, "Temporary table".into(), String::new()),
        "read_sorted_file" => (Op::Sort, "Read sorted rows".into(), String::new()),
        "subquery_cache" | "expression_cache" => (Op::Other, humanize(k), String::new()),
        "block-nl-join" => (
            Op::Join,
            "Block nested loop join".into(),
            text("join_type").unwrap_or_default(),
        ),
        _ => (Op::Other, humanize(k), String::new()),
    };
    n.op = op;
    n.title = title;
    n.detail = detail;
    n.facts = facts;
    n
}

/// Title for a table's `access_type`.
fn access(t: Option<&str>) -> (Op, &'static str) {
    match t {
        Some("ALL") => (Op::FullScan, "Full table scan"),
        Some("index") => (Op::IndexScan, "Full index scan"),
        Some("range") => (Op::RangeScan, "Index range scan"),
        Some("index_merge") => (Op::RangeScan, "Index merge"),
        Some("ref" | "ref_or_null") => (Op::Lookup, "Index lookup"),
        Some("eq_ref") => (Op::Lookup, "Unique index lookup"),
        Some("unique_subquery" | "index_subquery") => (Op::Lookup, "Subquery index lookup"),
        Some("fulltext") => (Op::Lookup, "Full-text search"),
        Some("const" | "system") => (Op::Const, "Constant row"),
        _ => (Op::Other, "Table access"),
    }
}

/// Keys whose values are already in the typed fields or the title.
const CONSUMED: &[&str] = &[
    "table_name",
    "key",
    "access_type",
    "rows_examined_per_scan",
    "rows",
    "r_rows",
    "r_output_rows",
    "r_loops",
    "cost",
    "query_cost",
    "r_total_time_ms",
    "select_id",
    "update",
    "delete",
    "insert",
    "replace",
];

/// Labels for the keys the servers use most.
fn label(key: &str) -> Option<&'static str> {
    Some(match key {
        "attached_condition" => "Condition",
        "index_condition" => "Index condition",
        "having_condition" => "Having",
        "possible_keys" => "Possible indexes",
        "used_key_parts" => "Key parts used",
        "key_length" => "Key length",
        "ref" => "Compared with",
        "rows_produced_per_join" => "Rows produced (estimate)",
        "filtered" => "Rows kept (estimate)",
        "r_filtered" => "Rows kept",
        "r_total_filtered" => "Rows kept, all conditions",
        "using_index" => "Covering index",
        "used_columns" => "Columns read",
        "data_read_per_join" => "Data read (estimate)",
        "read_cost" => "Read cost",
        "eval_cost" => "Evaluation cost",
        "prefix_cost" => "Cost so far",
        "sort_cost" => "Sort cost",
        "using_filesort" => "Filesort",
        "using_temporary_table" => "Temporary table",
        "pages_accessed" => "Pages accessed",
        "pages_updated" => "Pages updated",
        "pages_read_count" => "Pages read from disk",
        "pages_read_time_ms" => "Time reading pages",
        "r_table_time_ms" => "Time in storage engine",
        "r_other_time_ms" => "Other time",
        "loops" => "Runs (estimate)",
        "r_table_loops" => "Table runs",
        "sort_key" => "Sort key",
        "filesort_key" => "Sort key",
        "r_buffer_size" => "Sort buffer",
        "r_sort_mode" => "Sort mode",
        "r_used_priority_queue" => "Priority queue",
        "r_limit" => "Limit",
        "dependent" => "Dependent",
        "cacheable" => "Cacheable",
        "message" => "Message",
        "operation" => "Operation",
        "functions" => "Functions",
        "name" => "Name",
        "r_hit_ratio" => "Cache hits",
        "state" => "State",
        "buffer_type" => "Buffer",
        "join_type" => "Join type",
        "distinct" => "Distinct",
        "unique" => "Unique",
        _ => return None,
    })
}

/// `key_length` → `Key length`.
fn humanize(key: &str) -> String {
    let words = key.trim_start_matches("r_").replace(['_', '-'], " ");
    let mut c = words.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// Adds `key`'s value to `facts`; objects without steps add their own values.
fn flatten(key: &str, v: &Json, facts: &mut Vec<(String, String)>) {
    if CONSUMED.contains(&key) {
        return;
    }
    let value = match v {
        Json::Null => return,
        Json::Object(o) => {
            for (k, v) in o {
                flatten(k, v, facts);
            }
            return;
        }
        Json::Array(a) if a.iter().any(Json::is_object) => {
            for e in a {
                flatten(key, e, facts);
            }
            return;
        }
        Json::Array(a) => a.iter().map(scalar).collect::<Vec<_>>().join(", "),
        v => scalar(v),
    };
    let percent = matches!(
        key,
        "filtered" | "r_filtered" | "r_total_filtered" | "r_hit_ratio"
    );
    let ms = key.ends_with("_ms");
    let value = match number(v) {
        Some(n) if percent => format!("{}%", decimal(n)),
        Some(n) if ms => millis(n),
        _ => value,
    };
    let label = label(key).map_or_else(|| humanize(key), str::to_owned);
    facts.push((label, value));
}

fn scalar(v: &Json) -> String {
    match v {
        Json::Bool(true) => "yes".into(),
        Json::Bool(false) => "no".into(),
        Json::String(s) => s.clone(),
        Json::Number(n) => n.as_f64().map_or_else(|| n.to_string(), decimal),
        other => other.to_string(),
    }
}

/// A number, or a string holding one: MySQL writes costs and percentages as strings.
fn number(v: &Json) -> Option<f64> {
    match v {
        Json::Number(n) => n.as_f64(),
        Json::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn truthy(v: &Json) -> bool {
    match v {
        Json::Bool(b) => *b,
        Json::Number(n) => n.as_f64().is_some_and(|n| n != 0.),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// MySQL `EXPLAIN ANALYZE`: an indented tree of `-> step  (cost=… rows=…) (actual …)`.

/// Parses MySQL's `EXPLAIN ANALYZE` output.
///
/// # Errors
/// MySQL could not measure the statement: it writes a single
/// `<not executable by iterator executor>` step, or nothing.
pub fn mysql_tree(raw: &str) -> Result<Plan, String> {
    // The open steps, deepest last, with their depth.
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    let mut steps = Vec::new();
    let close = |stack: &mut Vec<(usize, PlanNode)>, steps: &mut Vec<PlanNode>, depth: usize| {
        while stack.last().is_some_and(|(d, _)| *d >= depth) {
            if let Some((_, n)) = stack.pop() {
                match stack.last_mut() {
                    Some((_, parent)) => parent.children.push(n),
                    None => steps.push(n),
                }
            }
        }
    };
    for line in raw.lines() {
        let trimmed = line.trim_start();
        let Some(text) = trimmed.strip_prefix("->") else {
            // A line break inside a step's text (a string in a condition).
            if let Some((_, n)) = stack.last_mut()
                && !trimmed.is_empty()
            {
                n.detail.push(' ');
                n.detail.push_str(trimmed);
            }
            continue;
        };
        let depth = (line.len() - trimmed.len()) / 4;
        close(&mut stack, &mut steps, depth);
        stack.push((depth, tree_step(text.trim())));
    }
    close(&mut stack, &mut steps, 0);
    if steps.is_empty() || steps.iter().any(|s| s.title.starts_with("<not executable")) {
        return Err(
            "MySQL cannot measure this statement: EXPLAIN ANALYZE runs only SELECT, \
                    TABLE and multi-table UPDATE and DELETE. The estimated plan works for it."
                .into(),
        );
    }
    // A write's top step reports the rows it returned to the client: none.
    let rows_returned = steps
        .first()
        .filter(|s| s.op != Op::Write)
        .and_then(|s| Some(s.actual_rows? * s.loops.unwrap_or(1.)));
    Ok(Plan {
        steps,
        measured: true,
        rows_returned,
        raw: raw.to_owned(),
        ..Plan::default()
    }
    .finish())
}

/// One line of the tree, without its arrow.
fn tree_step(text: &str) -> PlanNode {
    let cut = [
        "  (cost=",
        "  (actual time=",
        "  (never executed)",
        " (cost=",
        " (actual time=",
    ]
    .iter()
    .filter_map(|m| text.find(m))
    .min()
    .unwrap_or(text.len());
    let (head, metrics) = text.split_at(cut);
    let head = head.trim_end();
    let mut n = PlanNode::default();
    if let Some(m) = between(metrics, "(cost=", ")") {
        let mut parts = m.split_whitespace();
        n.cost = parts
            .next()
            .and_then(|c| c.rsplit("..").next())
            .and_then(|c| c.parse().ok());
        n.estimated_rows = parts
            .find_map(|p| p.strip_prefix("rows="))
            .and_then(|r| r.parse().ok());
    }
    if let Some(m) = between(metrics, "(actual time=", ")") {
        let mut parts = m.split_whitespace();
        let last: Option<f64> = parts
            .next()
            .and_then(|t| t.rsplit("..").next())
            .and_then(|t| t.parse().ok());
        for p in parts {
            if let Some(r) = p.strip_prefix("rows=") {
                n.actual_rows = r.parse().ok();
            } else if let Some(l) = p.strip_prefix("loops=") {
                n.loops = l.parse().ok();
            }
        }
        n.total_ms = last.map(|t| t * n.loops.unwrap_or(1.));
    } else if metrics.contains("(never executed)") {
        n.actual_rows = Some(0.);
        n.loops = Some(0.);
        n.total_ms = Some(0.);
    }
    let lower = head.to_ascii_lowercase();
    let starts = |p: &[&str]| p.iter().any(|p| lower.starts_with(p));
    n.op = if starts(&["table scan on "]) {
        Op::FullScan
    } else if starts(&["index scan on ", "covering index scan on "]) {
        Op::IndexScan
    } else if starts(&[
        "index range scan on ",
        "covering index range scan on ",
        "index skip scan on ",
    ]) {
        Op::RangeScan
    } else if lower.contains("index lookup on ") || starts(&["full-text index search on "]) {
        Op::Lookup
    } else if starts(&[
        "constant row from ",
        "rows fetched before execution",
        "zero rows",
    ]) {
        Op::Const
    } else if lower.contains(" join") || lower.ends_with("semijoin") || lower.ends_with("antijoin")
    {
        Op::Join
    } else if starts(&["filter:"]) {
        Op::Filter
    } else if starts(&["sort"]) {
        Op::Sort
    } else if starts(&["aggregate", "group aggregate"]) {
        Op::Group
    } else if starts(&["materialize", "union materialize", "temporary table"]) {
        Op::Materialize
    } else if starts(&["limit"]) {
        Op::Limit
    } else if starts(&["window"]) {
        Op::Window
    } else if starts(&["select #"]) {
        Op::Query
    } else if starts(&["update ", "delete from ", "insert into "]) {
        Op::Write
    } else {
        Op::Other
    };
    // `Index lookup on c using PRIMARY (id=o.customer_id)`: the table after ` on `, the
    // index after ` using `.
    let scan = matches!(
        n.op,
        Op::FullScan | Op::IndexScan | Op::RangeScan | Op::Lookup | Op::Const
    );
    if scan && let Some(at) = head.find(" on ") {
        n.title = head[..at].to_owned();
        n.detail = head[at + 1..].to_owned();
        n.table = word_after(head, " on ");
        n.index = word_after(head, " using ");
    } else if n.op == Op::Write {
        // `Update o (immediate)`, `Delete from t`.
        let at = head.find(' ').unwrap_or(head.len());
        n.title = head[..at].to_owned();
        n.detail = head[at..].trim().to_owned();
        n.table = word_after(head, " from ").or_else(|| word_after(head, " "));
    } else if let Some(at) = head.find(": ") {
        n.title = head[..at].to_owned();
        n.detail = head[at + 2..].to_owned();
    } else {
        n.title = head.to_owned();
    }
    n
}

/// The text between `open` and the next `close` in `s`.
fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = s.find(open)? + open.len();
    let len = s[start..].find(close)?;
    Some(&s[start..start + len])
}

/// The word after the first `marker`: up to a space, a comma or a parenthesis.
fn word_after(s: &str, marker: &str) -> Option<String> {
    let rest = &s[s.find(marker)? + marker.len()..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == ',' || c == '(')
        .unwrap_or(rest.len());
    let w = rest[..end].trim_matches('`');
    (!w.is_empty()).then(|| w.to_owned())
}

// ---------------------------------------------------------------------------
// Oracle.

/// Parses Oracle plan rows: a JSON array with one object per step, keyed by the
/// lower-case `PLAN_TABLE` / `V$SQL_PLAN_STATISTICS_ALL` column names, plus `starts`,
/// `output_rows`, `elapsed_us`, `buffer_gets`, `disk_reads` and `memory` (the `LAST_*`
/// columns) when measured. `raw` is `DBMS_XPLAN`'s text.
///
/// # Errors
/// The rows are not a JSON array, or hold no steps.
pub fn oracle(rows: &str, measured: bool, raw: String) -> Result<Plan, String> {
    let rows: Vec<Map<String, Json>> = serde_json::from_str(rows)
        .map_err(|e| format!("the server's plan rows are not JSON: {e}"))?;
    // Step ids as written; a step's children come after it, which also ends any loop.
    let id = |o: &Map<String, Json>, k: &str| {
        o.get(k)
            .map(scalar)
            .filter(|v| !v.is_empty() && v != "null")
    };
    let links: Vec<(Option<String>, Option<String>)> = rows
        .iter()
        .map(|r| (id(r, "id"), id(r, "parent_id")))
        .collect();
    fn build(
        rows: &[Map<String, Json>],
        links: &[(Option<String>, Option<String>)],
        at: usize,
        measured: bool,
    ) -> PlanNode {
        let mut n = oracle_step(&rows[at], measured);
        let me = &links[at].0;
        n.children = (at + 1..rows.len())
            .filter(|&i| me.is_some() && links[i].1 == *me)
            .map(|i| build(rows, links, i, measured))
            .collect();
        n
    }
    let roots: Vec<PlanNode> = (0..rows.len())
        .filter(|&i| links[i].1.is_none())
        .map(|i| build(&rows, &links, i, measured))
        .collect();
    if roots.is_empty() {
        return Err("the server returned no plan steps".into());
    }
    // The statement row (id 0) carries no run statistics: its first child does.
    let top = roots.first();
    let rows_returned = top.and_then(|r| r.actual_rows.or_else(|| r.children.first()?.actual_rows));
    let cost = top.and_then(|r| r.cost);
    Ok(Plan {
        steps: roots,
        measured,
        cost,
        rows_returned: rows_returned.filter(|_| measured),
        raw,
        ..Plan::default()
    }
    .finish())
}

/// One Oracle step: `TABLE ACCESS FULL on CUSTOMERS`.
fn oracle_step(o: &Map<String, Json>, measured: bool) -> PlanNode {
    let text = |k: &str| o.get(k).and_then(Json::as_str).map(str::to_owned);
    let operation = text("operation").unwrap_or_default();
    let options = text("options").unwrap_or_default();
    let title = [operation.as_str(), options.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let op = match (operation.as_str(), options.as_str()) {
        ("TABLE ACCESS", "FULL") | ("MAT_VIEW ACCESS", "FULL") => Op::FullScan,
        ("TABLE ACCESS" | "MAT_VIEW ACCESS", _) => Op::Lookup,
        ("INDEX", o) if o.starts_with("UNIQUE") => Op::Lookup,
        ("INDEX", o) if o.starts_with("RANGE") => Op::RangeScan,
        ("INDEX" | "BITMAP INDEX", _) => Op::IndexScan,
        ("NESTED LOOPS" | "HASH JOIN" | "MERGE JOIN", _) => Op::Join,
        ("FILTER", _) => Op::Filter,
        ("SORT", "AGGREGATE" | "GROUP BY" | "GROUP BY NOSORT") | ("HASH", "GROUP BY") => Op::Group,
        ("SORT" | "BUFFER", _) => Op::Sort,
        ("UNION-ALL" | "UNION ALL" | "CONCATENATION" | "MINUS" | "INTERSECTION", _) => Op::Union,
        ("WINDOW", _) => Op::Window,
        ("COUNT", "STOPKEY") | ("FIRST ROW", _) => Op::Limit,
        ("TEMP TABLE TRANSFORMATION" | "LOAD AS SELECT", _) => Op::Materialize,
        ("FAST DUAL", _) => Op::Const,
        (o, _) if o.ends_with(" STATEMENT") => Op::Query,
        ("UPDATE" | "DELETE" | "INSERT" | "MERGE" | "LOAD TABLE", _) => Op::Write,
        _ => Op::Other,
    };
    // `O@SEL$1` → `O`: the name the statement gives the table.
    let alias = text("object_alias")
        .map(|a| {
            a.split('@')
                .next()
                .unwrap_or_default()
                .trim_matches('"')
                .to_owned()
        })
        .filter(|a| !a.is_empty());
    let object = text("object_name");
    let is_index = operation.contains("INDEX");
    let (table, index) = if is_index {
        (alias, object.clone())
    } else {
        (alias.or_else(|| object.clone()), None)
    };
    let detail = match &object {
        Some(name) if is_index => name.clone(),
        Some(name) => format!("on {name}"),
        None => String::new(),
    };
    let num = |k: &str| o.get(k).and_then(number);
    let starts = num("starts").filter(|_| measured);
    let output = num("output_rows").filter(|_| measured);
    let mut facts = Vec::new();
    for (key, label) in [
        ("access_predicates", "Access"),
        ("filter_predicates", "Filter"),
        ("bytes", "Bytes (estimate)"),
        ("cpu_cost", "CPU cost"),
        ("io_cost", "I/O cost"),
        ("time", "Time (estimate, s)"),
        ("partition_start", "Partition start"),
        ("partition_stop", "Partition stop"),
        ("buffer_gets", "Buffer gets"),
        ("disk_reads", "Disk reads"),
        ("memory", "Memory used (bytes)"),
        ("object_type", "Object type"),
        ("qblock_name", "Query block"),
    ] {
        if let Some(v) = o.get(key).filter(|v| !v.is_null()) {
            facts.push((label.to_owned(), scalar(v)));
        }
    }
    PlanNode {
        op,
        title,
        detail,
        table,
        index,
        estimated_rows: num("cardinality"),
        cost: num("cost"),
        // Oracle counts a step's rows over all its runs; the model wants one run's.
        actual_rows: output.map(|r| r / starts.unwrap_or(1.).max(1.)),
        loops: starts,
        total_ms: num("elapsed_us").filter(|_| measured).map(|us| us / 1000.),
        own_ms: None,
        facts,
        children: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Finding a table in the statement.

/// Where a table is named in a statement, and which table it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// Bytes of the statement: the table name and its alias, `orders o`.
    pub range: Range<usize>,
    /// When the statement names it: `shop.orders`.
    pub database: Option<String>,
    pub table: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// A name or keyword, without its backquotes.
    Word(String),
    Dot,
    Comma,
    Other,
}

/// The tokens of a MySQL statement with their byte ranges. Strings and comments are
/// single `Other` tokens or skipped.
fn tokens(text: &str) -> Vec<(Tok, Range<usize>)> {
    let b = text.as_bytes();
    let ident = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$') || c >= 0x80;
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let next = b.get(i + 1).copied();
        let (tok, end) = match b[i] {
            b'`' => {
                let end = quoted_end(b, i, false);
                let inner = text.get(i + 1..end.saturating_sub(1)).unwrap_or_default();
                (Some(Tok::Word(inner.replace("``", "`"))), end)
            }
            b'\'' | b'"' => (Some(Tok::Other), quoted_end(b, i, true)),
            b'#' => (None, line_end(b, i)),
            b'-' if next == Some(b'-') => (None, line_end(b, i)),
            b'/' if next == Some(b'*') => (None, block_comment_end(b, i)),
            b'.' => (Some(Tok::Dot), i + 1),
            b',' => (Some(Tok::Comma), i + 1),
            c if ident(c) => {
                let end = b[i..]
                    .iter()
                    .position(|&c| !ident(c))
                    .map_or(b.len(), |n| i + n);
                (Some(Tok::Word(text[i..end].to_owned())), end)
            }
            c if c.is_ascii_whitespace() => (None, i + 1),
            _ => (Some(Tok::Other), i + 1),
        };
        if let Some(t) = tok {
            out.push((t, i..end));
        }
        i = end.max(i + 1);
    }
    out
}

/// The first place `statement` names the table the plan calls `name`: either the
/// table itself (`FROM orders`) or its alias (`FROM orders o`, `JOIN orders AS o`).
/// `None` for internal names such as `<derived2>` and for derived tables.
pub fn table_reference(statement: &str, name: &str) -> Option<Reference> {
    if name.is_empty() || name.starts_with('<') {
        return None;
    }
    let toks = tokens(statement);
    let word = |i: usize| match toks.get(i) {
        Some((Tok::Word(w), _)) => Some(w.as_str()),
        _ => None,
    };
    let is = |i: usize, kw: &[&str]| {
        word(i).is_some_and(|w| kw.iter().any(|k| w.eq_ignore_ascii_case(k)))
    };
    let from_seen = |i: usize| (0..i).any(|j| is(j, &["FROM", "UPDATE"]));
    // Starts a table reference: the word before a table name.
    let intro = |i: Option<usize>| {
        i.is_some_and(|i| {
            is(
                i,
                &["FROM", "JOIN", "UPDATE", "INTO", "STRAIGHT_JOIN", "TABLE"],
            ) || toks[i].0 == Tok::Comma && from_seen(i)
        })
    };
    // A table name ending at token `end`, with an optional `db.` before it:
    // (first token, database).
    let qualified = |end: usize| -> Option<(usize, Option<String>)> {
        word(end)?;
        if end >= 2 && toks[end - 1].0 == Tok::Dot {
            let db = word(end - 2)?;
            return Some((end - 2, Some(db.to_owned())));
        }
        Some((end, None))
    };
    for i in 0..toks.len() {
        if !word(i).is_some_and(|w| w.eq_ignore_ascii_case(name)) {
            continue;
        }
        let range_to = |start: usize| toks[start].1.start..toks[i].1.end;
        // The table itself.
        if let Some((start, database)) = qualified(i)
            && intro(start.checked_sub(1))
        {
            return Some(Reference {
                range: range_to(start),
                database,
                table: word(i)?.to_owned(),
            });
        }
        // An alias: `orders o` or `orders AS o`.
        let t = if is(i.wrapping_sub(1), &["AS"]) {
            i.checked_sub(2)
        } else {
            i.checked_sub(1)
        };
        if let Some(t) = t
            && toks[t].0 != Tok::Dot
            && let Some((start, database)) = qualified(t)
            && intro(start.checked_sub(1))
        {
            return Some(Reference {
                range: range_to(start),
                database,
                table: word(t)?.to_owned(),
            });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Numbers as the Plan tab shows them.

/// `18 412`, `1.87`, `0.008`: whole numbers with a space every three digits, others
/// with up to three significant decimals.
pub fn rows(n: f64) -> String {
    if n.fract() == 0. && n.abs() < 1e15 {
        group(format!("{n:.0}"))
    } else if n.abs() >= 100. {
        group(format!("{:.0}", n.round()))
    } else {
        decimal(n)
    }
}

/// `14.8 ms`, `0.05 ms`, `1.2 s`.
pub fn millis(ms: f64) -> String {
    if ms >= 1000. {
        format!("{:.1} s", ms / 1000.)
    } else if ms >= 10. {
        format!("{ms:.1} ms")
    } else if ms >= 0.1 {
        format!("{} ms", trim(format!("{ms:.2}")))
    } else if ms > 0. {
        format!("{} ms", trim(format!("{ms:.3}")))
    } else {
        "0 ms".into()
    }
}

/// A number with up to three decimals, without trailing zeros: `412.12`, `0.008`.
fn decimal(n: f64) -> String {
    if n.fract() == 0. && n.abs() < 1e15 {
        return group(format!("{n:.0}"));
    }
    let digits = if n.abs() >= 1. { 2 } else { 3 };
    trim(format!("{n:.digits$}"))
}

fn trim(s: String) -> String {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        s
    }
}

/// `18412` → `18 412`.
fn group(digits: String) -> String {
    let (sign, d) = digits
        .strip_prefix('-')
        .map_or(("", digits.as_str()), |d| ("-", d));
    let mut out = String::with_capacity(d.len() + d.len() / 3);
    for (i, c) in d.chars().enumerate() {
        if i > 0 && (d.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    format!("{sign}{out}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(own: Option<f64>, total: Option<f64>, children: Vec<PlanNode>) -> PlanNode {
        PlanNode {
            own_ms: own,
            total_ms: total,
            children,
            ..PlanNode::default()
        }
    }

    #[test]
    fn own_time_is_inclusive_minus_children() {
        // MySQL: inclusive totals everywhere.
        let mut n = node(
            None,
            Some(10.),
            vec![
                node(None, Some(4.), vec![node(None, Some(1.), vec![])]),
                node(None, Some(5.), vec![]),
            ],
        );
        derive_own(&mut n);
        assert_eq!(n.own_ms, Some(1.));
        assert_eq!(n.children[0].own_ms, Some(3.));
        assert_eq!(n.children[0].children[0].own_ms, Some(1.));
        assert_eq!(n.children[1].own_ms, Some(5.));
        // Children counted elsewhere too never make a step negative.
        let mut n = node(None, Some(2.), vec![node(None, Some(5.), vec![])]);
        derive_own(&mut n);
        assert_eq!(n.own_ms, Some(0.));
        // MariaDB: a block's inclusive time over tables that report their own time,
        // through a step that reports none.
        let mut n = node(
            None,
            Some(10.),
            vec![node(
                None,
                None,
                vec![node(Some(3.), None, vec![]), node(Some(4.), None, vec![])],
            )],
        );
        derive_own(&mut n);
        assert_eq!(n.own_ms, Some(3.));
        assert_eq!(n.children[0].own_ms, None);
        assert_eq!(n.children[0].inclusive_ms(), Some(7.));
    }

    #[test]
    fn tree_lines_carry_cost_rows_and_times() {
        let n = tree_step(
            "Index lookup on i using order_id (order_id=o.id)  (cost=0.467 rows=1.87) \
             (actual time=0.00449..0.00766 rows=2 loops=20000)",
        );
        assert_eq!(n.op, Op::Lookup);
        assert_eq!(
            (n.title.as_str(), n.detail.as_str()),
            ("Index lookup", "on i using order_id (order_id=o.id)")
        );
        assert_eq!(
            (n.table.as_deref(), n.index.as_deref()),
            (Some("i"), Some("order_id"))
        );
        assert_eq!((n.cost, n.estimated_rows), (Some(0.467), Some(1.87)));
        assert_eq!((n.actual_rows, n.loops), (Some(2.), Some(20000.)));
        assert!((n.total_ms.unwrap() - 153.2).abs() < 1e-9);
        let n = tree_step("Filter: (c.country = 'SK')  (cost=402 rows=4000) (never executed)");
        assert_eq!(
            (n.op, n.detail.as_str()),
            (Op::Filter, "(c.country = 'SK')")
        );
        assert_eq!((n.loops, n.total_ms), (Some(0.), Some(0.)));
        let n = tree_step("Nested loop inner join  (cost=40.9e+6 rows=409e+6)");
        assert_eq!((n.op, n.cost), (Op::Join, Some(40.9e6)));
        assert_eq!(tree_step("Something new").op, Op::Other);
    }

    #[test]
    fn unmeasurable_statements_say_so() {
        assert!(mysql_tree("-> <not executable by iterator executor>\n").is_err());
        assert!(mysql_tree("").is_err());
    }

    #[test]
    fn unknown_json_degrades_to_generic_steps() {
        let raw = r#"{"query_block": {"select_id": 1, "brand_new_op": {"mystery": 3,
            "table": {"table_name": "t", "access_type": "teleport", "strange": [1, 2]}}}}"#;
        let p = json(raw, false).unwrap();
        let block = &p.steps[0];
        assert_eq!(block.title, "Select #1");
        let generic = &block.children[0];
        assert_eq!(
            (generic.op, generic.title.as_str()),
            (Op::Other, "Brand new op")
        );
        assert_eq!(generic.facts, [("Mystery".to_owned(), "3".to_owned())]);
        let t = &generic.children[0];
        assert_eq!(
            (t.op, t.title.as_str(), t.detail.as_str()),
            (Op::Other, "Table access", "on t")
        );
        assert_eq!(t.facts, [("Strange".to_owned(), "1, 2".to_owned())]);
        assert!(json("[1, 2]", false).unwrap().steps.is_empty());
        let p = json(
            r#"{"operation": "Table scan on t", "inputs": [{"x": 1}]}"#,
            false,
        )
        .unwrap();
        assert_eq!((p.steps.len(), p.steps[0].title.as_str()), (1, "Plan"));
        assert!(
            p.steps[0]
                .facts
                .iter()
                .any(|(l, v)| l == "Operation" && v == "Table scan on t")
        );
        assert!(json("not json", false).is_err());
    }

    #[test]
    fn aliases_resolve_to_their_tables() {
        let q = "SELECT o.id FROM orders o JOIN shop.customers AS c ON c.id = o.customer_id, \
                 items WHERE 'orders x' <> ''";
        let r = table_reference(q, "o").unwrap();
        assert_eq!(
            (&q[r.range.clone()], r.table.as_str(), r.database),
            ("orders o", "orders", None)
        );
        let r = table_reference(q, "c").unwrap();
        assert_eq!(&q[r.range.clone()], "shop.customers AS c");
        assert_eq!(
            (r.table.as_str(), r.database.as_deref()),
            ("customers", Some("shop"))
        );
        let r = table_reference(q, "items").unwrap();
        assert_eq!((&q[r.range], r.table.as_str()), ("items", "items"));
        assert_eq!(table_reference(q, "x"), None);
        assert_eq!(table_reference(q, "<derived2>"), None);
        let q = "SELECT t.n FROM (SELECT 1 AS n) t";
        assert_eq!(table_reference(q, "t"), None);
        let q = "UPDATE `my orders` o SET o.total = 1";
        assert_eq!(table_reference(q, "o").unwrap().table, "my orders");
    }

    #[test]
    fn misestimates_flag_past_tenfold() {
        let n = |e: f64, a: f64| PlanNode {
            estimated_rows: Some(e),
            actual_rows: Some(a),
            ..PlanNode::default()
        };
        assert!(n(19890., 0.).misestimated());
        assert!(n(10., 101.).misestimated());
        assert!(!n(10., 100.).misestimated());
        assert!(!n(1., 0.008).misestimated());
        assert!(!PlanNode::default().misestimated());
    }

    #[test]
    fn numbers_read_like_the_design() {
        assert_eq!(rows(18412.), "18 412");
        assert_eq!(rows(1.87), "1.87");
        assert_eq!(rows(0.008), "0.008");
        assert_eq!(rows(409e6), "409 000 000");
        assert_eq!(rows(230.4), "230");
        assert_eq!(millis(14.83), "14.8 ms");
        assert_eq!(millis(0.6), "0.6 ms");
        assert_eq!(millis(0.0499), "0.05 ms");
        assert_eq!(millis(1250.), "1.2 s");
        assert_eq!(millis(0.), "0 ms");
        assert_eq!(decimal(412.123), "412.12");
    }
}
