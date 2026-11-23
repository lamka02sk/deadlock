//! Sessions and locks, engine-neutral: who is connected, what each runs and holds,
//! who waits on whom, and the last deadlock the server detected.

use std::collections::{BTreeSet, HashMap};

/// A server session (MySQL: a connection's processlist id).
pub type SessionId = u64;

/// What a session is doing, as the filters group it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Running a statement.
    Active,
    /// Running a statement that waits on a lock another session holds.
    Waiting,
    /// No statement running, but a transaction is open: its locks stay held.
    IdleInTransaction,
    Idle,
    /// Redis: blocked in a command that waits for data (`BLPOP`, `XREAD BLOCK`).
    Blocked,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Waiting => "waiting for lock",
            Self::IdleInTransaction => "idle in transaction",
            Self::Idle => "idle",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    /// When it started, as the server writes it.
    pub started: String,
    pub seconds: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockKind {
    /// A row (record) lock in a table's index.
    Row,
    Table,
    /// A metadata (dictionary) lock: DDL and open transactions take these.
    Metadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lock {
    pub kind: LockKind,
    /// Plain words: `exclusive`, `shared read`, `intention exclusive`.
    pub mode: String,
    pub schema: String,
    /// Empty when the server does not say which table.
    pub table: String,
    pub index: Option<String>,
    /// The locked row's key, where the server shows it.
    pub row: Option<String>,
    /// Rows this entry stands for, when several row locks are grouped.
    pub rows: usize,
}

impl Lock {
    fn mode_words(&self) -> String {
        let mut m = self.mode.clone();
        if let Some(first) = m.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        m
    }

    /// `Exclusive lock on the row in deadlock.orders with PRIMARY = 1`.
    pub fn describe(&self) -> String {
        let table = format!("{}.{}", self.schema, self.table);
        let mode = self.mode_words();
        match self.kind {
            LockKind::Row if self.rows > 1 => {
                format!("{mode} lock on {} rows in {table}", self.rows)
            }
            LockKind::Row => match (&self.index, &self.row) {
                (Some(i), Some(r)) => format!("{mode} lock on the row in {table} with {i} = {r}"),
                _ => format!("{mode} lock on a row in {table}"),
            },
            LockKind::Table => format!("{mode} lock on table {table}"),
            LockKind::Metadata if self.table.is_empty() => {
                "A metadata lock; the server does not say on which table".into()
            }
            LockKind::Metadata => format!("{mode} metadata lock on table {table}"),
        }
    }

    /// For the banner cards: `row in orders, PRIMARY = 1`.
    pub fn short(&self) -> String {
        match self.kind {
            LockKind::Row => match (&self.index, &self.row) {
                (Some(i), Some(r)) => format!("row in {}, {i} = {r}", self.table),
                _ => format!("row in {}", self.table),
            },
            LockKind::Table => format!("table {}", self.table),
            LockKind::Metadata if self.table.is_empty() => "a metadata lock".into(),
            LockKind::Metadata => format!("metadata lock on {}", self.table),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: SessionId,
    pub user: String,
    /// Client host, as the server reports it.
    pub host: String,
    /// The client's program name, where the server reports it.
    pub application: Option<String>,
    pub database: Option<String>,
    pub state: State,
    /// The server's own words for what it does: `Query: updating`, `Sleep`.
    pub detail: String,
    /// Seconds in the current state.
    pub seconds: Option<u64>,
    pub statement: Option<String>,
    /// `statement` runs now; otherwise it is the last one the session ran.
    pub running: bool,
    pub transaction: Option<Transaction>,
    pub holds: Vec<Lock>,
    /// This screen's own server session.
    pub own: bool,
    /// Opened by Deadlock, in this window or another.
    pub deadlock: bool,
    /// Seconds since the client connected, where the server reports it (Redis).
    pub age: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockWait {
    pub waiter: SessionId,
    /// `None` when the server does not say who holds the lock.
    pub blocker: Option<SessionId>,
    pub wants: Lock,
    pub seconds: Option<u64>,
}

/// One look at the server's sessions and locks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub sessions: Vec<Session>,
    pub waits: Vec<LockWait>,
    /// The user may see only their own sessions: the privilege that would show the rest
    /// (MySQL `PROCESS`).
    pub own_only: Option<&'static str>,
    /// Sources that could not be read, in plain words.
    pub notes: Vec<String>,
    /// The server's slow log, newest first, where it keeps one (Redis).
    pub slow_log: Option<Vec<SlowEntry>>,
}

/// One entry of a server's slow log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlowEntry {
    pub id: u64,
    /// Unix time in seconds.
    pub at: i64,
    pub micros: u64,
    /// The command line, as the server shortened it.
    pub command: String,
    /// The client's address and name.
    pub client: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    /// Running a statement, waiting included.
    Active,
    Waiting,
    IdleInTransaction,
}

impl Filter {
    pub const ALL: [Self; 4] = [
        Self::All,
        Self::Active,
        Self::Waiting,
        Self::IdleInTransaction,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Active => "Active",
            Self::Waiting => "Waiting",
            Self::IdleInTransaction => "Idle in transaction",
        }
    }

    pub fn matches(self, s: &Session) -> bool {
        match self {
            Self::All => true,
            Self::Active => matches!(s.state, State::Active | State::Waiting | State::Blocked),
            Self::Waiting => s.state == State::Waiting,
            Self::IdleInTransaction => s.state == State::IdleInTransaction,
        }
    }
}

/// A blocking chain: `levels[0]` holds what `levels[1]` waits for, and so on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    pub levels: Vec<Vec<SessionId>>,
    /// The sessions wait on each other: a deadlock the server has not broken (yet).
    pub cycle: bool,
    /// `levels[0]` waits too, on a holder the server does not name.
    pub unknown_blocker: bool,
}

impl Snapshot {
    pub fn session(&self, id: SessionId) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn count(&self, filter: Filter) -> usize {
        self.sessions.iter().filter(|s| filter.matches(s)).count()
    }

    /// Sessions waiting on a lock.
    pub fn waiting(&self) -> usize {
        self.count(Filter::Waiting)
    }

    /// Who `id` waits on, lowest first.
    pub fn blockers(&self, id: SessionId) -> Vec<SessionId> {
        let set: BTreeSet<_> = self
            .waits
            .iter()
            .filter(|w| w.waiter == id)
            .filter_map(|w| w.blocker)
            .collect();
        set.into_iter().collect()
    }

    /// The first wait of `id`: what it wants and for how long.
    pub fn wait_of(&self, id: SessionId) -> Option<&LockWait> {
        self.waits.iter().find(|w| w.waiter == id)
    }

    /// Marks waiters as [`State::Waiting`]. Drivers call it after filling `waits`.
    pub fn mark_waiting(&mut self) {
        for s in &mut self.sessions {
            if self.waits.iter().any(|w| w.waiter == s.id) {
                s.state = State::Waiting;
            }
        }
    }

    /// One chain per blocker that does not wait itself (root first), one per waiter on
    /// an unnamed holder, and one per cycle.
    pub fn chains(&self) -> Vec<Chain> {
        let mut edges: HashMap<SessionId, BTreeSet<SessionId>> = HashMap::new();
        let mut waiters = BTreeSet::new();
        let mut unknown = BTreeSet::new();
        for w in &self.waits {
            waiters.insert(w.waiter);
            match w.blocker {
                Some(b) if b != w.waiter => {
                    edges.entry(b).or_default().insert(w.waiter);
                }
                _ => {
                    unknown.insert(w.waiter);
                }
            }
        }
        let walk = |start: Vec<SessionId>, seen: &mut BTreeSet<SessionId>| {
            let mut levels = Vec::new();
            let mut level = start;
            seen.extend(level.iter().copied());
            while !level.is_empty() {
                let next: BTreeSet<SessionId> = level
                    .iter()
                    .filter_map(|id| edges.get(id))
                    .flatten()
                    .copied()
                    .filter(|id| !seen.contains(id))
                    .collect();
                levels.push(level);
                seen.extend(next.iter().copied());
                level = next.into_iter().collect();
            }
            levels
        };
        let mut reached = BTreeSet::new();
        let mut chains = Vec::new();
        let roots: BTreeSet<_> = edges.keys().filter(|b| !waiters.contains(b)).collect();
        for &root in roots {
            let mut seen = BTreeSet::new();
            let levels = walk(vec![root], &mut seen);
            reached.extend(seen);
            chains.push(Chain {
                levels,
                cycle: false,
                unknown_blocker: false,
            });
        }
        for &w in &unknown {
            if reached.contains(&w) && !edges.contains_key(&w) {
                continue;
            }
            let mut seen = BTreeSet::new();
            let levels = walk(vec![w], &mut seen);
            reached.extend(seen);
            chains.push(Chain {
                levels,
                cycle: false,
                unknown_blocker: true,
            });
        }
        // Whatever is left waits in a circle.
        while let Some(&start) = waiters.iter().find(|w| !reached.contains(w)) {
            let mut seen = BTreeSet::new();
            let levels = walk(vec![start], &mut seen);
            reached.extend(seen);
            chains.push(Chain {
                levels,
                cycle: true,
                unknown_blocker: false,
            });
        }
        chains
    }
}

/// `X,REC_NOT_GAP` → `exclusive`; `IX` → `intention exclusive`; `S,GAP` → `shared gap`.
pub fn innodb_mode(raw: &str) -> String {
    let mut words: Vec<&str> = Vec::new();
    for part in raw.split(',') {
        match part.trim() {
            "X" => words.push("exclusive"),
            "S" => words.push("shared"),
            "IX" => words.push("intention exclusive"),
            "IS" => words.push("intention shared"),
            "GAP" => words.push("gap"),
            "INSERT_INTENTION" => words.push("insert intention"),
            "AUTO_INC" => words.push("auto-increment"),
            "REC_NOT_GAP" | "" => {}
            other => words.push(other),
        }
    }
    words.join(" ")
}

/// `MDL_SHARED_READ` or `SHARED_READ` → `shared read`.
pub fn mdl_mode(raw: &str) -> String {
    raw.trim_start_matches("MDL_")
        .to_ascii_lowercase()
        .replace('_', " ")
}

/// Metadata lock types in the order of the server's compatibility tables (`mdl.cc`).
const MDL_TYPES: [&str; 10] = [
    "SHARED",
    "SHARED_HIGH_PRIO",
    "SHARED_READ",
    "SHARED_WRITE",
    "SHARED_WRITE_LOW_PRIO",
    "SHARED_UPGRADABLE",
    "SHARED_READ_ONLY",
    "SHARED_NO_WRITE",
    "SHARED_NO_READ_WRITE",
    "EXCLUSIVE",
];
/// Row: the requested type; column: a granted type. `-` conflicts.
const MDL_GRANTED: [&[u8; 10]; 10] = [
    b"+++++++++-",
    b"+++++++++-",
    b"++++++++--",
    b"++++++----",
    b"++++++----",
    b"+++++-+---",
    b"+++--+++--",
    b"+++---+---",
    b"++--------",
    b"----------",
];
/// Row: the requested type; column: a type another session waits for. `-`: the
/// waiting request goes first.
const MDL_PENDING: [&[u8; 10]; 10] = [
    b"+++++++++-",
    b"++++++++++",
    b"++++++++--",
    b"+++++++---",
    b"++++++----",
    b"+++++++++-",
    b"+++-++++--",
    b"+++++++++-",
    b"+++++++++-",
    b"++++++++++",
];

/// Whether a metadata lock request of type `wanted` must wait for `other`, which
/// another session holds (`granted`) or waits for. Unknown types never conflict.
pub fn mdl_blocks(wanted: &str, other: &str, granted: bool) -> bool {
    let ix = |t: &str| {
        MDL_TYPES
            .iter()
            .position(|m| *m == t.trim_start_matches("MDL_"))
    };
    let (Some(w), Some(o)) = (ix(wanted), ix(other)) else {
        return false;
    };
    let table = if granted { MDL_GRANTED } else { MDL_PENDING };
    table[w][o] == b'-'
}

/// The `LATEST DETECTED DEADLOCK` section of `SHOW ENGINE INNODB STATUS`, without its
/// header; `None` when the server has detected none since it started.
pub fn deadlock_section(status: &str) -> Option<&str> {
    let start = status.find("LATEST DETECTED DEADLOCK")?;
    let rest = &status[start..];
    let body = rest.find('\n').map_or("", |i| &rest[i + 1..]);
    let body = body
        .strip_prefix(|c| c == '-')
        .map_or(body, |_| body.find('\n').map_or("", |i| &body[i + 1..]));
    let end = body.find("\nTRANSACTIONS\n").unwrap_or(body.len());
    // Drop the dashes above the next header.
    let section = body[..end].trim_end();
    let section = section
        .rsplit_once('\n')
        .filter(|(_, last)| !last.is_empty() && last.chars().all(|c| c == '-'))
        .map_or(section, |(head, _)| head);
    Some(section.trim_end())
}

/// One transaction of a deadlock report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeadlockTx {
    /// `(1)`, `(2)` in the report.
    pub number: u32,
    pub trx_id: String,
    pub thread: Option<SessionId>,
    /// `root@localhost`, as far as the report says.
    pub who: String,
    pub statement: String,
    pub holds: Vec<String>,
    pub waits_for: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Deadlock {
    /// When, as the server writes it.
    pub at: String,
    pub transactions: Vec<DeadlockTx>,
    /// The `number` of the transaction the server rolled back.
    pub rolled_back: Option<u32>,
}

/// Parses a [`deadlock_section`]. `None` when it does not look like one: show the text.
pub fn parse_deadlock(section: &str) -> Option<Deadlock> {
    let mut d = Deadlock {
        at: section
            .lines()
            .next()
            .map(|l| l.split_whitespace().take(2).collect::<Vec<_>>().join(" "))
            .unwrap_or_default(),
        ..Deadlock::default()
    };
    // Statement lines follow the thread line until the next `***` or blank line.
    let mut in_statement = false;
    // A lock may name a transaction listed further down (MariaDB's CONFLICTING WITH).
    let mut locks = Vec::new();
    for line in section.lines() {
        let t = line.trim();
        if let Some(n) = t
            .strip_prefix("*** (")
            .and_then(|r| r.strip_suffix(") TRANSACTION:"))
        {
            d.transactions.push(DeadlockTx {
                number: n.parse().ok()?,
                ..DeadlockTx::default()
            });
            in_statement = false;
            continue;
        }
        if let Some(n) = t
            .strip_prefix("*** WE ROLL BACK TRANSACTION (")
            .and_then(|r| r.strip_suffix(')'))
        {
            d.rolled_back = n.parse().ok();
            continue;
        }
        if t.starts_with("***") || t.is_empty() {
            in_statement = false;
            continue;
        }
        if t.starts_with("RECORD LOCKS ") || t.starts_with("TABLE LOCK ") {
            in_statement = false;
            locks.extend(lock_line(t));
            continue;
        }
        let Some(tx) = d.transactions.last_mut() else {
            continue;
        };
        if let Some(r) = t.strip_prefix("TRANSACTION ") {
            tx.trx_id = r.split(',').next().unwrap_or_default().to_owned();
        } else if let Some(r) = t
            .strip_prefix("MySQL thread id ")
            .or_else(|| t.strip_prefix("MariaDB thread id "))
        {
            tx.thread = r.split(',').next().and_then(|n| n.trim().parse().ok());
            // `…, query id 110 localhost root updating`
            if let Some((_, after)) = r.split_once("query id ") {
                let mut words = after.split_whitespace().skip(1);
                let host = words.next().unwrap_or_default();
                let user = words.next().unwrap_or_default();
                tx.who = match (user, host) {
                    ("", h) => h.to_owned(),
                    (u, h) => format!("{u}@{h}"),
                };
            }
            in_statement = true;
        } else if in_statement {
            if !tx.statement.is_empty() {
                tx.statement.push('\n');
            }
            tx.statement.push_str(line);
        }
    }
    for (trx, text, waiting) in locks {
        if let Some(tx) = d.transactions.iter_mut().find(|x| x.trx_id == trx) {
            let list = if waiting {
                &mut tx.waits_for
            } else {
                &mut tx.holds
            };
            if !list.contains(&text) {
                list.push(text);
            }
        }
    }
    let complete = d.transactions.len() >= 2 && d.transactions.iter().all(|t| !t.trx_id.is_empty());
    complete.then_some(d)
}

/// A `RECORD LOCKS …` or `TABLE LOCK …` line: its transaction, its description, and
/// whether the transaction waits for it.
fn lock_line(t: &str) -> Option<(String, String, bool)> {
    let trx = t
        .split_once(" trx id ")?
        .1
        .split_whitespace()
        .next()?
        .to_owned();
    let waiting = t.ends_with(" waiting");
    let table = t
        .split_once("table `")
        .and_then(|(_, r)| r.split_once(' '))
        .map(|(name, _)| format!("`{name}").replace('`', ""))?;
    let mode = |key: &str| {
        t.split_once(key)
            .and_then(|(_, r)| r.split_whitespace().next())
            .map(innodb_mode)
            .unwrap_or_default()
    };
    let text = if t.starts_with("TABLE LOCK ") {
        format!("{} table lock on {table}", capital(&mode(" lock mode ")))
    } else {
        let index = t
            .split_once(" index ")
            .and_then(|(_, r)| r.split_whitespace().next())
            .unwrap_or("?");
        let what = if t.contains("insert intention") {
            "insert intention lock"
        } else if t.contains("locks gap before rec") {
            "gap lock"
        } else if t.contains("locks rec but not gap") {
            "row lock"
        } else {
            "row and gap lock"
        };
        format!(
            "{} {what} on index {index} of {table}",
            capital(&mode(" lock_mode "))
        )
    };
    Some((trx, text, waiting))
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// `48`, `2 min 14 s`, `3 h 5 min`, as the design writes durations.
pub fn duration(seconds: u64) -> String {
    match seconds {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min {} s", s / 60, s % 60),
        s if s < 86_400 => format!("{} h {} min", s / 3600, s / 60 % 60),
        s => format!("{} d {} h", s / 86_400, s / 3600 % 24),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: SessionId, state: State) -> Session {
        Session {
            id,
            user: "app".into(),
            host: "localhost".into(),
            application: None,
            database: None,
            state,
            detail: String::new(),
            seconds: None,
            statement: None,
            running: false,
            transaction: None,
            holds: Vec::new(),
            own: false,
            deadlock: false,
            age: None,
        }
    }

    fn wait(waiter: SessionId, blocker: Option<SessionId>) -> LockWait {
        LockWait {
            waiter,
            blocker,
            wants: Lock {
                kind: LockKind::Row,
                mode: "exclusive".into(),
                schema: "shop".into(),
                table: "orders".into(),
                index: Some("PRIMARY".into()),
                row: Some("1".into()),
                rows: 1,
            },
            seconds: Some(3),
        }
    }

    #[test]
    fn chains_start_at_the_root_blocker() {
        let mut s = Snapshot {
            sessions: [1, 2, 3, 4].map(|id| session(id, State::Active)).to_vec(),
            waits: vec![wait(2, Some(1)), wait(3, Some(2)), wait(4, None)],
            ..Snapshot::default()
        };
        s.mark_waiting();
        assert_eq!(s.waiting(), 3);
        assert_eq!(s.count(Filter::Active), 4);
        let chains = s.chains();
        assert_eq!(chains.len(), 2);
        assert_eq!(chains[0].levels, vec![vec![1], vec![2], vec![3]]);
        assert!(!chains[0].cycle);
        assert_eq!(chains[1].levels, vec![vec![4]]);
        assert!(chains[1].unknown_blocker);
    }

    #[test]
    fn a_circle_of_waits_is_a_cycle() {
        let s = Snapshot {
            waits: vec![wait(5, Some(6)), wait(6, Some(5))],
            ..Snapshot::default()
        };
        let chains = s.chains();
        assert_eq!(chains.len(), 1);
        assert!(chains[0].cycle);
        assert_eq!(chains[0].levels, vec![vec![5], vec![6]]);
    }

    #[test]
    fn one_holder_blocking_two_is_one_chain() {
        let s = Snapshot {
            waits: vec![wait(2, Some(1)), wait(3, Some(1))],
            ..Snapshot::default()
        };
        assert_eq!(s.chains()[0].levels, vec![vec![1], vec![2, 3]]);
    }

    #[test]
    fn metadata_lock_rules_follow_the_server() {
        // ALTER waits for EXCLUSIVE while an open transaction holds SHARED_READ.
        assert!(mdl_blocks("EXCLUSIVE", "SHARED_READ", true));
        // A SELECT queues behind the waiting ALTER, not behind the reader.
        assert!(!mdl_blocks("SHARED_READ", "SHARED_READ", true));
        assert!(!mdl_blocks("SHARED_READ", "SHARED_UPGRADABLE", true));
        assert!(mdl_blocks("SHARED_READ", "EXCLUSIVE", false));
        assert!(mdl_blocks("MDL_SHARED_WRITE", "MDL_SHARED_NO_WRITE", true));
        assert!(!mdl_blocks("EXCLUSIVE", "EXCLUSIVE", false));
        assert!(!mdl_blocks("BACKUP", "EXCLUSIVE", true));
    }

    #[test]
    fn modes_read_as_words() {
        assert_eq!(innodb_mode("X,REC_NOT_GAP"), "exclusive");
        assert_eq!(innodb_mode("S,GAP"), "shared gap");
        assert_eq!(innodb_mode("IX"), "intention exclusive");
        assert_eq!(mdl_mode("MDL_SHARED_READ"), "shared read");
        let l = wait(1, None).wants;
        assert_eq!(
            l.describe(),
            "Exclusive lock on the row in shop.orders with PRIMARY = 1"
        );
        assert_eq!(l.short(), "row in orders, PRIMARY = 1");
    }

    #[test]
    fn durations_read_like_the_design() {
        assert_eq!(duration(48), "48 s");
        assert_eq!(duration(134), "2 min 14 s");
        assert_eq!(duration(3 * 3600 + 300), "3 h 5 min");
    }

    #[test]
    fn mysql_deadlock_report_parses() {
        let status = include_str!("../tests/fixtures/innodb_status/mysql-8.4-deadlock.txt");
        let section = deadlock_section(status).unwrap();
        assert!(section.starts_with("2026-11-22 23:59:55"));
        assert!(section.ends_with("*** WE ROLL BACK TRANSACTION (2)"));
        let d = parse_deadlock(section).unwrap();
        assert_eq!(d.at, "2026-11-22 23:59:55");
        assert_eq!(d.rolled_back, Some(2));
        let [a, b] = d.transactions.as_slice() else {
            panic!("two transactions")
        };
        assert_eq!(
            (a.number, a.trx_id.as_str(), a.thread),
            (1, "85562", Some(42))
        );
        assert_eq!(a.who, "root@localhost");
        assert_eq!(a.statement, "UPDATE p9_orders SET total=5 WHERE id=1");
        assert_eq!(
            a.holds,
            ["Exclusive row lock on index PRIMARY of deadlock.p9_orders"]
        );
        assert_eq!(a.waits_for, a.holds);
        assert_eq!(b.statement, "UPDATE p9_orders SET status='a' WHERE id=2");
        assert_eq!(b.thread, Some(41));
        assert_eq!(b.holds.len(), 1);
        assert_eq!(b.waits_for.len(), 1);
    }

    #[test]
    fn mariadb_deadlock_report_parses() {
        let status = include_str!("../tests/fixtures/innodb_status/mariadb-11-deadlock.txt");
        let d = parse_deadlock(deadlock_section(status).unwrap()).unwrap();
        assert_eq!(d.rolled_back, Some(1));
        let [a, b] = d.transactions.as_slice() else {
            panic!("two transactions")
        };
        assert_eq!((a.trx_id.as_str(), a.thread), ("1991", Some(63)));
        assert_eq!(a.statement, "UPDATE p9_orders SET status='a' WHERE id=2");
        // MariaDB lists the other transaction's lock under CONFLICTING WITH.
        assert_eq!(a.holds.len(), 1);
        assert_eq!(a.waits_for.len(), 1);
        assert_eq!((b.trx_id.as_str(), b.thread), ("1992", Some(64)));
        assert_eq!(b.holds.len(), 1);
    }

    #[test]
    fn no_deadlock_and_garbage_are_told_apart() {
        assert_eq!(
            deadlock_section("=====\nTRANSACTIONS\n------------\n"),
            None
        );
        let odd = "LATEST DETECTED DEADLOCK\n------------------------\nsomething else\n------------\nTRANSACTIONS\n";
        assert_eq!(deadlock_section(odd), Some("something else"));
        assert_eq!(parse_deadlock("something else"), None);
    }
}
