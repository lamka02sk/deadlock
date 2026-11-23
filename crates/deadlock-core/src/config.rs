//! Server configuration: the engine-neutral model of a server's global variables,
//! their categories and units, value checks against what the server reports, and
//! the statements that change them.
//!
//! Engines report different facts. A fact the server does not report is `None` and
//! the UI leaves it out; nothing here guesses one.

use crate::edit::quote_str;

/// What kind of value a variable takes, as far as the server says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueType {
    Boolean,
    Integer,
    Float,
    Enum(Vec<String>),
    /// Any combination of the listed values, comma separated.
    Set(Vec<String>),
    /// Free text, also when the server does not report a type.
    Text,
}

/// The unit an integer counts in. Inputs scale by [`Unit::steps`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Bytes,
    Seconds,
}

impl Unit {
    /// The unit buttons, smallest first, with their size in the base unit.
    pub fn steps(self) -> &'static [(&'static str, i128)] {
        match self {
            Self::Bytes => &[
                ("bytes", 1),
                ("KB", 1 << 10),
                ("MB", 1 << 20),
                ("GB", 1 << 30),
            ],
            Self::Seconds => &[("s", 1), ("min", 60), ("h", 3600)],
        }
    }
}

/// Where the running value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The compiled-in default.
    Default,
    /// Sized by the server at startup (MariaDB `AUTO`).
    Auto,
    /// An option file; the path when the server reports it.
    ConfigFile(Option<String>),
    CommandLine,
    /// MySQL's `mysqld-auto.cnf`, written by `SET PERSIST`.
    Persisted,
    /// `SET GLOBAL` (or `SET PERSIST`) since the server started.
    Runtime {
        at: Option<String>,
        by: Option<String>,
    },
    /// A source the server names that has no better fit.
    Other(String),
    /// The server does not say where values come from (Redis).
    Unknown,
}

impl Source {
    /// The grid's "Set in" cell.
    pub fn short(&self) -> String {
        match self {
            Self::Default => "default".into(),
            Self::Auto => "server sized".into(),
            Self::ConfigFile(Some(p)) => p.rsplit(['/', '\\']).next().unwrap_or(p).to_owned(),
            Self::ConfigFile(None) => "config file".into(),
            Self::CommandLine => "command line".into(),
            Self::Persisted => "persisted".into(),
            Self::Runtime { .. } => "runtime".into(),
            Self::Other(s) => s.to_lowercase(),
            Self::Unknown => String::new(),
        }
    }

    /// The detail pane's "Set in".
    pub fn long(&self) -> String {
        match self {
            Self::Default => "Built-in default".into(),
            Self::Auto => "Sized by the server at startup".into(),
            Self::ConfigFile(Some(p)) => format!("Config file {p}"),
            Self::ConfigFile(None) => "Config file".into(),
            Self::CommandLine => "Server command line".into(),
            Self::Persisted => "Persisted (mysqld-auto.cnf)".into(),
            Self::Runtime { at, by } => {
                let mut s = "Set while the server runs".to_owned();
                if let Some(by) = by {
                    s.push_str(&format!(" by {by}"));
                }
                if let Some(at) = at {
                    s.push_str(&format!(", {at}"));
                }
                s
            }
            Self::Other(s) => s.clone(),
            Self::Unknown => "Not reported by the server".into(),
        }
    }

    /// Someone set it: anything but the built-in or server-sized default.
    pub fn is_modified(&self) -> bool {
        !matches!(self, Self::Default | Self::Auto | Self::Unknown)
    }
}

/// When a change takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applies {
    /// Right away.
    Now,
    /// At the next server start.
    Restart,
    /// Not settable from SQL at all: only an option file or the command line.
    Fixed,
    /// The server does not say.
    Unknown,
}

impl Applies {
    pub fn short(self) -> &'static str {
        match self {
            Self::Now => "Now",
            Self::Restart => "Restart",
            Self::Fixed => "Startup",
            Self::Unknown => "",
        }
    }
}

/// Which values a variable has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    GlobalOnly,
    /// A global value, and one per session that starts as a copy of it.
    GlobalAndSession,
}

/// One global variable.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigParam {
    /// Lower case.
    pub name: String,
    /// `None`: NULL.
    pub value: Option<String>,
    /// `None`: not reported.
    pub default: Option<String>,
    pub value_type: ValueType,
    pub min: Option<String>,
    pub max: Option<String>,
    /// Values must be a multiple of this (MariaDB reports it).
    pub block: Option<u64>,
    pub unit: Option<Unit>,
    pub source: Source,
    pub applies: Applies,
    pub scope: Option<Scope>,
    pub description: Option<String>,
    /// MySQL: the value `mysqld-auto.cnf` holds, used at the next start. Oracle: the
    /// spfile's.
    pub persisted: Option<String>,
    pub category: Category,
    /// Oracle: a change applies to sessions that start after it (`DEFERRED`).
    pub deferred: bool,
}

impl ConfigParam {
    /// A variable with only a name and a value; drivers fill in what they know.
    pub fn new(name: &str, value: Option<String>) -> Self {
        let name = name.to_ascii_lowercase();
        Self {
            category: categorize(&name),
            name,
            value,
            default: None,
            value_type: ValueType::Text,
            min: None,
            max: None,
            block: None,
            unit: None,
            source: Source::Default,
            applies: Applies::Unknown,
            scope: None,
            description: None,
            persisted: None,
            deferred: false,
        }
    }

    /// The value differs from the default, or (default not reported) was set by someone.
    pub fn is_modified(&self) -> bool {
        match &self.default {
            Some(d) => self.value.as_deref() != Some(d.as_str()) && self.source.is_modified(),
            None => self.source.is_modified(),
        }
    }

    /// A persisted value waits for the next start (MySQL `SET PERSIST_ONLY`).
    pub fn restart_pending(&self) -> bool {
        self.applies != Applies::Now
            && self
                .persisted
                .as_ref()
                .is_some_and(|p| Some(p.as_str()) != self.value.as_deref())
    }

    /// `value` as the grid shows it: sizes and durations in the largest exact unit.
    pub fn display(&self, value: Option<&str>) -> String {
        let Some(v) = value else {
            return "NULL".into();
        };
        match (self.unit, v.parse::<i128>()) {
            (Some(unit), Ok(n)) if n != 0 => unit
                .steps()
                .iter()
                .rev()
                .find(|(_, m)| n % m == 0)
                .map_or_else(
                    || v.to_owned(),
                    |(label, m)| match *label {
                        "bytes" => v.to_owned(),
                        l => format!("{}{l}", n / m),
                    },
                ),
            _ => v.to_owned(),
        }
    }

    /// "Allowed: …" under the editor; `None` when the server reports no limits.
    pub fn allowed(&self) -> Option<String> {
        match &self.value_type {
            ValueType::Integer | ValueType::Float => {
                let (min, max) = (self.min.as_deref()?, self.max.as_deref()?);
                let show = |v: &str| self.display(Some(v));
                let mut s = format!("Allowed: {} to {}", show(min), show(max));
                if let Some(b) = self.block.filter(|b| *b > 1) {
                    s.push_str(&format!(", in steps of {}", show(&b.to_string())));
                }
                Some(s)
            }
            ValueType::Enum(list) => Some(format!("Allowed: {}", list.join(", "))),
            ValueType::Boolean => Some("Allowed: ON, OFF".into()),
            ValueType::Set(_) | ValueType::Text => None,
        }
    }

    /// Checks `input` against what the server reports and returns the value to send,
    /// in the server's own spelling.
    ///
    /// # Errors
    /// A sentence saying what is wrong, for the editor.
    pub fn check(&self, input: &str) -> Result<String, String> {
        let t = input.trim();
        match &self.value_type {
            ValueType::Boolean => match t.to_ascii_uppercase().as_str() {
                "ON" | "1" | "TRUE" => Ok("ON".into()),
                "OFF" | "0" | "FALSE" => Ok("OFF".into()),
                _ => Err("Enter ON or OFF.".into()),
            },
            ValueType::Integer => {
                let digits = t.strip_prefix('-').unwrap_or(t);
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return Err("Enter a whole number.".into());
                }
                let n: i128 = t
                    .parse()
                    .map_err(|_| "The number is too large.".to_owned())?;
                let bound = |b: &Option<String>| b.as_deref().and_then(|b| b.parse::<i128>().ok());
                if let (Some(min), Some(max)) = (bound(&self.min), bound(&self.max))
                    && (n < min || n > max)
                {
                    return Err(format!(
                        "Must be between {} and {}.",
                        self.display(self.min.as_deref()),
                        self.display(self.max.as_deref())
                    ));
                }
                if let Some(b) = self.block.filter(|b| *b > 1)
                    && n % i128::from(b) != 0
                {
                    return Err(format!(
                        "Must be a multiple of {}.",
                        self.display(Some(&b.to_string()))
                    ));
                }
                Ok(n.to_string())
            }
            ValueType::Float => {
                let n: f64 = t
                    .parse()
                    .ok()
                    .filter(|n: &f64| n.is_finite())
                    .ok_or_else(|| "Enter a number.".to_owned())?;
                let bound = |b: &Option<String>| b.as_deref().and_then(|b| b.parse::<f64>().ok());
                if let (Some(min), Some(max)) = (bound(&self.min), bound(&self.max))
                    && (n < min || n > max)
                {
                    return Err(format!(
                        "Must be between {} and {}.",
                        self.min.as_deref().unwrap_or_default(),
                        self.max.as_deref().unwrap_or_default()
                    ));
                }
                Ok(t.to_owned())
            }
            ValueType::Enum(list) => list
                .iter()
                .find(|v| v.eq_ignore_ascii_case(t))
                .cloned()
                .ok_or_else(|| format!("Choose one of {}.", list.join(", "))),
            ValueType::Set(list) => {
                let mut picked = Vec::new();
                for part in t.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    let Some(v) = list.iter().find(|v| v.eq_ignore_ascii_case(part)) else {
                        return Err(format!("{part} is not one of the allowed values."));
                    };
                    if !picked.contains(&v) {
                        picked.push(v);
                    }
                }
                // The server lists a set in its own order.
                Ok(list
                    .iter()
                    .filter(|v| picked.contains(v))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(","))
            }
            ValueType::Text => Ok(input.to_owned()),
        }
    }

    /// The SQL literal for a checked value.
    fn literal(&self, value: &str, backslash_escapes: bool) -> String {
        match self.value_type {
            ValueType::Boolean | ValueType::Integer | ValueType::Float => value.to_owned(),
            _ => quote_str(value, backslash_escapes),
        }
    }
}

/// The parsed number `text` × `step`, when it is a whole number of base units.
/// `1.5` MB is 1572864 bytes.
pub fn scale(text: &str, step: i128) -> Option<i128> {
    let t = text.trim();
    let (negative, t) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t),
    };
    let (int, frac) = t.split_once('.').unwrap_or((t, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if int.is_empty() && frac.is_empty() || !digits(int) || !digits(frac) || frac.len() > 18 {
        return None;
    }
    let whole: i128 = if int.is_empty() { 0 } else { int.parse().ok()? };
    let ten = 10i128.checked_pow(u32::try_from(frac.len()).ok()?)?;
    let f: i128 = if frac.is_empty() {
        0
    } else {
        frac.parse().ok()?
    };
    let scaled_frac = f.checked_mul(step)?;
    if scaled_frac % ten != 0 {
        return None;
    }
    let n = whole.checked_mul(step)?.checked_add(scaled_frac / ten)?;
    Some(if negative { -n } else { n })
}

/// `value` ÷ `step` written out exactly, when it fits in a few decimals.
pub fn unscale(value: i128, step: i128) -> Option<String> {
    if step <= 0 {
        return None;
    }
    let sign = if value < 0 { "-" } else { "" };
    let v = value.unsigned_abs();
    let s = step.unsigned_abs();
    let (int, mut rem) = (v / s, v % s);
    if rem == 0 {
        return Some(format!("{sign}{int}"));
    }
    let mut frac = String::new();
    while rem != 0 {
        if frac.len() == 9 {
            return None;
        }
        rem *= 10;
        frac.push(char::from(b'0' + u8::try_from(rem / s).ok()?));
        rem %= s;
    }
    Some(format!("{sign}{int}.{frac}"))
}

/// A server's configuration, as one screen loads it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServerConfig {
    /// By name.
    pub params: Vec<ConfigParam>,
    /// The option file the server read, when it says.
    pub config_file: Option<String>,
    /// Changes can survive a restart (`SET PERSIST`, MySQL). Otherwise only
    /// `SET GLOBAL` exists (MariaDB).
    pub persist: bool,
    /// Why nothing can be changed (a missing privilege), if so.
    pub read_only: Option<String>,
    /// Why restart-only variables cannot be changed, when the rest can.
    pub restart_read_only: Option<String>,
    /// The session's `sql_mode` lets `\` escape in strings.
    pub backslash_escapes: bool,
    /// Oracle: `ALTER SYSTEM … SCOPE =` statements; `persist` means the server uses an
    /// spfile.
    pub oracle: bool,
    /// Redis: `CONFIG SET`; `persist` means the server has a config file that
    /// `CONFIG REWRITE` can write.
    pub redis: bool,
    /// Where changes apply, when that needs saying: an Oracle pluggable database.
    pub container: Option<String>,
}

impl ServerConfig {
    pub fn param(&self, name: &str) -> Option<&ConfigParam> {
        self.params.iter().find(|p| p.name == name)
    }

    /// Why `p` cannot be changed here, or `None` when it can.
    pub fn locked(&self, p: &ConfigParam) -> Option<String> {
        if let Some(r) = &self.read_only {
            return Some(r.clone());
        }
        if self.redis {
            if crate::redis::SECRETS.contains(&p.name.as_str()) {
                return Some("Deadlock does not show or change passwords here.".into());
            }
            return (p.applies == Applies::Fixed).then(|| {
                "Redis cannot change this while it runs. Set it in redis.conf, then restart.".into()
            });
        }
        if self.oracle {
            return match p.applies {
                Applies::Fixed => Some(
                    "Only the root container (CDB$ROOT) can change this parameter; a \
                     pluggable database cannot."
                        .into(),
                ),
                Applies::Restart if !self.persist => Some(
                    "The server started from a text parameter file (pfile), so this can only \
                     change there, followed by a restart."
                        .into(),
                ),
                Applies::Restart => self.restart_read_only.clone(),
                Applies::Now | Applies::Unknown => None,
            };
        }
        match p.applies {
            Applies::Fixed => Some(
                "The server cannot change this while it runs, not even for the next start. \
                 Set it in the server's option file or command line, then restart."
                    .into(),
            ),
            Applies::Restart if !self.persist => Some(
                "MariaDB cannot change this while the server runs. Set it in the server's \
                 option file, then restart."
                    .into(),
            ),
            Applies::Restart => self.restart_read_only.clone(),
            Applies::Now | Applies::Unknown => None,
        }
    }

    /// Who may change `p`, from the engine's documentation.
    pub fn who_can_set(&self, p: &ConfigParam) -> Option<&'static str> {
        if self.redis {
            return (p.applies != Applies::Fixed).then_some("CONFIG SET (the @admin category)");
        }
        if self.oracle {
            return (p.applies != Applies::Fixed).then_some("ALTER SYSTEM");
        }
        match (self.persist, p.applies) {
            (_, Applies::Fixed) => None,
            (true, Applies::Restart) => {
                Some("SYSTEM_VARIABLES_ADMIN and PERSIST_RO_VARIABLES_ADMIN")
            }
            (true, _) => Some("SYSTEM_VARIABLES_ADMIN or SUPER"),
            (false, _) => Some("SUPER"),
        }
    }

    /// The statements one change runs, in order.
    ///
    /// # Errors
    /// The change cannot be made: an unknown or unsafe name, a value the server would
    /// refuse, or a variable that cannot change from SQL.
    pub fn statements(&self, c: &ConfigChange) -> Result<Vec<String>, String> {
        if self.redis {
            return self.redis_statement(c).map(|s| vec![s]);
        }
        if !valid_name(&c.name) {
            return Err(format!("{:?} is not a variable name", c.name));
        }
        let p = self
            .param(&c.name)
            .ok_or_else(|| format!("the server has no variable {}", c.name))?;
        if let Some(why) = self.locked(p) {
            return Err(why);
        }
        if self.oracle {
            return self.oracle_statement(p, c).map(|s| vec![s]);
        }
        let name = &p.name;
        let value = match &c.value {
            NewValue::Default => "DEFAULT".to_owned(),
            NewValue::Set(v) => p.literal(&p.check(v)?, self.backslash_escapes),
        };
        let reset = format!("RESET PERSIST {name}");
        Ok(match (self.persist, p.applies, &c.value, c.mode) {
            (false, ..) => vec![format!("SET GLOBAL {name} = {value}")],
            (true, Applies::Restart, NewValue::Default, _) => {
                if p.persisted.is_none() {
                    return Err(format!(
                        "{name} is not persisted; its value comes from the server's start-up \
                         options. Change it there."
                    ));
                }
                vec![reset]
            }
            (true, Applies::Restart, NewValue::Set(_), _) => {
                vec![format!("SET PERSIST_ONLY {name} = {value}")]
            }
            (true, _, NewValue::Default, Mode::Persist) => {
                let mut v = vec![format!("SET GLOBAL {name} = DEFAULT")];
                if p.persisted.is_some() {
                    v.push(reset);
                }
                v
            }
            (true, _, NewValue::Set(_), Mode::Persist) => {
                vec![format!("SET PERSIST {name} = {value}")]
            }
            (true, _, _, Mode::RunOnly) => vec![format!("SET GLOBAL {name} = {value}")],
        })
    }

    /// `CONFIG SET name value`. Redis reports no defaults, so there is no reset.
    fn redis_statement(&self, c: &ConfigChange) -> Result<String, String> {
        let p = self
            .param(&c.name)
            .ok_or_else(|| format!("the server has no parameter {}", c.name))?;
        if let Some(why) = self.locked(p) {
            return Err(why);
        }
        match &c.value {
            NewValue::Default => Err(format!(
                "Redis does not report the default of {}: enter the value to set.",
                p.name
            )),
            NewValue::Set(v) => Ok(format!(
                "CONFIG SET {} {}",
                crate::redis::quote(p.name.as_bytes()),
                crate::redis::quoted(p.check(v)?.as_bytes())
            )),
        }
    }

    /// The statement that makes `changes` survive a restart, run after them: Redis's
    /// `CONFIG REWRITE` when a change asks to persist.
    pub fn trailer(&self, changes: &[ConfigChange]) -> Option<&'static str> {
        (self.redis && changes.iter().any(|c| c.mode == Mode::Persist)).then_some("CONFIG REWRITE")
    }

    /// `ALTER SYSTEM SET name = value [DEFERRED] SCOPE = …`, or `RESET` for the default.
    /// Scope: `SPFILE` for what only a restart applies, `MEMORY` for This run only (or
    /// without an spfile), else `BOTH`. A reset of a value the spfile does not hold
    /// resets memory only.
    fn oracle_statement(&self, p: &ConfigParam, c: &ConfigChange) -> Result<String, String> {
        let name = &p.name;
        let scope = match (p.applies, c.mode, self.persist, &c.value) {
            (Applies::Restart, ..) => "SPFILE",
            (_, Mode::RunOnly, ..) | (_, _, false, _) => "MEMORY",
            (_, _, true, NewValue::Default) if p.persisted.is_none() => "MEMORY",
            _ => "BOTH",
        };
        let deferred = if p.deferred && scope != "SPFILE" {
            " DEFERRED"
        } else {
            ""
        };
        Ok(match &c.value {
            NewValue::Default => format!("ALTER SYSTEM RESET {name}{deferred} SCOPE = {scope}"),
            NewValue::Set(v) => {
                let v = p.check(v)?;
                let bare = match &p.value_type {
                    ValueType::Integer | ValueType::Float => true,
                    ValueType::Enum(list) => list.iter().all(|x| x == "TRUE" || x == "FALSE"),
                    _ => false,
                };
                let value = if bare { v } else { quote_str(&v, false) };
                format!("ALTER SYSTEM SET {name} = {value}{deferred} SCOPE = {scope}")
            }
        })
    }

    /// The option-file line that keeps a MariaDB change after a restart.
    pub fn config_line(&self, c: &ConfigChange) -> Option<String> {
        let p = self.param(&c.name)?;
        Some(match &c.value {
            NewValue::Default => format!("# {}: remove its line to use the default", p.name),
            NewValue::Set(v) => {
                let v = p.check(v).ok()?;
                let plain = !v.is_empty()
                    && !v.contains(|c: char| {
                        c.is_whitespace() || matches!(c, '#' | '"' | '\'' | '\\')
                    });
                if plain {
                    format!("{} = {v}", p.name)
                } else {
                    format!(
                        "{} = \"{}\"",
                        p.name,
                        v.replace('\\', "\\\\").replace('"', "\\\"")
                    )
                }
            }
        })
    }
}

/// How a MySQL change is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// `SET PERSIST`: now and after a restart.
    #[default]
    Persist,
    /// `SET GLOBAL`: until the server restarts.
    RunOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewValue {
    /// What the user typed, before [`ConfigParam::check`].
    Set(String),
    /// Back to the server's default.
    Default,
}

/// One pending change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigChange {
    pub name: String,
    pub value: NewValue,
    pub mode: Mode,
}

/// The result of applying changes in order: how many went through before one failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub done: usize,
    /// The error of change `done + 1`, when one failed; later ones did not run.
    pub error: Option<String>,
}

/// A variable name that is safe to write into a statement as it is: one or two dotted
/// words of `[a-z0-9_]` (MySQL component variables have a prefix).
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.split('.').count() <= 2
        && name.split('.').all(|w| {
            !w.is_empty()
                && w.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

/// Names ending in `_size` that count things rather than bytes.
const COUNTS: [&str; 22] = [
    "binlog_transaction_dependency_history_size",
    "delayed_queue_size",
    "gtid_cleanup_batch_size",
    "host_cache_size",
    "innodb_doublewrite_batch_size",
    "innodb_ft_max_token_size",
    "innodb_ft_min_token_size",
    "innodb_lru_flush_size",
    "innodb_purge_batch_size",
    "innodb_sync_array_size",
    "key_cache_file_hash_size",
    "aria_pagecache_file_hash_size",
    "max_join_size",
    "metadata_locks_cache_size",
    "myisam_data_pointer_size",
    "ngram_token_size",
    "profiling_history_size",
    "thread_cache_size",
    "thread_pool_size",
    "wsrep_trx_fragment_size",
    "innodb_change_buffer_max_size",
    "histogram_size",
];

/// Byte sizes whose name does not end in `_size`.
const BYTES: [&str; 16] = [
    "connection_memory_limit",
    "global_connection_memory_limit",
    "group_concat_max_len",
    "innodb_buffer_pool_size_auto_min",
    "innodb_buffer_pool_size_max",
    "innodb_redo_log_capacity",
    "join_buffer_space_limit",
    "max_allowed_packet",
    "max_session_mem_used",
    "max_tmp_session_space_usage",
    "max_tmp_total_space_usage",
    "net_buffer_length",
    "replica_max_allowed_packet",
    "slave_max_allowed_packet",
    "temptable_max_mmap",
    "temptable_max_ram",
];

/// `_timeout` variables counted in milliseconds.
const MILLISECONDS: [&str; 2] = [
    "rpl_semi_sync_master_timeout",
    "rpl_semi_sync_source_timeout",
];

/// The unit of an integer variable, from its name. Neither engine reports units.
pub fn unit_of(name: &str) -> Option<Unit> {
    let performance = name.starts_with("performance_schema_")
        && name != "performance_schema_session_connect_attrs_size";
    if BYTES.contains(&name) || name.ends_with("_size") && !COUNTS.contains(&name) && !performance {
        Some(Unit::Bytes)
    } else if (name.ends_with("_timeout") || name.ends_with("_seconds"))
        && !MILLISECONDS.contains(&name)
    {
        Some(Unit::Seconds)
    } else {
        None
    }
}

/// A group of variables in the category list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    Innodb,
    Replication,
    BinaryLog,
    Connections,
    Memory,
    Logging,
    Optimizer,
    Security,
    PerformanceSchema,
    CharacterSets,
    /// Redis snapshots and the append-only file.
    Persistence,
    Other,
}

impl Category {
    pub const ALL: [Self; 12] = [
        Self::Connections,
        Self::Memory,
        Self::Innodb,
        Self::BinaryLog,
        Self::Persistence,
        Self::Replication,
        Self::Optimizer,
        Self::Logging,
        Self::Security,
        Self::CharacterSets,
        Self::PerformanceSchema,
        Self::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Innodb => "InnoDB",
            Self::Replication => "Replication",
            Self::BinaryLog => "Binary log",
            Self::Connections => "Connections",
            Self::Memory => "Memory and buffers",
            Self::Logging => "Logging",
            Self::Optimizer => "Optimizer",
            Self::Security => "Security and TLS",
            Self::PerformanceSchema => "Performance schema",
            Self::CharacterSets => "Character sets",
            Self::Persistence => "Persistence",
            Self::Other => "Other",
        }
    }
}

/// Category rules: exact names, then the longest matching prefix, then a word inside
/// the name. Anything left is [`Category::Other`].
struct Rule {
    category: Category,
    names: &'static [&'static str],
    prefixes: &'static [&'static str],
    contains: &'static [&'static str],
}

const RULES: [Rule; 10] = [
    Rule {
        category: Category::Innodb,
        names: &["ignore_builtin_innodb", "have_innodb"],
        prefixes: &["innodb_"],
        contains: &[],
    },
    Rule {
        category: Category::Replication,
        names: &[
            "enforce_gtid_consistency",
            "read_binlog_speed_limit",
            "terminology_use_previous",
            "default_master_connection",
            "sql_slave_skip_counter",
            "sql_replica_skip_counter",
            "server_id",
            "server_id_bits",
            "server_uid",
            "server_uuid",
        ],
        prefixes: &[
            "replica_",
            "replication_",
            "replicate_",
            "slave_",
            "rpl_",
            "gtid_",
            "relay_",
            "master_",
            "source_",
            "group_replication_",
            "wsrep_",
            "report_",
            "sync_master",
            "sync_relay",
            "sync_source",
            "init_replica",
            "init_slave",
            "skip_replica",
            "skip_slave",
            "log_replica",
            "log_slave",
            "log_slow_replica",
            "log_slow_slave",
            "max_relay_log",
        ],
        contains: &[],
    },
    Rule {
        category: Category::BinaryLog,
        names: &[
            "expire_logs_days",
            "log_statements_unsafe_for_binlog",
            "sql_log_bin",
        ],
        prefixes: &[
            "binlog_",
            "log_bin",
            "max_binlog_",
            "sync_binlog",
            "encrypt_binlog",
        ],
        contains: &["binlog"],
    },
    Rule {
        category: Category::Connections,
        names: &[
            "max_connections",
            "max_user_connections",
            "max_connect_errors",
            "connect_timeout",
            "wait_timeout",
            "interactive_timeout",
            "back_log",
            "bind_address",
            "port",
            "socket",
            "skip_networking",
            "skip_name_resolve",
            "host_cache_size",
            "thread_cache_size",
            "thread_handling",
            "admin_address",
            "admin_port",
            "create_admin_listener_thread",
            "offline_mode",
            "init_connect",
            "max_allowed_packet",
            "idle_transaction_timeout",
            "idle_readonly_transaction_timeout",
            "idle_write_transaction_timeout",
            "redirect_url",
        ],
        prefixes: &[
            "net_",
            "tcp_",
            "mysqlx_",
            "thread_pool_",
            "extra_",
            "proxy_protocol",
            "protocol_",
            "session_track_",
        ],
        contains: &[],
    },
    Rule {
        category: Category::Memory,
        names: &[
            "tmp_table_size",
            "tmp_memory_table_size",
            "tmp_disk_table_size",
            "max_heap_table_size",
            "thread_stack",
            "locked_in_memory",
            "max_session_mem_used",
            "stored_program_cache",
            "stored_program_definition_cache",
            "tablespace_definition_cache",
            "schema_definition_cache",
            "metadata_locks_cache_size",
            "metadata_locks_hash_instances",
            "query_alloc_block_size",
            "query_prealloc_size",
            "range_alloc_block_size",
            "transaction_alloc_block_size",
            "transaction_prealloc_size",
            "net_buffer_length",
            "parser_max_mem_size",
            "max_tmp_session_space_usage",
            "max_tmp_total_space_usage",
            "mhnsw_max_cache_size",
        ],
        prefixes: &[
            "key_buffer",
            "key_cache",
            "query_cache",
            "table_open_cache",
            "table_definition_cache",
            "temptable_",
            "connection_memory_",
            "global_connection_memory_",
            "large_page",
            "aria_pagecache",
        ],
        contains: &["_buffer", "buff_size"],
    },
    Rule {
        category: Category::Logging,
        names: &[
            "long_query_time",
            "min_examined_row_limit",
            "slow_launch_time",
            "note_verbosity",
        ],
        prefixes: &["log_", "general_log", "slow_query_log", "server_audit"],
        contains: &[],
    },
    Rule {
        category: Category::Optimizer,
        names: &[
            "use_stat_tables",
            "in_predicate_conversion_threshold",
            "expensive_subquery_limit",
            "cte_max_recursion_depth",
            "max_recursive_iterations",
            "max_execution_time",
            "max_statement_time",
            "big_tables",
            "internal_tmp_mem_storage_engine",
            "windowing_use_high_precision",
            "max_rowid_filter_size",
            "secondary_engine_cost_threshold",
        ],
        prefixes: &[
            "optimizer_",
            "histogram_",
            "join_cache",
            "analyze_",
            "eq_range",
            "range_optimizer",
            "max_join",
            "max_seeks",
            "max_sort",
            "max_length_for_sort",
            "explain_",
            "end_markers",
        ],
        contains: &[],
    },
    Rule {
        category: Category::Security,
        names: &[
            "require_secure_transport",
            "local_infile",
            "skip_grant_tables",
            "skip_show_database",
            "automatic_sp_privileges",
            "activate_all_roles_on_login",
            "mandatory_roles",
            "partial_revokes",
            "authentication_policy",
            "default_password_lifetime",
            "disconnect_on_expired_password",
            "check_proxy_users",
            "generated_random_password_length",
            "auto_generate_certs",
            "block_encryption_mode",
            "print_identified_with_as_hex",
            "allow_suspicious_udfs",
            "old_passwords",
            "default_authentication_plugin",
            "default_table_encryption",
            "table_encryption_privilege_check",
            "have_ssl",
            "have_openssl",
        ],
        prefixes: &[
            "ssl_",
            "tls_",
            "admin_ssl_",
            "admin_tls_",
            "caching_sha2_",
            "sha256_password",
            "password_",
            "validate_password",
            "keyring_",
            "mysql_native_password",
            "encrypt_tmp",
            "secure_",
            "persist_",
            "strict_password",
            "max_password",
        ],
        contains: &[],
    },
    Rule {
        category: Category::PerformanceSchema,
        names: &[],
        prefixes: &["performance_schema"],
        contains: &[],
    },
    Rule {
        category: Category::CharacterSets,
        names: &["default_collation_for_utf8mb4"],
        prefixes: &["character_set", "collation_", "lc_"],
        contains: &[],
    },
];

/// The categories the deciding rule tier offers for `name`. More than one would be a
/// fault in [`RULES`]; the tests check that it never happens.
fn candidates(name: &str) -> Vec<Category> {
    let exact: Vec<Category> = RULES
        .iter()
        .filter(|r| r.names.contains(&name))
        .map(|r| r.category)
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    let longest = RULES
        .iter()
        .flat_map(|r| r.prefixes.iter())
        .filter(|p| name.starts_with(**p))
        .map(|p| p.len())
        .max();
    if let Some(len) = longest {
        let mut c: Vec<Category> = RULES
            .iter()
            .filter(|r| {
                r.prefixes
                    .iter()
                    .any(|p| p.len() == len && name.starts_with(p))
            })
            .map(|r| r.category)
            .collect();
        c.dedup();
        return c;
    }
    RULES
        .iter()
        .filter(|r| r.contains.iter().any(|w| name.contains(w)))
        .map(|r| r.category)
        .collect()
}

/// The category `name` (lower case) is listed under.
pub fn categorize(name: &str) -> Category {
    candidates(name).first().copied().unwrap_or(Category::Other)
}

/// The category of an Oracle parameter, from words in its name.
pub fn oracle_category(name: &str) -> Category {
    let has = |words: &[&str]| words.iter().any(|w| name.contains(w));
    if name.starts_with("nls_") {
        Category::CharacterSets
    } else if name.starts_with("optimizer_")
        || has(&[
            "cursor_",
            "query_rewrite",
            "parallel_",
            "star_transformation",
            "result_cache",
        ])
    {
        Category::Optimizer
    } else if has(&[
        "sga", "pga", "_cache", "memory", "_pool", "inmemory", "buffer",
    ]) {
        Category::Memory
    } else if name.starts_with("sec_")
        || has(&[
            "audit",
            "password",
            "remote_login",
            "os_authent",
            "encrypt",
            "wallet",
            "tde_",
        ])
    {
        Category::Security
    } else if name.starts_with("log_archive")
        || name.starts_with("fal_")
        || name.starts_with("dg_")
        || has(&["standby", "goldengate", "db_unique_name", "replication"])
    {
        Category::Replication
    } else if has(&[
        "process",
        "session",
        "dispatcher",
        "shared_server",
        "listener",
        "service_names",
        "connection",
    ]) {
        Category::Connections
    } else if has(&[
        "trace",
        "dump",
        "diagnostic",
        "log_checkpoint",
        "statistics_level",
        "timed_",
    ]) {
        Category::Logging
    } else {
        Category::Other
    }
}

/// A one-line pointer to a related variable, only where the engine's documentation
/// states the rule: `(text, variable to show)`.
pub fn hint(cfg: &ServerConfig, p: &ConfigParam) -> Option<(String, String)> {
    let off = |name: &str| {
        cfg.param(name).is_some_and(|v| {
            v.value
                .as_deref()
                .is_some_and(|x| x.eq_ignore_ascii_case("OFF"))
        })
    };
    let n = p.name.as_str();
    if matches!(
        n,
        "long_query_time" | "min_examined_row_limit" | "slow_query_log_file"
    ) || n == "log_queries_not_using_indexes"
    {
        if off("slow_query_log") {
            return Some((
                "slow_query_log is off, so the slow query log is not written and this has no \
                 effect until it is on."
                    .into(),
                "slow_query_log".into(),
            ));
        }
    } else if n == "general_log_file" {
        if off("general_log") {
            return Some((
                "general_log is off, so nothing is written to this file until it is on.".into(),
                "general_log".into(),
            ));
        }
    } else if p.category == Category::BinaryLog && n != "log_bin" && off("log_bin") {
        return Some((
            "log_bin is off: the server writes no binary log, so this has no effect.".into(),
            "log_bin".into(),
        ));
    } else if n == "innodb_buffer_pool_size"
        && cfg.persist
        && cfg.param("innodb_buffer_pool_chunk_size").is_some()
    {
        return Some((
            "The server rounds the buffer pool size to a multiple of \
             innodb_buffer_pool_chunk_size × innodb_buffer_pool_instances."
                .into(),
            "innodb_buffer_pool_chunk_size".into(),
        ));
    } else if n == "read_only" && cfg.param("super_read_only").is_some() {
        return Some((
            "Users with SUPER or CONNECTION_ADMIN can still write while read_only is on; \
             super_read_only stops them too."
                .into(),
            "super_read_only".into(),
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(name: &str, value: &str, min: &str, max: &str, block: Option<u64>) -> ConfigParam {
        ConfigParam {
            value_type: ValueType::Integer,
            min: Some(min.into()),
            max: Some(max.into()),
            block,
            unit: unit_of(name),
            applies: Applies::Now,
            ..ConfigParam::new(name, Some(value.into()))
        }
    }

    fn mysql(params: Vec<ConfigParam>) -> ServerConfig {
        ServerConfig {
            params,
            persist: true,
            backslash_escapes: true,
            ..ServerConfig::default()
        }
    }

    fn change(name: &str, v: &str, mode: Mode) -> ConfigChange {
        ConfigChange {
            name: name.into(),
            value: NewValue::Set(v.into()),
            mode,
        }
    }

    #[test]
    fn every_captured_variable_lands_in_exactly_one_category() {
        for (file, list) in [
            (
                "mysql",
                include_str!("../tests/fixtures/config/mysql-8.4.11.txt"),
            ),
            (
                "mariadb",
                include_str!("../tests/fixtures/config/mariadb-11.8.9.txt"),
            ),
        ] {
            let names: Vec<&str> = list.lines().filter(|l| !l.is_empty()).collect();
            assert!(names.len() > 600, "{file}");
            let mut other = 0;
            for n in &names {
                let c = candidates(n);
                assert!(c.len() <= 1, "{file}: {n} matches {c:?}");
                if c.is_empty() {
                    other += 1;
                }
                assert!(valid_name(n), "{n}");
            }
            // Other is a rest, not the main bucket.
            assert!(other * 4 < names.len(), "{file}: {other} in Other");
        }
    }

    #[test]
    fn rules_list_no_name_or_prefix_twice() {
        let mut seen = std::collections::HashSet::new();
        for r in &RULES {
            for x in r.names.iter().chain(r.prefixes).chain(r.contains) {
                assert!(seen.insert(*x), "{x} is listed twice");
            }
        }
    }

    #[test]
    fn categories_follow_the_rules() {
        assert_eq!(categorize("innodb_buffer_pool_size"), Category::Innodb);
        assert_eq!(categorize("log_bin_basename"), Category::BinaryLog);
        assert_eq!(categorize("log_slave_updates"), Category::Replication);
        assert_eq!(categorize("log_error_verbosity"), Category::Logging);
        assert_eq!(categorize("sort_buffer_size"), Category::Memory);
        assert_eq!(categorize("max_connections"), Category::Connections);
        assert_eq!(categorize("tls_version"), Category::Security);
        assert_eq!(categorize("character_set_server"), Category::CharacterSets);
        assert_eq!(
            categorize("performance_schema"),
            Category::PerformanceSchema
        );
        assert_eq!(categorize("optimizer_switch"), Category::Optimizer);
        assert_eq!(categorize("autocommit"), Category::Other);
    }

    #[test]
    fn units_come_from_the_name_table() {
        assert_eq!(unit_of("innodb_buffer_pool_size"), Some(Unit::Bytes));
        assert_eq!(unit_of("max_allowed_packet"), Some(Unit::Bytes));
        assert_eq!(unit_of("thread_cache_size"), None);
        assert_eq!(unit_of("performance_schema_digests_size"), None);
        assert_eq!(unit_of("wait_timeout"), Some(Unit::Seconds));
        assert_eq!(unit_of("rpl_semi_sync_master_timeout"), None);
        assert_eq!(unit_of("max_connections"), None);
    }

    #[test]
    fn units_scale_both_ways_exactly() {
        assert_eq!(scale("64", 1 << 20), Some(67_108_864));
        assert_eq!(scale("1.5", 1 << 30), Some(1_610_612_736));
        assert_eq!(scale("0.5", 1024), Some(512));
        assert_eq!(scale("1.0001", 1024), None);
        assert_eq!(scale("2", 3600), Some(7200));
        assert_eq!(scale("", 1), None);
        assert_eq!(scale("1e3", 1), None);
        assert_eq!(scale("-3", 1), Some(-3));
        assert_eq!(unscale(67_108_864, 1 << 20).as_deref(), Some("64"));
        assert_eq!(unscale(1_610_612_736, 1 << 30).as_deref(), Some("1.5"));
        assert_eq!(unscale(512, 1024).as_deref(), Some("0.5"));
        assert_eq!(unscale(100, 60), None);
        assert_eq!(unscale(90, 60).as_deref(), Some("1.5"));
        // Every unit's round trip.
        for n in [1i128, 1000, 4096, 134_217_728, 3_000_000_001] {
            for (_, step) in Unit::Bytes.steps() {
                if let Some(text) = unscale(n, *step) {
                    assert_eq!(scale(&text, *step), Some(n), "{n} {step}");
                }
            }
        }
    }

    #[test]
    fn values_display_in_the_largest_exact_unit() {
        let p = int(
            "innodb_buffer_pool_size",
            "134217728",
            "5242880",
            "9223372036854775807",
            None,
        );
        assert_eq!(p.display(p.value.as_deref()), "128MB");
        assert_eq!(p.display(Some("8589934592")), "8GB");
        assert_eq!(p.display(Some("1000")), "1000");
        assert_eq!(p.display(Some("0")), "0");
        assert_eq!(p.display(None), "NULL");
        let w = int("wait_timeout", "28800", "1", "31536000", None);
        assert_eq!(w.display(Some("28800")), "8h");
        assert_eq!(w.display(Some("90")), "90s");
        assert_eq!(
            p.allowed().as_deref(),
            Some("Allowed: 5MB to 9223372036854775807")
        );
    }

    #[test]
    fn checks_follow_the_server_range_and_block() {
        let p = int(
            "innodb_buffer_pool_size",
            "134217728",
            "2097152",
            "18446744073701163008",
            Some(1_048_576),
        );
        assert_eq!(p.check("268435456"), Ok("268435456".into()));
        assert_eq!(p.check(" 268435456 "), Ok("268435456".into()));
        assert!(p.check("1048576").unwrap_err().contains("between 2MB"));
        assert!(p.check("3000000").unwrap_err().contains("multiple of 1MB"));
        assert!(p.check("12abc").is_err());
        assert!(p.check("1; DROP TABLE x").is_err());
        assert!(p.check("-5").is_err());
        let b = ConfigParam {
            value_type: ValueType::Boolean,
            ..ConfigParam::new("general_log", Some("OFF".into()))
        };
        assert_eq!(b.check("on"), Ok("ON".into()));
        assert_eq!(b.check("0"), Ok("OFF".into()));
        assert!(b.check("maybe").is_err());
        let e = ConfigParam {
            value_type: ValueType::Enum(vec!["MIXED".into(), "STATEMENT".into(), "ROW".into()]),
            ..ConfigParam::new("binlog_format", Some("MIXED".into()))
        };
        assert_eq!(e.check("row"), Ok("ROW".into()));
        assert!(e.check("ROWS").is_err());
        let s = ConfigParam {
            value_type: ValueType::Set(vec!["A".into(), "B".into(), "C".into()]),
            ..ConfigParam::new("x", None)
        };
        assert_eq!(s.check("c, a,a"), Ok("A,C".into()));
        assert_eq!(s.check(""), Ok(String::new()));
        assert!(s.check("A,Z").is_err());
        let f = ConfigParam {
            value_type: ValueType::Float,
            min: Some("0".into()),
            max: Some("31536000".into()),
            ..ConfigParam::new("long_query_time", Some("10.000000".into()))
        };
        assert_eq!(f.check("0.5"), Ok("0.5".into()));
        assert!(f.check("-1").is_err());
        assert!(f.check("NaN").is_err());
        assert!(f.check("inf").is_err());
    }

    #[test]
    fn names_that_could_inject_are_refused() {
        for bad in [
            "",
            "max_connections = 1; DROP TABLE t",
            "a b",
            "a`b",
            "a'b",
            "a.b.c",
            ".a",
            "a.",
            "A",
            "x--",
            "x/*",
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        assert!(valid_name("validate_password.length"));
        let cfg = mysql(vec![int("max_connections", "151", "1", "100000", None)]);
        let bad = change("max_connections; DROP TABLE t", "1", Mode::Persist);
        assert!(cfg.statements(&bad).is_err());
        assert!(
            cfg.statements(&change("unknown_var", "1", Mode::Persist))
                .is_err()
        );
    }

    #[test]
    fn mysql_statements_persist_by_default_and_reset_on_revert() {
        let mut restart = int(
            "innodb_log_buffer_size",
            "16777216",
            "262144",
            "4294967295",
            None,
        );
        restart.applies = Applies::Restart;
        let mut persisted = int("max_connections", "200", "1", "100000", None);
        persisted.persisted = Some("200".into());
        persisted.source = Source::Persisted;
        let text = ConfigParam {
            applies: Applies::Now,
            ..ConfigParam::new("init_connect", Some(String::new()))
        };
        let boolean = ConfigParam {
            value_type: ValueType::Boolean,
            applies: Applies::Now,
            ..ConfigParam::new("general_log", Some("OFF".into()))
        };
        let fixed = ConfigParam {
            applies: Applies::Fixed,
            ..ConfigParam::new("datadir", Some("/var/lib/mysql/".into()))
        };
        let cfg = mysql(vec![restart, persisted, text, boolean, fixed]);
        let one = |c: ConfigChange| cfg.statements(&c).map(|v| v.join("; "));
        assert_eq!(
            one(change("max_connections", "300", Mode::Persist)),
            Ok("SET PERSIST max_connections = 300".into())
        );
        assert_eq!(
            one(change("max_connections", "300", Mode::RunOnly)),
            Ok("SET GLOBAL max_connections = 300".into())
        );
        assert_eq!(
            one(change("innodb_log_buffer_size", "33554432", Mode::RunOnly)),
            Ok("SET PERSIST_ONLY innodb_log_buffer_size = 33554432".into())
        );
        assert_eq!(
            one(change("general_log", "on", Mode::Persist)),
            Ok("SET PERSIST general_log = ON".into())
        );
        assert_eq!(
            one(change(
                "init_connect",
                "SET NAMES 'utf8mb4' \\",
                Mode::Persist
            )),
            Ok(r"SET PERSIST init_connect = 'SET NAMES ''utf8mb4'' \\'".into())
        );
        let revert = |name: &str, mode| ConfigChange {
            name: name.into(),
            value: NewValue::Default,
            mode,
        };
        assert_eq!(
            one(revert("max_connections", Mode::Persist)),
            Ok("SET GLOBAL max_connections = DEFAULT; RESET PERSIST max_connections".into())
        );
        assert_eq!(
            one(revert("max_connections", Mode::RunOnly)),
            Ok("SET GLOBAL max_connections = DEFAULT".into())
        );
        assert_eq!(
            one(revert("general_log", Mode::Persist)),
            Ok("SET GLOBAL general_log = DEFAULT".into())
        );
        assert!(one(revert("innodb_log_buffer_size", Mode::Persist)).is_err());
        assert!(one(change("datadir", "/tmp", Mode::Persist)).is_err());
        assert!(one(change("max_connections", "0", Mode::Persist)).is_err());
    }

    #[test]
    fn mariadb_only_sets_global_and_writes_option_lines() {
        let mut ro = int(
            "innodb_log_file_size",
            "100663296",
            "4194304",
            "18446744073709551615",
            Some(4096),
        );
        ro.applies = Applies::Restart;
        let mode = ConfigParam {
            value_type: ValueType::Set(vec!["ANSI_QUOTES".into(), "STRICT_TRANS_TABLES".into()]),
            applies: Applies::Now,
            ..ConfigParam::new("sql_mode", Some("STRICT_TRANS_TABLES".into()))
        };
        let text = ConfigParam {
            applies: Applies::Now,
            ..ConfigParam::new("init_connect", Some(String::new()))
        };
        let cfg = ServerConfig {
            params: vec![
                int("max_connections", "151", "10", "100000", Some(1)),
                ro,
                mode,
                text,
            ],
            persist: false,
            backslash_escapes: false,
            ..ServerConfig::default()
        };
        let c = change("max_connections", "200", Mode::Persist);
        assert_eq!(
            cfg.statements(&c),
            Ok(vec!["SET GLOBAL max_connections = 200".into()])
        );
        assert_eq!(
            cfg.config_line(&c).as_deref(),
            Some("max_connections = 200")
        );
        let c = change("sql_mode", "strict_trans_tables,ansi_quotes", Mode::Persist);
        assert_eq!(
            cfg.statements(&c),
            Ok(vec![
                "SET GLOBAL sql_mode = 'ANSI_QUOTES,STRICT_TRANS_TABLES'".into()
            ])
        );
        assert_eq!(
            cfg.config_line(&c).as_deref(),
            Some("sql_mode = ANSI_QUOTES,STRICT_TRANS_TABLES")
        );
        // Without backslash escapes only the quote doubles.
        let c = change("init_connect", r#"SET @a = 'x\' "#, Mode::Persist);
        assert_eq!(
            cfg.statements(&c),
            Ok(vec![r"SET GLOBAL init_connect = 'SET @a = ''x\'' '".into()])
        );
        assert_eq!(
            cfg.config_line(&c).as_deref(),
            Some(r#"init_connect = "SET @a = 'x\\' ""#)
        );
        let revert = ConfigChange {
            name: "max_connections".into(),
            value: NewValue::Default,
            mode: Mode::Persist,
        };
        assert_eq!(
            cfg.statements(&revert),
            Ok(vec!["SET GLOBAL max_connections = DEFAULT".into()])
        );
        let locked = change("innodb_log_file_size", "201326592", Mode::Persist);
        assert!(
            cfg.statements(&locked)
                .unwrap_err()
                .contains("MariaDB cannot change")
        );
    }

    #[test]
    fn a_missing_privilege_locks_everything() {
        let mut cfg = mysql(vec![int("max_connections", "151", "1", "100000", None)]);
        cfg.read_only = Some("needs SYSTEM_VARIABLES_ADMIN".into());
        let c = change("max_connections", "200", Mode::Persist);
        assert_eq!(
            cfg.statements(&c),
            Err("needs SYSTEM_VARIABLES_ADMIN".into())
        );
    }

    #[test]
    fn modified_and_restart_pending_follow_the_server() {
        let mut p = int("max_connections", "151", "1", "100000", None);
        assert!(!p.is_modified());
        p.source = Source::ConfigFile(Some("/etc/my.cnf".into()));
        assert!(p.is_modified());
        p.default = Some("151".into());
        assert!(!p.is_modified(), "set in a file to the default");
        let mut r = int("innodb_log_buffer_size", "16777216", "1", "9", None);
        r.applies = Applies::Restart;
        r.persisted = Some("33554432".into());
        assert!(r.restart_pending());
        r.persisted = Some("16777216".into());
        assert!(!r.restart_pending());
    }

    #[test]
    fn hints_need_their_rule() {
        let off = |n: &str| ConfigParam {
            value_type: ValueType::Boolean,
            ..ConfigParam::new(n, Some("OFF".into()))
        };
        let cfg = mysql(vec![off("slow_query_log"), off("log_bin")]);
        let lqt = ConfigParam::new("long_query_time", Some("10".into()));
        assert_eq!(
            hint(&cfg, &lqt).map(|h| h.1).as_deref(),
            Some("slow_query_log")
        );
        let bf = ConfigParam::new("binlog_format", Some("ROW".into()));
        assert_eq!(hint(&cfg, &bf).map(|h| h.1).as_deref(), Some("log_bin"));
        assert_eq!(hint(&cfg, &ConfigParam::new("autocommit", None)), None);
    }
}
