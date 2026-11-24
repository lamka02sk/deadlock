//! Search everywhere: object names, columns and the source text of routines, triggers,
//! views and events, matched in the database's own metadata on demand (nothing is
//! cached). The pure parts live here: LIKE patterns, ranking, the matched line of a
//! source and where it is in a definition. The engines' queries sit with each driver
//! ([`crate::driver::Driver::search_everywhere`]).

use std::ops::Range;

use crate::driver::{NodeKind, ObjectNode};

/// Rows each engine query returns at most; a group that reaches it says so.
pub const LIMIT: usize = 200;

/// The palette's groups, in the order it shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Group {
    Objects,
    Columns,
    Code,
}

/// A source line that matches: 1-based number, its text, the matched byte ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub number: usize,
    pub text: String,
    pub hits: Vec<Range<usize>>,
}

/// One result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub group: Group,
    /// Database or schema (`db0` on Redis).
    pub schema: String,
    /// The object: table, routine, trigger, …; the key on Redis.
    pub name: String,
    /// Lower case, as the engine names it: `table`, `view`, `procedure`, `package body`,
    /// `trigger`, `event`; the key type on Redis; `column` for columns.
    pub kind: String,
    /// Columns: the column and its type.
    pub column: Option<(String, String)>,
    /// Code: the first line that matches.
    pub line: Option<Line>,
}

impl Hit {
    pub fn object(schema: String, name: String, kind: String) -> Self {
        Self {
            group: Group::Objects,
            schema,
            name,
            kind,
            column: None,
            line: None,
        }
    }

    pub fn column(schema: String, table: String, column: String, ty: String) -> Self {
        Self {
            group: Group::Columns,
            column: Some((column, ty)),
            ..Self::object(schema, table, "column".into())
        }
    }

    /// A code hit for `source`, or `None` when no line of it matches `query`.
    pub fn code(
        schema: String,
        name: String,
        kind: String,
        source: &str,
        query: &str,
    ) -> Option<Self> {
        Some(Self {
            group: Group::Code,
            line: Some(first_line(source, query)?),
            ..Self::object(schema, name, kind)
        })
    }

    /// The text the hit's rank comes from: the column for columns, else the name.
    pub fn key(&self) -> &str {
        self.column.as_ref().map_or(&self.name, |c| &c.0)
    }

    /// Whether the hit is a table-like object that opens in a data tab.
    pub fn is_table(&self) -> bool {
        matches!(self.kind.as_str(), "table" | "view" | "materialized view")
    }
}

/// `%text%` for `LIKE … ESCAPE '!'`, with `!`, `%` and `_` escaped. `!` rather than
/// `\`, which MySQL's `NO_BACKSLASH_ESCAPES` would change the meaning of.
pub fn like_contains(text: &str) -> String {
    format!("%{}%", like_escape(text))
}

/// `text%` for `LIKE … ESCAPE '!'`.
pub fn like_prefix(text: &str) -> String {
    format!("{}%", like_escape(text))
}

fn like_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '!' | '%' | '_') {
            out.push('!');
        }
        out.push(c);
    }
    out
}

/// 0 for an exact match of `name`, 1 for a prefix, 2 otherwise; ignoring case.
pub fn rank(name: &str, query: &str) -> u8 {
    let (n, q) = (name.to_lowercase(), query.trim().to_lowercase());
    if n == q {
        0
    } else if n.starts_with(&q) {
        1
    } else {
        2
    }
}

/// Group order, then exact before prefix before substring, shorter names first, then
/// by name, schema and line.
pub fn sort_by<T>(items: &mut [T], query: &str, hit: impl Fn(&T) -> &Hit) {
    items.sort_by_cached_key(|t| {
        let h = hit(t);
        (
            h.group,
            rank(h.key(), query),
            h.key().chars().count(),
            h.key().to_lowercase(),
            h.name.to_lowercase(),
            h.schema.clone(),
            h.line.as_ref().map(|l| l.number),
        )
    });
}

/// Byte ranges of `hay` where `needle` occurs, ignoring case. Characters whose lower
/// case is longer than one char compare by their first, so offsets stay in `hay`.
pub fn find_all(hay: &str, needle: &str) -> Vec<Range<usize>> {
    let lower = |c: char| c.to_lowercase().next().unwrap_or(c);
    let q: Vec<char> = needle.trim().chars().map(lower).collect();
    if q.is_empty() {
        return Vec::new();
    }
    let chars: Vec<(usize, char)> = hay.char_indices().map(|(i, c)| (i, lower(c))).collect();
    let mut out = Vec::new();
    let mut s = 0;
    while s + q.len() <= chars.len() {
        if chars[s..s + q.len()].iter().zip(&q).all(|(c, q)| c.1 == *q) {
            let end = chars.get(s + q.len()).map_or(hay.len(), |c| c.0);
            out.push(chars[s].0..end);
            s += q.len();
        } else {
            s += 1;
        }
    }
    out
}

/// The first line of `source` that contains `query`, ignoring case.
pub fn first_line(source: &str, query: &str) -> Option<Line> {
    source.lines().enumerate().find_map(|(i, text)| {
        let hits = find_all(text, query);
        (!hits.is_empty()).then(|| Line {
            number: i + 1,
            text: text.to_owned(),
            hits,
        })
    })
}

/// A matched line cut to about `width` characters around its first hit, leading
/// space dropped, with `…` where text was cut; the ranges follow the cut.
pub fn snippet(line: &Line, width: usize) -> (String, Vec<Range<usize>>) {
    let text = line.text.trim_end();
    let lead = text.len() - text.trim_start().len();
    let first = line.hits.first().map_or(lead, |h| h.start);
    // Keep a little context before the hit, then up to `width` characters.
    let back: usize = text[..first.max(lead)]
        .chars()
        .rev()
        .take(width / 4)
        .map(char::len_utf8)
        .sum();
    let fits = text[lead..].chars().count() <= width;
    let start = if fits {
        lead
    } else {
        (first.max(lead) - back).max(lead)
    };
    let end = text[start..]
        .char_indices()
        .nth(width)
        .map_or(text.len(), |(i, _)| start + i);
    let (head, tail) = (start > lead, end < text.len());
    let mut out = String::new();
    if head {
        out.push('…');
    }
    let shift = out.len();
    out.push_str(&text[start..end]);
    if tail {
        out.push('…');
    }
    let hits = line
        .hits
        .iter()
        .filter(|h| h.start >= start && h.end <= end)
        .map(|h| h.start - start + shift..h.end - start + shift)
        .collect();
    (out, hits)
}

/// Where to put the caret for `line` in `definition`: 0-based line and character of
/// the first hit on the line with the same text, else on the line with the same
/// number, else the first hit anywhere. `definition` is the `CREATE` statement, which
/// can start with more lines than the stored source.
pub fn locate(definition: &str, line: &Line, query: &str) -> Option<(usize, usize)> {
    let lines: Vec<&str> = definition.lines().collect();
    let at = |i: usize| {
        let text = lines.get(i)?;
        let hit = find_all(text, query).into_iter().next()?;
        Some((i, text[..hit.start].chars().count()))
    };
    let want = line.text.trim();
    lines
        .iter()
        .position(|l| !want.is_empty() && l.trim() == want)
        .and_then(at)
        .or_else(|| at(line.number.saturating_sub(1)))
        .or_else(|| (0..lines.len()).find_map(at))
}

/// Names and columns from a connection's loaded tree (`nodes`: its databases) that
/// contain `query`. A database's groups load whole, columns included, so a loaded
/// database is complete; the rest only the server can answer.
pub fn tree_hits(nodes: &[ObjectNode], query: &str) -> Vec<Hit> {
    let mut out = Vec::new();
    let matches = |s: &str| !find_all(s, query).is_empty();
    for db in nodes.iter().filter(|n| n.kind == NodeKind::Database) {
        for group in db
            .children
            .iter()
            .flatten()
            .filter(|n| n.kind == NodeKind::Group)
        {
            for o in group.children.iter().flatten() {
                let kind = match o.kind {
                    NodeKind::Routine if matches!(o.detail.as_str(), "procedure" | "function") => {
                        o.detail.clone()
                    }
                    NodeKind::Table | NodeKind::View | NodeKind::Routine => singular(&group.name),
                    _ => continue,
                };
                if matches(&o.name) {
                    out.push(Hit::object(db.name.clone(), o.name.clone(), kind));
                }
                for c in o.children.iter().flatten() {
                    if c.kind == NodeKind::Column && matches(&c.name) {
                        out.push(Hit::column(
                            db.name.clone(),
                            o.name.clone(),
                            c.name.clone(),
                            c.detail.clone(),
                        ));
                    }
                }
            }
        }
    }
    out
}

/// Drops the hits in a hidden schema (exact names, as the tree shows them).
pub fn retain_visible(hits: &mut Vec<Hit>, hidden: &[String]) {
    hits.retain(|h| !hidden.contains(&h.schema));
}

/// `Materialized views` → `materialized view`: a tree group's objects by kind.
fn singular(group: &str) -> String {
    let g = group.to_lowercase();
    g.strip_suffix('s').map_or(g.clone(), str::to_owned)
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init, reason = "hit ranges")]
mod tests {
    use super::*;

    #[test]
    fn like_patterns_escape_wildcards_and_the_escape() {
        assert_eq!(like_contains("a_b%c!d"), "%a!_b!%c!!d%");
        assert_eq!(like_prefix("x_"), "x!_%");
        assert_eq!(like_contains("plain"), "%plain%");
    }

    #[test]
    fn exact_beats_prefix_beats_substring_then_shorter() {
        let mut hits = vec![
            Hit::object("s".into(), "my_orders".into(), "table".into()),
            Hit::column("s".into(), "t".into(), "orders".into(), "int".into()),
            Hit::object("s".into(), "orders_archive".into(), "table".into()),
            Hit::object("s".into(), "ORDERS".into(), "view".into()),
            Hit::object("s".into(), "orders_x".into(), "table".into()),
        ];
        sort_by(&mut hits, "orders", |h| h);
        let names: Vec<&str> = hits.iter().map(Hit::key).collect();
        assert_eq!(
            names,
            [
                "ORDERS",
                "orders_x",
                "orders_archive",
                "my_orders",
                "orders"
            ]
        );
        assert_eq!(hits[4].group, Group::Columns);
        assert_eq!(rank("Orders", " orders "), 0);
    }

    #[test]
    fn finds_every_occurrence_ignoring_case() {
        assert_eq!(
            find_all("Total = total + TOTAL", "total"),
            vec![0..5, 8..13, 16..21]
        );
        assert_eq!(find_all("café CAFÉ", "é"), vec![3..5, 9..11]);
        assert!(find_all("abc", " ").is_empty());
        assert!(find_all("ab", "abc").is_empty());
    }

    #[test]
    fn first_line_numbers_from_one() {
        let src = "BEGIN\n  SELECT 1;\n  UPDATE stock SET qty = 0;\nEND";
        let l = first_line(src, "STOCK").unwrap();
        assert_eq!((l.number, l.hits.clone()), (3, vec![9..14]));
        assert_eq!(&l.text[9..14], "stock");
        assert!(first_line(src, "nope").is_none());
    }

    #[test]
    fn snippets_cut_around_the_hit_and_shift_ranges() {
        let line = Line {
            number: 1,
            text: "    SELECT x FROM t".into(),
            hits: vec![18..19],
        };
        let (s, h) = snippet(&line, 80);
        assert_eq!(s, "SELECT x FROM t");
        assert_eq!(&s[h[0].clone()], "t");

        let text = format!("{}needle{}", "a".repeat(100), "b".repeat(100));
        let line = Line {
            number: 1,
            hits: vec![100..106],
            text,
        };
        let (s, h) = snippet(&line, 40);
        assert!(s.starts_with('…') && s.ends_with('…'));
        assert_eq!(&s[h[0].clone()], "needle");
        assert_eq!(s.chars().count(), 42);
    }

    #[test]
    fn locate_prefers_the_same_line_text() {
        let def = "CREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\n  UPDATE stock SET q = 0;\nEND";
        let line = first_line(
            "BEGIN\n  SELECT 1;\n  UPDATE stock SET q = 0;\nEND",
            "stock",
        )
        .unwrap();
        assert_eq!(line.number, 3);
        assert_eq!(locate(def, &line, "stock"), Some((3, 9)));
        // Text changed since: same number, else the first hit anywhere.
        let moved = Line {
            text: "gone".into(),
            ..line.clone()
        };
        assert_eq!(locate(def, &moved, "select"), Some((2, 2)));
        assert_eq!(locate(def, &moved, "stock"), Some((3, 9)));
        assert_eq!(locate(def, &moved, "zzz"), None);
    }

    #[test]
    fn hidden_schemas_are_dropped_by_exact_name() {
        let mut hits = vec![
            Hit::object("app".into(), "t".into(), "table".into()),
            Hit::column("Secret".into(), "t".into(), "c".into(), "int".into()),
            Hit::object("secret".into(), "t".into(), "table".into()),
        ];
        retain_visible(&mut hits, &["Secret".into()]);
        let left: Vec<&str> = hits.iter().map(|h| h.schema.as_str()).collect();
        assert_eq!(left, ["app", "secret"]);
        retain_visible(&mut hits, &[]);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn tree_hits_cover_loaded_names_and_columns() {
        let col = |n: &str, t: &str| ObjectNode {
            detail: t.into(),
            ..ObjectNode::new(n, NodeKind::Column)
        };
        let with = |mut n: ObjectNode, c: Vec<ObjectNode>| {
            n.children = Some(c);
            n
        };
        let routine = ObjectNode {
            detail: "procedure".into(),
            ..ObjectNode::new("fix_stock", NodeKind::Routine)
        };
        let db = with(
            ObjectNode::new("app", NodeKind::Database),
            vec![
                with(
                    ObjectNode::new("Tables", NodeKind::Group),
                    vec![with(
                        ObjectNode::new("stock", NodeKind::Table),
                        vec![col("id", "int"), col("stock_qty", "int")],
                    )],
                ),
                with(ObjectNode::new("Routines", NodeKind::Group), vec![routine]),
                with(
                    ObjectNode::new("Packages", NodeKind::Group),
                    vec![ObjectNode::new("STOCK_API", NodeKind::Routine)],
                ),
            ],
        );
        // Not loaded: nothing to say about it.
        let other = ObjectNode::new("other", NodeKind::Database);
        let hits = tree_hits(&[db, other], "stock");
        let got: Vec<(Group, &str, &str)> = hits
            .iter()
            .map(|h| (h.group, h.key(), h.kind.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (Group::Objects, "stock", "table"),
                (Group::Columns, "stock_qty", "column"),
                (Group::Objects, "fix_stock", "procedure"),
                (Group::Objects, "STOCK_API", "package"),
            ]
        );
        assert_eq!(hits[1].column.as_ref().unwrap().1, "int");
    }
}
