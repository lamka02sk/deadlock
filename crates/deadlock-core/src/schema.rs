//! The sidebar's object tree: connections and the nodes their drivers (or the cache)
//! gave, which nodes are open, and the flat rows the sidebar draws, plain or filtered
//! by a search.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use crate::driver::{NodeKind, ObjectNode};

/// A node's place in the tree: the connection id, then the names down from it.
pub type NodeId = Vec<String>;

/// A connection at the top of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionTree {
    pub id: String,
    pub name: String,
    /// The databases, once loaded from the server or the cache.
    pub nodes: Option<Vec<ObjectNode>>,
    /// Databases left out of the rows and of searches; the tree still holds them.
    pub hidden: Vec<String>,
}

/// A load in flight or one that failed, shown as a row under the node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Load {
    Loading,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Connection,
    Node(NodeKind),
    Loading,
    Error,
    /// The Sessions and locks screen of a connection; the UI adds it.
    Sessions,
    /// The Server configuration screen of a connection; the UI adds it.
    Config,
    /// The Schema diagram of a database; the UI adds it.
    Diagram,
}

/// One line of the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: NodeId,
    pub kind: RowKind,
    pub label: String,
    /// Type label, group count, or `2 of 7` while searching. Empty on connection rows,
    /// whose state the UI knows.
    pub detail: String,
    /// Byte ranges of `label` that match the search.
    pub hits: Vec<Range<usize>>,
    pub expandable: bool,
    pub expanded: bool,
    /// A system schema, or a connection without matches.
    pub dim: bool,
    pub primary_key: bool,
    pub foreign_key: bool,
}

impl Row {
    fn node(id: NodeId, node: &ObjectNode, expanded: bool) -> Self {
        let detail = match node.kind {
            NodeKind::Group => node.children.as_ref().map_or(0, Vec::len).to_string(),
            _ => node.detail.clone(),
        };
        Self {
            id,
            kind: RowKind::Node(node.kind),
            label: node.name.clone(),
            detail,
            hits: Vec::new(),
            expandable: node.has_children(),
            expanded,
            dim: node.system,
            primary_key: node.primary_key,
            foreign_key: node.foreign_key,
        }
    }

    fn connection(c: &ConnectionTree, expanded: bool) -> Self {
        Self {
            id: vec![c.id.clone()],
            kind: RowKind::Connection,
            label: c.name.clone(),
            detail: String::new(),
            hits: Vec::new(),
            expandable: true,
            expanded,
            dim: false,
            primary_key: false,
            foreign_key: false,
        }
    }

    fn status(parent: &[String], load: &Load) -> Self {
        let mut id = parent.to_vec();
        id.push(String::new());
        let (kind, label) = match load {
            Load::Loading => (RowKind::Loading, "Loading…".to_owned()),
            Load::Failed(e) => (RowKind::Error, e.clone()),
        };
        Self {
            id,
            kind,
            label,
            detail: String::new(),
            hits: Vec::new(),
            expandable: false,
            expanded: false,
            dim: false,
            primary_key: false,
            foreign_key: false,
        }
    }

    /// Depth below the connection: 0 for the connection itself.
    pub fn depth(&self) -> usize {
        self.id.len() - 1
    }
}

#[derive(Debug, Clone, Default)]
pub struct Tree {
    pub connections: Vec<ConnectionTree>,
    pub expanded: HashSet<NodeId>,
    pub status: HashMap<NodeId, Load>,
}

fn child_id(parent: &[String], name: &str) -> NodeId {
    let mut id = Vec::with_capacity(parent.len() + 1);
    id.extend_from_slice(parent);
    id.push(name.to_owned());
    id
}

/// Moves loaded children from `old` onto same-named nodes of `new` that came without
/// them, so a reload keeps open branches until their own reload arrives.
fn carry_over(new: &mut [ObjectNode], old: Option<Vec<ObjectNode>>) {
    let Some(old) = old else { return };
    let mut old: HashMap<String, ObjectNode> =
        old.into_iter().map(|n| (n.name.clone(), n)).collect();
    for n in new.iter_mut().filter(|n| n.children.is_none()) {
        if let Some(o) = old.remove(&n.name).filter(|o| o.kind == n.kind) {
            n.children = o.children;
        }
    }
}

impl Tree {
    /// The children at `id`, or `None` when `id` does not exist or is not loaded.
    pub fn children(&self, id: &[String]) -> Option<&[ObjectNode]> {
        let (conn, rest) = id.split_first()?;
        let mut nodes = self
            .connections
            .iter()
            .find(|c| &c.id == conn)?
            .nodes
            .as_deref()?;
        for name in rest {
            nodes = nodes
                .iter()
                .find(|n| &n.name == name)?
                .children
                .as_deref()?;
        }
        Some(nodes)
    }

    /// The node at `id`; `None` for a connection or a missing node.
    pub fn node(&self, id: &[String]) -> Option<&ObjectNode> {
        let (name, parent) = id.split_last()?;
        self.children(parent)?.iter().find(|n| &n.name == name)
    }

    fn children_slot(&mut self, id: &[String]) -> Option<&mut Option<Vec<ObjectNode>>> {
        let (conn, rest) = id.split_first()?;
        let mut slot = &mut self.connections.iter_mut().find(|c| &c.id == conn)?.nodes;
        for name in rest {
            slot = &mut slot
                .as_mut()?
                .iter_mut()
                .find(|n| &n.name == name)?
                .children;
        }
        Some(slot)
    }

    /// Replaces the children at `id`. Returns false when `id` is not in the tree.
    pub fn set_children(&mut self, id: &[String], mut nodes: Vec<ObjectNode>) -> bool {
        let Some(slot) = self.children_slot(id) else {
            return false;
        };
        carry_over(&mut nodes, slot.take());
        *slot = Some(nodes);
        true
    }

    /// Whether `id` is known to be gone: an ancestor is loaded and lacks it.
    /// Nodes under something not loaded yet are not gone.
    pub fn is_gone(&self, id: &[String]) -> bool {
        let Some((conn, rest)) = id.split_first() else {
            return true;
        };
        let Some(c) = self.connections.iter().find(|c| &c.id == conn) else {
            return true;
        };
        let mut nodes = c.nodes.as_deref();
        for name in rest {
            let Some(list) = nodes else { return false };
            match list.iter().find(|n| &n.name == name) {
                Some(n) => nodes = n.children.as_deref(),
                None => return true,
            }
        }
        false
    }

    /// Forgets open nodes that no longer exist.
    pub fn prune(&mut self) {
        let gone: Vec<NodeId> = self
            .expanded
            .iter()
            .filter(|id| self.is_gone(id))
            .cloned()
            .collect();
        for id in gone {
            self.expanded.remove(&id);
        }
    }

    /// The rows of the tree as it is open.
    pub fn rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        for c in &self.connections {
            let row = Row::connection(c, self.expanded.contains(&[c.id.clone()][..]));
            let (open, id) = (row.expanded, row.id.clone());
            out.push(row);
            if open {
                self.push_open(&mut out, &id, c.nodes.as_deref(), &c.hidden, &|id| {
                    self.expanded.contains(id)
                });
            }
        }
        out
    }

    /// Rows for `nodes` under `parent`, opening the nodes `open` says.
    fn push_open(
        &self,
        out: &mut Vec<Row>,
        parent: &[String],
        nodes: Option<&[ObjectNode]>,
        hidden: &[String],
        open: &dyn Fn(&[String]) -> bool,
    ) {
        if let Some(load) = self.status.get(parent)
            && (nodes.is_none() || matches!(load, Load::Failed(_)))
        {
            out.push(Row::status(parent, load));
        }
        for n in nodes.unwrap_or_default() {
            if hidden.contains(&n.name) {
                continue;
            }
            let id = child_id(parent, &n.name);
            let expanded = n.has_children() && open(&id);
            out.push(Row::node(id, n, expanded));
            if expanded {
                let id = &out[out.len() - 1].id.clone();
                self.push_open(out, id, n.children.as_deref(), &[], open);
            }
        }
    }

    /// Rows for a search: branches with matches open by themselves, matches carry
    /// their hit ranges, groups count `matches of total`, connections without matches
    /// read `no matches`. `flipped` holds nodes the user opened or closed against that.
    /// Returns the rows and the number of matching objects.
    pub fn search(&self, query: &Query, flipped: &HashSet<NodeId>) -> (Vec<Row>, usize) {
        let mut out = Vec::new();
        let mut found = 0;
        for c in &self.connections {
            let hits = query.find(&c.name);
            let (children, n) = filter(c.nodes.as_deref().unwrap_or_default(), &c.hidden, query);
            found += n + usize::from(hits.is_some());
            let mut row = Row::connection(c, false);
            if hits.is_none() && children.is_empty() {
                row.detail = "no matches".into();
                row.dim = true;
            }
            row.hits = hits.unwrap_or_default();
            row.expanded = !children.is_empty() != flipped.contains(&row.id);
            let (open, id) = (row.expanded, row.id.clone());
            out.push(row);
            if open {
                self.push_hits(
                    &mut out,
                    &id,
                    &children,
                    (c.nodes.as_deref(), &c.hidden),
                    flipped,
                );
            }
        }
        (out, found)
    }

    fn push_hits(
        &self,
        out: &mut Vec<Row>,
        parent: &[String],
        hits: &[Hit<'_>],
        (all, hidden): (Option<&[ObjectNode]>, &[String]),
        flipped: &HashSet<NodeId>,
    ) {
        // Opened by hand with nothing matching inside: show it whole.
        if hits.is_empty() {
            self.push_open(out, parent, all, hidden, &|id| flipped.contains(id));
            return;
        }
        for h in hits {
            let id = child_id(parent, &h.node.name);
            let open = h.node.has_children() && (!h.children.is_empty() != flipped.contains(&id));
            let mut row = Row::node(id, h.node, open);
            row.hits.clone_from(&h.hits);
            if h.node.kind == NodeKind::Group {
                row.detail = format!("{} of {}", h.children.len(), row.detail);
            }
            out.push(row);
            if open {
                let id = &out[out.len() - 1].id.clone();
                self.push_hits(
                    out,
                    id,
                    &h.children,
                    (h.node.children.as_deref(), &[]),
                    flipped,
                );
            }
        }
    }
}

/// A node that matches, or holds matches.
struct Hit<'a> {
    node: &'a ObjectNode,
    hits: Vec<Range<usize>>,
    children: Vec<Hit<'a>>,
}

/// The matching part of `nodes`, and how many objects matched. Columns never match. Group names are labels,
/// not objects, so they never match themselves.
fn filter<'a>(nodes: &'a [ObjectNode], hidden: &[String], query: &Query) -> (Vec<Hit<'a>>, usize) {
    filter_under(nodes, hidden, query, false)
}

/// [`filter`] for the children of a table when `table` is set: its Triggers group
/// repeats triggers the schema's Triggers group lists, so they show but count once.
fn filter_under<'a>(
    nodes: &'a [ObjectNode],
    hidden: &[String],
    query: &Query,
    table: bool,
) -> (Vec<Hit<'a>>, usize) {
    let mut out = Vec::new();
    let mut found = 0;
    for node in nodes
        .iter()
        .filter(|n| n.kind != NodeKind::Column && !hidden.contains(&n.name))
    {
        let (children, n) = filter_under(
            node.children.as_deref().unwrap_or_default(),
            &[],
            query,
            node.kind == NodeKind::Table,
        );
        let hits = (node.kind != NodeKind::Group)
            .then(|| query.find(&node.name))
            .flatten();
        let repeat = table && node.kind == NodeKind::Group && node.name == "Triggers";
        if !repeat {
            found += n + usize::from(hits.is_some());
        }
        if hits.is_some() || !children.is_empty() {
            out.push(Hit {
                node,
                hits: hits.unwrap_or_default(),
                children,
            });
        }
    }
    (out, found)
}

/// A search query, lower-cased once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query(Vec<char>);

impl Query {
    /// `None` for a blank query.
    pub fn new(text: &str) -> Option<Self> {
        let chars: Vec<char> = text.trim().chars().flat_map(char::to_lowercase).collect();
        (!chars.is_empty()).then_some(Self(chars))
    }

    /// Fuzzy match, ignoring case: a contiguous run when there is one, else the query's
    /// characters in order. Returns the matched byte ranges of `name`.
    #[allow(
        clippy::single_range_in_vec_init,
        reason = "one hit range, not a range of values"
    )]
    pub fn find(&self, name: &str) -> Option<Vec<Range<usize>>> {
        let q = &self.0;
        // Lower-casing per char keeps byte offsets in `name`; characters whose lower
        // case is longer than one char compare by their first.
        let lower = |c: char| c.to_lowercase().next().unwrap_or(c);
        // Most names fail here, without allocating: not even a subsequence.
        let mut want = q.iter().peekable();
        for c in name.chars() {
            if want.peek() == Some(&&lower(c)) {
                want.next();
            }
        }
        if want.peek().is_some() {
            return None;
        }
        let chars: Vec<(usize, char, usize)> = name
            .char_indices()
            .map(|(i, c)| (i, lower(c), c.len_utf8()))
            .collect();
        if q.len() > chars.len() {
            return None;
        }
        if let Some(start) = (0..=chars.len() - q.len())
            .find(|&s| chars[s..s + q.len()].iter().zip(q).all(|(c, q)| c.1 == *q))
        {
            let (a, _, _) = chars[start];
            let (b, _, len) = chars[start + q.len() - 1];
            return Some(vec![a..b + len]);
        }
        let mut ranges: Vec<Range<usize>> = Vec::new();
        let mut want = q.iter().peekable();
        for &(i, c, len) in &chars {
            if want.peek() == Some(&&c) {
                want.next();
                match ranges.last_mut() {
                    Some(r) if r.end == i => r.end = i + len,
                    _ => ranges.push(i..i + len),
                }
            }
        }
        want.peek().is_none().then_some(ranges)
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init, reason = "hit ranges")]
mod tests {
    use super::*;

    fn q(s: &str) -> Query {
        Query::new(s).unwrap()
    }

    #[test]
    fn matcher_prefers_a_contiguous_run_and_ignores_case() {
        assert_eq!(q("ord").find("order_items"), Some(vec![0..3]));
        assert_eq!(q("ITEM").find("order_items"), Some(vec![6..10]));
        assert_eq!(q("oi").find("order_items"), Some(vec![0..1, 6..7]));
        assert_eq!(q("odr").find("order"), Some(vec![0..1, 2..3, 4..5]));
        assert_eq!(q("ordi").find("order_items"), Some(vec![0..3, 6..7]));
        assert_eq!(q("xyz").find("order"), None);
        assert_eq!(q("orders!").find("order"), None);
        assert_eq!(q("é").find("café"), Some(vec![3..5]));
        assert!(Query::new("  ").is_none());
    }

    fn col(name: &str) -> ObjectNode {
        ObjectNode::new(name, NodeKind::Column)
    }

    fn with(mut n: ObjectNode, children: Vec<ObjectNode>) -> ObjectNode {
        n.children = Some(children);
        n
    }

    fn group(name: &str, children: Vec<ObjectNode>) -> ObjectNode {
        with(ObjectNode::new(name, NodeKind::Group), children)
    }

    fn table(name: &str, cols: &[&str]) -> ObjectNode {
        with(
            ObjectNode::new(name, NodeKind::Table),
            cols.iter().map(|c| col(c)).collect(),
        )
    }

    fn id(parts: &[&str]) -> NodeId {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    /// `shop` with one database `app` holding three tables, and `warehouse` with
    /// `stock` cached but never opened.
    fn sample() -> Tree {
        let app = with(
            ObjectNode::new("app", NodeKind::Database),
            vec![
                group(
                    "Tables",
                    vec![
                        table("customers", &["id", "name"]),
                        table("order_items", &["order_id", "qty"]),
                        table("orders", &["id", "total"]),
                    ],
                ),
                group("Views", vec![]),
            ],
        );
        let stock = with(
            ObjectNode::new("stock", NodeKind::Database),
            vec![group("Tables", vec![table("bins", &["id"])])],
        );
        Tree {
            connections: vec![
                ConnectionTree {
                    id: "shop".into(),
                    name: "shop".into(),
                    nodes: Some(vec![app]),
                    hidden: Vec::new(),
                },
                ConnectionTree {
                    id: "wh".into(),
                    name: "warehouse".into(),
                    nodes: Some(vec![stock]),
                    hidden: Vec::new(),
                },
            ],
            ..Tree::default()
        }
    }

    fn labels(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| {
                format!("{}{} {}", "  ".repeat(r.depth()), r.label, r.detail)
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn rows_follow_the_open_nodes() {
        let mut t = sample();
        assert_eq!(labels(&t.rows()), ["shop", "warehouse"]);
        t.expanded.insert(id(&["shop"]));
        t.expanded.insert(id(&["shop", "app"]));
        t.expanded.insert(id(&["shop", "app", "Tables"]));
        t.expanded.insert(id(&["shop", "app", "Tables", "orders"]));
        t.expanded.insert(id(&["shop", "app", "Views"])); // empty: never opens
        let rows = t.rows();
        assert_eq!(
            labels(&rows),
            [
                "shop",
                "  app",
                "    Tables 3",
                "      customers",
                "      order_items",
                "      orders",
                "        id",
                "        total",
                "    Views 0",
                "warehouse"
            ]
        );
        assert!(rows[5].expanded && !rows[8].expandable && !rows[8].expanded);
    }

    #[test]
    fn loading_and_errors_show_under_their_node() {
        let mut t = sample();
        t.connections[1].nodes = None;
        t.expanded.insert(id(&["wh"]));
        t.status.insert(id(&["wh"]), Load::Loading);
        let rows = t.rows();
        assert_eq!(rows[2].kind, RowKind::Loading);
        t.status.insert(id(&["wh"]), Load::Failed("denied".into()));
        assert_eq!(t.rows()[2].kind, RowKind::Error);
        assert_eq!(t.rows()[2].label, "denied");
    }

    #[test]
    fn search_opens_matching_branches_and_counts() {
        let t = sample();
        let (rows, found) = t.search(&q("ord"), &HashSet::new());
        assert_eq!(found, 2); // order_items, orders; columns never match
        assert_eq!(
            labels(&rows),
            [
                "shop",
                "  app",
                "    Tables 2 of 3",
                "      order_items",
                "      orders",
                "warehouse no matches"
            ]
        );
        assert_eq!(rows[3].hits, vec![0..3]);
        assert!(!rows[3].expanded && rows[3].expandable);
        assert!(rows[5].dim && !rows[5].expanded);
    }

    #[test]
    fn a_trigger_under_its_table_and_its_group_counts_once() {
        let trig = || ObjectNode::new("orders_bu", NodeKind::Routine);
        let mut orders = table("orders", &["id"]);
        orders
            .children
            .as_mut()
            .unwrap()
            .push(group("Triggers", vec![trig()]));
        let mut t = sample();
        t.connections[0].nodes = Some(vec![with(
            ObjectNode::new("app", NodeKind::Database),
            vec![
                group("Tables", vec![orders]),
                group("Triggers", vec![trig()]),
            ],
        )]);
        let (rows, found) = t.search(&q("orders_bu"), &HashSet::new());
        assert_eq!(found, 1);
        // Still shown in both places.
        assert_eq!(rows.iter().filter(|r| r.label == "orders_bu").count(), 2);
    }

    #[test]
    fn search_finds_cached_nodes_never_opened() {
        let t = sample();
        assert!(t.expanded.is_empty());
        let (rows, found) = t.search(&q("bins"), &HashSet::new());
        assert_eq!(found, 1);
        assert_eq!(rows.last().unwrap().label, "bins");
        assert_eq!(labels(&rows)[0], "shop no matches");
    }

    #[test]
    fn flipped_nodes_override_and_show_whole_when_nothing_matches() {
        let t = sample();
        let mut flipped = HashSet::new();
        flipped.insert(id(&["shop", "app", "Tables", "orders"]));
        flipped.insert(id(&["shop", "app", "Tables", "order_items"]));
        let (rows, _) = t.search(&q("ord"), &flipped);
        assert_eq!(
            labels(&rows)[3..],
            [
                "      order_items",
                "        order_id",
                "        qty",
                "      orders",
                "        id",
                "        total",
                "warehouse no matches"
            ]
        );
    }

    #[test]
    fn set_children_keeps_open_branches_and_prune_forgets_gone_ones() {
        let mut t = sample();
        t.expanded.insert(id(&["shop", "app"]));
        t.expanded.insert(id(&["shop", "old"]));
        t.expanded.insert(id(&["wh", "stock", "Tables"]));
        // A reload of the databases: `app` comes back without children, `old` is gone.
        let fresh = vec![ObjectNode::new("app", NodeKind::Database)];
        assert!(t.set_children(&id(&["shop"]), fresh));
        assert_eq!(
            t.children(&id(&["shop", "app", "Tables"])).unwrap().len(),
            3
        );
        t.prune();
        assert!(t.expanded.contains(&id(&["shop", "app"])));
        assert!(!t.expanded.contains(&id(&["shop", "old"])));
        assert!(t.expanded.contains(&id(&["wh", "stock", "Tables"])));
        assert!(!t.set_children(&id(&["shop", "nope"]), vec![]));
        assert!(t.is_gone(&id(&["shop", "app", "Tables", "nope"])));
        assert!(!t.is_gone(&id(&["shop", "app", "Tables", "orders", "id"])));
    }

    #[test]
    fn hidden_databases_leave_rows_and_search() {
        let mut t = sample();
        t.connections[0].hidden = vec!["app".into()];
        t.expanded.insert(id(&["shop"]));
        assert_eq!(labels(&t.rows()), ["shop", "warehouse"]);
        let (rows, found) = t.search(&q("orders"), &HashSet::new());
        assert_eq!(found, 0);
        assert_eq!(labels(&rows), ["shop no matches", "warehouse no matches"]);
        t.connections[0].hidden.clear();
        assert!(t.search(&q("orders"), &HashSet::new()).1 > 0);
    }
}
