//! The Schema diagram tab: a schema's tables as cards on a canvas with the foreign keys
//! between them. Pan, zoom, drag, Arrange, Add / remove tables, the layout saved per
//! project, connection and schema, and Export image (SVG, PNG).

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use deadlock_core::connection::ConnectionConfig;
use deadlock_core::diagram::{
    self, Colors, Diagram, Point as MPoint, Rect, Saved, Step, card_rect, row_top,
};
use deadlock_core::driver::{Driver, ObjectPath};
use deadlock_core::edit::TableRef;
use deadlock_core::schema::Query;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::popover::Popover;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, BorderStyle, Bounds, ClickEvent, ContentMask, Context,
    Entity, EventEmitter, FontWeight, Hitbox, HitboxBehavior, Hsla, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement,
    PathBuilder, Pixels, Render, Rgba, ScrollEvent, SharedString, StatefulInteractiveElement,
    Styled, Subscription, Task, TextAlign, TextRun, Window, canvas, div, fill, font, point, px,
    quad, size,
};

use crate::app::Db;
use crate::assets::{Icon, icon, menu_item};
use crate::query::View;
use crate::shell::Trigger;
use crate::theme::{MONO_FONT, Palette, RADIUS, UI_FONT, row_height};
use crate::ui::Clickable as _;
use crate::ui::{Kind, button, button_icon};

pub const MIN_ZOOM: f32 = 0.25;
pub const MAX_ZOOM: f32 = 2.;
/// A new diagram shows every table up to this many; above, it starts empty.
const SHOW_ALL_UP_TO: usize = 30;
/// Below this zoom, text would be unreadable: rows draw as bars.
const TEXT_ZOOM: f32 = 0.35;
const SAVE_DELAY: Duration = Duration::from_millis(500);

/// What a click picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Card(usize),
    Link(usize),
}

/// The canvas: which tables are on it and where, zoom, pan and selection. Plain data;
/// [`paint`] draws it.
pub struct Board {
    pub diagram: Diagram,
    /// Card position per table, in diagram units; `None`: not on the canvas.
    at: Vec<Option<MPoint>>,
    /// Cards on the canvas, bottom to top.
    order: Vec<usize>,
    /// Per link: its tables, and the rows of every column pair.
    ends: Vec<LinkEnds>,
    pub zoom: f32,
    /// Where the diagram's origin is, in pixels from the canvas's top left.
    pub pan: MPoint,
    pub selected: Option<Pick>,
    pub hovered: Option<usize>,
}

struct LinkEnds {
    from: usize,
    to: usize,
    rows: Vec<(usize, usize)>,
}

impl Board {
    pub fn new(mut diagram: Diagram) -> Self {
        let tables: HashSet<&str> = diagram.tables.iter().map(|t| t.name.as_str()).collect();
        let keep: Vec<bool> = diagram
            .links
            .iter()
            .map(|l| tables.contains(l.from.as_str()) && tables.contains(l.to.as_str()))
            .collect();
        let mut keep = keep.into_iter();
        diagram.links.retain(|_| keep.next().unwrap_or(false));
        // Every link has both tables now: `ends[l]` belongs to `links[l]`.
        let ends = diagram
            .links
            .iter()
            .filter_map(|l| {
                let (from, to) = (diagram.table_index(&l.from)?, diagram.table_index(&l.to)?);
                let row = |t: usize, c: &String| {
                    diagram.tables[t].columns.iter().position(|x| &x.name == c)
                };
                let rows = l
                    .from_columns
                    .iter()
                    .zip(&l.to_columns)
                    .filter_map(|(a, b)| Some((row(from, a)?, row(to, b)?)))
                    .collect();
                Some(LinkEnds { from, to, rows })
            })
            .collect();
        Self {
            at: vec![None; diagram.tables.len()],
            diagram,
            order: Vec::new(),
            ends,
            zoom: 1.,
            pan: (24., 24.),
            selected: None,
            hovered: None,
        }
    }

    pub fn shown(&self) -> usize {
        self.order.len()
    }

    pub fn is_shown(&self, i: usize) -> bool {
        self.at.get(i).is_some_and(Option::is_some)
    }

    pub fn card(&self, i: usize) -> Option<Rect> {
        Some(card_rect(&self.diagram.tables[i], (*self.at.get(i)?)?))
    }

    /// Puts table `i` on the canvas at `p`, on top.
    pub fn show(&mut self, i: usize, p: MPoint) {
        if i >= self.at.len() {
            return;
        }
        if self.at[i].is_none() {
            self.order.push(i);
        }
        self.at[i] = Some(p);
    }

    pub fn hide(&mut self, i: usize) {
        if let Some(slot) = self.at.get_mut(i) {
            *slot = None;
        }
        self.order.retain(|&o| o != i);
        let gone = |p: Option<Pick>| match p {
            Some(Pick::Card(c)) => c == i,
            Some(Pick::Link(l)) => self.ends[l].from == i || self.ends[l].to == i,
            None => false,
        };
        if gone(self.selected) {
            self.selected = None;
        }
        if gone(self.hovered.map(Pick::Link)) {
            self.hovered = None;
        }
    }

    fn raise(&mut self, i: usize) {
        if let Some(k) = self.order.iter().position(|&o| o == i) {
            self.order.remove(k);
            self.order.push(i);
        }
    }

    pub fn show_all(&mut self) {
        for i in 0..self.at.len() {
            self.show(i, (0., 0.));
        }
    }

    /// Lays out the cards on the canvas with [`diagram::arrange`].
    pub fn arrange(&mut self) {
        let shown = self.order.clone();
        let index = |t: usize| shown.iter().position(|&s| s == t);
        let sizes: Vec<_> = shown
            .iter()
            .map(|&i| diagram::card_size(&self.diagram.tables[i]))
            .collect();
        let edges: Vec<_> = self
            .ends
            .iter()
            .filter_map(|e| Some((index(e.from)?, index(e.to)?)))
            .collect();
        for (k, p) in diagram::arrange(&sizes, &edges).into_iter().enumerate() {
            self.at[shown[k]] = Some(p);
        }
    }

    /// The line of link `l`, when both its cards are on the canvas.
    pub fn route(&self, l: usize) -> Option<Vec<MPoint>> {
        let e = self.ends.get(l)?;
        let (a, b) = (self.card(e.from)?, self.card(e.to)?);
        let (ra, rb) = diagram::line_pair(e.from == e.to, &e.rows);
        let mid = diagram::ROW / 2.;
        Some(diagram::route(
            &a,
            a.y + row_top(ra) + mid,
            &b,
            b.y + row_top(rb) + mid,
        ))
    }

    /// Screen offset from the canvas origin → diagram units.
    pub fn to_model(&self, x: f32, y: f32) -> MPoint {
        ((x - self.pan.0) / self.zoom, (y - self.pan.1) / self.zoom)
    }

    pub fn hit_card(&self, x: f32, y: f32) -> Option<usize> {
        self.order
            .iter()
            .rev()
            .copied()
            .find(|&i| self.card(i).is_some_and(|r| r.contains(x, y)))
    }

    /// The line within a few screen pixels of `(x, y)` (diagram units).
    pub fn hit_link(&self, x: f32, y: f32) -> Option<usize> {
        let reach = 5. / self.zoom;
        (0..self.ends.len())
            .filter_map(|l| Some((l, diagram::distance(&self.route(l)?, x, y))))
            .filter(|&(_, d)| d <= reach)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(l, _)| l)
    }

    /// Zooms to `z` keeping the point under screen offset `(sx, sy)` in place.
    pub fn zoom_at(&mut self, sx: f32, sy: f32, z: f32) {
        let z = z.clamp(MIN_ZOOM, MAX_ZOOM);
        let (mx, my) = self.to_model(sx, sy);
        self.zoom = z;
        self.pan = (sx - mx * z, sy - my * z);
    }

    fn bounds(&self) -> Option<Rect> {
        self.order
            .iter()
            .filter_map(|&i| self.card(i))
            .reduce(|a, b| a.union(&b))
    }

    /// Zooms and pans so every card fits a `w` × `h` viewport, at most at 100 %.
    pub fn fit(&mut self, w: f32, h: f32) {
        let Some(b) = self.bounds() else {
            self.zoom = 1.;
            self.pan = (24., 24.);
            return;
        };
        let margin = 48.;
        let z = ((w - margin) / b.w)
            .min((h - margin) / b.h)
            .clamp(MIN_ZOOM, 1.);
        self.zoom = z;
        // Centered, or from the top left when it does not fit even at the smallest zoom.
        let start = |view: f32, size: f32, at: f32| {
            if size * z <= view - margin {
                (view - size * z) / 2. - at * z
            } else {
                margin / 2. - at * z
            }
        };
        self.pan = (start(w, b.w, b.x), start(h, b.h, b.y));
    }

    /// A free spot for a new card near the middle of a `w` × `h` viewport.
    fn free_spot(&self, i: usize, w: f32, h: f32) -> MPoint {
        let (cx, cy) = self.to_model(w / 2., h / 3.);
        let (cw, ch) = diagram::card_size(&self.diagram.tables[i]);
        let mut p = (cx - cw / 2., cy);
        for _ in 0..200 {
            let r = Rect {
                x: p.0,
                y: p.1,
                w: cw,
                h: ch,
            };
            match self
                .order
                .iter()
                .filter_map(|&o| self.card(o))
                .find(|o| o.intersects(&r))
            {
                Some(o) => p.1 = o.bottom() + 40.,
                None => break,
            }
        }
        p
    }

    /// Links lit by the hover or the selection.
    fn lit(&self) -> Vec<bool> {
        (0..self.ends.len())
            .map(|l| {
                self.hovered == Some(l)
                    || match self.selected {
                        Some(Pick::Link(s)) => s == l,
                        Some(Pick::Card(c)) => self.ends[l].from == c || self.ends[l].to == c,
                        None => false,
                    }
            })
            .collect()
    }

    /// The layout to save.
    pub fn saved(&self) -> Saved {
        Saved {
            tables: self
                .order
                .iter()
                .filter_map(|&i| {
                    let (x, y) = self.at[i]?;
                    Some(diagram::Placed {
                        name: self.diagram.tables[i].name.clone(),
                        x,
                        y,
                    })
                })
                .collect(),
            zoom: self.zoom,
            pan: self.pan,
        }
    }

    /// Applies a saved layout; tables no longer in the schema are skipped.
    pub fn restore(&mut self, s: &Saved) {
        for p in &s.tables {
            if let Some(i) = self.diagram.table_index(&p.name) {
                self.show(i, (p.x, p.y));
            }
        }
        self.zoom = s.zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.pan = s.pan;
    }

    pub fn svg(&self, colors: &Colors) -> String {
        let placed: Vec<_> = self
            .order
            .iter()
            .filter_map(|&i| Some((i, self.at[i]?)))
            .collect();
        diagram::svg(&self.diagram, &placed, colors)
    }
}

/// `#RRGGBB` for the SVG export.
fn css(c: Hsla) -> String {
    let c = Rgba::from(c);
    let b = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    format!("#{:02X}{:02X}{:02X}", b(c.r), b(c.g), b(c.b))
}

pub fn colors(p: &Palette) -> Colors {
    Colors {
        panel: css(p.panel),
        bg: css(p.bg),
        line2: css(p.line2),
        tx: css(p.tx),
        tx3: css(p.tx3),
        link: css(p.link),
    }
}

/// Draws `board` into `bounds`: lines, then cards bottom to top. Only what crosses the
/// viewport is painted.
pub fn paint(
    board: &Board,
    bounds: Bounds<Pixels>,
    p: &Palette,
    window: &mut Window,
    cx: &mut App,
) {
    let z = board.zoom;
    let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
    let (vw, vh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    let (vx, vy) = board.to_model(0., 0.);
    let view = Rect {
        x: vx,
        y: vy,
        w: vw / z,
        h: vh / z,
    };
    let sx = |x: f32| px(ox + board.pan.0 + x * z);
    let sy = |y: f32| px(oy + board.pan.1 + y * z);
    let at = |(x, y): MPoint| point(sx(x), sy(y));
    let lit = board.lit();
    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        // Lines: lit ones last, so they draw over the rest.
        let mut lit_rows: HashSet<(usize, usize)> = HashSet::new();
        // One path per color: tessellating and drawing 400 small paths costs more.
        for pass in [false, true] {
            let mut path = PathBuilder::stroke(px((2. * z).max(1.)));
            let mut any = false;
            for (l, e) in board.ends.iter().enumerate() {
                if lit[l] != pass {
                    continue;
                }
                let Some(route) = board.route(l) else {
                    continue;
                };
                if pass {
                    for &(a, b) in &e.rows {
                        lit_rows.insert((e.from, a));
                        lit_rows.insert((e.to, b));
                    }
                }
                let bbox = route
                    .iter()
                    .map(|&(x, y)| Rect {
                        x: x - 16.,
                        y: y - 16.,
                        w: 32.,
                        h: 32.,
                    })
                    .reduce(|a, b| a.union(&b));
                if !bbox.is_some_and(|b| b.intersects(&view)) {
                    continue;
                }
                any = true;
                for step in diagram::rounded(&route) {
                    match step {
                        Step::Move(q) => path.move_to(at(q)),
                        Step::Line(q) => path.line_to(at(q)),
                        Step::Quad(c, q) => path.curve_to(at(q), at(c)),
                    }
                }
                for [a, b] in diagram::markers(&route) {
                    path.move_to(at(a));
                    path.line_to(at(b));
                }
            }
            if any && let Ok(path) = path.build() {
                window.paint_path(path, if pass { p.blue } else { p.tx3 });
            }
        }
        let ui = |w| gpui_kit::Font {
            weight: w,
            ..font(UI_FONT)
        };
        let mono = |w| gpui_kit::Font {
            weight: w,
            ..font(MONO_FONT)
        };
        for &i in &board.order {
            let Some(r) = board.card(i) else { continue };
            if !r.intersects(&view) {
                continue;
            }
            let t = &board.diagram.tables[i];
            let b = Bounds {
                origin: at((r.x, r.y)),
                size: size(px(r.w * z), px(r.h * z)),
            };
            let selected = board.selected == Some(Pick::Card(i));
            if selected {
                // `outline: 2px solid var(--sel)` around the blue border.
                let o = Bounds {
                    origin: point(b.origin.x - px(2.), b.origin.y - px(2.)),
                    size: size(b.size.width + px(4.), b.size.height + px(4.)),
                };
                window.paint_quad(quad(
                    o,
                    px(diagram::CARD_RADIUS * z + 2.),
                    p.sel,
                    px(0.),
                    p.sel,
                    BorderStyle::Solid,
                ));
            }
            window.paint_quad(quad(
                b,
                px(diagram::CARD_RADIUS * z),
                p.bg,
                px(1.),
                if selected { p.blue } else { p.line2 },
                BorderStyle::Solid,
            ));
            let name_w = diagram::name_column(t);
            for k in 0..t.columns.len() {
                if lit_rows.contains(&(i, k)) {
                    let top = r.y + row_top(k);
                    window.paint_quad(fill(
                        Bounds {
                            origin: point(b.origin.x + px(1.), sy(top)),
                            size: size(b.size.width - px(2.), px(diagram::ROW * z)),
                        },
                        p.sel,
                    ));
                }
            }
            if z < TEXT_ZOOM {
                let bar = |x: f32, y: f32, chars: usize, color: Hsla, window: &mut Window| {
                    window.paint_quad(fill(
                        Bounds {
                            origin: at((x, y - 3.)),
                            size: size(px(chars as f32 * diagram::MONO_ADVANCE * z), px(6. * z)),
                        },
                        color,
                    ));
                };
                bar(
                    r.x + diagram::PAD_X,
                    r.y + diagram::PAD_Y + 12.,
                    t.name.chars().count(),
                    p.tx3,
                    window,
                );
                for (k, c) in t.columns.iter().enumerate() {
                    let color = if c.foreign_key { p.link } else { p.line2 };
                    let y = r.y + row_top(k) + diagram::ROW / 2.;
                    bar(
                        r.x + diagram::PAD_X,
                        y,
                        c.name.chars().count().min(14),
                        color,
                        window,
                    );
                }
                continue;
            }
            let text = |s: &str,
                        x: f32,
                        y: f32,
                        size_: f32,
                        line: f32,
                        f: gpui_kit::Font,
                        color: Hsla,
                        window: &mut Window,
                        cx: &mut App| {
                let run = TextRun {
                    len: s.len(),
                    font: f,
                    color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let shaped = window.text_system().shape_line(
                    SharedString::from(s.to_owned()),
                    // Half-pixel steps: zooming reuses the shaped lines of the last frame.
                    px((size_ * z * 2.).round() / 2.),
                    &[run],
                    None,
                );
                let _ = shaped.paint(at((x, y)), px(line * z), TextAlign::Left, None, window, cx);
            };
            text(
                &t.name,
                r.x + diagram::PAD_X,
                r.y + diagram::PAD_Y,
                13.,
                diagram::HEADER,
                ui(FontWeight::SEMIBOLD),
                p.tx,
                window,
                cx,
            );
            for (k, c) in t.columns.iter().enumerate() {
                let top = r.y + row_top(k);
                if top + diagram::ROW < view.y || top > view.bottom() {
                    continue;
                }
                let color = if c.foreign_key { p.link } else { p.tx };
                let weight = if c.primary_key {
                    FontWeight::BOLD
                } else {
                    FontWeight::NORMAL
                };
                let x = r.x + diagram::PAD_X;
                text(
                    &c.name,
                    x,
                    top,
                    12.5,
                    diagram::ROW,
                    mono(weight),
                    color,
                    window,
                    cx,
                );
                text(
                    &c.data_type,
                    x + name_w,
                    top,
                    12.5,
                    diagram::ROW,
                    mono(FontWeight::NORMAL),
                    p.tx3,
                    window,
                    cx,
                );
            }
        }
    });
}

pub enum DiagramEvent {
    Open(TableRef, View),
}

enum Load {
    Loading,
    Failed(String),
    Ready(Board),
}

/// A mouse drag in progress.
#[derive(Clone, Copy)]
enum Drag {
    /// A card, held at `grab` from its top left (diagram units).
    Card { i: usize, grab: MPoint },
    /// The canvas, from the last pointer position (screen pixels).
    Pan { last: MPoint },
}

/// What a context-menu item does.
#[derive(Clone, Copy)]
enum Act {
    Open(usize, View),
    Remove(usize),
    Arrange,
    Fit,
}

/// The header menus.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    Schema,
    Add,
    Zoom,
    Export,
}

pub struct DiagramTab {
    pub connection: ConnectionConfig,
    pub schema: String,
    root: PathBuf,
    driver: Arc<dyn Driver>,
    /// The connection's schemas, for the picker: name and whether it is a system one.
    schemas: Vec<(String, bool)>,
    load: Load,
    viewport: Rc<Cell<Bounds<Pixels>>>,
    fit_pending: bool,
    drag: Option<Drag>,
    /// The card a right click landed on, for the context menu.
    menu_card: Option<usize>,
    menu: Option<Menu>,
    filter: Entity<InputState>,
    /// What the last export did, and whether it failed.
    message: Option<(String, bool)>,
    save: Option<Task<()>>,
    /// The typed save path, when no save dialog is available.
    path_field: Option<Entity<crate::export::PathField>>,
    window: gpui_kit::AnyWindowHandle,
    /// Paint times per frame, when `DEADLOCK_DIAGRAM_STATS` is set.
    stats: Option<RefCell<Vec<Duration>>>,
    _filter: Subscription,
}

impl EventEmitter<DiagramEvent> for DiagramTab {}

impl DiagramTab {
    pub fn new(
        connection: ConnectionConfig,
        root: PathBuf,
        driver: Arc<dyn Driver>,
        schema: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter tables"));
        let sub = cx.subscribe(&filter, |_, _, e: &InputEvent, cx| {
            if matches!(e, InputEvent::Change) {
                cx.notify();
            }
        });
        cx.on_release(|this, _| this.save_now()).detach();
        let mut this = Self {
            connection,
            schema,
            root,
            driver,
            schemas: Vec::new(),
            load: Load::Loading,
            viewport: Rc::new(Cell::new(Bounds::default())),
            fit_pending: false,
            drag: None,
            menu_card: None,
            menu: None,
            filter,
            message: None,
            save: None,
            path_field: None,
            window: window.window_handle(),
            stats: std::env::var_os("DEADLOCK_DIAGRAM_STATS").map(|_| RefCell::new(Vec::new())),
            _filter: sub,
        };
        this.load_schemas(cx);
        this.reload(cx);
        this
    }

    fn load_schemas(&mut self, cx: &mut Context<Self>) {
        let d = Arc::clone(&self.driver);
        let rx = cx
            .global::<Db>()
            .0
            .spawn(async move { d.children(&ObjectPath(Vec::new())).await });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(nodes)) = rx.await {
                let _ = this.update(cx, |t, cx| {
                    t.schemas = nodes
                        .into_iter()
                        // The open schema stays listed even when hidden.
                        .filter(|n| n.name == t.schema || !t.connection.is_hidden(&n.name))
                        .map(|n| (n.name, n.system))
                        .collect();
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Loads the schema's tables and keys, then the saved layout.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.load = Load::Loading;
        let (d, schema) = (Arc::clone(&self.driver), self.schema.clone());
        let rx = cx
            .global::<Db>()
            .0
            .spawn(async move { d.diagram(&schema).await.map_err(|e| e.to_string()) });
        let schema = self.schema.clone();
        cx.spawn(async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err("the diagram task stopped".into()));
            let _ = this.update(cx, |t, cx| {
                if t.schema == schema {
                    t.loaded(result);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn loaded(&mut self, result: Result<Diagram, String>) {
        let d = match result {
            Ok(d) => d,
            Err(e) => {
                self.load = Load::Failed(e);
                return;
            }
        };
        let mut board = Board::new(d);
        match diagram::load(&self.root, &self.connection.id, &self.schema) {
            Some(saved) => board.restore(&saved),
            None if board.diagram.tables.len() <= SHOW_ALL_UP_TO => {
                board.show_all();
                board.arrange();
                self.fit_pending = true;
            }
            None => self.menu = Some(Menu::Add),
        }
        self.load = Load::Ready(board);
    }

    /// Table `from` of the shown schema was renamed (`to`) or dropped: its saved card
    /// follows, then the diagram reloads.
    pub fn table_changed(&mut self, from: &str, to: Option<&str>, cx: &mut Context<Self>) {
        self.save_now();
        if let Err(e) =
            diagram::rename_table(&self.root, &self.connection.id, &self.schema, from, to)
        {
            eprintln!("deadlock: {e}");
        }
        self.reload(cx);
    }

    /// Switches to another schema of the connection.
    pub fn set_schema(&mut self, schema: String, cx: &mut Context<Self>) {
        if schema == self.schema && !matches!(self.load, Load::Failed(_)) {
            return;
        }
        self.save_now();
        self.schema = schema;
        self.menu = None;
        self.reload(cx);
    }

    fn board(&mut self) -> Option<&mut Board> {
        match &mut self.load {
            Load::Ready(b) => Some(b),
            _ => None,
        }
    }

    fn save_now(&mut self) {
        self.save = None;
        if let Load::Ready(b) = &self.load
            && let Err(e) = diagram::save(&self.root, &self.connection.id, &self.schema, &b.saved())
        {
            eprintln!("deadlock: {e}");
        }
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.save = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let _ = this.update(cx, |t, _| t.save_now());
        }));
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_save(cx);
        cx.notify();
    }

    fn viewport_size(&self) -> (f32, f32) {
        let b = self.viewport.get();
        (f32::from(b.size.width), f32::from(b.size.height))
    }

    fn local(&self, p: gpui_kit::Point<Pixels>) -> MPoint {
        let b = self.viewport.get();
        (f32::from(p.x - b.origin.x), f32::from(p.y - b.origin.y))
    }

    fn mouse_down(&mut self, e: &MouseDownEvent, cx: &mut Context<Self>) {
        let (sx, sy) = self.local(e.position);
        let schema = self.schema.clone();
        let Some(b) = self.board() else { return };
        let (mx, my) = b.to_model(sx, sy);
        let card = b.hit_card(mx, my);
        match e.button {
            MouseButton::Right => {
                if let Some(i) = card {
                    b.selected = Some(Pick::Card(i));
                    b.raise(i);
                }
                self.menu_card = card;
            }
            MouseButton::Left => {
                if let Some(i) = card {
                    b.selected = Some(Pick::Card(i));
                    b.raise(i);
                    if e.click_count >= 2 {
                        let table = TableRef {
                            database: schema,
                            table: b.diagram.tables[i].name.clone(),
                        };
                        cx.emit(DiagramEvent::Open(table, View::Data));
                        return;
                    }
                    let r = b.card(i).map_or((0., 0.), |r| (mx - r.x, my - r.y));
                    self.drag = Some(Drag::Card { i, grab: r });
                } else if let Some(l) = b.hit_link(mx, my) {
                    b.selected = Some(Pick::Link(l));
                } else {
                    b.selected = None;
                    self.drag = Some(Drag::Pan { last: (sx, sy) });
                }
            }
            _ => return,
        }
        cx.notify();
    }

    fn mouse_move(&mut self, e: &MouseMoveEvent, inside: bool, cx: &mut Context<Self>) {
        let (sx, sy) = self.local(e.position);
        let drag = self.drag;
        let Some(b) = self.board() else { return };
        match drag {
            Some(Drag::Card { i, grab }) => {
                let (mx, my) = b.to_model(sx, sy);
                b.at[i] = Some((mx - grab.0, my - grab.1));
                cx.notify();
            }
            Some(Drag::Pan { last }) => {
                b.pan = (b.pan.0 + sx - last.0, b.pan.1 + sy - last.1);
                self.drag = Some(Drag::Pan { last: (sx, sy) });
                cx.notify();
            }
            None => {
                let hover = if inside {
                    let (mx, my) = b.to_model(sx, sy);
                    b.hit_card(mx, my)
                        .map_or_else(|| b.hit_link(mx, my), |_| None)
                } else {
                    None
                };
                if hover != b.hovered {
                    b.hovered = hover;
                    cx.notify();
                }
            }
        }
    }

    fn mouse_up(&mut self, cx: &mut Context<Self>) {
        if self.drag.take().is_some() {
            self.changed(cx);
        }
    }

    fn scroll(&mut self, e: &ScrollEvent, cx: &mut Context<Self>) {
        let (sx, sy) = self.local(e.position);
        let d = e.delta.pixel_delta(px(20.));
        let (dx, dy) = (f32::from(d.x), f32::from(d.y));
        let zoom = if cfg!(target_os = "macos") {
            e.modifiers.platform
        } else {
            e.modifiers.control
        };
        let Some(b) = self.board() else { return };
        if zoom {
            let z = b.zoom * 2f32.powf(dy / 240.);
            b.zoom_at(sx, sy, z);
        } else {
            b.pan = (b.pan.0 + dx, b.pan.1 + dy);
        }
        self.changed(cx);
    }

    fn set_zoom(&mut self, z: Option<f32>, cx: &mut Context<Self>) {
        let (w, h) = self.viewport_size();
        self.menu = None;
        let Some(b) = self.board() else { return };
        match z {
            Some(z) => b.zoom_at(w / 2., h / 2., z),
            None => b.fit(w, h),
        }
        self.changed(cx);
    }

    fn step_zoom(&mut self, up: bool, cx: &mut Context<Self>) {
        let Some(b) = self.board() else { return };
        let z = if up { b.zoom * 1.25 } else { b.zoom / 1.25 };
        self.set_zoom(Some(z), cx);
    }

    fn arrange(&mut self, cx: &mut Context<Self>) {
        let (w, h) = self.viewport_size();
        let Some(b) = self.board() else { return };
        b.arrange();
        b.fit(w, h);
        self.changed(cx);
    }

    fn add(&mut self, i: usize, cx: &mut Context<Self>) {
        let (w, h) = self.viewport_size();
        let Some(b) = self.board() else { return };
        let p = b.free_spot(i, w, h);
        b.show(i, p);
        b.selected = Some(Pick::Card(i));
        self.changed(cx);
    }

    fn remove(&mut self, i: usize, cx: &mut Context<Self>) {
        if let Some(b) = self.board() {
            b.hide(i);
        }
        self.changed(cx);
    }

    fn open(&mut self, i: usize, view: View, cx: &mut Context<Self>) {
        let schema = self.schema.clone();
        if let Some(b) = self.board() {
            let table = b.diagram.tables[i].name.clone();
            cx.emit(DiagramEvent::Open(
                TableRef {
                    database: schema,
                    table,
                },
                view,
            ));
        }
    }

    fn act(&mut self, act: Act, cx: &mut Context<Self>) {
        match act {
            Act::Open(i, view) => self.open(i, view, cx),
            Act::Remove(i) => self.remove(i, cx),
            Act::Arrange => self.arrange(cx),
            Act::Fit => self.set_zoom(None, cx),
        }
    }

    fn export(&mut self, png: bool, cx: &mut Context<Self>) {
        self.menu = None;
        let p = *cx.global::<Palette>();
        let Load::Ready(b) = &self.load else { return };
        let svg = b.svg(&colors(&p));
        let name = format!("{}.{}", self.schema, if png { "png" } else { "svg" });
        if self.path_field.is_some() {
            return;
        }
        let picked = crate::export::save_path(self.root.clone(), name, self.window, cx);
        cx.spawn(async move |this, cx| {
            let Some(path) = picked.await else { return };
            let written = cx
                .background_executor()
                .spawn(async move {
                    let bytes = if png {
                        rasterize(&svg)?
                    } else {
                        svg.into_bytes()
                    };
                    std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
                    Ok::<_, String>(path)
                })
                .await;
            let _ = this.update(cx, |t, cx| {
                t.message = Some(match written {
                    Ok(path) => (
                        format!("Exported {}", deadlock_core::project::folder_name(&path)),
                        false,
                    ),
                    Err(e) => (e, true),
                });
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn set_menu(&mut self, m: Menu, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = if open { Some(m) } else { None };
        if open && m == Menu::Add {
            self.filter.update(cx, |s, cx| {
                s.set_value("", window, cx);
                s.focus(window, cx);
            });
        }
        cx.notify();
    }
}

/// The SVG as a PNG at twice the size, smaller for very large diagrams.
fn rasterize(svg: &str) -> Result<Vec<u8>, String> {
    use resvg::{tiny_skia, usvg};
    let mut opt = usvg::Options::default();
    for f in crate::FONTS {
        opt.fontdb_mut().load_font_data(f.to_vec());
    }
    let tree = usvg::Tree::from_str(svg, &opt).map_err(|e| format!("export: {e}"))?;
    let s = tree.size();
    // At most 16k px a side and 128 Mpx in all.
    let scale = 2f32
        .min(16_384. / s.width().max(s.height()))
        .min((128e6 / (s.width() * s.height())).sqrt());
    let (w, h) = (
        (s.width() * scale).ceil() as u32,
        (s.height() * scale).ceil() as u32,
    );
    let mut pixmap =
        tiny_skia::Pixmap::new(w.max(1), h.max(1)).ok_or("export: the image is too large")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap.encode_png().map_err(|e| format!("export: {e}"))
}

impl Render for DiagramTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *cx.global::<Palette>();
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header(self, window, cx))
            .children(self.path_field.clone())
            .child(body(self, &p, cx))
    }
}

impl crate::export::PathHost for DiagramTab {
    fn path_field(&mut self) -> &mut Option<Entity<crate::export::PathField>> {
        &mut self.path_field
    }
}

/// A list row in a header menu.
fn menu_row(
    id: impl Into<gpui_kit::ElementId>,
    on: bool,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(8.))
        .h(row_height())
        .px(px(8.))
        .rounded(RADIUS)
        .whitespace_nowrap()
        .text_color(p.tx)
        .when(on, |d| d.bg(p.sel))
        .hover(|s| s.bg(p.hover))
}

fn popover(
    id: &'static str,
    m: Menu,
    tab: &DiagramTab,
    trigger: gpui_kit::Stateful<gpui_kit::Div>,
    tip: &'static str,
    cx: &mut Context<DiagramTab>,
) -> Popover {
    let open = tab.menu == Some(m);
    Popover::new(id)
        .p(px(4.))
        .open(open)
        .on_open_change(
            cx.listener(move |t, open: &bool, window, cx| t.set_menu(m, *open, window, cx)),
        )
        .trigger(Trigger(trigger, open, tip))
}

fn header(
    tab: &DiagramTab,
    _window: &mut Window,
    cx: &mut Context<DiagramTab>,
) -> impl IntoElement {
    let p = *cx.global::<Palette>();
    let this = cx.entity().downgrade();
    let ready = matches!(tab.load, Load::Ready(_));
    let (shown, total, zoom) = match &tab.load {
        Load::Ready(b) => (b.shown(), b.diagram.tables.len(), b.zoom),
        _ => (0, 0, 1.),
    };

    let schemas = tab.schemas.clone();
    let current = tab.schema.clone();
    let schema = popover(
        "diagram-schema",
        Menu::Schema,
        tab,
        button("diagram-schema-button", Kind::Outline, true, "", cx)
            .child(div().text_color(p.tx3).child("Schema"))
            .child(SharedString::from(tab.schema.clone()))
            .child(button_icon(
                "diagram-schema-button",
                Icon::ChevronDown,
                Kind::Outline,
                true,
                cx,
            )),
        "Show another schema of this connection",
        cx,
    )
    .max_h(px(420.))
    .content({
        let this = this.clone();
        move |_, _, _| {
            let rows = schemas.iter().enumerate().map(|(i, (name, system))| {
                let (this, name) = (this.clone(), name.clone());
                menu_row(("diagram-schema-row", i), name == current, &p)
                    .when(*system, |d| d.text_color(p.tx3))
                    .child(SharedString::from(name.clone()))
                    .clickable(move |_, _, cx| {
                        let _ = this.update(cx, |t, cx| t.set_schema(name.clone(), cx));
                    })
            });
            div()
                .id("diagram-schema-list")
                .flex()
                .flex_col()
                .gap(px(1.))
                .min_w(px(200.))
                .max_h(px(400.))
                .overflow_y_scroll()
                .children(rows)
        }
    });

    let hidden: Vec<(usize, String)> = match &tab.load {
        Load::Ready(b) => {
            let q = Query::new(&tab.filter.read(cx).value());
            b.diagram
                .tables
                .iter()
                .enumerate()
                .filter(|(i, t)| {
                    !b.is_shown(*i) && q.as_ref().is_none_or(|q| q.find(&t.name).is_some())
                })
                .map(|(i, t)| (i, t.name.clone()))
                .collect()
        }
        _ => Vec::new(),
    };
    let filter = tab.filter.clone();
    let add = popover(
        "diagram-add",
        Menu::Add,
        tab,
        button("diagram-add-button", Kind::Outline, ready, "", cx).child("Add table"),
        "Add a table of the schema to the diagram",
        cx,
    )
    .content({
        let this = this.clone();
        move |_, _, _| {
            let rows = hidden.iter().map(|(i, name)| {
                let (this, i) = (this.clone(), *i);
                menu_row(("diagram-add-row", i), false, &p)
                    .child(icon(Icon::Table, p.tx3))
                    .child(SharedString::from(name.clone()))
                    .clickable(move |_, _, cx| {
                        let _ = this.update(cx, |t, cx| t.add(i, cx));
                    })
            });
            let empty = hidden.is_empty();
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .w(px(280.))
                .child(
                    div()
                        .h(row_height())
                        .px(px(8.))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .border_1()
                        .border_color(p.line2)
                        .rounded(RADIUS)
                        .child(icon(Icon::Search, p.tx3))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(&filter).appearance(false).h(px(24.)).px_0()),
                        ),
                )
                .child(
                    div()
                        .id("diagram-add-list")
                        .flex()
                        .flex_col()
                        .gap(px(1.))
                        .max_h(px(360.))
                        .overflow_y_scroll()
                        .when(empty, |d| {
                            d.child(
                                div()
                                    .px(px(8.))
                                    .py(px(4.))
                                    .text_color(p.tx3)
                                    .child("Every table is on the diagram."),
                            )
                        })
                        .children(rows),
                )
        }
    });

    let zoom_label = popover(
        "diagram-zoom",
        Menu::Zoom,
        tab,
        div()
            .id("diagram-zoom-label")
            .h(row_height())
            .min_w(px(44.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(RADIUS)
            .hover(|s| s.bg(p.hover))
            .child(format!("{:.0}%", zoom * 100.)),
        "",
        cx,
    )
    .content({
        let this = this.clone();
        move |_, _, _| {
            let levels = [
                None,
                Some(0.25),
                Some(0.5),
                Some(0.75),
                Some(1.),
                Some(1.5),
                Some(2.),
            ];
            let rows = levels.into_iter().enumerate().map(|(i, z)| {
                let this = this.clone();
                let label = z.map_or_else(|| "Fit".to_owned(), |z| format!("{:.0}%", z * 100.));
                menu_row(
                    ("diagram-zoom-row", i),
                    z.is_some_and(|z: f32| (z - zoom).abs() < 0.001),
                    &p,
                )
                .child(label)
                .clickable(move |_, _, cx| {
                    let _ = this.update(cx, |t, cx| t.set_zoom(z, cx));
                })
            });
            div()
                .flex()
                .flex_col()
                .gap(px(1.))
                .min_w(px(96.))
                .children(rows)
        }
    });

    let export = popover(
        "diagram-export",
        Menu::Export,
        tab,
        button("diagram-export-button", Kind::Outline, ready, "", cx).child("Export image"),
        "Save the whole diagram as an image",
        cx,
    )
    .anchor(gpui_kit::Anchor::TopRight)
    .content({
        let this = this.clone();
        move |_, _, _| {
            let rows = [
                (false, "SVG image", ".svg, scales without loss"),
                (true, "PNG image", ".png, twice the screen size"),
            ]
            .into_iter()
            .map(|(png, label, what)| {
                let this = this.clone();
                menu_row(label, false, &p)
                    .child(div().font_weight(FontWeight::MEDIUM).child(label))
                    .child(div().ml_auto().pl(px(8.)).text_color(p.tx3).child(what))
                    .clickable(move |_, _, cx| {
                        let _ = this.update(cx, |t, cx| t.export(png, cx));
                    })
            });
            div()
                .flex()
                .flex_col()
                .gap(px(1.))
                .min_w(px(260.))
                .children(rows)
        }
    });

    let zoom_button =
        |id: &'static str, i: Icon, up: bool, tip: &'static str, cx: &mut Context<DiagramTab>| {
            button(id, Kind::Ghost, ready, tip, cx)
                .child(button_icon(id, i, Kind::Ghost, ready, cx))
                .when(ready, |b| {
                    b.clickable(cx.listener(move |t, _: &ClickEvent, _, cx| t.step_zoom(up, cx)))
                })
        };

    div()
        .flex()
        .flex_wrap()
        .flex_none()
        .items_center()
        .gap_x(px(16.))
        .gap_y(px(8.))
        .px(px(16.))
        .py(px(10.))
        .border_b_1()
        .border_color(p.line)
        .child(schema)
        .when(ready, |d| {
            d.child(
                div()
                    .text_color(p.tx3)
                    .child(format!("{shown} of {total} tables shown")),
            )
        })
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child("Bold is a primary key"),
        )
        .child(div().text_color(p.link).child("Blue is a foreign key"))
        .when_some(tab.message.clone(), |d, (m, bad)| {
            d.child(
                div()
                    .text_color(if bad { p.danger } else { p.tx3 })
                    .child(m),
            )
        })
        .child(div().flex_grow(1.))
        .child(add)
        .child(
            button(
                "diagram-arrange",
                Kind::Outline,
                ready,
                "Lay out the tables again",
                cx,
            )
            .child("Arrange")
            .when(ready, |b| {
                b.clickable(cx.listener(|t, _: &ClickEvent, _, cx| t.arrange(cx)))
            }),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(2.))
                .child(zoom_button(
                    "diagram-zoom-out",
                    Icon::Minus,
                    false,
                    "Zoom out (Ctrl+wheel)",
                    cx,
                ))
                .child(zoom_label)
                .child(zoom_button(
                    "diagram-zoom-in",
                    Icon::Plus,
                    true,
                    "Zoom in (Ctrl+wheel)",
                    cx,
                )),
        )
        .child(export)
}

fn body(tab: &DiagramTab, p: &Palette, cx: &mut Context<DiagramTab>) -> AnyElement {
    let note = |text: String, color: Hsla| {
        div()
            .flex_1()
            .min_h_0()
            .bg(p.panel)
            .flex()
            .items_center()
            .justify_center()
            .text_color(color)
            .child(text)
            .into_any_element()
    };
    match &tab.load {
        Load::Loading => return note("Loading the schema…".into(), p.tx3),
        Load::Failed(e) => return note(e.clone(), p.danger),
        Load::Ready(_) => {}
    }
    let entity = cx.entity();
    let viewport = Rc::clone(&tab.viewport);
    let palette = *p;
    let empty = matches!(&tab.load, Load::Ready(b) if b.shown() == 0);
    let surface = canvas(
        {
            let (entity, viewport) = (entity.clone(), viewport);
            move |bounds, window, cx| {
                viewport.set(bounds);
                entity.update(cx, |t, cx| {
                    if std::mem::take(&mut t.fit_pending)
                        && let Load::Ready(b) = &mut t.load
                    {
                        b.fit(f32::from(bounds.size.width), f32::from(bounds.size.height));
                        // The header shows the new zoom on the next frame.
                        cx.notify();
                    }
                });
                window.insert_hitbox(bounds, HitboxBehavior::Normal)
            }
        },
        {
            let entity = entity.clone();
            move |bounds, hitbox: Hitbox, window, cx| {
                entity.update(cx, |t, cx| {
                    let started = Instant::now();
                    if let Load::Ready(b) = &t.load {
                        paint(b, bounds, &palette, window, cx);
                    }
                    if let Some(stats) = &t.stats {
                        record(stats, started.elapsed());
                    }
                });
                listen(&entity, bounds, hitbox, window);
            }
        },
    )
    .size_full();
    let menu_entity = entity.clone();
    div()
        .id("diagram-canvas")
        .relative()
        .flex_1()
        .min_h_0()
        .bg(p.panel)
        .overflow_hidden()
        .child(surface)
        .when(empty, |d| {
            d.child(
                div()
                    .absolute()
                    .top(px(24.))
                    .left(px(24.))
                    .text_color(p.tx3)
                    .child("No tables on the diagram. Add one with Add table."),
            )
        })
        .context_menu(move |menu, _, cx| {
            let t = menu_entity.read(cx);
            let e = menu_entity.clone();
            let item = |label: &'static str, ic: Icon, act: Act| {
                let e = e.clone();
                menu_item(label, ic).on_click(
                    move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                        e.update(cx, |t, cx| t.act(act, cx));
                    },
                )
            };
            match t.menu_card {
                Some(i) => menu
                    .item(item("Open data", Icon::Table, Act::Open(i, View::Data)))
                    .item(item(
                        "Open structure",
                        Icon::Config,
                        Act::Open(i, View::Structure),
                    ))
                    .separator()
                    .item(item("Remove from diagram", Icon::Minus, Act::Remove(i))),
                None => menu
                    .item(item("Arrange", Icon::Diagram, Act::Arrange))
                    .item(item("Fit", Icon::SelectAll, Act::Fit)),
            }
        })
        .into_any_element()
}

/// Collects paint times and prints a summary every 120 frames.
fn record(stats: &RefCell<Vec<Duration>>, took: Duration) {
    let mut s = stats.borrow_mut();
    s.push(took);
    if s.len() >= 120 {
        s.sort();
        let ms = |d: Duration| d.as_secs_f64() * 1000.;
        eprintln!(
            "deadlock: diagram paint p50 {:.2} ms, p95 {:.2} ms, max {:.2} ms",
            ms(s[s.len() / 2]),
            ms(s[s.len() * 95 / 100]),
            ms(s[s.len() - 1])
        );
        s.clear();
    }
}

/// Mouse handling for the canvas: drag cards, pan, hover and pick lines, wheel and
/// Ctrl+wheel. Moves and releases are followed outside the canvas while dragging.
fn listen(
    entity: &Entity<DiagramTab>,
    bounds: Bounds<Pixels>,
    hitbox: Hitbox,
    window: &mut Window,
) {
    let e = entity.clone();
    let hb = hitbox.clone();
    window.on_mouse_event(move |ev: &MouseDownEvent, phase, window, cx| {
        if phase.bubble() && hb.is_hovered(window) {
            e.update(cx, |t, cx| t.mouse_down(ev, cx));
        }
    });
    let e = entity.clone();
    let hb = hitbox.clone();
    window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, cx| {
        if phase.bubble() {
            let inside = hb.is_hovered(window) && bounds.contains(&ev.position);
            e.update(cx, |t, cx| t.mouse_move(ev, inside, cx));
        }
    });
    let e = entity.clone();
    window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
        if phase.bubble() {
            e.update(cx, |t, cx| t.mouse_up(cx));
        }
    });
    let e = entity.clone();
    window.on_mouse_event(move |ev: &ScrollEvent, phase, window, cx| {
        if phase.bubble() && hitbox.is_hovered(window) {
            e.update(cx, |t, cx| t.scroll(ev, cx));
            cx.stop_propagation();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadlock_core::diagram::{Column, Link, Table};

    fn table(name: &str, cols: &[&str]) -> Table {
        Table {
            name: name.into(),
            columns: cols
                .iter()
                .map(|c| Column {
                    name: (*c).into(),
                    data_type: "bigint".into(),
                    primary_key: *c == "id",
                    foreign_key: c.ends_with("_id"),
                })
                .collect(),
        }
    }

    fn board() -> Board {
        Board::new(Diagram {
            tables: vec![
                table("customers", &["id"]),
                table("orders", &["id", "customer_id"]),
            ],
            links: vec![Link {
                name: "fk".into(),
                from: "orders".into(),
                from_columns: vec!["customer_id".into()],
                to: "customers".into(),
                to_columns: vec!["id".into()],
            }],
        })
    }

    #[test]
    fn zoom_keeps_the_point_under_the_pointer() {
        let mut b = board();
        let before = b.to_model(300., 200.);
        b.zoom_at(300., 200., 1.7);
        let after = b.to_model(300., 200.);
        assert!((before.0 - after.0).abs() < 1e-3 && (before.1 - after.1).abs() < 1e-3);
        b.zoom_at(0., 0., 9.);
        assert_eq!(b.zoom, MAX_ZOOM);
        b.zoom_at(0., 0., 0.01);
        assert_eq!(b.zoom, MIN_ZOOM);
    }

    #[test]
    fn show_hide_restore_and_hits() {
        let mut b = board();
        b.show_all();
        b.arrange();
        assert_eq!(b.shown(), 2);
        let c = b.card(0).unwrap();
        assert_eq!(b.hit_card(c.x + 5., c.y + 5.), Some(0));
        let route = b.route(0).unwrap();
        let mid = route[route.len() / 2];
        assert_eq!(b.hit_link(mid.0, mid.1), Some(0));
        b.selected = Some(Pick::Card(1));
        assert_eq!(b.lit(), [true]);
        let saved = b.saved();
        b.hide(1);
        assert_eq!((b.shown(), b.selected, b.route(0)), (1, None, None));
        // A restored layout drops tables that are gone and keeps the rest in place.
        let mut fresh = Board::new(Diagram {
            tables: vec![table("orders", &["id", "customer_id"])],
            links: vec![],
        });
        fresh.restore(&saved);
        assert_eq!(fresh.shown(), 1);
        assert_eq!(
            fresh.card(0).map(|r| (r.x, r.y)),
            saved
                .tables
                .iter()
                .find(|p| p.name == "orders")
                .map(|p| (p.x, p.y))
        );
        fresh.fit(800., 600.);
        assert!(fresh.zoom <= 1.);
    }

    #[test]
    fn exported_png_renders() {
        let mut b = board();
        b.show_all();
        b.arrange();
        let p = Palette::light();
        let png = rasterize(&b.svg(&colors(&p))).unwrap();
        assert_eq!(&png[1..4], b"PNG");
        assert_eq!(css(p.link), "#1D4ED8");
    }
}
