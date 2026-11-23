//! A table tab's Structure view: columns, indexes, foreign keys both ways and the
//! server's definition, with Add column, Add index and drop actions. Each change is
//! one `alter table` on the tab's session, behind the same prompts as a run.

use std::sync::Arc;
use std::time::Duration;

use deadlock_core::driver::DriverError;
use deadlock_core::edit::TableRef;
use deadlock_core::structure::{
    self, ForeignKey, IndexKind, IndexPart, NewColumn, NewIndex, Position, TableStructure,
    size_label,
};
use gpui_kit::component::IndexPath;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, Div, Entity, FontWeight,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Stateful,
    StatefulInteractiveElement, Styled, Subscription, Window, div, px,
};

use super::table::highlighted;
use super::{QueryTab, Session, TabEvent};
use crate::app::Db;
use crate::assets::Icon;
use crate::theme::{MONO_FONT, Palette, RADIUS, data_size, row_height, small_size};
use crate::ui::{Kind as ButtonKind, button, icon_button};

/// The four lists, for the selected row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Columns,
    Indexes,
    ForeignKeys,
    ReferencedBy,
    Checks,
}

type Choice = Entity<SelectState<Vec<String>>>;

struct ColumnForm {
    name: Entity<InputState>,
    data_type: Choice,
    length: Entity<InputState>,
    default: Entity<InputState>,
    comment: Entity<InputState>,
    /// `At the end`, `First`, then `After <column>` for each column.
    position: Choice,
    nullable: bool,
    auto_increment: bool,
}

struct Part {
    column: String,
    descending: bool,
    prefix: Entity<InputState>,
}

struct IndexForm {
    name: Entity<InputState>,
    kind: Choice,
    parts: Vec<Part>,
    /// Picks the next column to add.
    add: Choice,
}

enum Form {
    Column(ColumnForm),
    Index(IndexForm),
}

#[derive(Default)]
pub(crate) struct Structure {
    data: Option<TableStructure>,
    busy: bool,
    /// A load error, or the outcome of the last ALTER; `true` for an error.
    notice: Option<(String, bool)>,
    selected: Option<(Section, usize)>,
    form: Option<Form>,
    copied: bool,
    generation: u64,
    _subs: Vec<Subscription>,
}

impl Structure {
    pub(super) fn is_loaded(&self) -> bool {
        self.data.is_some() || self.busy
    }

    pub(super) fn fail(&mut self, e: String) {
        self.notice = Some((e, true));
    }
}

/// The column types as the form's list holds them.
fn types(mariadb: bool, oracle: bool) -> Vec<String> {
    if oracle {
        return structure::ORACLE_TYPES.map(str::to_owned).to_vec();
    }
    structure::column_types(mariadb)
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// The index kinds the form offers.
fn index_kinds(oracle: bool) -> &'static [IndexKind] {
    if oracle {
        &IndexKind::ORACLE
    } else {
        &IndexKind::ALL
    }
}

impl QueryTab {
    fn structure_mut(&mut self) -> Option<&mut Structure> {
        self.table.as_mut().map(|t| &mut t.structure)
    }

    /// Fetches the structure on the tab's session.
    pub(super) fn load_structure(&mut self, cx: &mut Context<Self>) {
        let Session::Ready(driver) = &self.session else {
            return;
        };
        let driver = Arc::clone(driver);
        let Some(t) = &mut self.table else { return };
        let table = t.table.clone();
        let s = &mut t.structure;
        s.busy = true;
        s.generation += 1;
        let generation = s.generation;
        let rx = cx.global::<Db>().0.spawn(async move {
            driver
                .structure(&table)
                .await
                .map_err(|e: DriverError| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err("the structure task stopped".into()));
            let _ = this.update(cx, |this, cx| {
                let Some(s) = this.structure_mut() else {
                    return;
                };
                if s.generation != generation {
                    return;
                }
                s.busy = false;
                match result {
                    Ok(data) => {
                        // The selected row may be gone after an ALTER.
                        s.selected = None;
                        s.data = Some(data);
                    }
                    Err(e) => s.notice = Some((e, true)),
                }
                cx.emit(TabEvent::Changed);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn select_row(&mut self, section: Section, i: usize, cx: &mut Context<Self>) {
        if let Some(s) = self.structure_mut() {
            s.selected = Some((section, i));
        }
        cx.notify();
    }

    fn copy_definition(&mut self, cx: &mut Context<Self>) {
        let Some(s) = self.structure_mut() else {
            return;
        };
        let Some(ddl) = s.data.as_ref().map(|d| d.ddl.clone()) else {
            return;
        };
        s.copied = true;
        cx.write_to_clipboard(ClipboardItem::new_string(ddl));
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(s) = this.structure_mut() {
                    s.copied = false;
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn oracle(&self) -> bool {
        self.connection
            .as_ref()
            .is_some_and(|c| c.engine == deadlock_core::connection::Engine::Oracle)
    }

    fn mariadb(&self) -> bool {
        match &self.session {
            Session::Ready(d) => d.server_version().starts_with("MariaDB"),
            _ => false,
        }
    }

    fn open_column_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(columns) = self.table.as_ref().and_then(|t| {
            let d = t.structure.data.as_ref()?;
            Some(d.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>())
        }) else {
            return;
        };
        let mut positions = vec!["At the end".to_owned(), "First".to_owned()];
        positions.extend(columns.iter().map(|c| format!("After {c}")));
        let input = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            let placeholder = placeholder.to_owned();
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let name = input("name", window, cx);
        let length = input("255 or 10,2", window, cx);
        let default = input(
            "none, NULL, a value, CURRENT_TIMESTAMP or (expr)",
            window,
            cx,
        );
        let comment = input("", window, cx);
        let types = types(self.mariadb(), self.oracle());
        let varchar = types
            .iter()
            .position(|t| t == "varchar" || t == "varchar2")
            .unwrap_or(0);
        let data_type = cx.new(|cx| {
            SelectState::new(types, Some(IndexPath::default().row(varchar)), window, cx)
                .searchable(true)
        });
        let position =
            cx.new(|cx| SelectState::new(positions, Some(IndexPath::default().row(0)), window, cx));
        let subs = self.form_subs(
            &[&name, &length, &default, &comment],
            &[&data_type, &position],
            cx,
        );
        name.update(cx, |n, cx| n.focus(window, cx));
        if let Some(s) = self.structure_mut() {
            s.form = Some(Form::Column(ColumnForm {
                name,
                data_type,
                length,
                default,
                comment,
                position,
                nullable: true,
                auto_increment: false,
            }));
            s._subs = subs;
            s.notice = None;
        }
        cx.notify();
    }

    fn open_index_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(columns) = self.table.as_ref().and_then(|t| {
            let d = t.structure.data.as_ref()?;
            Some(d.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>())
        }) else {
            return;
        };
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("name, or empty"));
        let kinds = index_kinds(self.oracle())
            .iter()
            .map(|k| k.label().to_owned())
            .collect();
        let kind =
            cx.new(|cx| SelectState::new(kinds, Some(IndexPath::default().row(0)), window, cx));
        let add = cx.new(|cx| SelectState::new(columns, None, window, cx).searchable(true));
        let mut subs = self.form_subs(&[&name], &[&kind], cx);
        subs.push(cx.subscribe_in(
            &add,
            window,
            |this, add, e: &SelectEvent<Vec<String>>, window, cx| {
                let SelectEvent::Confirm(Some(column)) = e else {
                    return;
                };
                let prefix = cx.new(|cx| InputState::new(window, cx).placeholder("prefix"));
                let sub = cx.subscribe(&prefix, |_, _, e: &InputEvent, cx| {
                    if matches!(e, InputEvent::Change) {
                        cx.notify();
                    }
                });
                if let Some(s) = this.structure_mut()
                    && let Some(Form::Index(f)) = &mut s.form
                {
                    f.parts.push(Part {
                        column: column.clone(),
                        descending: false,
                        prefix,
                    });
                    s._subs.push(sub);
                }
                add.update(cx, |a, cx| a.set_selected_index(None, window, cx));
                cx.notify();
            },
        ));
        name.update(cx, |n, cx| n.focus(window, cx));
        if let Some(s) = self.structure_mut() {
            s.form = Some(Form::Index(IndexForm {
                name,
                kind,
                parts: Vec::new(),
                add,
            }));
            s._subs = subs;
            s.notice = None;
        }
        cx.notify();
    }

    /// The form's preview follows every keystroke and choice.
    fn form_subs(
        &self,
        inputs: &[&Entity<InputState>],
        choices: &[&Choice],
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        let mut subs: Vec<Subscription> = inputs
            .iter()
            .map(|i| {
                cx.subscribe(*i, |_, _, e: &InputEvent, cx| {
                    if matches!(e, InputEvent::Change) {
                        cx.notify();
                    }
                })
            })
            .collect();
        subs.extend(
            choices
                .iter()
                .map(|c| cx.observe(*c, |_, _, cx| cx.notify())),
        );
        subs
    }

    fn close_form(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.structure_mut() {
            s.form = None;
            s._subs.clear();
            s.notice = None;
        }
        cx.notify();
    }

    /// The open form's statement.
    fn form_sql(&self, cx: &App) -> Option<Result<String, String>> {
        let t = self.table.as_ref()?;
        let s = &t.structure;
        let data = s.data.as_ref()?;
        let text = |i: &Entity<InputState>| i.read(cx).value().to_string();
        let choice = |c: &Choice| {
            let st = c.read(cx);
            st.selected_index(cx).map(|ix| ix.row)
        };
        Some(match s.form.as_ref()? {
            Form::Column(f) => {
                let types = types(self.mariadb(), self.oracle());
                let position = match choice(&f.position) {
                    None | Some(0) => Position::Last,
                    Some(1) => Position::First,
                    Some(i) => data
                        .columns
                        .get(i - 2)
                        .map_or(Position::Last, |c| Position::After(c.name.clone())),
                };
                let c = NewColumn {
                    name: text(&f.name),
                    data_type: choice(&f.data_type)
                        .and_then(|i| types.get(i).cloned())
                        .unwrap_or_default(),
                    length: text(&f.length),
                    nullable: f.nullable,
                    default: text(&f.default),
                    auto_increment: f.auto_increment,
                    comment: text(&f.comment),
                    position,
                };
                structure::add_column_sql(&t.table, &c, data)
            }
            Form::Index(f) => {
                let mut parts = Vec::with_capacity(f.parts.len());
                for p in &f.parts {
                    let prefix = text(&p.prefix);
                    let prefix = match prefix.trim() {
                        "" => None,
                        n => match n.parse::<u32>() {
                            Ok(n) if n > 0 => Some(n),
                            _ => {
                                return Some(Err(format!("{}: prefix must be a number", p.column)));
                            }
                        },
                    };
                    parts.push(IndexPart {
                        column: p.column.clone(),
                        expression: false,
                        descending: p.descending,
                        prefix,
                    });
                }
                let i = NewIndex {
                    name: text(&f.name),
                    kind: choice(&f.kind)
                        .and_then(|i| index_kinds(self.oracle()).get(i).copied())
                        .unwrap_or(IndexKind::Index),
                    parts,
                };
                structure::add_index_sql(&t.table, &i, data)
            }
        })
    }

    fn apply_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(sql)) = self.form_sql(cx) else {
            return;
        };
        let done = match self.table.as_ref().and_then(|t| t.structure.form.as_ref()) {
            Some(Form::Column(_)) => "Column added.",
            _ => "Index added.",
        };
        self.run_ddl(sql, None, done, window, cx);
    }

    /// A drop from a row's menu: always confirmed, showing the statement.
    fn ask_drop(
        &mut self,
        section: Section,
        i: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(t) = &self.table else { return };
        let Some(data) = &t.structure.data else {
            return;
        };
        let (sql, what, done) = match section {
            Section::Columns => {
                let Some(c) = data.columns.get(i) else { return };
                (
                    structure::drop_column_sql(&t.table, &c.name, data),
                    format!("Drops column {} and every value in it", c.name),
                    "Column dropped.",
                )
            }
            Section::Indexes => {
                let Some(ix) = data.indexes.get(i) else {
                    return;
                };
                (
                    structure::drop_index_sql(&t.table, ix, data),
                    format!("Drops index {}", ix.name),
                    "Index dropped.",
                )
            }
            Section::ForeignKeys => {
                let Some(k) = data.foreign_keys.get(i) else {
                    return;
                };
                (
                    structure::drop_foreign_key_sql(&t.table, &k.name, data),
                    format!("Drops foreign key {}", k.name),
                    "Foreign key dropped.",
                )
            }
            Section::Checks => {
                let Some(c) = data.checks.get(i) else { return };
                (
                    data.drop_constraint_sql(&t.table, &c.name),
                    format!("Drops check constraint {}", c.name),
                    "Check constraint dropped.",
                )
            }
            Section::ReferencedBy => return,
        };
        self.run_ddl(sql, Some(format!("{what}.")), done, window, cx);
    }

    /// Runs one ALTER on the tab's session: pending data changes are settled first,
    /// then the run prompts (production, implicit commit, `forced`) ask.
    fn run_ddl(
        &mut self,
        sql: String,
        forced: Option<String>,
        done: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.after_discard(window, cx, move |this, _, cx| {
            this.run_ddl_now(sql, forced, done, cx);
        });
    }

    fn run_ddl_now(
        &mut self,
        sql: String,
        forced: Option<String>,
        done: &'static str,
        cx: &mut Context<Self>,
    ) {
        let Session::Ready(driver) = &self.session else {
            if let Some(s) = self.structure_mut() {
                s.notice = Some(("Not connected: reopen the tab.".into(), true));
            }
            cx.notify();
            return;
        };
        let driver = Arc::clone(driver);
        let rt = Arc::clone(&cx.global::<Db>().0);
        if let Some(s) = self.structure_mut() {
            s.busy = true;
            s.notice = None;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let go = super::confirm(&this, cx, &rt, &driver, vec![sql.clone()], forced).await;
            let result = match go {
                Ok(true) => rt
                    .spawn(async move { driver.execute(&sql).await?.collect().await })
                    .await
                    .unwrap_or(Err(DriverError::Gone))
                    .map(drop)
                    .map_err(|e| e.to_string()),
                Ok(false) => {
                    let _ = this.update(cx, |this, cx| {
                        if let Some(s) = this.structure_mut() {
                            s.busy = false;
                        }
                        cx.notify();
                    });
                    return;
                }
                Err(e) => Err(e),
            };
            let _ = this.update(cx, |this, cx| this.ddl_done(result, done, cx));
        })
        .detach();
    }

    /// After an ALTER: on success the structure, the Data view's columns and the
    /// sidebar reload.
    fn ddl_done(&mut self, result: Result<(), String>, done: &'static str, cx: &mut Context<Self>) {
        let Some(database) = self.table.as_ref().map(|t| t.table.database.clone()) else {
            return;
        };
        let ok = result.is_ok();
        if let Some(s) = self.structure_mut() {
            s.busy = false;
            match result {
                Ok(()) => {
                    s.notice = Some((done.into(), false));
                    s.form = None;
                    s._subs.clear();
                }
                Err(e) => s.notice = Some((e, true)),
            }
        }
        if ok {
            self.load_structure(cx);
            self.columns_changed(cx);
            cx.emit(TabEvent::SchemaChanged(database));
        }
        self.check_tx(false, !ok, cx).detach();
        cx.notify();
    }
}

/// A grid column: a fixed width, or a share of the rest with a minimum.
#[derive(Clone, Copy)]
enum W {
    Fixed(f32),
    Flex(f32, f32),
}

const COLUMNS: [W; 5] = [
    W::Fixed(52.),
    W::Flex(1., 130.),
    W::Flex(1., 130.),
    W::Fixed(90.),
    W::Flex(1.3, 170.),
];
const INDEXES: [W; 3] = [W::Flex(1.3, 200.), W::Flex(1., 130.), W::Fixed(100.)];
const KEYS: [W; 5] = [
    W::Flex(1.3, 200.),
    W::Flex(1., 130.),
    W::Flex(1., 130.),
    W::Fixed(100.),
    W::Fixed(100.),
];

/// `.gc`: 12 px side padding, cut with an ellipsis.
fn cell(w: W) -> Div {
    let d = div()
        .px(px(12.))
        .overflow_hidden()
        .text_ellipsis()
        .whitespace_nowrap();
    match w {
        W::Fixed(n) => d.flex_none().w(px(n)),
        W::Flex(grow, min) => d
            .flex_grow(grow)
            .flex_shrink(1.)
            .flex_basis(px(0.))
            .min_w(px(min)),
    }
}

/// `.r.h`: the header row.
fn header(widths: &[W], labels: &[&'static str], p: &Palette) -> Div {
    div()
        .flex()
        .items_center()
        .min_h(px(32.))
        .border_b_1()
        .border_color(p.line)
        .bg(p.panel)
        .text_color(p.tx2)
        .text_size(small_size())
        .font_weight(FontWeight::MEDIUM)
        .children(widths.iter().zip(labels).map(|(w, l)| cell(*w).child(*l)))
}

/// `.sh`: a section title with an optional button on the right.
fn section_head(title: &'static str, action: Option<Stateful<Div>>) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.))
        .px(px(16.))
        .pt(px(20.))
        .pb(px(8.))
        .child(
            div()
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        )
        .children(action)
}

/// The scroll box around a grid: 600 px wide at least, scrolling sideways below that.
fn grid_box(id: &'static str, rows: Vec<AnyElement>, p: &Palette) -> impl IntoElement {
    div().id(id).overflow_x_scroll().child(
        div()
            .min_w(px(600.))
            .border_t_1()
            .border_color(p.line)
            .children(rows),
    )
}

/// A labelled form field.
fn labeled(label: &'static str, field: impl IntoElement, basis: f32, p: &Palette) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .flex_grow(1.)
        .flex_shrink(1.)
        .flex_basis(px(basis))
        .min_w(px(basis.min(120.)))
        .child(div().text_size(small_size()).text_color(p.tx2).child(label))
        .child(field)
}

fn text_field(state: &Entity<InputState>, mono: bool) -> Input {
    let input = Styled::h(Input::new(state), row_height());
    if mono {
        input.font_family(MONO_FONT).text_size(data_size())
    } else {
        input
    }
}

pub(super) fn render(this: &mut QueryTab, segments: Div, cx: &mut Context<QueryTab>) -> AnyElement {
    let p = *cx.global::<Palette>();
    let read_only = this.connection.as_ref().is_some_and(|c| c.read_only);
    let form_sql = this.form_sql(cx);
    let Some(t) = &this.table else {
        return div().into_any_element();
    };
    let s = &t.structure;
    let data = s.data.as_ref();
    let mut toolbar = div()
        .flex()
        .flex_wrap()
        .flex_none()
        .items_center()
        .gap_x(px(16.))
        .gap_y(px(8.))
        .px(px(16.))
        .py(px(10.))
        .min_h(px(49.))
        .border_b_1()
        .border_color(p.line)
        .child(segments)
        .child(
            div()
                .font_family(MONO_FONT)
                .text_size(data_size())
                .child(format!("{}.{}", t.table.database, t.table.table)),
        );
    if let Some(d) = data {
        let n = d.columns.len();
        let mut facts = vec![format!("{n} {}", if n == 1 { "column" } else { "columns" })];
        if !d.view {
            let n = d.indexes.len();
            facts.push(format!("{n} {}", if n == 1 { "index" } else { "indexes" }));
            facts.extend(d.facts.size().map(size_label));
        } else {
            facts.push("view".into());
        }
        if d.oracle {
            facts.extend(d.facts.tablespace.iter().map(|t| format!("tablespace {t}")));
            facts.extend(d.facts.rows.map(|n| format!("about {n} rows")));
            if !d.facts.comment.is_empty() {
                facts.push(d.facts.comment.clone());
            }
        }
        toolbar = toolbar.children(facts.into_iter().map(|f| div().text_color(p.tx3).child(f)));
    }
    let line = |text: String, color| {
        div()
            .flex_none()
            .px(px(16.))
            .py(px(6.))
            .border_b_1()
            .border_color(p.line)
            .text_size(small_size())
            .text_color(color)
            .child(text)
    };
    let notes = [
        read_only.then(|| {
            line(
                "Read-only connection: the structure cannot be changed.".into(),
                p.tx2,
            )
            .bg(p.panel)
        }),
        s.notice
            .clone()
            .map(|(n, err)| line(n, if err { p.danger } else { p.tx2 })),
    ];
    let Some(d) = data else {
        let text = if s.busy || t.waiting {
            "Loading structure…"
        } else {
            "No structure loaded."
        };
        return div()
            .size_full()
            .flex()
            .flex_col()
            .child(toolbar)
            .children(notes.into_iter().flatten())
            .child(div().p(px(16.)).text_color(p.tx3).child(text))
            .into_any_element();
    };
    let selected = s.selected;
    let busy = s.busy;
    let view = d.view;
    let entity = cx.entity();

    // A grid row: hover and selection colors, click to select, right click for `menu`.
    let row = |section: Section,
               i: usize,
               id: &'static str,
               cells: Vec<AnyElement>,
               menu: Vec<(String, Option<TableRef>)>,
               cx: &mut Context<QueryTab>|
     -> AnyElement {
        let on = selected == Some((section, i));
        let d = div()
            .id((id, i))
            .flex()
            .items_center()
            .min_h(px(32.))
            .border_b_1()
            .border_color(p.line)
            .when(on, |d| d.bg(p.sel))
            .when(!on, |d| d.hover(|s| s.bg(p.panel)))
            // The font sits on the cells, so the row's menu keeps the UI font.
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .font_family(MONO_FONT)
                    .text_size(data_size())
                    .children(cells),
            )
            .on_click(
                cx.listener(move |this, _: &ClickEvent, _, cx| this.select_row(section, i, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| this.select_row(section, i, cx)),
            );
        if menu.is_empty() {
            return d.into_any_element();
        }
        let entity = entity.clone();
        d.context_menu(move |mut m, _, _| {
            for (label, open) in &menu {
                let entity = entity.clone();
                let open = open.clone();
                m = m.item(PopupMenuItem::new(label.clone()).on_click(
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        entity.update(cx, |this, cx| match &open {
                            Some(t) => cx.emit(TabEvent::OpenTable(t.clone())),
                            None => this.ask_drop(section, i, window, cx),
                        });
                    },
                ));
            }
            m
        })
        .into_any_element()
    };
    // A table name in the link color; a click opens its Structure view.
    let link =
        |id: &'static str, i: usize, text: String, target: TableRef, cx: &mut Context<QueryTab>| {
            div()
                .id((id, i))
                .text_color(p.link)
                .cursor_pointer()
                .hover(|s| s.text_color(p.tx))
                .child(text)
                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    cx.emit(TabEvent::OpenTable(target.clone()));
                }))
        };
    let other = |k: &TableRef| {
        if k.database == t.table.database {
            k.table.clone()
        } else {
            format!("{}.{}", k.database, k.table)
        }
    };

    let mut column_rows = vec![
        header(
            &COLUMNS,
            &["Key", "Name", "Type", "Nullable", "Default"],
            &p,
        )
        .into_any_element(),
    ];
    for (i, c) in d.columns.iter().enumerate() {
        let cells = vec![
            cell(COLUMNS[0])
                .text_color(p.link)
                .font_weight(FontWeight::MEDIUM)
                .child(c.key_label())
                .into_any_element(),
            cell(COLUMNS[1]).child(c.name.clone()).into_any_element(),
            cell(COLUMNS[2])
                .child(c.column_type.clone())
                .into_any_element(),
            cell(COLUMNS[3])
                .when(!c.nullable, |d| d.text_color(p.tx3))
                .child(if c.nullable { "null" } else { "not null" })
                .into_any_element(),
            cell(COLUMNS[4])
                .text_color(p.tx3)
                .child(c.default_label())
                .into_any_element(),
        ];
        let menu = if view {
            Vec::new()
        } else {
            vec![(format!("Drop column {}", c.name), None)]
        };
        column_rows.push(row(
            Section::Columns,
            i,
            "structure-column",
            cells,
            menu,
            cx,
        ));
    }
    let add = |id: &'static str, label: &'static str, tip: &'static str, cx: &App| {
        icon_button(
            id,
            Icon::Plus,
            Some(label),
            ButtonKind::Outline,
            !busy,
            tip,
            cx,
        )
    };
    let add_column = (!view).then(|| {
        add("add-column", "Add column", "Add a column to the table", cx).when(!busy, |b| {
            b.on_click(cx.listener(|this, _, window, cx| this.open_column_form(window, cx)))
        })
    });
    let column_form = match (&s.form, &form_sql) {
        (Some(Form::Column(f)), Some(sql)) => Some(column_form(f, sql, busy, &p, cx)),
        _ => None,
    };
    let mut main = div()
        .flex_grow(999.)
        .flex_shrink(1.)
        .flex_basis(px(480.))
        .min_w_0()
        .pb(px(24.))
        .child(section_head("Columns", add_column))
        .children(column_form)
        .child(grid_box("structure-columns", column_rows, &p));

    if !view {
        let mut index_rows =
            vec![header(&INDEXES, &["Name", "Columns", "Kind"], &p).into_any_element()];
        for (i, ix) in d.indexes.iter().enumerate() {
            let plain = ix.primary || ix.unique;
            let cells = vec![
                cell(INDEXES[0]).child(ix.name.clone()).into_any_element(),
                cell(INDEXES[1]).child(ix.parts_label()).into_any_element(),
                cell(INDEXES[2])
                    .when(!plain, |d| d.text_color(p.tx3))
                    .child(ix.kind_label())
                    .into_any_element(),
            ];
            let label = if ix.primary {
                "Drop primary key".to_owned()
            } else {
                format!("Drop index {}", ix.name)
            };
            index_rows.push(row(
                Section::Indexes,
                i,
                "structure-index",
                cells,
                vec![(label, None)],
                cx,
            ));
        }
        let add_index = add("add-index", "Add index", "Add an index to the table", cx)
            .when(!busy, |b| {
                b.on_click(cx.listener(|this, _, window, cx| this.open_index_form(window, cx)))
            });
        let index_form = match (&s.form, &form_sql) {
            (Some(Form::Index(f)), Some(sql)) => Some(index_form(f, sql, busy, &p, cx)),
            _ => None,
        };
        let keys = |list: &[ForeignKey], back: bool, cx: &mut Context<QueryTab>| {
            let labels = if back {
                ["Name", "From", "To column", "On delete", "On update"]
            } else {
                ["Name", "Column", "References", "On delete", "On update"]
            };
            let mut rows = vec![header(&KEYS, &labels, &p).into_any_element()];
            let (section, id) = if back {
                (Section::ReferencedBy, "structure-referenced-by")
            } else {
                (Section::ForeignKeys, "structure-foreign-key")
            };
            for (i, k) in list.iter().enumerate() {
                let (target, linked, plain) = if back {
                    (
                        k.from.clone(),
                        reference(other(&k.from), &k.from_columns),
                        k.to_columns.join(", "),
                    )
                } else {
                    (
                        k.to.clone(),
                        reference(other(&k.to), &k.to_columns),
                        k.from_columns.join(", "),
                    )
                };
                let link_cell = cell(KEYS[if back { 1 } else { 2 }])
                    .child(link(id, i, linked, target.clone(), cx))
                    .into_any_element();
                let plain_cell = cell(KEYS[if back { 2 } else { 1 }])
                    .child(plain)
                    .into_any_element();
                let (second, third) = if back {
                    (link_cell, plain_cell)
                } else {
                    (plain_cell, link_cell)
                };
                let cells = vec![
                    cell(KEYS[0]).child(k.name.clone()).into_any_element(),
                    second,
                    third,
                    cell(KEYS[3])
                        .text_color(p.tx3)
                        .child(k.on_delete.clone())
                        .into_any_element(),
                    cell(KEYS[4])
                        .text_color(p.tx3)
                        .child(k.on_update.clone())
                        .into_any_element(),
                ];
                let mut menu = vec![(format!("Open {}", other(&target)), Some(target))];
                if !back {
                    menu.insert(0, (format!("Drop foreign key {}", k.name), None));
                }
                rows.push(row(section, i, id, cells, menu, cx));
            }
            rows
        };
        let fk_rows = keys(&d.foreign_keys, false, cx);
        let back_rows = keys(&d.referenced_by, true, cx);
        main = main
            .child(section_head("Indexes", Some(add_index)))
            .children(index_form)
            .child(grid_box("structure-indexes", index_rows, &p))
            .child(section_head("Foreign keys", None))
            .child(grid_box("structure-foreign-keys", fk_rows, &p))
            .child(section_head("Referenced by", None))
            .child(grid_box("structure-referenced", back_rows, &p));
        if !d.checks.is_empty() {
            let mut check_rows =
                vec![header(&INDEXES, &["Name", "Condition", "Enforced"], &p).into_any_element()];
            for (i, c) in d.checks.iter().enumerate() {
                let cells = vec![
                    cell(INDEXES[0]).child(c.name.clone()).into_any_element(),
                    cell(INDEXES[1])
                        .child(c.condition.clone())
                        .into_any_element(),
                    cell(INDEXES[2])
                        .text_color(p.tx3)
                        .child(if c.enabled { "yes" } else { "no" })
                        .into_any_element(),
                ];
                let menu = vec![(format!("Drop check constraint {}", c.name), None)];
                check_rows.push(row(Section::Checks, i, "structure-check", cells, menu, cx));
            }
            main = main
                .child(section_head("Check constraints", None))
                .child(grid_box("structure-checks", check_rows, &p));
        }
    }

    let copy = button(
        "copy-definition",
        ButtonKind::Outline,
        true,
        "Copy the definition",
        cx,
    )
    .child(if s.copied { "Copied" } else { "Copy" })
    .on_click(cx.listener(|this, _, _, cx| this.copy_definition(cx)));
    let aside = div()
        .flex_grow(1.)
        .flex_shrink(1.)
        .flex_basis(px(400.))
        .min_w_0()
        .border_l_1()
        .border_color(p.line)
        .bg(p.panel)
        .child(section_head("Definition", Some(copy)))
        .child(
            div()
                .id("structure-definition")
                .overflow_x_scroll()
                .px(px(16.))
                .pt(px(4.))
                .pb(px(20.))
                .font_family(MONO_FONT)
                .text_size(data_size())
                .line_height(px(21.))
                .whitespace_nowrap()
                .flex()
                // Not shrunk to the pane, so long lines scroll sideways.
                .child(div().flex_none().child(highlighted(&d.ddl, &p))),
        );
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(toolbar)
        .children(notes.into_iter().flatten())
        .child(
            div()
                .id("structure-page")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(div().flex().flex_wrap().child(main).child(aside)),
        )
        .into_any_element()
}

/// `customers.id`, or `orders (a, b)` for a composite key.
fn reference(table: String, columns: &[String]) -> String {
    match columns {
        [one] => format!("{table}.{one}"),
        many => format!("{table} ({})", many.join(", ")),
    }
}

/// The preview of a form's statement, or why there is none yet.
fn preview(sql: &Result<String, String>, p: &Palette) -> AnyElement {
    match sql {
        Ok(sql) => div()
            .id("form-sql")
            .overflow_x_scroll()
            .px(px(12.))
            .py(px(8.))
            .bg(p.bg)
            .border_1()
            .border_color(p.line2)
            .rounded(RADIUS)
            .font_family(MONO_FONT)
            .text_size(data_size())
            .line_height(px(20.))
            .whitespace_nowrap()
            .flex()
            .child(div().flex_none().child(highlighted(&format!("{sql};"), p)))
            .into_any_element(),
        Err(e) => div()
            .text_size(small_size())
            .text_color(p.tx3)
            .child(format!("To finish: {e}."))
            .into_any_element(),
    }
}

/// The box a form sits in, under its section's title, with Cancel and Apply.
fn form_box(
    fields: Vec<AnyElement>,
    sql: &Result<String, String>,
    busy: bool,
    p: &Palette,
    cx: &mut Context<QueryTab>,
) -> Div {
    let ready = sql.is_ok() && !busy;
    div()
        .mx(px(16.))
        .mb(px(12.))
        .p(px(12.))
        .flex()
        .flex_col()
        .gap(px(12.))
        .border_1()
        .border_color(p.line2)
        .rounded(RADIUS)
        .children(fields)
        .child(preview(sql, p))
        .child(
            div()
                .flex()
                .justify_end()
                .gap(px(8.))
                .child(
                    button(
                        "form-cancel",
                        ButtonKind::Outline,
                        true,
                        "Close the form",
                        cx,
                    )
                    .child("Cancel")
                    .on_click(cx.listener(|this, _, _, cx| this.close_form(cx))),
                )
                .child(
                    button(
                        "form-apply",
                        ButtonKind::Primary,
                        ready,
                        "Run the statement on the tab's session",
                        cx,
                    )
                    .child("Apply")
                    .when(ready, |b| {
                        b.on_click(cx.listener(|this, _, window, cx| this.apply_form(window, cx)))
                    }),
                ),
        )
}

fn fields_row(children: Vec<AnyElement>) -> AnyElement {
    div()
        .flex()
        .flex_wrap()
        .gap_x(px(12.))
        .gap_y(px(10.))
        .children(children)
        .into_any_element()
}

fn column_form(
    f: &ColumnForm,
    sql: &Result<String, String>,
    busy: bool,
    p: &Palette,
    cx: &mut Context<QueryTab>,
) -> Div {
    let toggle =
        |id: &'static str, label: &'static str, on: bool, set: fn(&mut ColumnForm, bool)| {
            Checkbox::new(id)
                .checked(on)
                .label(label)
                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                    if let Some(s) = this.structure_mut()
                        && let Some(Form::Column(f)) = &mut s.form
                    {
                        set(f, *checked);
                    }
                    cx.notify();
                }))
                .into_any_element()
        };
    let fields = vec![
        fields_row(vec![
            labeled("Name", text_field(&f.name, true), 160., p).into_any_element(),
            labeled("Type", Select::new(&f.data_type).h(row_height()), 160., p).into_any_element(),
            labeled("Length or members", text_field(&f.length, true), 120., p).into_any_element(),
        ]),
        fields_row(vec![
            labeled("Default", text_field(&f.default, true), 220., p).into_any_element(),
            labeled(
                "Position",
                Select::new(&f.position).h(row_height()),
                160.,
                p,
            )
            .into_any_element(),
        ]),
        fields_row(vec![
            labeled("Comment", text_field(&f.comment, false), 220., p).into_any_element(),
        ]),
        div()
            .flex()
            .flex_wrap()
            .gap(px(16.))
            .child(toggle("column-nullable", "Nullable", f.nullable, |f, v| {
                f.nullable = v
            }))
            .child(toggle(
                "column-auto-increment",
                "Auto increment",
                f.auto_increment,
                |f, v| f.auto_increment = v,
            ))
            .into_any_element(),
    ];
    form_box(fields, sql, busy, p, cx)
}

fn index_form(
    f: &IndexForm,
    sql: &Result<String, String>,
    busy: bool,
    p: &Palette,
    cx: &mut Context<QueryTab>,
) -> Div {
    let parts = f.parts.iter().enumerate().map(|(i, part)| {
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .font_family(MONO_FONT)
                    .text_size(data_size())
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(part.column.clone()),
            )
            .child(
                button_at("part-order", i, cx)
                    .child(if part.descending { "desc" } else { "asc" })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(s) = this.structure_mut()
                            && let Some(Form::Index(f)) = &mut s.form
                            && let Some(part) = f.parts.get_mut(i)
                        {
                            part.descending = !part.descending;
                        }
                        cx.notify();
                    })),
            )
            .child(div().w(px(90.)).child(text_field(&part.prefix, true)))
            .child(
                button_at("part-remove", i, cx)
                    .child("Remove")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(s) = this.structure_mut()
                            && let Some(Form::Index(f)) = &mut s.form
                            && i < f.parts.len()
                        {
                            f.parts.remove(i);
                        }
                        cx.notify();
                    })),
            )
            .into_any_element()
    });
    let fields = vec![
        fields_row(vec![
            labeled("Name", text_field(&f.name, true), 200., p).into_any_element(),
            labeled("Kind", Select::new(&f.kind).h(row_height()), 160., p).into_any_element(),
        ]),
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .text_size(small_size())
                    .text_color(p.tx2)
                    .child("Columns, in order"),
            )
            .children(parts)
            .child(
                div().max_w(px(280.)).child(
                    Select::new(&f.add)
                        .h(row_height())
                        .placeholder("Add a column…"),
                ),
            )
            .into_any_element(),
    ];
    form_box(fields, sql, busy, p, cx)
}

/// An outline button for row `i` of the index form's column list.
fn button_at(id: &'static str, i: usize, cx: &App) -> Stateful<Div> {
    let p = cx.global::<Palette>();
    div()
        .id((id, i))
        .flex()
        .flex_none()
        .items_center()
        .h(row_height())
        .px(px(8.))
        .border_1()
        .border_color(p.line2)
        .rounded(RADIUS)
        .cursor_pointer()
        .hover(|s| s.bg(p.hover))
}
