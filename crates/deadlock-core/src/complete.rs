//! SQL autocomplete: what the caret is in (table names, a table's columns, plain
//! keywords), the tables a statement names and their aliases, and the ranked
//! suggestions from the schema tree's nodes. Works on the tokens [`split`] scans with,
//! so half-typed SQL never fails to parse.

use std::ops::Range;

use crate::connection::Engine;
use crate::driver::{NodeKind, ObjectNode};
use crate::settings::KeywordCase;
use crate::split;

/// The most suggestions one popup lists.
pub const LIMIT: usize = 50;

#[rustfmt::skip]
const MYSQL_KEYWORDS: &[&str] = &[
    "ADD", "AFTER", "ALL", "ALTER", "AND", "AS", "ASC", "AUTO_INCREMENT", "BEGIN",
    "BETWEEN", "BY", "CALL", "CASE", "CHANGE", "CHARACTER", "CHECK", "COLLATE", "COLUMN",
    "COMMIT", "CONSTRAINT", "CREATE", "CROSS", "DATABASE", "DEFAULT", "DELETE", "DESC",
    "DESCRIBE", "DISTINCT", "DROP", "DUPLICATE", "ELSE", "END", "ENGINE", "EXCEPT",
    "EXISTS", "EXPLAIN", "FALSE", "FOR", "FOREIGN", "FROM", "FUNCTION", "GRANT", "GROUP",
    "HAVING", "IF", "IGNORE", "IN", "INDEX", "INNER", "INSERT", "INTERSECT", "INTERVAL",
    "INTO", "IS", "JOIN", "KEY", "LEFT", "LIKE", "LIMIT", "LOCK", "MODIFY", "NATURAL",
    "NOT", "NULL", "OFFSET", "ON", "OR", "ORDER", "OUTER", "OVER", "PARTITION", "PRIMARY",
    "PROCEDURE", "RECURSIVE", "REFERENCES", "RENAME", "REPLACE", "RETURNING", "REVOKE",
    "RIGHT", "ROLLBACK", "SCHEMA", "SELECT", "SET", "SHOW", "START", "TABLE", "THEN", "TO",
    "TRANSACTION", "TRIGGER", "TRUE", "TRUNCATE", "UNION", "UNIQUE", "UPDATE", "USE",
    "USING", "VALUES", "VIEW", "WHEN", "WHERE", "WINDOW", "WITH",
];

#[rustfmt::skip]
const ORACLE_KEYWORDS: &[&str] = &[
    "ADD", "ALL", "ALTER", "AND", "AS", "ASC", "BEGIN", "BETWEEN", "BODY", "BY", "CASE",
    "CHECK", "COMMENT", "COMMIT", "CONNECT", "CONSTRAINT", "CREATE", "CROSS", "DECLARE",
    "DEFAULT", "DELETE", "DESC", "DISTINCT", "DROP", "ELSE", "END", "EXCEPTION", "EXISTS",
    "FETCH", "FIRST", "FOR", "FOREIGN", "FROM", "FULL", "FUNCTION", "GRANT", "GROUP",
    "HAVING", "IN", "INDEX", "INNER", "INSERT", "INTERSECT", "INTO", "IS", "JOIN", "KEY",
    "LEFT", "LIKE", "MATCHED", "MERGE", "MINUS", "MODIFY", "NATURAL", "NEXT", "NOT", "NULL",
    "NULLS", "OFFSET", "ON", "ONLY", "OR", "ORDER", "OUTER", "OVER", "PACKAGE", "PARTITION",
    "PRIMARY", "PRIOR", "PROCEDURE", "REFERENCES", "RENAME", "REPLACE", "RETURN",
    "RETURNING", "REVOKE", "RIGHT", "ROLLBACK", "ROWS", "SAVEPOINT", "SELECT", "SEQUENCE",
    "SET", "START", "SYNONYM", "TABLE", "THEN", "TO", "TRIGGER", "TRUNCATE", "UNION",
    "UNIQUE", "UPDATE", "USING", "VALUES", "VIEW", "WHEN", "WHERE", "WITH",
];

#[rustfmt::skip]
const MYSQL_FUNCTIONS: &[&str] = &[
    "ABS", "AVG", "CAST", "CEIL", "CHAR_LENGTH", "COALESCE", "CONCAT", "CONCAT_WS",
    "CONVERT", "COUNT", "CURDATE", "CURTIME", "DATE", "DATEDIFF", "DATE_ADD", "DATE_FORMAT",
    "DATE_SUB", "DAY", "DENSE_RANK", "FIND_IN_SET", "FIRST_VALUE", "FLOOR", "FORMAT",
    "FROM_UNIXTIME", "GREATEST", "GROUP_CONCAT", "HEX", "HOUR", "IFNULL", "INSTR",
    "JSON_ARRAY", "JSON_EXTRACT", "JSON_OBJECT", "JSON_UNQUOTE", "LAG", "LAST_VALUE",
    "LEAD", "LEAST", "LENGTH", "LOCATE", "LOWER", "LPAD", "LTRIM", "MAX", "MD5", "MIN",
    "MINUTE", "MOD", "MONTH", "NOW", "NULLIF", "RAND", "RANK", "ROUND", "ROW_NUMBER",
    "RPAD", "RTRIM", "SHA2", "STR_TO_DATE", "SUBSTRING", "SUBSTRING_INDEX", "SUM",
    "TIMESTAMPDIFF", "TRIM", "UNIX_TIMESTAMP", "UPPER", "UUID", "YEAR",
];

#[rustfmt::skip]
const ORACLE_FUNCTIONS: &[&str] = &[
    "ABS", "ADD_MONTHS", "AVG", "CAST", "CEIL", "COALESCE", "CONCAT", "COUNT", "DECODE",
    "DENSE_RANK", "EXTRACT", "FIRST_VALUE", "FLOOR", "GREATEST", "INITCAP", "INSTR",
    "JSON_QUERY", "JSON_VALUE", "LAG", "LAST_DAY", "LAST_VALUE", "LEAD", "LEAST", "LENGTH",
    "LISTAGG", "LOWER", "LPAD", "LTRIM", "MAX", "MIN", "MOD", "MONTHS_BETWEEN", "NEXT_DAY",
    "NULLIF", "NVL", "NVL2", "RANK", "REGEXP_LIKE", "REGEXP_REPLACE", "REGEXP_SUBSTR",
    "REPLACE", "ROUND", "ROW_NUMBER", "RPAD", "RTRIM", "SUBSTR", "SUM", "SYSDATE",
    "SYSTIMESTAMP", "SYS_GUID", "TO_CHAR", "TO_DATE", "TO_NUMBER", "TO_TIMESTAMP", "TRIM",
    "TRUNC", "UPPER",
];

pub fn keywords(engine: Engine) -> &'static [&'static str] {
    if engine == Engine::Oracle {
        ORACLE_KEYWORDS
    } else {
        MYSQL_KEYWORDS
    }
}

pub fn functions(engine: Engine) -> &'static [&'static str] {
    if engine == Engine::Oracle {
        ORACLE_FUNCTIONS
    } else {
        MYSQL_FUNCTIONS
    }
}

fn is_keyword(engine: Engine, word: &str) -> bool {
    keywords(engine)
        .iter()
        .any(|k| k.eq_ignore_ascii_case(word))
}

/// `name` as SQL text: bare when the engine reads it back unchanged, else quoted
/// (backticks on MySQL and MariaDB; double quotes on Oracle for anything but a plain
/// upper-case identifier).
pub fn ident(engine: Engine, name: &str) -> String {
    let mut chars = name.chars();
    let first = chars.next();
    if engine == Engine::Oracle {
        let plain = first.is_some_and(|c| c.is_ascii_uppercase())
            && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || "_$#".contains(c));
        if plain && !is_keyword(engine, name) {
            name.to_owned()
        } else {
            format!("\"{}\"", name.replace('"', "\"\""))
        }
    } else {
        let plain = first.is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
        if plain && !is_keyword(engine, name) {
            name.to_owned()
        } else {
            crate::edit::quote_ident(name)
        }
    }
}

/// A quoted identifier token without its quotes; anything else as written.
fn unquote(engine: Engine, token: &str) -> String {
    let q = token.as_bytes().first().copied();
    let quoted = match q {
        Some(b'`') => engine != Engine::Oracle,
        Some(b'"') => engine == Engine::Oracle,
        _ => false,
    };
    if quoted && token.len() >= 2 {
        let q = &token[..1];
        token[1..token.len() - 1].replace(&q.repeat(2), q)
    } else {
        token.to_owned()
    }
}

fn is_word(token: &str) -> bool {
    token
        .bytes()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$' | b'@') || c >= 0x80)
}

/// A name token: a word or a quoted identifier.
fn is_name(engine: Engine, token: &str) -> bool {
    is_word(token)
        || match token.as_bytes()[0] {
            b'`' => engine != Engine::Oracle,
            b'"' => engine == Engine::Oracle,
            _ => false,
        }
}

/// The statement around `offset`: its start, and its end (at least `offset`).
fn statement(text: &str, offset: usize, engine: Engine) -> Range<usize> {
    let Some(s) = split::split(text, engine)
        .into_iter()
        .take_while(|s| s.start <= offset)
        .last()
    else {
        return offset..offset;
    };
    if s.end < offset && text[s.end..offset].contains(';') {
        return offset..offset;
    }
    s.start..s.end.max(offset)
}

/// Whether the end of `before` is inside a string, a quoted identifier or a comment.
fn in_literal(before: &str, engine: Engine) -> bool {
    let tokens = split::tokens(before, engine);
    let last_end = tokens.last().map_or(0, |t| t.end);
    if let Some(t) = tokens.last().filter(|t| t.end == before.len()) {
        let tok = &before.as_bytes()[t.clone()];
        let mysql = engine != Engine::Oracle;
        match tok[0] {
            b'\'' | b'"' | b'`' => return !closed(tok, mysql && tok[0] != b'`'),
            b'q' | b'Q' if tok.get(1) == Some(&b'\'') && !mysql => {
                let close = match tok.get(2) {
                    Some(b'[') => b']',
                    Some(b'(') => b')',
                    Some(b'{') => b'}',
                    Some(b'<') => b'>',
                    Some(&c) => c,
                    None => return true,
                };
                let n = tok.len();
                return !(n >= 5 && tok[n - 1] == b'\'' && tok[n - 2] == close);
            }
            _ => {}
        }
    }
    // Only blanks and comments follow the last token: is a comment still open?
    let gap = &before[last_end..];
    let mut i = 0;
    let b = gap.as_bytes();
    while i < b.len() {
        let line =
            b[i] == b'-' && b.get(i + 1) == Some(&b'-') || b[i] == b'#' && engine != Engine::Oracle;
        if line {
            match gap[i..].find('\n') {
                Some(n) => i += n + 1,
                None => return true,
            }
        } else if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            match gap[i + 2..].find("*/") {
                Some(n) => i += 2 + n + 2,
                None => return true,
            }
        } else {
            i += 1;
        }
    }
    false
}

/// A quoted token that ends with its closing quote.
fn closed(tok: &[u8], backslash: bool) -> bool {
    let q = tok[0];
    let mut j = 1;
    while j < tok.len() {
        match tok[j] {
            b'\\' if backslash => j += 2,
            c if c == q && tok.get(j + 1) == Some(&q) => j += 2,
            c if c == q => return j + 1 == tok.len(),
            _ => j += 1,
        }
    }
    false
}

/// What the caret is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Context {
    /// After FROM, JOIN, INTO, UPDATE or TABLE: a table name.
    Tables,
    /// SELECT, WHERE, ON, BY, HAVING, SET: columns, functions and keywords.
    Columns,
    /// After `name.` or `schema.table.`: what is inside it. The bool is a table position.
    Member(Vec<String>, bool),
    Keywords,
}

const TABLE_CLAUSES: &[&str] = &[
    "FROM", "JOIN", "INTO", "UPDATE", "TABLE", "DESCRIBE", "TRUNCATE",
];
const COLUMN_CLAUSES: &[&str] = &[
    "SELECT",
    "WHERE",
    "ON",
    "BY",
    "HAVING",
    "SET",
    "USING",
    "RETURNING",
];
const OTHER_CLAUSES: &[&str] = &["VALUES", "LIMIT", "WITH", "FETCH", "OFFSET", "UNION"];

/// The context at the end of `before` (a statement up to the caret) and the word typed
/// so far. `None` inside a string or comment.
pub fn context(before: &str, engine: Engine) -> Option<(Context, String)> {
    if in_literal(before, engine) {
        return None;
    }
    let tokens: Vec<&str> = split::tokens(before, engine)
        .into_iter()
        .map(|r| &before[r])
        .collect();
    let mut rest = &tokens[..];
    let mut prefix = String::new();
    if let Some((last, head)) = rest.split_last()
        && is_word(last)
        && before.ends_with(last)
    {
        prefix = (*last).to_owned();
        rest = head;
    }
    let mut qualifier = Vec::new();
    while let [head @ .., name, "."] = rest
        && is_name(engine, name)
        && qualifier.len() < 2
    {
        qualifier.insert(0, unquote(engine, name));
        rest = head;
    }
    // A dot after something that is not a name: nothing to offer.
    if rest.last() == Some(&".") {
        return None;
    }
    let clause = clause(rest);
    let table_position = clause.as_ref().is_some_and(|(kw, i, crossed)| {
        TABLE_CLAUSES.contains(&kw.as_str())
            && !crossed
            && (i + 1 == rest.len() || rest.last() == Some(&",") && kw == "FROM")
    });
    if !qualifier.is_empty() {
        return Some((Context::Member(qualifier, table_position), prefix));
    }
    let context = match clause {
        _ if table_position => Context::Tables,
        Some((kw, _, _)) if COLUMN_CLAUSES.contains(&kw.as_str()) => Context::Columns,
        // `INSERT INTO t (` lists the table's columns.
        Some((kw, _, true)) if TABLE_CLAUSES.contains(&kw.as_str()) => Context::Columns,
        _ => Context::Keywords,
    };
    Some((context, prefix))
}

/// The nearest clause keyword before the caret, outside parentheses the caret is not
/// in: the keyword upper-cased, its index, and whether an open `(` lies between.
fn clause(tokens: &[&str]) -> Option<(String, usize, bool)> {
    let mut depth = 0usize;
    let mut crossed = false;
    for (i, t) in tokens.iter().enumerate().rev() {
        match *t {
            ")" => depth += 1,
            "(" if depth == 0 => crossed = true,
            "(" => depth -= 1,
            ";" => return None,
            t if depth == 0 => {
                let up = t.to_ascii_uppercase();
                if [TABLE_CLAUSES, COLUMN_CLAUSES, OTHER_CLAUSES]
                    .iter()
                    .any(|l| l.contains(&up.as_str()))
                {
                    return Some((up, i, crossed));
                }
            }
            _ => {}
        }
    }
    None
}

/// A table the statement names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
    pub alias: Option<String>,
    /// How the statement refers to it: the alias, else the name, as written.
    pub written: String,
}

/// Clause keywords that end a FROM list.
const FROM_ENDS: &[&str] = &[
    "WHERE",
    "GROUP",
    "ORDER",
    "HAVING",
    "LIMIT",
    "UNION",
    "SET",
    "SELECT",
    "WINDOW",
    "RETURNING",
    "FETCH",
    "CONNECT",
    "START",
];

/// The tables after FROM (and its commas), JOIN, UPDATE, INTO and TABLE in
/// `statement`, with aliases. Derived tables are skipped.
pub fn references(statement: &str, engine: Engine) -> Vec<TableRef> {
    let t: Vec<&str> = split::tokens(statement, engine)
        .into_iter()
        .map(|r| &statement[r])
        .collect();
    let mut out = Vec::new();
    let (mut i, mut depth) = (0, 0usize);
    // The parenthesis depth of the FROM list the scan is in.
    let mut from: Option<usize> = None;
    while i < t.len() {
        let up = t[i].to_ascii_uppercase();
        match t[i] {
            "(" => depth += 1,
            ")" => {
                depth = depth.saturating_sub(1);
                from = from.filter(|&d| d <= depth);
            }
            _ => {}
        }
        let starts = TABLE_CLAUSES.contains(&up.as_str()) || t[i] == "," && from == Some(depth);
        if up == "FROM" {
            from = Some(depth);
        } else if FROM_ENDS.contains(&up.as_str()) && from == Some(depth) {
            from = None;
        }
        i += 1;
        if starts {
            let (r, next) = table_at(&t, i, engine);
            out.extend(r);
            i = next;
        }
    }
    out
}

/// The table named at token `j`, with its alias, and the token after them.
fn table_at(t: &[&str], mut j: usize, engine: Engine) -> (Option<TableRef>, usize) {
    let mut parts = Vec::new();
    let mut last_written = "";
    if t.get(j) == Some(&"(") {
        // A derived table: its columns are unknown.
        let mut depth = 0;
        while j < t.len() {
            match t[j] {
                "(" => depth += 1,
                ")" => depth -= 1,
                _ => {}
            }
            j += 1;
            if depth == 0 {
                break;
            }
        }
    } else {
        while let Some(name) = t.get(j).filter(|n| plain_name(engine, n)) {
            parts.push(unquote(engine, name));
            last_written = *name;
            j += 1;
            if t.get(j) == Some(&".") {
                j += 1;
            } else {
                break;
            }
        }
    }
    if t.get(j).is_some_and(|a| a.eq_ignore_ascii_case("AS")) {
        j += 1;
    }
    let alias = t.get(j).filter(|a| plain_name(engine, a)).map(|a| {
        j += 1;
        *a
    });
    let r = parts.pop().map(|name| TableRef {
        schema: parts.pop(),
        name,
        alias: alias.map(|a| unquote(engine, a)),
        written: alias.unwrap_or(last_written).to_owned(),
    });
    (r, j)
}

/// A name that is not a keyword or a number.
fn plain_name(engine: Engine, token: &str) -> bool {
    is_name(engine, token)
        && !token.starts_with(|c: char| c.is_ascii_digit())
        && !(is_word(token) && is_keyword(engine, token))
}

/// How well `query` matches `name`: 0 a prefix in the same case, 1 a prefix in any
/// case, 2 a substring, 3 the characters in order. `None`: no match.
pub fn rank(query: &str, name: &str) -> Option<u8> {
    if name.starts_with(query) {
        return Some(0);
    }
    let (q, n) = (query.to_lowercase(), name.to_lowercase());
    if n.starts_with(&q) {
        Some(1)
    } else if n.contains(&q) {
        Some(2)
    } else {
        let mut rest = n.chars();
        q.chars()
            .all(|c| rest.by_ref().any(|n| n == c))
            .then_some(3)
    }
}

/// Keywords in the case of what was typed; with nothing typed, as the setting says.
pub fn upper_keywords(prefix: &str, case: KeywordCase) -> bool {
    match prefix.chars().find(|c| c.is_alphabetic()) {
        Some(first) => first.is_uppercase(),
        None => case != KeywordCase::Lower,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Keyword,
    Function,
    Schema,
    Table,
    View,
    Column,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub label: String,
    pub insert: String,
    pub detail: String,
    pub kind: ItemKind,
}

/// The schema tree of the connection: its databases (schemas) as the sidebar holds them.
pub struct Catalog<'a> {
    pub schemas: &'a [ObjectNode],
    pub hidden: &'a [String],
    /// The schema unqualified names resolve in.
    pub current: Option<&'a str>,
}

impl Catalog<'_> {
    fn schema(&self, name: &str) -> Option<&ObjectNode> {
        self.schemas.iter().find(|s| s.name == name).or_else(|| {
            self.schemas
                .iter()
                .find(|s| s.name.eq_ignore_ascii_case(name))
        })
    }

    fn visible(&self) -> impl Iterator<Item = &ObjectNode> {
        self.schemas
            .iter()
            .filter(|s| !self.hidden.contains(&s.name))
    }

    /// The tables and views of `schema`, `None` when not loaded.
    fn tables(schema: &ObjectNode) -> Option<impl Iterator<Item = &ObjectNode>> {
        Some(
            schema
                .children
                .as_deref()?
                .iter()
                .flat_map(|g| g.children.as_deref().unwrap_or_default())
                .filter(|n| matches!(n.kind, NodeKind::Table | NodeKind::View)),
        )
    }

    /// The table `name` in `schema` (else the current schema, else any loaded one).
    /// A schema to look in that is not loaded goes to `missing`.
    fn table(
        &self,
        schema: Option<&str>,
        name: &str,
        missing: &mut Vec<String>,
    ) -> Option<&ObjectNode> {
        if let Some(s) = schema {
            return find_table(self.schema(s)?, name, missing);
        }
        let current = self.current.and_then(|c| self.schema(c));
        current
            .and_then(|s| find_table(s, name, missing))
            .or_else(|| {
                self.schemas
                    .iter()
                    .filter(|s| s.children.is_some())
                    .find_map(|s| find_table(s, name, missing))
            })
    }
}

/// The table `name` in `schema`; a schema not loaded goes to `missing`.
fn find_table<'a>(
    schema: &'a ObjectNode,
    name: &str,
    missing: &mut Vec<String>,
) -> Option<&'a ObjectNode> {
    let Some(list) = Catalog::tables(schema) else {
        missing.push(schema.name.clone());
        return None;
    };
    let list: Vec<&ObjectNode> = list.collect();
    list.iter()
        .find(|t| t.name == name)
        .or_else(|| list.iter().find(|t| t.name.eq_ignore_ascii_case(name)))
        .copied()
}

fn columns(t: &ObjectNode) -> Vec<&ObjectNode> {
    t.children
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|c| c.kind == NodeKind::Column)
        .collect()
}

/// A schema object a Ctrl/Cmd-click on its name opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Byte range of the name in the text.
    pub range: Range<usize>,
    pub schema: String,
    /// The tree group it is listed in (`Tables`, `Views`, `Procedures`…).
    pub group: String,
    pub name: String,
    pub kind: NodeKind,
}

impl Catalog<'_> {
    /// The table, view or routine `name` in `schema` (else the current schema), with
    /// the schema and group it is listed under. A schema not loaded goes to `missing`.
    fn object(
        &self,
        schema: Option<&str>,
        name: &str,
        missing: &mut Vec<String>,
    ) -> Option<(&ObjectNode, &ObjectNode, &ObjectNode)> {
        let s = self.schema(schema.or(self.current)?)?;
        let Some(groups) = s.children.as_deref() else {
            missing.push(s.name.clone());
            return None;
        };
        let objects = || {
            groups.iter().flat_map(|g| {
                g.children
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .filter(|o| {
                        matches!(o.kind, NodeKind::Table | NodeKind::View | NodeKind::Routine)
                    })
                    .map(move |o| (g, o))
            })
        };
        objects()
            .find(|(_, o)| o.name == name)
            .or_else(|| objects().find(|(_, o)| o.name.eq_ignore_ascii_case(name)))
            .map(|(g, o)| (s, g, o))
    }
}

/// The object whose name is under `offset` in `text`: `schema.name`, or `name` in
/// the current schema, else the table a statement alias stands for (`FROM orders o`:
/// `o` and `o.id`'s `o` are `orders`). Columns, schemas, keywords, strings and
/// comments are not links. A schema to look in that is not loaded goes to `missing`.
pub fn link_at(
    text: &str,
    offset: usize,
    engine: Engine,
    catalog: &Catalog,
    missing: &mut Vec<String>,
) -> Option<Link> {
    if engine == Engine::Redis {
        return None;
    }
    let stmt = statement(text, offset, engine);
    let sql = &text[stmt.clone()];
    let at = offset - stmt.start;
    let toks = split::tokens(sql, engine);
    let tok = |i: usize| toks.get(i).map(|r| &sql[r.clone()]);
    let name = |i: usize| tok(i).filter(|t| plain_name(engine, t));
    // The pointer on a name's last half character sits at its end.
    let k = toks
        .iter()
        .position(|r| r.start <= at && at < r.end)
        .filter(|&i| name(i).is_some())
        .or_else(|| toks.iter().position(|r| r.end == at))
        .filter(|&i| name(i).is_some())?;
    // The dotted chain the name is part of: `schema.table.column`.
    let mut first = k;
    while first >= 2 && tok(first - 1) == Some(".") && name(first - 2).is_some() {
        first -= 2;
    }
    let mut last = k;
    while tok(last + 1) == Some(".") && name(last + 2).is_some() {
        last += 2;
    }
    let parts: Vec<String> = (first..=last)
        .step_by(2)
        .map(|i| unquote(engine, &sql[toks[i].clone()]))
        .collect();
    let found = match (k - first) / 2 {
        0 => catalog.object(None, &parts[0], missing).or_else(|| {
            let r = references(sql, engine).into_iter().find(|r| {
                r.alias
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(&parts[0]))
            })?;
            catalog.object(r.schema.as_deref(), &r.name, missing)
        }),
        // `schema.name`; after a table or an alias it is a column.
        1 if catalog.schema(&parts[0]).is_some() => {
            catalog.object(Some(&parts[0]), &parts[1], missing)
        }
        _ => None,
    };
    let (schema, group, object) = found?;
    let r = &toks[k];
    Some(Link {
        range: stmt.start + r.start..stmt.start + r.end,
        schema: schema.name.clone(),
        group: group.name.clone(),
        name: object.name.clone(),
        kind: object.kind,
    })
}

/// Suggestions for the caret at `offset` in `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Byte range the chosen item replaces: the word typed so far.
    pub replace: Range<usize>,
    /// Best first, at most [`LIMIT`].
    pub items: Vec<Item>,
    /// Schemas whose objects would add suggestions but are not loaded.
    pub missing: Vec<String>,
}

/// What to offer at `offset`. `None` inside strings and comments, or after a number.
pub fn complete(
    text: &str,
    offset: usize,
    engine: Engine,
    catalog: &Catalog,
    case: KeywordCase,
) -> Option<Completion> {
    let stmt = statement(text, offset, engine);
    let before = &text[stmt.start..offset];
    let (context, prefix) = context(before, engine)?;
    if prefix.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let refs = references(&text[stmt.clone()], engine);
    let upper = upper_keywords(&prefix, case);
    let cased = |w: &str| {
        if upper {
            w.to_owned()
        } else {
            w.to_lowercase()
        }
    };
    let id = |n: &str| ident(engine, n);
    // (group, the text matched, item)
    let mut found: Vec<(u8, String, Item)> = Vec::new();
    let mut missing = Vec::new();
    let table_item = |group: u8, schema: Option<&str>, t: &ObjectNode| {
        let kind = if t.kind == NodeKind::View {
            ItemKind::View
        } else {
            ItemKind::Table
        };
        let detail = if kind == ItemKind::View {
            "view"
        } else {
            "table"
        };
        let (label, insert) = match schema {
            Some(s) => (
                format!("{s}.{}", t.name),
                format!("{}.{}", id(s), id(&t.name)),
            ),
            None => (t.name.clone(), id(&t.name)),
        };
        (
            group,
            t.name.clone(),
            Item {
                label,
                insert,
                detail: detail.to_owned(),
                kind,
            },
        )
    };
    let column_item = |group: u8, qualifier: Option<&str>, c: &ObjectNode, table: &str| {
        let (label, insert) = match qualifier {
            Some(q) => (format!("{q}.{}", c.name), format!("{q}.{}", id(&c.name))),
            None => (c.name.clone(), id(&c.name)),
        };
        let detail = if c.detail.is_empty() {
            table.to_owned()
        } else {
            format!("{}  {table}", c.detail)
        };
        (
            group,
            c.name.clone(),
            Item {
                label,
                insert,
                detail,
                kind: ItemKind::Column,
            },
        )
    };
    let keywords_and_functions = |found: &mut Vec<(u8, String, Item)>, functions: bool| {
        if functions {
            for f in self::functions(engine) {
                let f = cased(f);
                found.push((
                    1,
                    f.clone(),
                    Item {
                        label: f.clone(),
                        insert: f,
                        detail: "function".into(),
                        kind: ItemKind::Function,
                    },
                ));
            }
        }
        for k in keywords(engine) {
            let k = cased(k);
            found.push((
                2,
                k.clone(),
                Item {
                    label: k.clone(),
                    insert: k,
                    detail: "keyword".into(),
                    kind: ItemKind::Keyword,
                },
            ));
        }
    };
    let schema_tables = |found: &mut Vec<(u8, String, Item)>,
                         missing: &mut Vec<String>,
                         s: &ObjectNode,
                         qualify: bool,
                         group: u8| {
        match Catalog::tables(s) {
            Some(list) => {
                for t in list {
                    found.push(table_item(group, qualify.then_some(s.name.as_str()), t));
                }
            }
            None => missing.push(s.name.clone()),
        }
    };

    match &context {
        Context::Tables => {
            let current = catalog.current.and_then(|c| catalog.schema(c));
            if let Some(s) = current.filter(|s| !catalog.hidden.contains(&s.name)) {
                schema_tables(&mut found, &mut missing, s, false, 0);
            }
            for s in catalog.visible() {
                if current.is_some_and(|c| c.name == s.name) {
                    continue;
                }
                if let Some(list) = Catalog::tables(s) {
                    for t in list {
                        found.push(table_item(1, Some(&s.name), t));
                    }
                }
                found.push((
                    2,
                    s.name.clone(),
                    Item {
                        label: s.name.clone(),
                        insert: id(&s.name),
                        detail: "schema".into(),
                        kind: ItemKind::Schema,
                    },
                ));
            }
        }
        Context::Member(q, table_position) => {
            let schema_only = |found: &mut Vec<_>, missing: &mut Vec<_>| {
                if let Some(s) = (q.len() == 1).then(|| catalog.schema(&q[0])).flatten() {
                    schema_tables(found, missing, s, false, 0);
                }
            };
            if *table_position {
                schema_only(&mut found, &mut missing);
            } else {
                let (schema, name) = match q.as_slice() {
                    [s, t] => (Some(s.as_str()), t.as_str()),
                    [t] => (None, t.as_str()),
                    _ => return None,
                };
                // An alias or table of the statement, then any table by that name.
                let r = schema.is_none().then(|| {
                    refs.iter()
                        .find(|r| {
                            r.alias
                                .as_deref()
                                .is_some_and(|a| a.eq_ignore_ascii_case(name))
                        })
                        .or_else(|| {
                            refs.iter()
                                .find(|r| r.alias.is_none() && r.name.eq_ignore_ascii_case(name))
                        })
                });
                let table = match r.flatten() {
                    Some(r) => catalog.table(r.schema.as_deref(), &r.name, &mut missing),
                    None => catalog.table(schema, name, &mut missing),
                };
                match table {
                    Some(t) => {
                        for c in columns(t) {
                            found.push(column_item(0, None, c, &t.name));
                        }
                    }
                    None => schema_only(&mut found, &mut missing),
                }
            }
        }
        Context::Columns => {
            let tables: Vec<(&TableRef, &ObjectNode)> = refs
                .iter()
                .filter_map(|r| {
                    let t = catalog.table(r.schema.as_deref(), &r.name, &mut missing)?;
                    Some((r, t))
                })
                .collect();
            let count = |name: &str| {
                tables
                    .iter()
                    .filter(|(_, t)| columns(t).iter().any(|c| c.name == name))
                    .count()
            };
            for (r, t) in &tables {
                for c in columns(t) {
                    let q = (count(&c.name) > 1).then_some(r.written.as_str());
                    found.push(column_item(0, q, c, &t.name));
                }
            }
            keywords_and_functions(&mut found, true);
        }
        Context::Keywords => keywords_and_functions(&mut found, false),
    }

    let mut ranked: Vec<(u8, u8, usize, Item)> = found
        .into_iter()
        .filter_map(|(group, matched, item)| {
            let r = rank(&prefix, &matched)?;
            Some((r, group, matched.len(), item))
        })
        .collect();
    ranked.sort_by(|a, b| {
        (a.0, a.1, a.2)
            .cmp(&(b.0, b.1, b.2))
            .then_with(|| a.3.label.cmp(&b.3.label))
    });
    ranked.dedup_by(|a, b| a.3.insert == b.3.insert);
    ranked.truncate(LIMIT);
    missing.sort();
    missing.dedup();
    Some(Completion {
        replace: offset - prefix.len()..offset,
        items: ranked.into_iter().map(|r| r.3).collect(),
        missing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MY: Engine = Engine::Mysql;
    const ORA: Engine = Engine::Oracle;

    fn ctx(before: &str) -> Option<(Context, String)> {
        context(before, MY)
    }

    #[test]
    fn contexts() {
        let k = |c, p: &str| Some((c, p.to_owned()));
        assert_eq!(ctx(""), k(Context::Keywords, ""));
        assert_eq!(ctx("sel"), k(Context::Keywords, "sel"));
        assert_eq!(ctx("SELECT * FROM "), k(Context::Tables, ""));
        assert_eq!(ctx("SELECT * FROM us"), k(Context::Tables, "us"));
        assert_eq!(ctx("SELECT * FROM a, b"), k(Context::Tables, "b"));
        assert_eq!(ctx("SELECT * FROM a JOIN "), k(Context::Tables, ""));
        assert_eq!(ctx("SELECT * FROM a x WH"), k(Context::Keywords, "WH"));
        assert_eq!(ctx("UPDATE t"), k(Context::Tables, "t"));
        assert_eq!(ctx("INSERT INTO "), k(Context::Tables, ""));
        assert_eq!(ctx("INSERT INTO t (na"), k(Context::Columns, "na"));
        assert_eq!(ctx("SELECT na"), k(Context::Columns, "na"));
        assert_eq!(ctx("SELECT count(i"), k(Context::Columns, "i"));
        assert_eq!(
            ctx("SELECT * FROM t WHERE a = 1 AND b"),
            k(Context::Columns, "b")
        );
        assert_eq!(ctx("SELECT * FROM t ORDER BY "), k(Context::Columns, ""));
        assert_eq!(
            ctx("SELECT * FROM t WHERE id IN (SELECT x FROM "),
            k(Context::Tables, "")
        );
        assert_eq!(
            ctx("SELECT u."),
            k(Context::Member(vec!["u".into()], false), "")
        );
        assert_eq!(
            ctx("SELECT `my db`.t.na"),
            k(
                Context::Member(vec!["my db".into(), "t".into()], false),
                "na"
            )
        );
        assert_eq!(
            ctx("SELECT * FROM app.o"),
            k(Context::Member(vec!["app".into()], true), "o")
        );
        assert_eq!(ctx("SELECT 1; SEL"), k(Context::Keywords, "SEL"));
    }

    #[test]
    fn nothing_inside_strings_and_comments() {
        assert_eq!(ctx("SELECT 'abc"), None);
        assert_eq!(ctx("SELECT 'it''s"), None);
        assert!(ctx("SELECT 'abc' ").is_some());
        assert_eq!(ctx("SELECT 1 -- fro"), None);
        assert_eq!(ctx("SELECT 1 # fro"), None);
        assert!(ctx("SELECT 1 -- x\nFROM ").is_some());
        assert_eq!(ctx("SELECT /* a"), None);
        assert!(ctx("SELECT /* a */ x").is_some());
        assert_eq!(context("SELECT q'[abc", ORA), None);
        assert!(context("SELECT q'[abc]' ", ORA).is_some());
        assert_eq!(context("SELECT \"Mi", ORA), None);
    }

    #[test]
    fn aliases_resolve() {
        let r = references(
            "SELECT * FROM shop.orders o JOIN `order items` AS i ON i.id = o.id, users \
             WHERE x IN (SELECT y FROM z) ",
            MY,
        );
        let got: Vec<_> = r
            .iter()
            .map(|r| {
                (
                    r.schema.as_deref(),
                    r.name.as_str(),
                    r.alias.as_deref(),
                    r.written.as_str(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (Some("shop"), "orders", Some("o"), "o"),
                (None, "order items", Some("i"), "i"),
                (None, "users", None, "users"),
                (None, "z", None, "z"),
            ]
        );
        let r = references("UPDATE t SET a = 1 WHERE ", MY);
        assert_eq!((r[0].name.as_str(), r[0].alias.as_deref()), ("t", None));
        let r = references("SELECT * FROM (SELECT 1) d JOIN e ON", MY);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "e");
    }

    #[test]
    fn ranking() {
        assert_eq!(rank("us", "users"), Some(0));
        assert_eq!(rank("US", "users"), Some(1));
        assert_eq!(rank("ser", "users"), Some(2));
        assert_eq!(rank("urs", "users"), Some(3));
        assert_eq!(rank("x", "users"), None);
        assert_eq!(rank("", "users"), Some(0));
    }

    #[test]
    fn quoting() {
        assert_eq!(ident(MY, "users"), "users");
        assert_eq!(ident(MY, "order"), "`order`");
        assert_eq!(ident(MY, "my table"), "`my table`");
        assert_eq!(ident(MY, "1st"), "`1st`");
        assert_eq!(ident(MY, "a`b"), "`a``b`");
        assert_eq!(ident(ORA, "EMPLOYEES"), "EMPLOYEES");
        assert_eq!(ident(ORA, "Mixed"), "\"Mixed\"");
        assert_eq!(ident(ORA, "SELECT"), "\"SELECT\"");
        assert_eq!(ident(ORA, "A\"B"), "\"A\"\"B\"");
    }

    #[test]
    fn keyword_case_follows_typing() {
        assert!(!upper_keywords("sel", KeywordCase::Upper));
        assert!(upper_keywords("SEL", KeywordCase::Lower));
        assert!(upper_keywords("Sel", KeywordCase::Lower));
        assert!(!upper_keywords("", KeywordCase::Lower));
        assert!(upper_keywords("", KeywordCase::Upper));
        assert!(upper_keywords("", KeywordCase::Keep));
    }

    fn col(name: &str, ty: &str) -> ObjectNode {
        ObjectNode {
            detail: ty.into(),
            ..ObjectNode::new(name, NodeKind::Column)
        }
    }

    fn table(name: &str, cols: &[&str]) -> ObjectNode {
        ObjectNode {
            children: Some(cols.iter().map(|c| col(c, "int")).collect()),
            ..ObjectNode::new(name, NodeKind::Table)
        }
    }

    fn schema(name: &str, tables: Option<Vec<ObjectNode>>) -> ObjectNode {
        ObjectNode {
            children: tables.map(|t| {
                vec![ObjectNode {
                    children: Some(t),
                    ..ObjectNode::new("Tables", NodeKind::Group)
                }]
            }),
            ..ObjectNode::new(name, NodeKind::Database)
        }
    }

    #[test]
    fn links_name_tables_views_and_routines() {
        let mut app = schema(
            "app",
            Some(vec![table("orders", &["id"]), table("users", &["id"])]),
        );
        if let Some(groups) = app.children.as_mut() {
            groups.push(ObjectNode {
                children: Some(vec![ObjectNode::new("recent", NodeKind::View)]),
                ..ObjectNode::new("Views", NodeKind::Group)
            });
            groups.push(ObjectNode {
                children: Some(vec![ObjectNode::new("bump", NodeKind::Routine)]),
                ..ObjectNode::new("Procedures", NodeKind::Group)
            });
        }
        let schemas = vec![
            app,
            schema("log", Some(vec![table("events", &[])])),
            schema("cold", None),
        ];
        let cat = Catalog {
            schemas: &schemas,
            hidden: &[],
            current: Some("app"),
        };
        // `|` marks the pointer.
        let link = |marked: &str| {
            let at = marked.find('|').unwrap();
            let text = marked.replace('|', "");
            let mut missing = Vec::new();
            let found = link_at(&text, at, MY, &cat, &mut missing).map(|l| {
                let target = format!("{}.{}.{}", l.schema, l.group, l.name);
                (text[l.range].to_owned(), target)
            });
            (found, missing)
        };
        let hit = |n: &str, t: &str| (Some((n.to_owned(), t.to_owned())), Vec::<String>::new());
        let none = (None, Vec::<String>::new());
        assert_eq!(
            link("SELECT * FROM ord|ers"),
            hit("orders", "app.Tables.orders")
        );
        assert_eq!(
            link("SELECT * FROM |ORDERS"),
            hit("ORDERS", "app.Tables.orders")
        );
        assert_eq!(
            link("SELECT * FROM orders|"),
            hit("orders", "app.Tables.orders")
        );
        assert_eq!(
            link("SELECT * FROM log.ev|ents"),
            hit("events", "log.Tables.events")
        );
        assert_eq!(
            link("SELECT * FROM `log`.`ev|ents`"),
            hit("`events`", "log.Tables.events")
        );
        assert_eq!(
            link("SELECT * FROM rec|ent"),
            hit("recent", "app.Views.recent")
        );
        assert_eq!(link("CALL bu|mp(1)"), hit("bump", "app.Procedures.bump"));
        // Aliases stand for their table, alone or before a column.
        assert_eq!(
            link("SELECT o|.id FROM orders o"),
            hit("o", "app.Tables.orders")
        );
        assert_eq!(
            link("SELECT e|.id FROM log.events AS e"),
            hit("e", "log.Tables.events")
        );
        // Columns, schemas, keywords, strings and comments are not links.
        assert_eq!(link("SELECT o.i|d FROM orders o"), none);
        assert_eq!(link("SELECT orders.i|d FROM orders"), none);
        assert_eq!(link("SELECT * FROM lo|g.events"), none);
        assert_eq!(link("SEL|ECT 1"), none);
        assert_eq!(link("SELECT 'ord|ers'"), none);
        assert_eq!(link("SELECT 1 -- ord|ers"), none);
        assert_eq!(link("SELECT * FROM nowh|ere"), none);
        // A schema that is not loaded yet is asked for.
        assert_eq!(
            link("SELECT * FROM cold.t|1"),
            (None, vec!["cold".to_owned()])
        );
        // The statement under the pointer, not the one before it.
        assert_eq!(
            link("SELECT 1 FROM users u; SELECT u|.id FROM orders u"),
            hit("u", "app.Tables.orders")
        );
    }

    #[test]
    fn oracle_links_follow_upper_case_names() {
        let schemas = vec![schema("APP", Some(vec![table("ORDERS", &[])]))];
        let cat = Catalog {
            schemas: &schemas,
            hidden: &[],
            current: Some("APP"),
        };
        let at =
            |text: &str, n: usize| link_at(text, n, ORA, &cat, &mut Vec::new()).map(|l| l.name);
        assert_eq!(at("select * from orders", 16).as_deref(), Some("ORDERS"));
        assert_eq!(
            at("select * from app.orders", 20).as_deref(),
            Some("ORDERS")
        );
        assert_eq!(
            at(r#"select * from "APP"."ORDERS""#, 23).as_deref(),
            Some("ORDERS")
        );
    }

    fn labels(text: &str, cat: &Catalog) -> Vec<String> {
        complete(text, text.len(), MY, cat, KeywordCase::Lower)
            .unwrap()
            .items
            .into_iter()
            .map(|i| i.label)
            .collect()
    }

    #[test]
    fn suggestions_from_the_tree() {
        let schemas = vec![
            schema(
                "app",
                Some(vec![
                    table("users", &["id", "name"]),
                    table("orders", &["id", "user_id"]),
                ]),
            ),
            schema("other", Some(vec![table("users_old", &["id"])])),
            schema("secret", Some(vec![table("users_s", &["id"])])),
            schema("cold", None),
        ];
        let hidden = vec!["secret".to_owned()];
        let cat = Catalog {
            schemas: &schemas,
            hidden: &hidden,
            current: Some("app"),
        };
        // Current schema first, other schemas qualified, hidden left out.
        assert_eq!(
            labels("SELECT * FROM us", &cat),
            ["users", "other.users_old"]
        );
        // A hidden schema typed by name still completes.
        assert_eq!(labels("SELECT * FROM secret.", &cat), ["users_s"]);
        // Alias columns.
        let c = complete("SELECT u.", 9, MY, &cat, KeywordCase::Lower);
        assert!(c.unwrap().items.is_empty());
        let text = "SELECT u.n FROM users u";
        let c = complete(text, 10, MY, &cat, KeywordCase::Lower).unwrap();
        assert_eq!(c.replace, 9..10);
        assert_eq!(c.items[0].label, "name");
        // Ambiguous columns are qualified; then functions and keywords.
        let text = "SELECT i FROM users u JOIN orders o ON ";
        let c = complete(text, 8, MY, &cat, KeywordCase::Lower).unwrap();
        let l: Vec<_> = c.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(&l[..2], ["o.id", "u.id"]);
        assert!(l.contains(&"ifnull") && l.contains(&"in"));
        // A table in a schema not loaded yet asks for it.
        let c = complete(
            "SELECT * FROM cold.t WHERE x",
            28,
            MY,
            &cat,
            KeywordCase::Lower,
        );
        assert_eq!(c.unwrap().missing, ["cold"]);
        // Keywords keep the typed case.
        assert_eq!(labels("SEL", &cat), ["SELECT"]);
    }
}
