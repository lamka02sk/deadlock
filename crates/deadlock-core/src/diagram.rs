//! The schema diagram: a schema's tables with their columns and the foreign keys
//! between them, the card geometry from the design, line routing, the layered auto
//! layout, the layout saved per schema in `.deadlock/diagrams.json`, and the SVG export.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::driver::{NodeKind, ObjectNode};
use crate::{Result, read_json, write_json};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diagram {
    /// By name, in the server's order.
    pub tables: Vec<Table>,
    /// Foreign keys whose both ends are in `tables`.
    pub links: Vec<Link>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub data_type: String,
    pub primary_key: bool,
    pub foreign_key: bool,
}

/// A foreign key: `from` (`from_columns`) references `to` (`to_columns`), pairwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub name: String,
    pub from: String,
    pub from_columns: Vec<String>,
    pub to: String,
    pub to_columns: Vec<String>,
}

/// One foreign-key column pair: `(constraint, table, column, referenced table,
/// referenced column)`, ordered by table, constraint and position.
pub type KeyRow = (String, String, String, String, String);

impl Diagram {
    /// The tables of a schema as the sidebar tree loads them (its groups, down to the
    /// columns), and its foreign keys. Keys to tables outside the schema are left out.
    pub fn from_tree(groups: Vec<ObjectNode>, keys: Vec<KeyRow>) -> Self {
        let tables: Vec<Table> = groups
            .into_iter()
            .find(|g| g.kind == NodeKind::Group && g.name == "Tables")
            .and_then(|g| g.children)
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.kind == NodeKind::Table)
            .map(|t| Table {
                columns: t
                    .children
                    .unwrap_or_default()
                    .into_iter()
                    // A table also holds its Triggers group.
                    .filter(|c| c.kind == NodeKind::Column)
                    .map(|c| Column {
                        name: c.name,
                        data_type: c.detail,
                        primary_key: c.primary_key,
                        foreign_key: c.foreign_key,
                    })
                    .collect(),
                name: t.name,
            })
            .collect();
        let mut links: Vec<Link> = Vec::new();
        for (name, from, column, to, to_column) in keys {
            match links.last_mut() {
                Some(l) if l.name == name && l.from == from => {
                    l.from_columns.push(column);
                    l.to_columns.push(to_column);
                }
                _ => links.push(Link {
                    name,
                    from,
                    from_columns: vec![column],
                    to,
                    to_columns: vec![to_column],
                }),
            }
        }
        let known = |n: &str| tables.iter().any(|t| t.name == n);
        links.retain(|l| known(&l.from) && known(&l.to));
        Self { tables, links }
    }

    pub fn table_index(&self, name: &str) -> Option<usize> {
        self.tables.iter().position(|t| t.name == name)
    }
}

// Card geometry, from the design's `.er`: 12 / 14 px padding, a 28 px header in Inter
// 13 semibold, 22 px rows in JetBrains Mono 12.5, names in a 104 px column, 256 px wide.
pub const CARD_WIDTH: f32 = 256.;
pub const PAD_X: f32 = 14.;
pub const PAD_Y: f32 = 12.;
pub const HEADER: f32 = 28.;
pub const ROW: f32 = 22.;
pub const NAME_COLUMN: f32 = 104.;
pub const CARD_RADIUS: f32 = 8.;
/// JetBrains Mono advances 0.6 em: 7.5 px at 12.5 px.
pub const MONO_ADVANCE: f32 = 7.5;
/// Inter 13 semibold, a little wide on average, for the table name.
const HEADER_ADVANCE: f32 = 8.;
/// How far a line runs out of a card before it turns.
pub const LEAD: f32 = 24.;
/// How far from the foreign-key card a line between facing cards turns.
const TURN: f32 = 40.;
/// Corner radius of the lines.
pub const BEND: f32 = 8.;

fn chars(s: &str) -> f32 {
    s.chars().count() as f32
}

/// Width of the name column: 104 px, or wider for long names.
pub fn name_column(t: &Table) -> f32 {
    let longest = t.columns.iter().map(|c| chars(&c.name)).fold(0., f32::max);
    NAME_COLUMN.max(longest * MONO_ADVANCE + 12.)
}

/// A card's width and height.
pub fn card_size(t: &Table) -> (f32, f32) {
    let types = t
        .columns
        .iter()
        .map(|c| chars(&c.data_type))
        .fold(0., f32::max);
    let w = CARD_WIDTH
        .max(2. * PAD_X + name_column(t) + types * MONO_ADVANCE)
        .max(2. * PAD_X + chars(&t.name) * HEADER_ADVANCE);
    let h = 2. * PAD_Y + HEADER + ROW * t.columns.len() as f32;
    (w.ceil(), h)
}

/// The top of column row `i`, from the card's top.
pub fn row_top(i: usize) -> f32 {
    PAD_Y + HEADER + ROW * i as f32
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn intersects(&self, o: &Rect) -> bool {
        self.x < o.right() && o.x < self.right() && self.y < o.bottom() && o.y < self.bottom()
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    pub fn union(&self, o: &Rect) -> Rect {
        let (x, y) = (self.x.min(o.x), self.y.min(o.y));
        Rect {
            x,
            y,
            w: self.right().max(o.right()) - x,
            h: self.bottom().max(o.bottom()) - y,
        }
    }
}

pub type Point = (f32, f32);

/// An orthogonal line from the foreign-key row of card `a` (at height `ay`) to the key
/// row of card `b` (at `by`). Each end leaves from the side of its card that faces the
/// other card; cards above one another share the nearer side, and a self-reference
/// loops out of the right side. The first and last points lie on the card edges.
pub fn route(a: &Rect, ay: f32, b: &Rect, by: f32) -> Vec<Point> {
    let pts = if a == b {
        let x = a.right() + LEAD;
        vec![(a.right(), ay), (x, ay), (x, by), (a.right(), by)]
    } else if a.right() + 2. * LEAD <= b.x {
        // The turn stays in the gap beside the foreign-key card, so a line that spans
        // several layers never runs behind the cards in between.
        let x = a.right() + TURN.min((b.x - a.right()) / 2.);
        vec![(a.right(), ay), (x, ay), (x, by), (b.x, by)]
    } else if b.right() + 2. * LEAD <= a.x {
        let x = a.x - TURN.min((a.x - b.right()) / 2.);
        vec![(a.x, ay), (x, ay), (x, by), (b.right(), by)]
    } else {
        let right = a.right().max(b.right()) + LEAD;
        let left = a.x.min(b.x) - LEAD;
        let via_right = (right - a.right()) + (right - b.right());
        let via_left = (a.x - left) + (b.x - left);
        if via_right <= via_left {
            vec![(a.right(), ay), (right, ay), (right, by), (b.right(), by)]
        } else {
            vec![(a.x, ay), (left, ay), (left, by), (b.x, by)]
        }
    };
    let mut out: Vec<Point> = Vec::with_capacity(4);
    for p in pts {
        if out.last() != Some(&p) {
            out.push(p);
        }
    }
    // Straight runs need no middle points.
    if let (Some(&f), Some(&l)) = (out.first(), out.last())
        && out.len() > 2
        && a != b
        && out.iter().all(|p| (p.1 - f.1).abs() < 0.5)
    {
        out = vec![f, (l.0, f.1)];
    }
    out
}

/// A step of a line with rounded corners.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    Move(Point),
    Line(Point),
    /// A quadratic curve through the corner `ctrl`.
    Quad(Point, Point),
}

/// `points` as steps with corners rounded to [`BEND`], smaller where a run is short.
pub fn rounded(points: &[Point]) -> Vec<Step> {
    let Some(&first) = points.first() else {
        return Vec::new();
    };
    let mut out = vec![Step::Move(first)];
    for w in points.windows(3) {
        let (p, c, n) = (w[0], w[1], w[2]);
        let len = |a: Point, b: Point| (a.0 - b.0).abs() + (a.1 - b.1).abs();
        let r = BEND.min(len(p, c) / 2.).min(len(c, n) / 2.);
        let toward = |from: Point, to: Point, d: f32| {
            let l = len(from, to).max(f32::EPSILON);
            (
                from.0 + (to.0 - from.0) * d / l,
                from.1 + (to.1 - from.1) * d / l,
            )
        };
        out.push(Step::Line(toward(c, p, r)));
        out.push(Step::Quad(c, toward(c, n, r)));
    }
    if let Some(&last) = points.last().filter(|_| points.len() > 1) {
        out.push(Step::Line(last));
    }
    out
}

/// The distance from `(x, y)` to the polyline.
pub fn distance(points: &[Point], x: f32, y: f32) -> f32 {
    points
        .windows(2)
        .map(|w| {
            let ((ax, ay), (bx, by)) = (w[0], w[1]);
            let (dx, dy) = (bx - ax, by - ay);
            let l2 = dx * dx + dy * dy;
            let t = if l2 > 0. {
                (((x - ax) * dx + (y - ay) * dy) / l2).clamp(0., 1.)
            } else {
                0.
            };
            let (px, py) = (ax + t * dx, ay + t * dy);
            ((x - px).powi(2) + (y - py).powi(2)).sqrt()
        })
        .fold(f32::INFINITY, f32::min)
}

/// The end marks of a line: a crow's foot where it meets the foreign-key card (many)
/// and a bar where it meets the referenced card (one), as strokes from the design.
pub fn markers(points: &[Point]) -> Vec<[Point; 2]> {
    let (Some(&a), Some(&a2), Some(&b), Some(&b2)) = (
        points.first(),
        points.get(1),
        points.last(),
        points.len().checked_sub(2).and_then(|i| points.get(i)),
    ) else {
        return Vec::new();
    };
    let dir = |from: Point, to: Point| {
        let (dx, dy) = (to.0 - from.0, to.1 - from.1);
        let l = (dx * dx + dy * dy).sqrt().max(f32::EPSILON);
        (dx / l, dy / l)
    };
    // Crow's foot: 14 px out from the edge, spread 7 px either side at the edge.
    let (ux, uy) = dir(a, a2);
    let tip = (a.0 + ux * 14., a.1 + uy * 14.);
    let (nx, ny) = (-uy, ux);
    let mut out = vec![
        [tip, (a.0 + nx * 7., a.1 + ny * 7.)],
        [tip, (a.0 - nx * 7., a.1 - ny * 7.)],
    ];
    // Bar: 8 px out from the edge, 12 px long.
    let (ux, uy) = dir(b, b2);
    let c = (b.0 + ux * 8., b.1 + uy * 8.);
    let (nx, ny) = (-uy, ux);
    out.push([
        (c.0 + nx * 6., c.1 + ny * 6.),
        (c.0 - nx * 6., c.1 - ny * 6.),
    ]);
    out
}

/// Gap between layers, and between cards in a layer or the grid.
const LAYER_GAP: f32 = 120.;
const CARD_GAP: f32 = 40.;

/// Positions for cards of `sizes`, where `edges` are `(referencing, referenced)` pairs.
/// Related cards go in layers, referenced tables left of the tables that reference
/// them (cycles are broken at the edge that closes them), ordered within each layer by
/// a few barycenter sweeps. Unrelated cards go in a grid below. Deterministic.
pub fn arrange(sizes: &[(f32, f32)], edges: &[(usize, usize)]) -> Vec<Point> {
    let n = sizes.len();
    let mut out_edges: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut related = vec![false; n];
    for &(from, to) in edges {
        if from != to && from < n && to < n {
            out_edges[from].push(to);
            related[from] = true;
            related[to] = true;
        }
    }
    for e in &mut out_edges {
        e.sort_unstable();
        e.dedup();
    }
    let dag = acyclic(&out_edges);

    // Longest path from the referenced end: layer(from) > layer(to).
    let mut layer = vec![0usize; n];
    for &v in &topological(&dag).into_iter().rev().collect::<Vec<_>>() {
        for &to in &dag[v] {
            layer[v] = layer[v].max(layer[to] + 1);
        }
    }
    let layers = (0..n)
        .filter(|&v| related[v])
        .map(|v| layer[v] + 1)
        .max()
        .unwrap_or(0);
    let mut rows: Vec<Vec<usize>> = vec![Vec::new(); layers];
    for v in (0..n).filter(|&v| related[v]) {
        rows[layer[v]].push(v);
    }
    order_layers(&mut rows, &out_edges);

    let mut pos = vec![(0., 0.); n];
    let mut x = 0.;
    let mut bottom: f32 = 0.;
    for row in &rows {
        let mut y = 0.;
        let mut width: f32 = 0.;
        for &v in row {
            pos[v] = (x, y);
            y += sizes[v].1 + CARD_GAP;
            width = width.max(sizes[v].0);
        }
        bottom = bottom.max(y);
        x += width + LAYER_GAP;
    }

    let rest: Vec<usize> = (0..n).filter(|&v| !related[v]).collect();
    if !rest.is_empty() {
        let top = if layers > 0 { bottom + LAYER_GAP } else { 0. };
        let cols = (rest.len() as f32).sqrt().ceil().max(1.) as usize;
        let width = rest.iter().map(|&v| sizes[v].0).fold(0., f32::max) + CARD_GAP;
        let mut y = top;
        for chunk in rest.chunks(cols) {
            let mut h: f32 = 0.;
            for (i, &v) in chunk.iter().enumerate() {
                pos[v] = (i as f32 * width, y);
                h = h.max(sizes[v].1);
            }
            y += h + CARD_GAP;
        }
    }
    pos
}

/// `edges` without the edges that close a cycle, found by a depth-first walk in index
/// order (iterative, so deep chains do not overflow the stack).
fn acyclic(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        New,
        Open,
        Done,
    }
    let n = edges.len();
    let mut mark = vec![Mark::New; n];
    let mut dag: Vec<Vec<usize>> = vec![Vec::new(); n];
    for start in 0..n {
        if mark[start] != Mark::New {
            continue;
        }
        let mut stack = vec![(start, 0usize)];
        mark[start] = Mark::Open;
        while let Some(&mut (v, ref mut i)) = stack.last_mut() {
            if let Some(&to) = edges[v].get(*i) {
                *i += 1;
                match mark[to] {
                    Mark::Open => {} // back edge: dropped
                    Mark::Done => dag[v].push(to),
                    Mark::New => {
                        dag[v].push(to);
                        mark[to] = Mark::Open;
                        stack.push((to, 0));
                    }
                }
            } else {
                mark[v] = Mark::Done;
                stack.pop();
            }
        }
    }
    dag
}

/// Nodes of an acyclic graph with every node before the nodes it points to.
fn topological(dag: &[Vec<usize>]) -> Vec<usize> {
    let n = dag.len();
    let mut incoming = vec![0usize; n];
    for e in dag {
        for &to in e {
            incoming[to] += 1;
        }
    }
    let mut ready: std::collections::VecDeque<usize> =
        (0..n).filter(|&v| incoming[v] == 0).collect();
    let mut out = Vec::with_capacity(n);
    while let Some(v) = ready.pop_front() {
        out.push(v);
        for &to in &dag[v] {
            incoming[to] -= 1;
            if incoming[to] == 0 {
                ready.push_back(to);
            }
        }
    }
    out
}

/// Orders each layer by the mean position of its neighbours in the layers already
/// placed: sweeps left to right, then right to left, a few times.
fn order_layers(rows: &mut [Vec<usize>], edges: &[Vec<usize>]) {
    let n = edges.len();
    let mut neighbours: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (v, e) in edges.iter().enumerate() {
        for &to in e {
            neighbours[v].push(to);
            neighbours[to].push(v);
        }
    }
    let mut at = vec![0f32; n];
    for row in rows.iter() {
        for (i, &v) in row.iter().enumerate() {
            at[v] = i as f32;
        }
    }
    let layer_of: HashMap<usize, usize> = rows
        .iter()
        .enumerate()
        .flat_map(|(l, r)| r.iter().map(move |&v| (v, l)))
        .collect();
    let sweep = |rows: &mut [Vec<usize>], at: &mut Vec<f32>, l: usize, from: usize| {
        let keys: HashMap<usize, f32> = rows[l]
            .iter()
            .map(|&v| {
                let near: Vec<f32> = neighbours[v]
                    .iter()
                    .filter(|u| layer_of.get(u) == Some(&from))
                    .map(|&u| at[u])
                    .collect();
                let key = if near.is_empty() {
                    at[v]
                } else {
                    near.iter().sum::<f32>() / near.len() as f32
                };
                (v, key)
            })
            .collect();
        rows[l].sort_by(|a, b| keys[a].total_cmp(&keys[b]).then(at[*a].total_cmp(&at[*b])));
        for (i, &v) in rows[l].iter().enumerate() {
            at[v] = i as f32;
        }
    };
    for _ in 0..4 {
        for l in 1..rows.len() {
            sweep(rows, &mut at, l, l - 1);
        }
        for l in (0..rows.len().saturating_sub(1)).rev() {
            sweep(rows, &mut at, l, l + 1);
        }
    }
}

/// A diagram's layout as saved: the tables on the canvas with their positions, the
/// zoom and the pan (the screen offset of the diagram's origin).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    pub tables: Vec<Placed>,
    pub zoom: f32,
    pub pan: Point,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Placed {
    pub name: String,
    pub x: f32,
    pub y: f32,
}

/// Saved diagrams by connection id, then schema.
type SavedAll = BTreeMap<String, BTreeMap<String, Saved>>;

fn saved_path(root: &Path) -> std::path::PathBuf {
    root.join(".deadlock").join("diagrams.json")
}

/// The saved layout of `schema` on `connection` in the project at `root`. A missing or
/// unreadable file reads as nothing saved.
pub fn load(root: &Path, connection: &str, schema: &str) -> Option<Saved> {
    let mut all: SavedAll = read_json(&saved_path(root)).ok()?;
    all.get_mut(connection)?.remove(schema)
}

/// Saves the layout of `schema` on `connection`, keeping the other diagrams.
///
/// # Errors
/// Fails when `.deadlock/diagrams.json` cannot be written.
pub fn save(root: &Path, connection: &str, schema: &str, saved: &Saved) -> Result<()> {
    let path = saved_path(root);
    let mut all: SavedAll = read_json(&path).unwrap_or_default();
    all.entry(connection.to_owned())
        .or_default()
        .insert(schema.to_owned(), saved.clone());
    write_json(&path, &all)
}

/// Renames (`to = Some`) or removes (`None`) table `from` in the saved layout of
/// `schema` on `connection`. Nothing saved, or no such card: no write.
///
/// # Errors
/// Fails when `.deadlock/diagrams.json` cannot be written.
pub fn rename_table(
    root: &Path,
    connection: &str,
    schema: &str,
    from: &str,
    to: Option<&str>,
) -> Result<()> {
    let path = saved_path(root);
    let Ok(mut all) = read_json::<SavedAll>(&path) else {
        return Ok(());
    };
    let Some(saved) = all.get_mut(connection).and_then(|s| s.get_mut(schema)) else {
        return Ok(());
    };
    let Some(i) = saved.tables.iter().position(|t| t.name == from) else {
        return Ok(());
    };
    match to {
        Some(to) => to.clone_into(&mut saved.tables[i].name),
        None => drop(saved.tables.remove(i)),
    }
    write_json(&path, &all)
}

/// Colors for [`svg`], as CSS colors.
#[derive(Debug, Clone)]
pub struct Colors {
    pub panel: String,
    pub bg: String,
    pub line2: String,
    pub tx: String,
    pub tx3: String,
    pub link: String,
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// A table's card on the canvas.
pub fn card_rect(t: &Table, at: Point) -> Rect {
    let (w, h) = card_size(t);
    Rect {
        x: at.0,
        y: at.1,
        w,
        h,
    }
}

/// The column rows a line joins: the first pair, or for a self-reference the first
/// pair on two different rows, so the loop does not shrink to nothing.
pub fn line_pair(same_table: bool, pairs: &[(usize, usize)]) -> (usize, usize) {
    pairs
        .iter()
        .find(|(a, b)| !same_table || a != b)
        .or(pairs.first())
        .copied()
        .unwrap_or((0, 0))
}

/// The line of `link` between the placed cards, from its first column pair.
pub fn link_route(
    d: &Diagram,
    link: &Link,
    placed: &dyn Fn(usize) -> Option<Point>,
) -> Option<Vec<Point>> {
    let (fi, ti) = (d.table_index(&link.from)?, d.table_index(&link.to)?);
    let (from, to) = (&d.tables[fi], &d.tables[ti]);
    let (a, b) = (card_rect(from, placed(fi)?), card_rect(to, placed(ti)?));
    let row = |t: &Table, c: &String| t.columns.iter().position(|x| &x.name == c);
    let pairs: Vec<(usize, usize)> = link
        .from_columns
        .iter()
        .zip(&link.to_columns)
        .filter_map(|(f, t)| Some((row(from, f)?, row(to, t)?)))
        .collect();
    let (ra, rb) = line_pair(fi == ti, &pairs);
    let ay = a.y + row_top(ra) + ROW / 2.;
    let by = b.y + row_top(rb) + ROW / 2.;
    Some(route(&a, ay, &b, by))
}

/// The whole diagram as SVG: every placed card and every line between them, in the
/// colors given. `placed` holds `(table index, position)`.
pub fn svg(d: &Diagram, placed: &[(usize, Point)], colors: &Colors) -> String {
    const MARGIN: f32 = 40.;
    let at: HashMap<usize, Point> = placed.iter().copied().collect();
    let cards: Vec<(usize, Rect)> = placed
        .iter()
        .map(|&(i, p)| (i, card_rect(&d.tables[i], p)))
        .collect();
    let routes: Vec<Vec<Point>> = d
        .links
        .iter()
        .filter_map(|l| link_route(d, l, &|i| at.get(&i).copied()))
        .collect();
    let mut bounds = cards.first().map_or(
        Rect {
            x: 0.,
            y: 0.,
            w: 0.,
            h: 0.,
        },
        |c| c.1,
    );
    for (_, r) in &cards {
        bounds = bounds.union(r);
    }
    for p in routes.iter().flatten() {
        bounds = bounds.union(&Rect {
            x: p.0,
            y: p.1,
            w: 0.,
            h: 0.,
        });
    }
    let (ox, oy) = (bounds.x - MARGIN, bounds.y - MARGIN);
    let (w, h) = (bounds.w + 2. * MARGIN, bounds.h + 2. * MARGIN);
    let mut s = String::new();
    let _ = write!(
        s,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="{ox} {oy} {w} {h}">"#
    );
    let _ = write!(
        s,
        r#"<rect x="{ox}" y="{oy}" width="{w}" height="{h}" fill="{}"/>"#,
        colors.panel
    );
    let _ = write!(
        s,
        r#"<g fill="none" stroke="{}" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">"#,
        colors.tx3
    );
    for r in &routes {
        s.push_str(r#"<path d=""#);
        for step in rounded(r) {
            let _ = match step {
                Step::Move((x, y)) => write!(s, "M{x} {y}"),
                Step::Line((x, y)) => write!(s, "L{x} {y}"),
                Step::Quad((cx, cy), (x, y)) => write!(s, "Q{cx} {cy} {x} {y}"),
            };
        }
        for [(x1, y1), (x2, y2)] in markers(r) {
            let _ = write!(s, "M{x1} {y1}L{x2} {y2}");
        }
        s.push_str(r#""/>"#);
    }
    s.push_str("</g>");
    for (i, r) in &cards {
        let t = &d.tables[*i];
        let _ = write!(
            s,
            r#"<rect x="{}" y="{}" width="{}" height="{}" rx="{CARD_RADIUS}" fill="{}" stroke="{}"/>"#,
            r.x + 0.5,
            r.y + 0.5,
            r.w - 1.,
            r.h - 1.,
            colors.bg,
            colors.line2
        );
        let _ = write!(
            s,
            r#"<text x="{}" y="{}" font-family="Inter, sans-serif" font-size="13" font-weight="600" fill="{}">{}</text>"#,
            r.x + PAD_X,
            r.y + PAD_Y + 15.,
            colors.tx,
            escape(&t.name)
        );
        let name_w = name_column(t);
        for (k, c) in t.columns.iter().enumerate() {
            let base = r.y + row_top(k) + 15.5;
            let fill = if c.foreign_key {
                &colors.link
            } else {
                &colors.tx
            };
            let weight = if c.primary_key {
                r#" font-weight="700""#
            } else {
                ""
            };
            let _ = write!(
                s,
                r#"<text y="{base}" font-family="JetBrains Mono, monospace" font-size="12.5"><tspan x="{}" fill="{fill}"{weight}>{}</tspan><tspan x="{}" fill="{}">{}</tspan></text>"#,
                r.x + PAD_X,
                escape(&c.name),
                r.x + PAD_X + name_w,
                colors.tx3,
                escape(&c.data_type)
            );
        }
    }
    s.push_str("</svg>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, kind: NodeKind, children: Vec<ObjectNode>) -> ObjectNode {
        ObjectNode {
            children: Some(children),
            ..ObjectNode::new(name, kind)
        }
    }

    fn col(name: &str, pk: bool, fk: bool) -> ObjectNode {
        ObjectNode {
            detail: "bigint".into(),
            primary_key: pk,
            foreign_key: fk,
            ..ObjectNode::new(name, NodeKind::Column)
        }
    }

    fn key(n: &str, from: &str, c: &str, to: &str, tc: &str) -> KeyRow {
        (n.into(), from.into(), c.into(), to.into(), tc.into())
    }

    #[test]
    fn tree_and_keys_become_tables_and_links() {
        let groups = vec![
            node(
                "Tables",
                NodeKind::Group,
                vec![
                    node(
                        "a",
                        NodeKind::Table,
                        vec![col("id", true, false), col("v", true, false)],
                    ),
                    node(
                        "b",
                        NodeKind::Table,
                        vec![
                            col("a_id", false, true),
                            col("a_v", false, true),
                            col("parent", false, true),
                        ],
                    ),
                ],
            ),
            node(
                "Views",
                NodeKind::Group,
                vec![node("v", NodeKind::View, vec![])],
            ),
        ];
        let d = Diagram::from_tree(
            groups,
            vec![
                key("fk_a", "b", "a_id", "a", "id"),
                key("fk_a", "b", "a_v", "a", "v"),
                key("fk_self", "b", "parent", "b", "a_id"),
                key("fk_out", "b", "parent", "elsewhere", "id"),
            ],
        );
        assert_eq!(d.tables.len(), 2);
        assert_eq!(d.links.len(), 2);
        assert_eq!(d.links[0].from_columns, ["a_id", "a_v"]);
        assert_eq!(d.links[0].to_columns, ["id", "v"]);
        assert_eq!(
            (d.links[1].from.as_str(), d.links[1].to.as_str()),
            ("b", "b")
        );
        assert!(d.tables[0].columns[0].primary_key && d.tables[1].columns[0].foreign_key);
    }

    #[test]
    fn card_sizes_follow_the_design() {
        let t = |n: usize| Table {
            name: "customers".into(),
            columns: (0..n)
                .map(|i| Column {
                    name: format!("c{i}"),
                    data_type: "text".into(),
                    primary_key: false,
                    foreign_key: false,
                })
                .collect(),
        };
        // The design's customers (4 columns) and orders (7) cards.
        assert_eq!(card_size(&t(4)), (256., 140.));
        assert_eq!(card_size(&t(7)), (256., 206.));
        let mut long = t(1);
        long.columns[0].data_type = "timestamp(6) with local time zone".into();
        assert!(card_size(&long).0 > 256.);
    }

    fn r(x: f32, y: f32) -> Rect {
        Rect {
            x,
            y,
            w: 256.,
            h: 140.,
        }
    }

    #[test]
    fn lines_leave_from_the_facing_sides() {
        // `a` references `b` to its right.
        let p = route(&r(0., 0.), 70., &r(400., 0.), 90.);
        assert_eq!(p, [(256., 70.), (296., 70.), (296., 90.), (400., 90.)]);
        // `b` to the left.
        let p = route(&r(400., 0.), 70., &r(0., 0.), 70.);
        assert_eq!(p, [(400., 70.), (256., 70.)]);
        // Stacked: both on the nearer side.
        let p = route(&r(0., 300.), 370., &r(40., 0.), 70.);
        assert_eq!(p.first().unwrap().0, 256.);
        assert_eq!(p.last().unwrap().0, 296.);
        assert!(p[1].0 > 296.);
        // A self-reference loops out to the right.
        let p = route(&r(0., 0.), 70., &r(0., 0.), 92.);
        assert_eq!(p, [(256., 70.), (280., 70.), (280., 92.), (256., 92.)]);
        let steps = rounded(&p);
        assert!(matches!(steps[0], Step::Move(_)));
        assert_eq!(
            steps.iter().filter(|s| matches!(s, Step::Quad(..))).count(),
            2
        );
        assert!(distance(&p, 280., 80.) < 0.01);
        assert!(distance(&p, 100., 80.) > 100.);
        assert_eq!(markers(&p).len(), 3);
        assert_eq!(line_pair(true, &[(0, 0), (2, 1)]), (2, 1));
        assert_eq!(line_pair(false, &[(0, 0), (2, 1)]), (0, 0));
        assert_eq!(line_pair(true, &[]), (0, 0));
    }

    fn overlaps(sizes: &[(f32, f32)], pos: &[Point]) -> bool {
        let rects: Vec<Rect> = sizes
            .iter()
            .zip(pos)
            .map(|(s, p)| Rect {
                x: p.0,
                y: p.1,
                w: s.0,
                h: s.1,
            })
            .collect();
        rects
            .iter()
            .enumerate()
            .any(|(i, a)| rects[i + 1..].iter().any(|b| a.intersects(b)))
    }

    #[test]
    fn arrange_puts_referenced_tables_left_without_overlaps() {
        // The design's shop: customers 0, orders 1, order_items 2, shipments 3,
        // payments 4, products 5, and an unrelated table 6.
        let sizes = [
            (256., 140.),
            (256., 206.),
            (256., 162.),
            (256., 140.),
            (256., 162.),
            (256., 140.),
            (256., 96.),
        ];
        let edges = [(1, 0), (2, 1), (2, 5), (3, 1), (4, 1)];
        let pos = arrange(&sizes, &edges);
        assert!(!overlaps(&sizes, &pos));
        for &(from, to) in &edges {
            assert!(
                pos[to].0 + sizes[to].0 <= pos[from].0,
                "{from} → {to}: {pos:?}"
            );
        }
        // The unrelated table sits below everything else.
        let bottom = (0..6).map(|i| pos[i].1 + sizes[i].1).fold(0., f32::max);
        assert!(pos[6].1 > bottom);
        assert_eq!(arrange(&sizes, &edges), pos, "deterministic");
    }

    #[test]
    fn arrange_terminates_on_cycles_and_self_references() {
        let sizes = vec![(256., 140.); 5];
        let edges = [(0, 1), (1, 2), (2, 0), (3, 3), (4, 0), (0, 4)];
        let pos = arrange(&sizes, &edges);
        assert_eq!(pos.len(), 5);
        assert!(!overlaps(&sizes, &pos));
        // A long chain, a wide fan and many unrelated cards.
        let n = 2000;
        let sizes = vec![(256., 140.); n];
        let mut edges: Vec<(usize, usize)> = (1..1000).map(|i| (i, i - 1)).collect();
        edges.extend((1000..1500).map(|i| (i, 0)));
        edges.push((0, 999));
        let pos = arrange(&sizes, &edges);
        assert!(!overlaps(&sizes, &pos));
    }

    #[test]
    fn arrange_reduces_crossings() {
        // Two parents with their own children, listed crossed: b's child first.
        let sizes = vec![(256., 100.); 4];
        let edges = [(2, 1), (3, 0)];
        let pos = arrange(&sizes, &edges);
        // Children follow their parents' order top to bottom.
        assert_eq!(pos[0].1 < pos[1].1, pos[3].1 < pos[2].1);
    }

    #[test]
    fn layouts_save_per_connection_and_schema() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".deadlock")).unwrap();
        let saved = Saved {
            tables: vec![Placed {
                name: "orders".into(),
                x: 10.,
                y: 20.,
            }],
            zoom: 0.5,
            pan: (3., 4.),
        };
        assert_eq!(load(dir.path(), "c", "shop"), None);
        save(dir.path(), "c", "shop", &saved).unwrap();
        save(
            dir.path(),
            "c",
            "other",
            &Saved {
                zoom: 1.,
                ..saved.clone()
            },
        )
        .unwrap();
        save(
            dir.path(),
            "d",
            "shop",
            &Saved {
                zoom: 2.,
                ..saved.clone()
            },
        )
        .unwrap();
        assert_eq!(load(dir.path(), "c", "shop"), Some(saved));
        assert_eq!(load(dir.path(), "d", "shop").unwrap().zoom, 2.);
    }

    #[test]
    fn renamed_and_dropped_tables_follow_in_saved_layouts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Nothing saved: no file is written.
        rename_table(root, "c", "shop", "orders", None).unwrap();
        assert!(!root.join(".deadlock").exists());
        std::fs::create_dir(root.join(".deadlock")).unwrap();
        let placed = |name: &str, x: f32| Placed {
            name: name.into(),
            x,
            y: 20.,
        };
        let saved = Saved {
            tables: vec![placed("orders", 10.), placed("items", 30.)],
            zoom: 0.5,
            pan: (3., 4.),
        };
        for (conn, schema) in [("c", "shop"), ("c", "other"), ("d", "shop")] {
            save(root, conn, schema, &saved).unwrap();
        }
        rename_table(root, "c", "shop", "orders", Some("sales")).unwrap();
        let after = load(root, "c", "shop").unwrap();
        assert_eq!(
            after.tables,
            vec![placed("sales", 10.), placed("items", 30.)]
        );
        rename_table(root, "c", "shop", "items", None).unwrap();
        assert_eq!(
            load(root, "c", "shop").unwrap().tables,
            vec![placed("sales", 10.)]
        );
        // No such card: the file stays as it is.
        let before = std::fs::read(saved_path(root)).unwrap();
        rename_table(root, "c", "shop", "gone", None).unwrap();
        assert_eq!(std::fs::read(saved_path(root)).unwrap(), before);
        assert_eq!(load(root, "c", "other"), Some(saved.clone()));
        assert_eq!(load(root, "d", "shop"), Some(saved));
    }

    #[test]
    fn svg_draws_cards_lines_and_escapes_names() {
        let groups = vec![node(
            "Tables",
            NodeKind::Group,
            vec![
                node("a<b", NodeKind::Table, vec![col("id", true, false)]),
                node("c", NodeKind::Table, vec![col("a_id", false, true)]),
            ],
        )];
        let d = Diagram::from_tree(groups, vec![key("f", "c", "a_id", "a<b", "id")]);
        let colors = Colors {
            panel: "#FAFAFA".into(),
            bg: "#FFFFFF".into(),
            line2: "#D4D4D4".into(),
            tx: "#0A0A0A".into(),
            tx3: "#6B6B6B".into(),
            link: "#1D4ED8".into(),
        };
        let s = svg(&d, &[(0, (0., 0.)), (1, (400., 0.))], &colors);
        assert!(s.starts_with("<svg") && s.trim_end().ends_with("</svg>"));
        assert!(s.contains("a&lt;b"));
        assert_eq!(s.matches("<path").count(), 1);
        assert_eq!(s.matches(r#"rx="8""#).count(), 2);
        assert!(s.contains(r##"fill="#1D4ED8""##));
    }
}
