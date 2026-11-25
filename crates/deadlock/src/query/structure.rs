//! A table tab's Structure view: columns, indexes, foreign keys both ways, checks and
//! the server's definition, with Add column, Edit column, Move, Add index, Add foreign
//! key, Add check and drop actions. Each change runs on the tab's session (Oracle may need several
//! statements), behind the same prompts as a run. The table's own Rename, Truncate
//! and Drop (from the sidebar's and the tab's menus) ask in their own dialog. New
//! table… opens the tab as a draft: column rows, primary key and Create.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use deadlock_core::connection::Environment;
use deadlock_core::driver::{DriverError, NodeKind, ObjectPath, Value};
use deadlock_core::edit::{self, TableRef};
use deadlock_core::structure::{
    self, ForeignKey, Generated, IndexKind, IndexPart, NewColumn, NewForeignKey, NewIndex,
    NewTable, Position, StructureColumn, TableStructure, size_label,
};
use gpui_kit::component::IndexPath;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectItem, SelectState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, Div, ElementId, Entity,
    EntityId, FontWeight, Hsla, InteractiveElement, IntoElement, MouseButton, ParentElement,
    SharedString, Stateful, StatefulInteractiveElement, Styled, Subscription, Window, div, px,
};

use super::table::{View, highlighted, wrap_sql};
use super::{QueryTab, Session, TabEvent};
use crate::app::Db;
use crate::assets::{Icon, icon, menu_item, menu_item_danger, menu_item_disabled, menu_item_hint};
use crate::theme::{MONO_FONT, Palette, RADIUS, data_size, row_height, small_size};
use crate::ui::Clickable as _;
use crate::ui::{Kind as ButtonKind, button, icon_button, warning_band, warning_line};

/// The server's collations per connection id, read once per connection
/// ([`deadlock_core::driver::Driver::collations`]).
#[derive(Default)]
pub(crate) struct Collations(HashMap<String, Rc<Vec<(String, String)>>>);

impl gpui_kit::Global for Collations {}

/// The lists, for the selected row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Columns,
    Indexes,
    ForeignKeys,
    ReferencedBy,
    Checks,
}

type Choice = Entity<SelectState<SearchableVec<String>>>;

struct ColumnForm {
    name: Entity<InputState>,
    data_type: Choice,
    length: Entity<InputState>,
    default: Entity<InputState>,
    comment: Entity<InputState>,
    /// MySQL / MariaDB text types: the column's character set's collations; none
    /// picked for the table default.
    collation: Choice,
    /// Generated columns only, in place of Default.
    expression: Entity<InputState>,
    /// Add: `At the end`, `First`, then `After <column>`; edit: `First`, then
    /// `After <column>` for the other columns.
    position: Choice,
    /// What each choice of `position` means.
    positions: Vec<Position>,
    nullable: bool,
    auto_increment: bool,
    on_update: bool,
    /// The column as it was; `None` for Add column.
    editing: Option<StructureColumn>,
}

/// What a row's menu item does.
#[derive(Clone)]
enum RowAction {
    Open(TableRef),
    Edit,
    MoveUp,
    MoveDown,
    Drop,
}

/// The forced reason of a type or length change on a table that may have rows.
const MAY_TRUNCATE: &str = "Existing values may be truncated or rejected.";

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

struct FkForm {
    name: Entity<InputState>,
    /// The schema's tables are still loading.
    loading: bool,
    /// `ref_table`'s choices: the schema's tables, then other schemas' as
    /// `schema.table`.
    tables: Vec<TableRef>,
    ref_table: Choice,
    /// The referenced table's structure, once loaded.
    target: Option<TableStructure>,
    /// This table's columns in order, each with its pick of the column it references.
    pairs: Vec<(String, Entity<SelectState<SearchableVec<RefColumn>>>)>,
    /// Picks the next column to add.
    add: Choice,
    on_delete: Choice,
    /// MySQL and MariaDB: Oracle has no on update.
    on_update: Option<Choice>,
}

struct CheckForm {
    name: Entity<InputState>,
    condition: Entity<InputState>,
}

/// A row of the New table draft: the Column form's fields.
struct DraftColumn {
    form: ColumnForm,
}

impl DraftColumn {
    /// Stays with the row as it moves: what [`Draft::key`] lists.
    fn id(&self) -> EntityId {
        self.form.name.entity_id()
    }
}

/// The New table draft: the table's fields, its column rows in column order and the
/// primary key picker.
struct Draft {
    name: Entity<InputState>,
    /// MySQL and MariaDB.
    engine: Option<Choice>,
    /// MySQL and MariaDB; none picked for the schema default.
    collation: Choice,
    comment: Entity<InputState>,
    columns: Vec<DraftColumn>,
    /// The primary key's rows ([`DraftColumn::id`]) in key order, which the rows'
    /// order does not change.
    key: Vec<EntityId>,
    /// Picks the next primary key column from the named rows not in it.
    key_add: Choice,
    mariadb: bool,
    /// MySQL / MariaDB: whether the session reads `\` in strings as an escape (its
    /// `sql_mode`); `None` until read, and Create waits for it.
    backslash: Option<bool>,
}

/// Which Column form a checkbox writes to: the open form or a draft row.
#[derive(Clone, Copy)]
enum FormAt {
    Form,
    Draft(usize),
}

fn column_form_mut(s: &mut Structure, at: FormAt) -> Option<&mut ColumnForm> {
    match at {
        FormAt::Form => match s.form.as_mut()? {
            Form::Column(f) => Some(f),
            _ => None,
        },
        FormAt::Draft(i) => Some(&mut s.draft.as_mut()?.columns.get_mut(i)?.form),
    }
}

enum Form {
    Column(Box<ColumnForm>),
    Index(IndexForm),
    ForeignKey(Box<FkForm>),
    Check(CheckForm),
}

/// Rename…, Truncate… or Drop table… from a table's menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TableAction {
    Rename,
    Truncate,
    Drop,
}

/// What follows a change that ran.
enum After {
    /// The structure, the Data view and the sidebar reload.
    Reload,
    /// The table has this name now: the tab follows it, then [`After::Reload`].
    Renamed(String),
    /// The table is gone: the sidebar reloads and the tab closes.
    Dropped,
    /// The draft is a table now, by this name: the tab becomes its tab.
    Created(String),
}

#[derive(Default)]
pub(crate) struct Structure {
    data: Option<TableStructure>,
    busy: bool,
    /// A load error, or the outcome of the last ALTER; `true` for an error.
    notice: Option<(String, bool)>,
    selected: Option<(Section, usize)>,
    /// A column to select once the structure loads: Search everywhere opened it.
    focus: Option<String>,
    form: Option<Form>,
    /// New table…: nothing to load until Create.
    draft: Option<Draft>,
    /// A table action to ask once the structure loads.
    then: Option<TableAction>,
    /// Some statements of the open form ran: it edits the column as it reloads.
    rebase: bool,
    /// The connection's collations are being read.
    collations_loading: bool,
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

    pub(crate) fn is_draft(&self) -> bool {
        self.draft.is_some()
    }
}

/// A Column form `c` holds typing: a change to the edited column `before`, or a name
/// for a new one.
fn column_typed(c: &NewColumn, before: Option<&NewColumn>) -> bool {
    before.map_or(!c.name.trim().is_empty(), |b| b != c)
}

/// Whether a key of `d`, or a key pointing at it, names table `t`.
fn keys_name(d: &TableStructure, t: &TableRef) -> bool {
    d.foreign_keys.iter().any(|k| k.to == *t) || d.referenced_by.iter().any(|k| k.from == *t)
}

/// An Add index, foreign key or check form holds typing: a field with text, or a pick.
fn form_typed(texts: &[String], picked: bool) -> bool {
    picked || texts.iter().any(|t| !t.trim().is_empty())
}

/// The Columns row of column `name`, if it has one.
fn column_row(data: &TableStructure, name: Option<String>) -> Option<(Section, usize)> {
    let name = name?;
    let i = data.columns.iter().position(|c| c.name == name)?;
    Some((Section::Columns, i))
}

/// A Column form as a [`NewColumn`]; `before` is the edited column as it was.
fn column_of(f: &ColumnForm, before: Option<&NewColumn>, oracle: bool, cx: &App) -> NewColumn {
    let text = |i: &Entity<InputState>| i.read(cx).value().to_string();
    let choice = |c: &Choice| c.read(cx).selected_index(cx).map(|ix| ix.row);
    // By value: a search filters the list, so its index no longer points into `types`.
    let data_type = f
        .data_type
        .read(cx)
        .selected_value()
        .cloned()
        .unwrap_or_default();
    let collation = f.collation.read(cx).selected_value().cloned();
    NewColumn {
        name: text(&f.name),
        length: text(&f.length),
        nullable: f.nullable,
        default: text(&f.default),
        auto_increment: f.auto_increment,
        comment: text(&f.comment),
        position: choice(&f.position)
            .and_then(|i| f.positions.get(i).cloned())
            .unwrap_or_default(),
        // The checkbox shows for these types only.
        on_update: f.on_update && !oracle && structure::is_on_update_type(&data_type),
        collation,
        generated: before.and_then(|b| b.generated.clone()).map(|g| Generated {
            expression: text(&f.expression),
            ..g
        }),
        data_type,
        // `invisible` has no field: it stays as the column has it.
        ..before.cloned().unwrap_or_default()
    }
}

/// Whether the New table grid offers Auto increment (Identity on Oracle): integers.
fn takes_auto_increment(data_type: &str) -> bool {
    matches!(
        data_type.split_whitespace().next(),
        Some("tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "number")
    )
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

/// The referential actions the Add foreign key form offers.
fn fk_actions(oracle: bool) -> &'static [&'static str] {
    if oracle {
        &structure::ORACLE_FK_ACTIONS
    } else {
        &structure::FK_ACTIONS
    }
}

/// A Collation list: searchable, cleared back to the default.
fn collation_select(
    items: Vec<String>,
    current: Option<&str>,
    window: &mut Window,
    cx: &mut Context<QueryTab>,
) -> Choice {
    let at = current
        .and_then(|c| items.iter().position(|i| i == c))
        .map(|r| IndexPath::default().row(r));
    cx.new(|cx| SelectState::new(SearchableVec::new(items), at, window, cx).searchable(true))
}

fn collation_field(choice: &Choice, default: &'static str) -> Select<SearchableVec<String>> {
    Select::new(choice)
        .h(row_height())
        .placeholder(default)
        .cleanable(true)
}

/// A Collation list's items: the collations of `charset` (all of them when `None`),
/// and `current` when the server's list lacks it, so a value is never lost.
fn collation_choices(
    list: &[(String, String)],
    charset: Option<&str>,
    current: Option<&str>,
) -> Vec<String> {
    let mut out: Vec<String> = list
        .iter()
        .filter(|(_, cs)| charset.is_none_or(|c| c.eq_ignore_ascii_case(cs)))
        .map(|(n, _)| n.clone())
        .collect();
    if let Some(c) = current.filter(|c| !out.iter().any(|n| n == c)) {
        out.insert(0, c.to_owned());
    }
    out
}

/// The character set of `collation`, from the server's list.
fn charset_of<'a>(list: &'a [(String, String)], collation: &str) -> Option<&'a str> {
    list.iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(collation))
        .map(|(_, cs)| cs.as_str())
}

/// The primary key's column names in key order: `key` lists row ids, `rows` each
/// row's id and name in row order.
fn key_names<T: PartialEq>(rows: &[(T, String)], key: &[T]) -> Vec<String> {
    key.iter()
        .filter_map(|k| rows.iter().find(|(id, _)| id == k).map(|(_, n)| n.clone()))
        .collect()
}

/// Up / down in the primary key list: entry `k` trades places with its neighbour.
fn move_key<T>(key: &mut [T], k: usize, up: bool) {
    match up {
        true if k > 0 && k < key.len() => key.swap(k - 1, k),
        false if k + 1 < key.len() => key.swap(k, k + 1),
        _ => {}
    }
}

/// A References table choice: `orders` in the table's own schema, else `shop.orders`.
fn table_label(t: &TableRef, schema: &str) -> String {
    if t.database == schema {
        t.table.clone()
    } else {
        format!("{}.{}", t.database, t.table)
    }
}

/// A referenced-column choice: the name, then its type and key dimmed
/// (`id bigint unsigned · PK`).
#[derive(Clone)]
struct RefColumn {
    name: String,
    /// `bigint unsigned · PK`.
    detail: String,
    /// The detail's color: items draw without the palette.
    dim: Hsla,
}

impl RefColumn {
    fn label(&self) -> AnyElement {
        div()
            .flex()
            .gap(px(6.))
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .child(self.name.clone())
            .child(div().text_color(self.dim).child(self.detail.clone()))
            .into_any_element()
    }
}

impl SelectItem for RefColumn {
    type Value = String;

    fn title(&self) -> SharedString {
        format!("{} {}", self.name, self.detail).into()
    }

    fn display_title(&self) -> Option<AnyElement> {
        Some(self.label())
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        self.label()
    }

    fn value(&self) -> &String {
        &self.name
    }
}

/// The referenced-column choices of `target`.
fn ref_columns(target: &TableStructure, dim: Hsla) -> Vec<RefColumn> {
    target
        .columns
        .iter()
        .map(|c| RefColumn {
            name: c.name.clone(),
            detail: format!(
                "{}{}",
                c.column_type,
                if c.primary_key { " · PK" } else { "" }
            ),
            dim,
        })
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
        let window = self.window;
        cx.spawn(async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err("the structure task stopped".into()));
            let then = this.update(cx, |this, cx| {
                let s = this.structure_mut()?;
                if s.generation != generation {
                    return None;
                }
                s.busy = false;
                let then = s.then.take();
                let loaded = result.is_ok();
                let rebase = std::mem::take(&mut s.rebase);
                match result {
                    Ok(data) => {
                        // The selected row may be gone after an ALTER.
                        s.selected = column_row(&data, s.focus.take());
                        s.data = Some(data);
                        if rebase {
                            rebase_form(s, cx);
                        }
                    }
                    Err(e) => {
                        s.notice = Some((e, true));
                        if rebase {
                            // It knew the column as it was: Apply would be wrong.
                            s.form = None;
                            s._subs.clear();
                        }
                    }
                }
                cx.emit(TabEvent::Changed);
                cx.notify();
                then.filter(|_| loaded)
            });
            if let Ok(Some(action)) = then {
                let _ = cx.update_window(window, |_, window, cx| {
                    this.update(cx, |this, cx| this.ask_table_action(action, window, cx))
                });
            }
        })
        .detach();
        cx.notify();
    }

    /// Table `other` of connection `conn` was renamed or dropped: a loaded Structure
    /// whose keys point to or from it reads itself again, so no key names a table that
    /// is gone.
    pub(crate) fn other_table_changed(
        &mut self,
        conn: &str,
        other: &TableRef,
        cx: &mut Context<Self>,
    ) {
        let stale = self.connection.as_ref().is_some_and(|c| c.id == conn)
            && self.table.as_ref().is_some_and(|t| {
                t.table != *other
                    && t.structure
                        .data
                        .as_ref()
                        .is_some_and(|d| keys_name(d, other))
            });
        if stale {
            self.load_structure(cx);
        }
    }

    /// Selects column `name` in the Structure view, now or once it loads.
    pub(crate) fn focus_column(&mut self, name: String, cx: &mut Context<Self>) {
        if let Some(s) = self.structure_mut() {
            match &s.data {
                Some(data) => s.selected = column_row(data, Some(name)),
                None => s.focus = Some(name),
            }
        }
        cx.notify();
    }

    fn select_row(&mut self, section: Section, i: usize, cx: &mut Context<Self>) {
        if let Some(s) = self.structure_mut() {
            s.selected = Some((section, i));
        }
        cx.notify();
    }

    /// Copy: the server's definition, or a draft's statements.
    fn copy_definition(&mut self, cx: &mut Context<Self>) {
        let ddl = match self.draft_sql(cx) {
            Some(sql) => sql.ok().map(|sql| format!("{};", sql.join(";\n"))),
            None => self
                .table
                .as_ref()
                .and_then(|t| Some(t.structure.data.as_ref()?.ddl.clone())),
        };
        let (Some(ddl), Some(s)) = (ddl, self.structure_mut()) else {
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
        self.show_column_form(None, window, cx);
    }

    /// Edit column…: the Column form prefilled from column `i`.
    fn open_edit_column(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let column = self
            .table
            .as_ref()
            .and_then(|t| t.structure.data.as_ref()?.columns.get(i).cloned());
        if let Some(c) = column {
            self.select_row(Section::Columns, i, cx);
            self.show_column_form(Some(c), window, cx);
        }
    }

    fn show_column_form(
        &mut self,
        editing: Option<StructureColumn>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((form, subs)) = self.new_column_form(editing, window, cx) else {
            return;
        };
        form.name.update(cx, |n, cx| n.focus(window, cx));
        if let Some(s) = self.structure_mut() {
            s.form = Some(Form::Column(Box::new(form)));
            s._subs = subs;
            s.notice = None;
            s.rebase = false;
        }
        cx.notify();
    }

    /// The Column form, empty for Add column or prefilled from `editing`. A draft's
    /// rows have no position (the row order is the column order) and no type yet.
    fn new_column_form(
        &mut self,
        editing: Option<StructureColumn>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(ColumnForm, Vec<Subscription>)> {
        let t = self.table.as_ref()?;
        let draft = t.structure.draft.as_ref().map(|d| d.mariadb);
        let empty = TableStructure::default();
        let data = match draft {
            Some(_) => &empty,
            None => t.structure.data.as_ref()?,
        };
        let before = editing.as_ref().map_or_else(
            || NewColumn {
                nullable: true,
                ..NewColumn::default()
            },
            |c| NewColumn::from_existing(c, data),
        );
        let others = data
            .columns
            .iter()
            .filter(|c| editing.as_ref().is_none_or(|e| e.name != c.name))
            .map(|c| Position::After(c.name.clone()));
        let positions: Vec<Position> = if draft.is_some() {
            Vec::new()
        } else if editing.is_some() {
            std::iter::once(Position::First).chain(others).collect()
        } else {
            [Position::Last, Position::First]
                .into_iter()
                .chain(others)
                .collect()
        };
        let labels: Vec<String> = positions
            .iter()
            .map(|p| match p {
                Position::Last => "At the end".to_owned(),
                Position::First => "First".to_owned(),
                Position::After(c) => format!("After {c}"),
            })
            .collect();
        let at = positions
            .iter()
            .position(|p| *p == before.position)
            .unwrap_or(0);
        let mut types = types(draft.unwrap_or_else(|| self.mariadb()), self.oracle());
        let ty = if editing.is_some() {
            Some(
                types
                    .iter()
                    .position(|t| *t == before.data_type)
                    .unwrap_or_else(|| {
                        // A type the list does not offer stays, as free text.
                        types.push(before.data_type.clone());
                        types.len() - 1
                    }),
            )
        } else if draft.is_some() {
            None
        } else {
            types
                .iter()
                .position(|t| t == "varchar" || t == "varchar2")
                .or(Some(0))
        };
        let input =
            |placeholder: &str, value: &str, window: &mut Window, cx: &mut Context<Self>| {
                let (placeholder, value) = (placeholder.to_owned(), value.to_owned());
                cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder(placeholder)
                        .default_value(value)
                })
            };
        let name = input("name", &before.name, window, cx);
        let length = input("255 or 10,2", &before.length, window, cx);
        let default = input(
            "none, NULL, a value, CURRENT_TIMESTAMP or (expr)",
            &before.default,
            window,
            cx,
        );
        let comment = input("", &before.comment, window, cx);
        let current = before.collation.clone();
        let items = self.collation_items(editing.as_ref(), current.as_deref(), cx);
        let collation = collation_select(items, current.as_deref(), window, cx);
        self.ensure_collations(cx);
        let expression = input(
            "(expression)",
            before
                .generated
                .as_ref()
                .map(|g| g.expression.as_str())
                .unwrap_or_default(),
            window,
            cx,
        );
        // The Type list: the engine's types, plus the edited column's when missing.
        let data_type = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(types),
                ty.map(|r| IndexPath::default().row(r)),
                window,
                cx,
            )
            .searchable(true)
        });
        let at = (!labels.is_empty()).then(|| IndexPath::default().row(at));
        let position = cx.new(|cx| SelectState::new(SearchableVec::new(labels), at, window, cx));
        let subs = self.form_subs(
            &[&name, &length, &default, &comment, &expression],
            &[&data_type, &position, &collation],
            cx,
        );
        Some((
            ColumnForm {
                name,
                data_type,
                length,
                default,
                comment,
                collation,
                expression,
                position,
                positions,
                nullable: before.nullable,
                auto_increment: before.auto_increment,
                on_update: before.on_update,
                editing,
            },
            subs,
        ))
    }

    fn open_index_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(columns) = self.table.as_ref().and_then(|t| {
            let d = t.structure.data.as_ref()?;
            Some(d.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>())
        }) else {
            return;
        };
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("name, or empty"));
        let kinds: Vec<String> = index_kinds(self.oracle())
            .iter()
            .map(|k| k.label().to_owned())
            .collect();
        let kind = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(kinds),
                Some(IndexPath::default().row(0)),
                window,
                cx,
            )
        });
        let add = cx.new(|cx| {
            SelectState::new(SearchableVec::new(columns), None, window, cx).searchable(true)
        });
        let mut subs = self.form_subs(&[&name], &[&kind], cx);
        subs.push(cx.subscribe_in(
            &add,
            window,
            |this, add, e: &SelectEvent<SearchableVec<String>>, window, cx| {
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

    fn open_check_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("name, or empty"));
        let condition = cx.new(|cx| InputState::new(window, cx).placeholder("qty >= 0"));
        let subs = self.form_subs(&[&name, &condition], &[], cx);
        name.update(cx, |n, cx| n.focus(window, cx));
        if let Some(s) = self.structure_mut() {
            s.form = Some(Form::Check(CheckForm { name, condition }));
            s._subs = subs;
            s.notice = None;
        }
        cx.notify();
    }

    fn fk_form(&self) -> Option<&FkForm> {
        match self.table.as_ref()?.structure.form.as_ref()? {
            Form::ForeignKey(f) => Some(f),
            _ => None,
        }
    }

    fn fk_form_mut(&mut self) -> Option<&mut FkForm> {
        match self.structure_mut()?.form.as_mut()? {
            Form::ForeignKey(f) => Some(f),
            _ => None,
        }
    }

    /// Add foreign key: the form, then the schema's tables, which the shell answers
    /// with [`QueryTab::reference_tables`].
    fn open_fk_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((schema, columns)) = self.table.as_ref().and_then(|t| {
            let d = t.structure.data.as_ref()?;
            let columns: Vec<String> = d.columns.iter().map(|c| c.name.clone()).collect();
            Some((t.table.database.clone(), columns))
        }) else {
            return;
        };
        let oracle = self.oracle();
        let actions = |list: &[&str]| list.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
        let first = Some(IndexPath::default().row(0));
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("name, or empty"));
        let ref_table = cx.new(|cx| {
            SelectState::new(SearchableVec::new(Vec::new()), None, window, cx).searchable(true)
        });
        let add = cx.new(|cx| {
            SelectState::new(SearchableVec::new(columns), None, window, cx).searchable(true)
        });
        let on_delete = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(actions(fk_actions(oracle))),
                first,
                window,
                cx,
            )
        });
        let on_update = (!oracle).then(|| {
            cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(actions(&structure::FK_ACTIONS)),
                    first,
                    window,
                    cx,
                )
            })
        });
        let mut choices = vec![&ref_table, &on_delete];
        choices.extend(on_update.as_ref());
        let mut subs = self.form_subs(&[&name], &choices, cx);
        subs.push(cx.subscribe_in(
            &ref_table,
            window,
            |this, _, e: &SelectEvent<SearchableVec<String>>, window, cx| {
                if let SelectEvent::Confirm(Some(table)) = e {
                    this.load_target(table.clone(), window, cx);
                }
            },
        ));
        subs.push(cx.subscribe_in(
            &add,
            window,
            |this, add, e: &SelectEvent<SearchableVec<String>>, window, cx| {
                let SelectEvent::Confirm(Some(column)) = e else {
                    return;
                };
                let dim = cx.global::<Palette>().tx3;
                let items = this
                    .fk_form()
                    .and_then(|f| f.target.as_ref())
                    .map(|t| ref_columns(t, dim))
                    .unwrap_or_default();
                let pick = cx.new(|cx| {
                    SelectState::new(SearchableVec::new(items), None, window, cx).searchable(true)
                });
                let sub = cx.observe(&pick, |_, _, cx| cx.notify());
                if let Some(s) = this.structure_mut()
                    && let Some(Form::ForeignKey(f)) = &mut s.form
                {
                    f.pairs.push((column.clone(), pick));
                    s._subs.push(sub);
                }
                add.update(cx, |a, cx| a.set_selected_index(None, window, cx));
                cx.notify();
            },
        ));
        name.update(cx, |n, cx| n.focus(window, cx));
        if let Some(s) = self.structure_mut() {
            s.form = Some(Form::ForeignKey(Box::new(FkForm {
                name,
                loading: true,
                tables: Vec::new(),
                ref_table,
                target: None,
                pairs: Vec::new(),
                add,
                on_delete,
                on_update,
            })));
            s._subs = subs;
            s.notice = None;
        }
        cx.emit(TabEvent::NeedTables(schema));
        cx.notify();
    }

    /// The Add foreign key form's tables: the schema's own, read again from the
    /// server each time, then `others` (other schemas' tables from the sidebar).
    pub(crate) fn reference_tables(
        &mut self,
        others: Vec<TableRef>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Session::Ready(driver), Some(t)) = (&self.session, &self.table) else {
            return;
        };
        let database = t.table.database.clone();
        let driver = Arc::clone(driver);
        let path = ObjectPath(vec![t.table.database.clone(), "Tables".into()]);
        let rx = cx.global::<Db>().0.spawn(async move {
            driver
                .children(&path)
                .await
                .map_err(|e: DriverError| e.to_string())
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err("the tables task stopped".into()));
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(nodes) => {
                    let mut tables: Vec<TableRef> = nodes
                        .into_iter()
                        .filter(|n| n.kind == NodeKind::Table)
                        .map(|n| TableRef {
                            database: database.clone(),
                            table: n.name,
                        })
                        .collect();
                    tables.extend(others);
                    this.fill_tables(tables, window, cx);
                }
                Err(e) => this.structure_notice(e, cx),
            });
        })
        .detach();
    }

    /// The table list changed (a schema reloaded): an open Add foreign key form asks
    /// for it again.
    pub(crate) fn tables_changed(&mut self, cx: &mut Context<Self>) {
        if let (Some(_), Some(t)) = (self.fk_form(), &self.table) {
            cx.emit(TabEvent::NeedTables(t.table.database.clone()));
        }
    }

    fn fill_tables(&mut self, tables: Vec<TableRef>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(schema) = self.table.as_ref().map(|t| t.table.database.clone()) else {
            return;
        };
        let Some(f) = self.fk_form_mut() else { return };
        let labels: Vec<String> = tables.iter().map(|t| table_label(t, &schema)).collect();
        // The pick is kept by its label, so a refresh leaves it as it was.
        f.ref_table.update(cx, |c, cx| {
            c.set_items(SearchableVec::new(labels), window, cx)
        });
        f.tables = tables;
        f.loading = false;
        cx.notify();
    }

    /// The Add foreign key form's referenced table, as picked.
    fn picked_table(&self, cx: &App) -> Option<TableRef> {
        let schema = &self.table.as_ref()?.table.database;
        let f = self.fk_form()?;
        let label = f.ref_table.read(cx).selected_value()?;
        f.tables
            .iter()
            .find(|t| table_label(t, schema) == *label)
            .cloned()
    }

    fn structure_notice(&mut self, e: String, cx: &mut Context<Self>) {
        if let Some(s) = self.structure_mut() {
            s.notice = Some((e, true));
        }
        cx.notify();
    }

    /// A referenced table was picked: its structure (this table's own when it points
    /// at itself) fills the column pickers.
    fn load_target(&mut self, table: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.picked_table(cx) else {
            return;
        };
        let Some(t) = &self.table else { return };
        let own = if target == t.table {
            t.structure.data.clone()
        } else {
            None
        };
        let loaded = own.is_some();
        self.set_target(own, window, cx);
        let Session::Ready(driver) = &self.session else {
            return;
        };
        if loaded {
            return;
        }
        let driver = Arc::clone(driver);
        let rx = cx.global::<Db>().0.spawn(async move {
            driver
                .structure(&target)
                .await
                .map_err(|e: DriverError| e.to_string())
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err("the structure task stopped".into()));
            let _ = this.update_in(cx, |this, window, cx| {
                // A later pick wins.
                let picked = this
                    .fk_form()
                    .and_then(|f| f.ref_table.read(cx).selected_value().cloned());
                if picked.as_ref() != Some(&table) {
                    return;
                }
                match result {
                    Ok(s) => this.set_target(Some(s), window, cx),
                    Err(e) => this.structure_notice(e, cx),
                }
            });
        })
        .detach();
    }

    /// The referenced table's structure, or `None` while it loads: every pair's pick
    /// starts over.
    fn set_target(
        &mut self,
        target: Option<TableStructure>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dim = cx.global::<Palette>().tx3;
        let items = target
            .as_ref()
            .map(|t| ref_columns(t, dim))
            .unwrap_or_default();
        let Some(f) = self.fk_form_mut() else { return };
        for (_, pick) in &f.pairs {
            pick.update(cx, |c, cx| {
                c.set_items(SearchableVec::new(items.clone()), window, cx);
                c.set_selected_index(None, window, cx);
            });
        }
        f.target = target;
        cx.notify();
    }

    /// The Add foreign key form as a [`NewForeignKey`]. Referenced columns not picked
    /// yet are left out, so the counts differ until each pair has one.
    fn fk_draft(&self, cx: &App) -> Option<NewForeignKey> {
        let t = self.table.as_ref()?;
        let f = self.fk_form()?;
        let row = |c: &Choice| c.read(cx).selected_index(cx).map(|ix| ix.row);
        let actions = fk_actions(t.structure.data.as_ref()?.oracle);
        let action = |c: &Choice, list: &[&str]| {
            row(c)
                .and_then(|i| list.get(i))
                .map(|a| (*a).to_owned())
                .unwrap_or_default()
        };
        let ref_columns = f
            .pairs
            .iter()
            .filter_map(|(_, c)| c.read(cx).selected_value().cloned())
            .collect();
        Some(NewForeignKey {
            name: f.name.read(cx).value().to_string(),
            columns: f.pairs.iter().map(|(c, _)| c.clone()).collect(),
            ref_table: self.picked_table(cx).unwrap_or_else(|| TableRef {
                database: t.table.database.clone(),
                table: String::new(),
            }),
            ref_columns,
            on_delete: action(&f.on_delete, actions),
            on_update: f
                .on_update
                .as_ref()
                .map(|c| action(c, &structure::FK_ACTIONS))
                .unwrap_or_default(),
        })
    }

    /// What the server will likely refuse in the Add foreign key form, once every
    /// pair has its referenced column (before that the lists do not line up).
    fn fk_hints(&self, cx: &App) -> Vec<String> {
        let target = self.fk_form().and_then(|f| f.target.as_ref());
        let data = self.table.as_ref().and_then(|t| t.structure.data.as_ref());
        match (self.fk_draft(cx), data, target) {
            (Some(k), Some(from), Some(to)) if k.ref_columns.len() == k.columns.len() => {
                structure::foreign_key_hints(&k, from, to)
            }
            _ => Vec::new(),
        }
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
            s.rebase = false;
        }
        cx.notify();
    }

    /// New table…: the tab becomes a draft on its Structure view, with one empty
    /// column row; nothing loads until Create. `mariadb` picks the type and engine lists.
    pub(crate) fn start_draft(
        &mut self,
        mariadb: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            let placeholder = placeholder.to_owned();
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let name = input("table name", window, cx);
        let items = self.collation_items(None, None, cx);
        let collation = collation_select(items, None, window, cx);
        let comment = input("", window, cx);
        let engine = (!self.oracle()).then(|| {
            let engines: Vec<String> = structure::table_engines(mariadb)
                .into_iter()
                .map(str::to_owned)
                .collect();
            // InnoDB, the server's own default.
            cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(engines),
                    Some(IndexPath::default().row(0)),
                    window,
                    cx,
                )
            })
        });
        let key_add = cx
            .new(|cx| SelectState::new(SearchableVec::new(Vec::<String>::new()), None, window, cx));
        let mut choices: Vec<&Choice> = engine.iter().collect();
        choices.push(&collation);
        let mut subs = self.form_subs(&[&name, &comment], &choices, cx);
        subs.push(cx.subscribe_in(
            &key_add,
            window,
            |this, add, e: &SelectEvent<SearchableVec<String>>, window, cx| {
                let SelectEvent::Confirm(Some(column)) = e else {
                    return;
                };
                if let Some(d) = this.structure_mut().and_then(|s| s.draft.as_mut())
                    && let Some(id) = d
                        .columns
                        .iter()
                        .find(|c| {
                            !d.key.contains(&c.id())
                                && c.form.name.read(cx).value().trim() == column
                        })
                        .map(DraftColumn::id)
                {
                    d.key.push(id);
                }
                add.update(cx, |a, cx| a.set_selected_index(None, window, cx));
                this.refresh_key_choices(window, cx);
            },
        ));
        let Some(t) = &mut self.table else { return };
        t.view = View::Structure;
        let s = &mut t.structure;
        s.data = None;
        s.form = None;
        s.notice = None;
        s._subs = subs;
        s.draft = Some(Draft {
            name: name.clone(),
            engine,
            collation,
            comment,
            columns: Vec::new(),
            key: Vec::new(),
            key_add,
            mariadb,
            backslash: None,
        });
        self.add_draft_column(window, cx);
        name.update(cx, |n, cx| n.focus(window, cx));
        self.read_draft_mode(cx);
    }

    /// A MySQL / MariaDB draft reads the session's `sql_mode` once it is open, so its
    /// literals are quoted the way the server reads them; the preview follows.
    pub(super) fn read_draft_mode(&mut self, cx: &mut Context<Self>) {
        let Session::Ready(driver) = &self.session else {
            return;
        };
        let waiting = self
            .table
            .as_ref()
            .and_then(|t| t.structure.draft.as_ref())
            .is_some_and(|d| d.backslash.is_none());
        if !waiting || self.oracle() {
            return;
        }
        let driver = Arc::clone(driver);
        let rx = cx.global::<Db>().0.spawn(async move {
            let rows = driver
                .execute("SELECT @@SESSION.sql_mode")
                .await?
                .collect()
                .await?;
            match rows.first().and_then(|r| r.first()) {
                Some(Value::Text(mode)) => Ok(mode.clone()),
                Some(Value::Bytes(mode)) => Ok(String::from_utf8_lossy(mode).into_owned()),
                v => Err(DriverError::Invalid(format!("sql_mode came back as {v:?}"))),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err(DriverError::Gone))
                .map_err(|e| e.to_string());
            let _ = this.update(cx, |this, cx| {
                let Some(s) = this.structure_mut() else {
                    return;
                };
                match (result, s.draft.as_mut()) {
                    (Ok(mode), Some(d)) => d.backslash = Some(edit::backslash_escapes(&mode)),
                    (Err(e), Some(_)) => s.notice = Some((format!("Reading sql_mode: {e}"), true)),
                    (_, None) => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Add column in the draft: an empty row at the end, its name focused.
    fn add_draft_column(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((form, subs)) = self.new_column_form(None, window, cx) else {
            return;
        };
        // A renamed row renames its entry in the primary key picker.
        let renamed = cx.subscribe_in(&form.name, window, |this, _, e: &InputEvent, window, cx| {
            if matches!(e, InputEvent::Change) {
                this.refresh_key_choices(window, cx);
            }
        });
        form.name.update(cx, |n, cx| n.focus(window, cx));
        if let Some(s) = self.structure_mut()
            && let Some(d) = &mut s.draft
        {
            d.columns.push(DraftColumn { form });
            s._subs.extend(subs);
            s._subs.push(renamed);
        }
        self.refresh_key_choices(window, cx);
        cx.notify();
    }

    /// A draft row's Move up, Move down or Remove; the primary key follows the rows.
    fn change_draft_row(
        &mut self,
        i: usize,
        action: &RowAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(d) = self.structure_mut().and_then(|s| s.draft.as_mut()) {
            let n = d.columns.len();
            match action {
                RowAction::MoveUp if i > 0 && i < n => d.columns.swap(i - 1, i),
                RowAction::MoveDown if i + 1 < n => d.columns.swap(i, i + 1),
                RowAction::Drop if i < n => {
                    let id = d.columns.remove(i).id();
                    d.key.retain(|k| *k != id);
                }
                _ => {}
            }
        }
        self.refresh_key_choices(window, cx);
        cx.notify();
    }

    /// The primary key's Remove: entry `k` of the key leaves it.
    fn drop_from_key(&mut self, k: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(d) = self.structure_mut().and_then(|s| s.draft.as_mut())
            && k < d.key.len()
        {
            d.key.remove(k);
        }
        self.refresh_key_choices(window, cx);
        cx.notify();
    }

    /// The primary key's up / down: entry `k` trades places with its neighbour.
    fn move_in_key(&mut self, k: usize, up: bool, cx: &mut Context<Self>) {
        if let Some(d) = self.structure_mut().and_then(|s| s.draft.as_mut()) {
            move_key(&mut d.key, k, up);
        }
        cx.notify();
    }

    /// The primary key picker lists the named rows not in the key.
    fn refresh_key_choices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.table.as_ref().and_then(|t| t.structure.draft.as_ref()) else {
            return;
        };
        let names: Vec<String> = d
            .columns
            .iter()
            .filter(|c| !d.key.contains(&c.id()))
            .map(|c| c.form.name.read(cx).value().trim().to_owned())
            .filter(|n| !n.is_empty())
            .collect();
        let add = d.key_add.clone();
        add.update(cx, |a, cx| {
            a.set_items(SearchableVec::new(names), window, cx)
        });
    }

    /// The draft's statements; `None` when the tab is no draft.
    fn draft_sql(&self, cx: &App) -> Option<Result<Vec<String>, String>> {
        let t = self.table.as_ref()?;
        let d = t.structure.draft.as_ref()?;
        let oracle = self.oracle();
        let text = |i: &Entity<InputState>| i.read(cx).value().to_string();
        let mut columns = Vec::with_capacity(d.columns.len());
        let mut names = Vec::with_capacity(d.columns.len());
        for row in &d.columns {
            let mut c = column_of(&row.form, None, oracle, cx);
            // The checkbox shows on integer rows only.
            c.auto_increment &= takes_auto_increment(&c.data_type);
            names.push((row.id(), c.name.trim().to_owned()));
            columns.push(c);
        }
        let primary_key = key_names(&names, &d.key);
        let collation = d.collation.read(cx).selected_value().cloned();
        let table = NewTable {
            name: text(&d.name),
            columns,
            primary_key,
            engine: d
                .engine
                .as_ref()
                .and_then(|e| e.read(cx).selected_value().cloned()),
            collation,
            comment: text(&d.comment),
        };
        let backslash_escapes = match d.backslash {
            Some(b) => b,
            None if oracle => false,
            None => return Some(Err("wait for the server's sql_mode".into())),
        };
        let flags = TableStructure {
            oracle,
            mariadb: d.mariadb,
            backslash_escapes,
            ..TableStructure::default()
        };
        Some(structure::create_table_sql(
            &t.table.database,
            &table,
            &flags,
        ))
    }

    /// Create: the statements run behind the run prompts; the tab then becomes the
    /// table's.
    fn create_draft(&mut self, cx: &mut Context<Self>) {
        let Some(Ok(sql)) = self.draft_sql(cx) else {
            return;
        };
        let Some(d) = self.table.as_ref().and_then(|t| t.structure.draft.as_ref()) else {
            return;
        };
        let name = d.name.read(cx).value().trim().to_owned();
        self.run_ddl_now(sql, None, "Table created.", After::Created(name), false, cx);
    }

    /// The connection's collations, once read.
    fn collation_list(&self, cx: &App) -> Option<Rc<Vec<(String, String)>>> {
        let id = &self.connection.as_ref()?.id;
        cx.try_global::<Collations>()?.0.get(id).cloned()
    }

    /// The Collation list of a column form: `editing`'s character set, else the
    /// table's (none in a draft: every collation).
    fn collation_items(
        &self,
        editing: Option<&StructureColumn>,
        current: Option<&str>,
        cx: &App,
    ) -> Vec<String> {
        let list = self.collation_list(cx).unwrap_or_default();
        let table = self
            .table
            .as_ref()
            .and_then(|t| t.structure.data.as_ref()?.facts.collation.clone());
        let charset = editing
            .and_then(|c| c.charset.clone())
            .or_else(|| table.and_then(|t| charset_of(&list, &t).map(str::to_owned)));
        collation_choices(&list, charset.as_deref(), current)
    }

    /// Reads the connection's collations once (MySQL / MariaDB), then fills the open
    /// Collation lists.
    pub(super) fn ensure_collations(&mut self, cx: &mut Context<Self>) {
        let (Session::Ready(driver), Some(conn)) = (&self.session, &self.connection) else {
            return;
        };
        let (driver, id, window) = (Arc::clone(driver), conn.id.clone(), self.window);
        if self.oracle() || self.collation_list(cx).is_some() {
            return;
        }
        let Some(s) = self.structure_mut().filter(|s| !s.collations_loading) else {
            return;
        };
        s.collations_loading = true;
        let rx = cx
            .global::<Db>()
            .0
            .spawn(async move { driver.collations().await.map_err(|e| e.to_string()) });
        cx.spawn(async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err("the collations task stopped".into()));
            let _ = cx.update_window(window, |_, window, cx| {
                let _ = this.update(cx, |this, cx| {
                    if let Some(s) = this.structure_mut() {
                        s.collations_loading = false;
                    }
                    match result {
                        Ok(list) => {
                            cx.default_global::<Collations>()
                                .0
                                .insert(id, Rc::new(list));
                            this.refill_collations(window, cx);
                        }
                        // The lists keep the values they show.
                        Err(e) => eprintln!("deadlock: reading collations: {e}"),
                    }
                });
            });
        })
        .detach();
    }

    /// The open Collation lists get the server's collations, keeping their picks.
    fn refill_collations(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.table.as_ref().map(|t| &t.structure) else {
            return;
        };
        let mut lists: Vec<(Choice, Option<StructureColumn>)> = Vec::new();
        if let Some(Form::Column(f)) = &s.form {
            lists.push((f.collation.clone(), f.editing.clone()));
        }
        if let Some(d) = &s.draft {
            lists.push((d.collation.clone(), None));
            lists.extend(d.columns.iter().map(|c| (c.form.collation.clone(), None)));
        }
        for (choice, editing) in lists {
            let current = choice.read(cx).selected_value().cloned();
            let items = self.collation_items(editing.as_ref(), current.as_deref(), cx);
            choice.update(cx, |c, cx| {
                c.set_items(SearchableVec::new(items), window, cx)
            });
        }
        cx.notify();
    }

    /// Whether closing the draft would lose typing: a name, a comment or a column.
    pub(crate) fn draft_typed(&self, cx: &App) -> bool {
        let Some(d) = self.table.as_ref().and_then(|t| t.structure.draft.as_ref()) else {
            return false;
        };
        let typed = |i: &Entity<InputState>| !i.read(cx).value().trim().is_empty();
        typed(&d.name)
            || typed(&d.comment)
            || d.columns.len() > 1
            || d.columns.iter().any(|c| typed(&c.form.name))
    }

    /// Whether closing would lose the open form's typing, as the question and text to
    /// ask: Edit column with a change, Add column with a name, or an Add index, Add
    /// foreign key or Add check form with anything typed or picked.
    pub(crate) fn form_unapplied(&self, cx: &App) -> Option<(&'static str, &'static str)> {
        let text = |i: &Entity<InputState>| i.read(cx).value().to_string();
        let added = "It has not been added.";
        match self.table.as_ref()?.structure.form.as_ref()? {
            Form::Column(_) => self
                .column_draft(cx)
                .filter(|(c, before)| column_typed(c, before.as_ref()))
                .map(|_| ("Discard the column changes?", "They have not been applied.")),
            Form::Index(f) => form_typed(&[text(&f.name)], !f.parts.is_empty())
                .then_some(("Discard the new index?", added)),
            Form::ForeignKey(f) => form_typed(
                &[text(&f.name)],
                !f.pairs.is_empty() || f.ref_table.read(cx).selected_value().is_some(),
            )
            .then_some(("Discard the new foreign key?", added)),
            Form::Check(f) => form_typed(&[text(&f.name), text(&f.condition)], false)
                .then_some(("Discard the new check constraint?", added)),
        }
    }

    /// The Column form as a [`NewColumn`], and the edited column as one (`None` for
    /// Add column).
    fn column_draft(&self, cx: &App) -> Option<(NewColumn, Option<NewColumn>)> {
        let t = self.table.as_ref()?;
        let data = t.structure.data.as_ref()?;
        let Some(Form::Column(f)) = &t.structure.form else {
            return None;
        };
        let before = f
            .editing
            .as_ref()
            .map(|c| NewColumn::from_existing(c, data));
        Some((column_of(f, before.as_ref(), data.oracle, cx), before))
    }

    /// The open form's statements.
    fn form_sql(&self, cx: &App) -> Option<Result<Vec<String>, String>> {
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
                let (c, before) = self.column_draft(cx)?;
                match (&f.editing, before) {
                    (Some(_), Some(b)) if b == c => Err("change something first".into()),
                    (Some(old), _) => structure::modify_column_sql(&t.table, old, &c, data),
                    (None, _) => structure::add_column_sql(&t.table, &c, data),
                }
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
                structure::add_index_sql(&t.table, &i, data).map(|s| vec![s])
            }
            Form::ForeignKey(_) => {
                structure::add_foreign_key_sql(&t.table, &self.fk_draft(cx)?, data).map(|s| vec![s])
            }
            Form::Check(f) => {
                structure::add_check_sql(&t.table, &text(&f.name), &text(&f.condition), data)
                    .map(|s| vec![s])
            }
        })
    }

    /// The forced reason of the Edit column form: its type or length changes on a
    /// table that may have rows (an unknown count counts as rows).
    fn edit_risk(&self, cx: &App) -> Option<&'static str> {
        let (c, before) = self.column_draft(cx)?;
        let before = before?;
        let rows = self.table.as_ref()?.structure.data.as_ref()?.facts.rows;
        let changed = (c.data_type.trim(), c.length.trim())
            != (before.data_type.trim(), before.length.trim());
        (changed && rows != Some(0)).then_some(MAY_TRUNCATE)
    }

    /// What the Edit column form says the server will refuse; Apply stays enabled.
    fn column_hints(&self, cx: &App) -> Vec<String> {
        let Some(t) = &self.table else {
            return Vec::new();
        };
        match (&t.structure.form, &t.structure.data, self.column_draft(cx)) {
            (Some(Form::Column(f)), Some(data), Some((c, _))) => f
                .editing
                .as_ref()
                .map(|old| structure::modify_column_hints(old, &c, data))
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    fn apply_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(sql)) = self.form_sql(cx) else {
            return;
        };
        let done = match self.table.as_ref().and_then(|t| t.structure.form.as_ref()) {
            Some(Form::Column(f)) if f.editing.is_some() => "Column changed.",
            Some(Form::Column(_)) => "Column added.",
            Some(Form::ForeignKey(_)) => "Foreign key added.",
            Some(Form::Check(_)) => "Check constraint added.",
            _ => "Index added.",
        };
        let forced = self.edit_risk(cx).map(str::to_owned);
        self.run_ddl(sql, forced, done, window, cx);
    }

    /// Move up / Move down: the same `change column` with a new position.
    fn move_column(&mut self, i: usize, up: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(t) = &self.table else { return };
        let Some(data) = &t.structure.data else {
            return;
        };
        let Some(c) = data.columns.get(i) else { return };
        let position = match (up, i) {
            (true, 0) => return,
            (true, 1) => Position::First,
            (true, i) => Position::After(data.columns[i - 2].name.clone()),
            (false, i) => match data.columns.get(i + 1) {
                Some(next) => Position::After(next.name.clone()),
                None => return,
            },
        };
        let new = NewColumn {
            position,
            ..NewColumn::from_existing(c, data)
        };
        match structure::modify_column_sql(&t.table, c, &new, data) {
            Ok(sql) => self.run_ddl(sql, None, "Column moved.", window, cx),
            Err(e) => {
                if let Some(s) = self.structure_mut() {
                    s.notice = Some((e, true));
                }
                cx.notify();
            }
        }
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
        self.run_ddl(vec![sql], Some(format!("{what}.")), done, window, cx);
    }

    /// A table menu's Rename…, Truncate… or Drop table…: asks now, or once the
    /// structure (keys pointing here, the row estimate) has loaded.
    pub(crate) fn start_table_action(
        &mut self,
        action: TableAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(s) = self.structure_mut() else {
            return;
        };
        if s.data.is_none() {
            s.then = Some(action);
            return;
        }
        self.ask_table_action(action, window, cx);
    }

    /// The action's dialog, which says what the run prompts would (production, the
    /// open transaction) and that pending data changes go; then the statement runs.
    fn ask_table_action(
        &mut self,
        action: TableAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.connection.as_ref().is_some_and(|c| c.read_only) {
            return;
        }
        let Some(t) = &self.table else { return };
        let Some(data) = t.structure.data.clone() else {
            return;
        };
        if data.view {
            if let Some(s) = self.structure_mut() {
                s.notice = Some((
                    "Views are renamed and dropped from the sidebar's view menu.".into(),
                    true,
                ));
            }
            cx.notify();
            return;
        }
        let table = t.table.clone();
        let open_tx = self.tx_mode == super::TxMode::Manual && self.tx_open;
        let production = self
            .connection
            .as_ref()
            .is_some_and(|c| c.environment == Environment::Production);
        let ask = table_ask(action, &table, data, self.pending_changes(cx), open_tx)
            .production(production);
        let answer = crate::dialog::ask_inputs(window, cx, ask);
        cx.spawn(async move |this, cx| {
            let Some((0, to, cascade)) = answer.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.run_table_action(action, &table, &to, cascade, cx);
            });
        })
        .detach();
    }

    fn run_table_action(
        &mut self,
        action: TableAction,
        table: &TableRef,
        to: &str,
        cascade: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(data) = self.table.as_ref().and_then(|t| t.structure.data.as_ref()) else {
            return;
        };
        let Ok(sql) = table_action_sql(action, table, to, cascade, data) else {
            return;
        };
        // The dialog said so.
        self.discard_now(cx);
        let (done, after) = match action {
            TableAction::Rename => ("Table renamed.", After::Renamed(to.trim().to_owned())),
            TableAction::Truncate => ("Table truncated.", After::Reload),
            TableAction::Drop => ("Table dropped.", After::Dropped),
        };
        self.run_ddl_now(vec![sql], None, done, after, true, cx);
    }

    /// Runs the statements in order on the tab's session: pending data changes are
    /// settled first, then the run prompts (production, implicit commit, `forced`) ask.
    fn run_ddl(
        &mut self,
        sql: Vec<String>,
        forced: Option<String>,
        done: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.after_discard(window, cx, move |this, _, cx| {
            this.run_ddl_now(sql, forced, done, After::Reload, false, cx);
        });
    }

    /// `asked`: the caller's own dialog already asked, so the run prompts do not.
    fn run_ddl_now(
        &mut self,
        sql: Vec<String>,
        forced: Option<String>,
        done: &'static str,
        after: After,
        asked: bool,
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
        let labels: Vec<String> = sql.iter().map(|s| structure::statement_label(s)).collect();
        cx.spawn(async move |this, cx| {
            let go = if asked {
                Ok(true)
            } else {
                super::confirm(&this, cx, &rt, &driver, sql.clone(), forced).await
            };
            let result = match go {
                // Stops at the first error: `Err((how many ran, error))`.
                Ok(true) => rt
                    .spawn(async move {
                        for (k, s) in sql.iter().enumerate() {
                            let ran = async { driver.execute(s).await?.collect().await }.await;
                            if let Err(e) = ran {
                                return Err((k, e.to_string()));
                            }
                        }
                        Ok(())
                    })
                    .await
                    .unwrap_or_else(|_| Err((0, DriverError::Gone.to_string()))),
                Ok(false) => {
                    let _ = this.update(cx, |this, cx| {
                        if let Some(s) = this.structure_mut() {
                            s.busy = false;
                        }
                        cx.notify();
                    });
                    return;
                }
                Err(e) => Err((0, e)),
            };
            let _ = this.update(cx, |this, cx| {
                this.ddl_done(result, &labels, done, after, cx)
            });
        })
        .detach();
    }

    /// After the statements: once any of them ran, the structure, the Data view's
    /// columns and the sidebar reload.
    fn ddl_done(
        &mut self,
        result: Result<(), (usize, String)>,
        labels: &[String],
        done: &'static str,
        after: After,
        cx: &mut Context<Self>,
    ) {
        let Some(table) = self.table.as_ref().map(|t| t.table.clone()) else {
            return;
        };
        let database = table.database.clone();
        let failed = result.is_err();
        let ran = !matches!(result, Err((0, _)));
        if let Some(s) = self.structure_mut() {
            s.busy = false;
            match result {
                Ok(()) => {
                    s.notice = Some((done.into(), false));
                    s.form = None;
                    s._subs.clear();
                }
                Err((0, e)) => s.notice = Some((e, true)),
                Err((k, e)) => {
                    // DDL commits on its own, so what ran stays. The form stays open
                    // and follows the column as it reloads, so Apply runs the rest.
                    s.notice = Some((partial_failure(labels, k, &e), true));
                    s.rebase = s.form.is_some();
                }
            }
        }
        match after {
            _ if !ran => {}
            After::Dropped => {
                cx.emit(TabEvent::SchemaChanged(database));
                cx.emit(TabEvent::TableDropped(table));
            }
            After::Renamed(to) => {
                if let Some(t) = &mut self.table {
                    t.table.table = to;
                    self.kind = super::Kind::Table(t.table.clone());
                    cx.emit(TabEvent::TableRenamed(table, t.table.clone()));
                }
                self.load_structure(cx);
                self.columns_changed(cx);
                cx.emit(TabEvent::SchemaChanged(database));
                cx.emit(TabEvent::Changed);
            }
            After::Reload => {
                self.load_structure(cx);
                self.columns_changed(cx);
                cx.emit(TabEvent::SchemaChanged(database));
            }
            After::Created(name) => {
                if let Some(t) = &mut self.table {
                    t.table.table = name;
                    t.structure.draft = None;
                    self.kind = super::Kind::Table(t.table.clone());
                    // The tab list saves it now, and an open diagram of the schema reloads.
                    cx.emit(TabEvent::TableRenamed(table, t.table.clone()));
                }
                self.load_structure(cx);
                cx.emit(TabEvent::SchemaChanged(database));
                cx.emit(TabEvent::Changed);
            }
        }
        self.check_tx(false, failed, cx).detach();
        cx.notify();
    }
}

/// "Ran 1 of 3 (rename column). Statement 2 (modify) failed: …": `k` of `labels` ran.
fn partial_failure(labels: &[String], k: usize, e: &str) -> String {
    format!(
        "Ran {k} of {} ({}). Statement {} ({}) failed: {e}",
        labels.len(),
        labels[..k.min(labels.len())].join(", "),
        k + 1,
        labels.get(k).map_or("", String::as_str)
    )
}

/// After "Ran k of n": the open Column form edits the column as it reloaded, found by
/// the name it had or, when the rename ran, the name typed; Apply builds what is left.
/// An Add column form becomes the new column's Edit form.
fn rebase_form(s: &mut Structure, cx: &App) {
    let (Some(Form::Column(f)), Some(data)) = (s.form.as_mut(), s.data.as_ref()) else {
        return;
    };
    let typed = f.name.read(cx).value().trim().to_owned();
    let old = f.editing.as_ref().map(|c| c.name.clone());
    let found = old
        .iter()
        .chain([&typed])
        .find_map(|n| data.columns.iter().find(|c| &c.name == n));
    let Some(c) = found.cloned() else {
        s.form = None;
        s._subs.clear();
        return;
    };
    if f.editing.is_none() {
        // It was added at the end, where it stays.
        let at = NewColumn::from_existing(&c, data).position;
        for p in f.positions.iter_mut().filter(|p| **p == Position::Last) {
            *p = at.clone();
        }
    }
    f.editing = Some(c);
}

fn table_action_sql(
    action: TableAction,
    t: &TableRef,
    to: &str,
    cascade: bool,
    s: &TableStructure,
) -> Result<String, String> {
    match action {
        TableAction::Rename => structure::rename_table_sql(t, to, s),
        TableAction::Truncate => Ok(structure::truncate_table_sql(t, s)),
        TableAction::Drop => Ok(structure::drop_table_sql(t, cascade, s)),
    }
}

/// The dialog of a table action, as the design's `TableActions` shows it: `pending`
/// data changes of the tab, `open_tx` an open Manual-mode transaction the DDL commits.
fn table_ask(
    action: TableAction,
    t: &TableRef,
    s: TableStructure,
    pending: usize,
    open_tx: bool,
) -> crate::dialog::Ask {
    let name = &t.table;
    let engine = match (s.oracle, s.mariadb) {
        (true, _) => "Oracle",
        (_, true) => "MariaDB",
        _ => "MySQL",
    };
    let rows = s.facts.rows.map(super::table::grouped);
    // A key of the table itself does not stop a truncate or a drop.
    let keys: Vec<(String, String)> = s
        .referenced_by
        .iter()
        .filter(|k| k.from != *t)
        .map(|k| {
            let action = match k.on_delete.as_str() {
                "no action" => "no action".to_owned(),
                d => format!("on delete {d}"),
            };
            (reference(k.from.table.clone(), &k.from_columns), action)
        })
        .collect();
    let n = keys.len();
    let point = if n == 1 {
        "1 foreign key points here.".to_owned()
    } else {
        format!("{n} foreign keys point here.")
    };
    let (mut ask, text, refuses) = match action {
        TableAction::Rename => {
            let mut ask =
                crate::dialog::Ask::new(format!("Rename table {name}"), &["Rename", "Cancel"]);
            ask.field = Some(("New name".into(), name.clone()));
            let server = if s.oracle {
                format!(
                    "Oracle marks views, packages and triggers that use {name} invalid: \
                     recompile them after."
                )
            } else {
                format!(
                    "Views and routines that name {name} stop working: {engine} does not rewrite them."
                )
            };
            ask.warning = Some(format!("{server} Open editors still use the old name."));
            (ask, None, None)
        }
        TableAction::Truncate => {
            // "about 2, and resets": the count is its own clause before "and".
            let comma = if s.oracle { "" } else { "," };
            let rows = rows.map_or(String::new(), |r| format!(", about {r}{comma}"));
            let reset = if s.oracle {
                ""
            } else {
                " and resets auto_increment"
            };
            let refuses = if s.oracle {
                "Oracle refuses it until they are disabled.".to_owned()
            } else {
                format!("{engine} refuses it while they exist.")
            };
            (
                crate::dialog::Ask::new(format!("Truncate table {name}?"), &["Truncate", "Cancel"])
                    .danger(true),
                Some(format!(
                    "Deletes all rows{rows}{reset}. It commits at once and cannot be rolled back."
                )),
                Some(refuses),
            )
        }
        TableAction::Drop => {
            let text = match (s.oracle, rows) {
                (true, _) => format!(
                    "It goes to the recycle bin: flashback table {}.{name} to before drop \
                     brings it back.",
                    t.database
                ),
                (false, Some(r)) => {
                    format!(
                        "Deletes the table, its indexes and about {r} rows. It cannot be undone."
                    )
                }
                (false, None) => {
                    "Deletes the table, its indexes and every row. It cannot be undone.".into()
                }
            };
            let mut ask =
                crate::dialog::Ask::new(format!("Drop table {name}?"), &["Drop table", "Cancel"])
                    .danger(true);
            if s.oracle && n > 0 {
                ask.check = Some(if n == 1 {
                    "Cascade constraints: also drop this key".into()
                } else {
                    format!("Cascade constraints: also drop these {n} keys")
                });
            }
            let refuses =
                (!s.oracle).then(|| format!("{engine} refuses the drop while they exist."));
            (ask, Some(text), refuses)
        }
    };
    let mut lines: Vec<String> = text.into_iter().collect();
    if open_tx {
        lines.push(
            "It commits the open transaction first: its changes become permanent and can no \
             longer be rolled back."
                .into(),
        );
    }
    if pending > 0 {
        let changes = if pending == 1 { "change" } else { "changes" };
        lines.push(format!(
            "The tab's {pending} pending {changes} are discarded."
        ));
    }
    if !lines.is_empty() {
        ask = ask.text(lines.join("\n\n"));
    }
    if n > 0 && action != TableAction::Rename {
        let label = match refuses {
            Some(r) => format!("{point} {r}"),
            None => point,
        };
        ask.list = Some((label, keys));
    }
    let t = t.clone();
    ask.preview = Some(crate::dialog::Preview(std::rc::Rc::new(
        move |to, cascade| {
            table_action_sql(action, &t, to, cascade, &s).map(|sql| format!("{sql};"))
        },
    )));
    ask
}

/// A grid column: a fixed width, or a share of the rest with a minimum.
#[derive(Clone, Copy)]
enum W {
    Fixed(f32),
    Flex(f32, f32),
}

const COLUMNS: [W; 5] = [
    W::Fixed(64.),
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
    labeled_dot(label, field, basis, false, p)
}

/// A labelled form field, with the design's changed dot after the label.
fn labeled_dot(
    label: &'static str,
    field: impl IntoElement,
    basis: f32,
    changed: bool,
    p: &Palette,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .flex_grow(1.)
        .flex_shrink(1.)
        .flex_basis(px(basis))
        .min_w(px(basis.min(120.)))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(small_size())
                .text_color(p.tx2)
                .child(label)
                .when(changed, |d| {
                    d.child(div().size(px(6.)).rounded_full().bg(p.warn).flex_none())
                }),
        )
        .child(field)
}

/// A read-only field: `.in.ro`.
fn fixed_field(text: String, p: &Palette) -> Div {
    div()
        .flex()
        .items_center()
        .h(row_height())
        .px(px(8.))
        .border_1()
        .border_color(p.line2)
        .rounded(RADIUS)
        .bg(p.panel)
        .text_color(p.tx3)
        .font_family(MONO_FONT)
        .text_size(data_size())
        .overflow_hidden()
        .whitespace_nowrap()
        .child(text)
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
    if this.table.as_ref().is_some_and(|t| t.structure.is_draft()) {
        return draft_view(this, segments, cx);
    }
    let p = *cx.global::<Palette>();
    let read_only = this.connection.as_ref().is_some_and(|c| c.read_only);
    let form_sql = this.form_sql(cx);
    let draft = this.column_draft(cx);
    let risk = this.edit_risk(cx);
    let fk_hints = this.fk_hints(cx);
    let fk_note = this
        .fk_draft(cx)
        .zip(this.table.as_ref().and_then(|t| t.structure.data.as_ref()))
        .and_then(|(k, data)| structure::foreign_key_index_note(&k, data));
    let column_hints = this.column_hints(cx);
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
    let oracle = d.oracle;
    // Double-click and the row menu's changes: tables on writable connections only.
    let editable = !view && !read_only;
    let entity = cx.entity();

    // A grid row: hover and selection colors, click to select, right click for `menu`.
    let row = |section: Section,
               i: usize,
               id: &'static str,
               cells: Vec<AnyElement>,
               menu: Vec<(String, RowAction, bool)>,
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
            .clickable(cx.listener(move |this, e: &ClickEvent, window, cx| {
                this.select_row(section, i, cx);
                if section == Section::Columns && editable && e.click_count() >= 2 {
                    this.open_edit_column(i, window, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| this.select_row(section, i, cx)),
            );
        if menu.is_empty() {
            return d.into_any_element();
        }
        let entity = entity.clone();
        d.context_menu(move |mut m, _, cx| {
            for (n, (label, action, off)) in menu.iter().enumerate() {
                let moving = matches!(action, RowAction::MoveUp | RowAction::MoveDown);
                if oracle && moving {
                    let (label, icon) = match action {
                        RowAction::MoveUp => ("Move up", Icon::SortAsc),
                        _ => ("Move down", Icon::SortDesc),
                    };
                    m = m.item(menu_item_disabled(
                        label,
                        "Oracle cannot reorder columns",
                        icon,
                    ));
                    continue;
                }
                let item = match action {
                    RowAction::Open(_) => menu_item(label.clone(), Icon::Open),
                    RowAction::Edit => menu_item_hint(label.clone(), "Double-click", Icon::Rename),
                    RowAction::MoveUp => menu_item(label.clone(), Icon::SortAsc),
                    RowAction::MoveDown => menu_item(label.clone(), Icon::SortDesc),
                    RowAction::Drop => {
                        if n > 0 {
                            m = m.separator();
                        }
                        if *off {
                            // A red item reads as available.
                            menu_item(label.clone(), Icon::Trash)
                        } else {
                            menu_item_danger(label.clone(), Icon::Trash, cx)
                        }
                    }
                };
                let (entity, action) = (entity.clone(), action.clone());
                m = m.item(item.disabled(*off).on_click(
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        entity.update(cx, |this, cx| match &action {
                            RowAction::Open(t) => cx.emit(TabEvent::OpenTable(t.clone())),
                            RowAction::Edit => this.open_edit_column(i, window, cx),
                            RowAction::MoveUp => this.move_column(i, true, window, cx),
                            RowAction::MoveDown => this.move_column(i, false, window, cx),
                            RowAction::Drop => this.ask_drop(section, i, window, cx),
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
                .hover(|s| s.text_color(p.tx))
                .child(text)
                .clickable(cx.listener(move |_, _: &ClickEvent, _, cx| {
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
            let last = i + 1 == d.columns.len();
            vec![
                ("Edit column…".to_owned(), RowAction::Edit, read_only),
                ("Move up".to_owned(), RowAction::MoveUp, read_only || i == 0),
                (
                    "Move down".to_owned(),
                    RowAction::MoveDown,
                    read_only || last,
                ),
                (
                    format!("Drop column {}", c.name),
                    RowAction::Drop,
                    read_only,
                ),
            ]
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
    // One form at a time: another Add would drop the open one's typing.
    let can_add = !busy && !read_only && s.form.is_none();
    let add = |id: &'static str, label: &'static str, tip: &'static str, cx: &App| {
        icon_button(
            id,
            Icon::Plus,
            Some(label),
            ButtonKind::Outline,
            can_add,
            tip,
            cx,
        )
    };
    let add_column = (!view).then(|| {
        add("add-column", "Add column", "Add a column to the table", cx).when(can_add, |b| {
            b.clickable(cx.listener(|this, _, window, cx| this.open_column_form(window, cx)))
        })
    });
    let column_form = match (&s.form, &form_sql, draft) {
        (Some(Form::Column(f)), Some(sql), Some(draft)) => {
            let look = Look {
                oracle,
                mariadb: d.mariadb,
                rows: risk.is_some(),
                hints: &column_hints,
            };
            Some(column_form(f, sql, &draft, look, busy || read_only, &p, cx))
        }
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
                vec![(label, RowAction::Drop, read_only)],
                cx,
            ));
        }
        let add_index = add("add-index", "Add index", "Add an index to the table", cx)
            .when(can_add, |b| {
                b.clickable(cx.listener(|this, _, window, cx| this.open_index_form(window, cx)))
            });
        let index_form = match (&s.form, &form_sql) {
            (Some(Form::Index(f)), Some(sql)) => {
                Some(index_form(f, sql, busy || read_only, &p, cx))
            }
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
                let mut menu = vec![(
                    format!("Open {}", other(&target)),
                    RowAction::Open(target),
                    false,
                )];
                if !back {
                    menu.insert(
                        0,
                        (
                            format!("Drop foreign key {}", k.name),
                            RowAction::Drop,
                            read_only,
                        ),
                    );
                }
                rows.push(row(section, i, id, cells, menu, cx));
            }
            rows
        };
        // `.empty`: the line of a section with no rows.
        let empty = |text: &'static str| {
            div()
                .px(px(16.))
                .py(px(10.))
                .border_b_1()
                .border_color(p.line)
                .text_color(p.tx3)
                .child(text)
                .into_any_element()
        };
        let mut fk_rows = keys(&d.foreign_keys, false, cx);
        if d.foreign_keys.is_empty() {
            fk_rows.push(empty("No foreign keys yet."));
        }
        let mut back_rows = keys(&d.referenced_by, true, cx);
        if d.referenced_by.is_empty() {
            back_rows.push(empty("No other tables reference this one."));
        }
        let add_fk = add(
            "add-foreign-key",
            "Add foreign key",
            "Add a foreign key to the table",
            cx,
        )
        .when(can_add, |b| {
            b.clickable(cx.listener(|this, _, window, cx| this.open_fk_form(window, cx)))
        });
        let fk_form = match (&s.form, &form_sql) {
            (Some(Form::ForeignKey(f)), Some(sql)) => Some(fk_form(
                f,
                d,
                sql,
                &fk_hints,
                fk_note,
                busy || read_only,
                &p,
                cx,
            )),
            _ => None,
        };
        main = main
            .child(section_head("Indexes", Some(add_index)))
            .children(index_form)
            .child(grid_box("structure-indexes", index_rows, &p))
            .child(section_head("Foreign keys", Some(add_fk)))
            .children(fk_form)
            .child(grid_box("structure-foreign-keys", fk_rows, &p))
            .child(section_head("Referenced by", None))
            .child(grid_box("structure-referenced", back_rows, &p));
        // Shown while empty too, so Add check is always there; hidden on servers
        // without check constraints.
        if !d.checks_unsupported {
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
                let menu = vec![(
                    format!("Drop check constraint {}", c.name),
                    RowAction::Drop,
                    read_only,
                )];
                check_rows.push(row(Section::Checks, i, "structure-check", cells, menu, cx));
            }
            if d.checks.is_empty() {
                check_rows.push(empty("No check constraints yet."));
            }
            let add_check = add(
                "add-check",
                "Add check",
                "Add a check constraint to the table",
                cx,
            )
            .when(can_add, |b| {
                b.clickable(cx.listener(|this, _, window, cx| this.open_check_form(window, cx)))
            });
            let check_form = match (&s.form, &form_sql) {
                (Some(Form::Check(f)), Some(sql)) => {
                    Some(check_form(f, sql, busy || read_only, &p, cx))
                }
                _ => None,
            };
            main = main
                .child(section_head("Check constraints", Some(add_check)))
                .children(check_form)
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
    .clickable(cx.listener(|this, _, _, cx| this.copy_definition(cx)));
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

/// The New table draft, as the design's `NewTable` lays it out: the table's fields,
/// one row per column, the primary key, the statement, then Discard and Create.
fn draft_view(this: &mut QueryTab, segments: Div, cx: &mut Context<QueryTab>) -> AnyElement {
    let p = *cx.global::<Palette>();
    let read_only = this.connection.as_ref().is_some_and(|c| c.read_only);
    let oracle = this.oracle();
    let (Some(sql), Some(t)) = (this.draft_sql(cx), &this.table) else {
        return div().into_any_element();
    };
    let s = &t.structure;
    let Some(d) = &s.draft else {
        return div().into_any_element();
    };
    let busy = s.busy;
    let editable = !busy && !read_only;
    let name = d.name.read(cx).value().trim().to_owned();
    let n = d.columns.len();
    let small = |text: String, color| {
        div()
            .text_size(small_size())
            .text_color(color)
            .line_height(px(18.))
            .child(text)
    };
    let toolbar = div()
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
                .child(if name.is_empty() {
                    t.table.database.clone()
                } else {
                    format!("{}.{name}", t.table.database)
                }),
        )
        .child(
            div()
                .px(px(6.))
                .rounded(px(4.))
                .border_1()
                .border_color(p.line2)
                .text_color(p.tx2)
                .text_size(px(11.))
                .font_weight(FontWeight::MEDIUM)
                .child("Draft"),
        )
        .child(
            div()
                .text_color(p.tx3)
                .child(format!("{n} {}", if n == 1 { "column" } else { "columns" })),
        );

    let mut table_fields =
        vec![labeled("Name", text_field(&d.name, true), 220., &p).into_any_element()];
    if let Some(engine) = &d.engine {
        table_fields.push(
            labeled(
                "Engine",
                Select::new(engine)
                    .h(row_height())
                    .placeholder("Server default")
                    .cleanable(true),
                140.,
                &p,
            )
            .into_any_element(),
        );
        table_fields.push(
            labeled(
                "Collation",
                collation_field(&d.collation, "schema default"),
                200.,
                &p,
            )
            .into_any_element(),
        );
    }
    table_fields
        .push(labeled("Comment", text_field(&d.comment, false), 320., &p).into_any_element());

    // The grid's columns; Collation and On update are MySQL's, Identity is Oracle's AI.
    let mut heads = vec![
        (W::Fixed(40.), "#"),
        (W::Flex(1.2, 140.), "Name"),
        (W::Flex(1.2, 160.), "Type"),
        (W::Flex(1., 120.), "Length or members"),
    ];
    if !oracle {
        heads.push((W::Flex(1., 140.), "Collation"));
    }
    heads.push((W::Fixed(52.), "Null"));
    heads.push(if oracle {
        (W::Fixed(76.), "Identity")
    } else {
        (W::Fixed(48.), "AI")
    });
    heads.push((W::Flex(1.2, 160.), "Default"));
    if !oracle {
        heads.push((W::Fixed(84.), "On update"));
    }
    heads.push((W::Flex(1.4, 160.), "Comment"));
    heads.push((W::Fixed(48.), ""));
    let widths: Vec<W> = heads.iter().map(|(w, _)| *w).collect();
    let labels: Vec<&'static str> = heads.iter().map(|(_, l)| *l).collect();
    let mut rows = vec![header(&widths, &labels, &p).into_any_element()];
    let entity = cx.entity();
    for (i, row) in d.columns.iter().enumerate() {
        let f = &row.form;
        let ty = f
            .data_type
            .read(cx)
            .selected_value()
            .cloned()
            .unwrap_or_default();
        let at = FormAt::Draft(i);
        let none = || div().into_any_element();
        let mut cells = vec![
            div()
                .pl(px(8.))
                .text_color(p.tx3)
                .child(format!("{}", i + 1))
                .into_any_element(),
            text_field(&f.name, true).into_any_element(),
            Select::new(&f.data_type)
                .h(row_height())
                .placeholder("Choose a type")
                .into_any_element(),
            text_field(&f.length, true).into_any_element(),
        ];
        if !oracle {
            cells.push(if structure::is_text_type(&ty) {
                collation_field(&f.collation, "table default").into_any_element()
            } else {
                none()
            });
        }
        cells.push(form_check(
            ("draft-null", i),
            None,
            f.nullable,
            at,
            |f, v| f.nullable = v,
            cx,
        ));
        cells.push(if takes_auto_increment(&ty) {
            form_check(
                ("draft-ai", i),
                None,
                f.auto_increment,
                at,
                |f, v| f.auto_increment = v,
                cx,
            )
        } else {
            none()
        });
        cells.push(text_field(&f.default, true).into_any_element());
        if !oracle {
            cells.push(if structure::is_on_update_type(&ty) {
                form_check(
                    ("draft-on-update", i),
                    None,
                    f.on_update,
                    at,
                    |f, v| f.on_update = v,
                    cx,
                )
            } else {
                none()
            });
        }
        cells.push(text_field(&f.comment, false).into_any_element());
        cells.push(
            button_at("draft-remove", i, cx)
                .child(icon(Icon::Close, p.tx3))
                .clickable(cx.listener(move |this, _, window, cx| {
                    this.change_draft_row(i, &RowAction::Drop, window, cx);
                }))
                .into_any_element(),
        );
        let menu = [
            ("Move up", Icon::SortAsc, RowAction::MoveUp, i == 0),
            ("Move down", Icon::SortDesc, RowAction::MoveDown, i + 1 == n),
            ("Remove column", Icon::Trash, RowAction::Drop, false),
        ];
        let entity = entity.clone();
        rows.push(
            div()
                .id(("draft-column", i))
                .flex()
                .items_center()
                .min_h(px(40.))
                .border_b_1()
                .border_color(p.line)
                .children(
                    widths
                        .iter()
                        .zip(cells)
                        .map(|(w, c)| cell(*w).px(px(4.)).child(c)),
                )
                .context_menu(move |mut m, _, _| {
                    for (label, ic, action, off) in &menu {
                        let (entity, action) = (entity.clone(), action.clone());
                        m = m.item(menu_item(*label, *ic).disabled(*off).on_click(
                            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                entity.update(cx, |this, cx| {
                                    this.change_draft_row(i, &action, window, cx);
                                });
                            },
                        ));
                    }
                    m
                })
                .into_any_element(),
        );
    }
    let add_column = icon_button(
        "add-column",
        Icon::Plus,
        Some("Add column"),
        ButtonKind::Outline,
        editable,
        "Add a column row",
        cx,
    )
    .when(editable, |b| {
        b.clickable(cx.listener(|this, _, window, cx| this.add_draft_column(window, cx)))
    });

    let mut key = div()
        .px(px(16.))
        .flex()
        .flex_col()
        .gap(px(6.))
        .max_w(px(420.))
        .child(small("Columns, in order".into(), p.tx2));
    let keyed = !d.key.is_empty();
    let last = d.key.len().saturating_sub(1);
    for (k, id) in d.key.iter().enumerate() {
        let Some(c) = d.columns.iter().find(|c| c.id() == *id) else {
            continue;
        };
        let step = |id: &'static str, ic: Icon, up: bool, on: bool, cx: &mut Context<QueryTab>| {
            let b = button_at(id, k, cx)
                .px(px(6.))
                .child(icon(ic, if on { p.tx2 } else { p.line2 }));
            if on {
                b.clickable(cx.listener(move |this, _, _, cx| this.move_in_key(k, up, cx)))
            } else {
                b.cursor_default()
            }
        };
        key = key.child(
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
                        .child(c.form.name.read(cx).value().to_string()),
                )
                .child(step("key-up", Icon::SortAsc, true, k > 0, cx))
                .child(step("key-down", Icon::SortDesc, false, k < last, cx))
                .child(
                    button_at("key-remove", k, cx)
                        .child("Remove")
                        .clickable(cx.listener(move |this, _, window, cx| {
                            this.drop_from_key(k, window, cx);
                        })),
                ),
        );
    }
    let key = key.child(
        div().max_w(px(280.)).child(
            Select::new(&d.key_add)
                .h(row_height())
                .placeholder("Add a column…"),
        ),
    );
    let copy = button(
        "copy-definition",
        ButtonKind::Outline,
        sql.is_ok(),
        "Copy the statement",
        cx,
    )
    .child(if s.copied { "Copied" } else { "Copy" })
    .clickable(cx.listener(|this, _, _, cx| this.copy_definition(cx)));
    let notes = [
        (!keyed).then(|| {
            small(
                "No primary key yet: the table can be created without one.".into(),
                p.tx2,
            )
        }),
        s.notice
            .clone()
            .map(|(n, err)| small(n, if err { p.danger } else { p.tx2 })),
    ];
    let ready = sql.is_ok() && editable;
    let footer = div()
        .flex()
        .flex_none()
        .flex_wrap()
        .items_center()
        .gap(px(12.))
        .px(px(16.))
        .py(px(10.))
        .border_t_1()
        .border_color(p.line)
        .child(small(
            "Indexes and foreign keys are added after Create, with the Structure forms.".into(),
            p.tx3,
        ))
        .child(
            div()
                .ml_auto()
                .flex()
                .gap(px(8.))
                .child(
                    button(
                        "draft-discard",
                        ButtonKind::Outline,
                        !busy,
                        "Close the tab without creating the table",
                        cx,
                    )
                    .child("Discard")
                    .when(!busy, |b| {
                        b.clickable(cx.listener(|_, _, _, cx| cx.emit(TabEvent::Discard)))
                    }),
                )
                .child(
                    button(
                        "draft-create",
                        ButtonKind::Primary,
                        ready,
                        "Run the statement on the tab's session",
                        cx,
                    )
                    .child("Create")
                    .when(ready, |b| {
                        b.clickable(cx.listener(|this, _, _, cx| this.create_draft(cx)))
                    }),
                ),
        );
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(toolbar)
        .when(read_only, |d| {
            d.child(
                div()
                    .flex_none()
                    .px(px(16.))
                    .py(px(6.))
                    .border_b_1()
                    .border_color(p.line)
                    .bg(p.panel)
                    .text_size(small_size())
                    .text_color(p.tx2)
                    .child("Read-only connection: the structure cannot be changed."),
            )
        })
        .child(
            div()
                .id("draft-page")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .pb(px(16.))
                .child(section_head("Table", None))
                .child(div().px(px(16.)).child(fields_row(table_fields)))
                .child(section_head("Columns", Some(add_column)))
                .child(
                    div().id("draft-columns").overflow_x_scroll().child(
                        div()
                            .min_w(px(if oracle { 980. } else { 1240. }))
                            .border_t_1()
                            .border_color(p.line)
                            .children(rows),
                    ),
                )
                .child(section_head("Primary key", None))
                .child(key)
                .child(section_head("Statement to run", Some(copy)))
                .child(div().px(px(16.)).child(preview(&sql, &p)))
                .child(
                    div()
                        .px(px(16.))
                        .pt(px(10.))
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .children(notes.into_iter().flatten()),
                ),
        )
        .child(footer)
        .into_any_element()
}

/// `customers.id`, or `orders (a, b)` for a composite key.
fn reference(table: String, columns: &[String]) -> String {
    match columns {
        [one] => format!("{table}.{one}"),
        many => format!("{table} ({})", many.join(", ")),
    }
}

/// `sql` as the preview shows it: each statement ends with `;`, and [`wrap_sql`]
/// breaks the long lines. Display only: what runs is `sql`.
fn wrap_statements(sql: &[String], width: usize) -> String {
    let ended: Vec<String> = sql.iter().map(|s| format!("{s};")).collect();
    wrap_sql(&ended.join("\n"), width)
}

/// The preview of a form's statement, or why there is none yet.
fn preview(sql: &Result<Vec<String>, String>, p: &Palette) -> AnyElement {
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
            .child(
                div()
                    .flex_none()
                    .child(highlighted(&wrap_statements(sql, 84), p)),
            )
            .into_any_element(),
        Err(e) => div()
            .text_size(small_size())
            .text_color(p.tx3)
            .child(format!("To finish: {e}."))
            .into_any_element(),
    }
}

/// The box a form sits in, under its section's title, with an optional title line,
/// Cancel and Apply.
fn form_box(
    title: Option<AnyElement>,
    fields: Vec<AnyElement>,
    sql: &Result<Vec<String>, String>,
    busy: bool,
    p: &Palette,
    cx: &mut Context<QueryTab>,
) -> Div {
    let ready = sql.is_ok() && !busy;
    let apply = match sql {
        Ok(s) if s.len() > 1 => format!("Apply {} statements", s.len()),
        _ => "Apply".to_owned(),
    };
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
        .children(title)
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
                    .clickable(cx.listener(|this, _, _, cx| this.close_form(cx))),
                )
                .child(
                    button(
                        "form-apply",
                        ButtonKind::Primary,
                        ready,
                        "Run the statement on the tab's session",
                        cx,
                    )
                    .child(apply)
                    .when(ready, |b| {
                        b.clickable(cx.listener(|this, _, window, cx| this.apply_form(window, cx)))
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

/// What the Column form shows besides its own state.
#[derive(Clone, Copy)]
struct Look<'a> {
    oracle: bool,
    mariadb: bool,
    /// The type or length changes on a table that may have rows.
    rows: bool,
    /// What the server will likely refuse.
    hints: &'a [String],
}

/// The Add column or Edit column form. `draft` is the form as a column, with the
/// edited column as it was: fields that differ get the changed dot.
fn column_form(
    f: &ColumnForm,
    sql: &Result<Vec<String>, String>,
    draft: &(NewColumn, Option<NewColumn>),
    look: Look,
    busy: bool,
    p: &Palette,
    cx: &mut Context<QueryTab>,
) -> Div {
    let (c, before) = (&draft.0, draft.1.as_ref());
    let changed = |pick: fn(&NewColumn) -> String| before.is_some_and(|b| pick(b) != pick(c));
    let toggle =
        |id: &'static str, label: &'static str, on: bool, set: fn(&mut ColumnForm, bool)| {
            form_check(id, Some(label), on, FormAt::Form, set, cx)
        };
    let hint = |text: &'static str| {
        div()
            .text_size(small_size())
            .text_color(p.tx3)
            .child(text)
            .into_any_element()
    };
    let generated = c.generated.as_ref();
    let editing = f.editing.is_some();
    let title = f.editing.as_ref().map(|e| {
        div()
            .flex()
            .items_center()
            .gap(px(6.))
            .text_size(px(13.))
            .font_weight(FontWeight::SEMIBOLD)
            .child("Edit column")
            .child(div().font_family(MONO_FONT).child(e.name.clone()))
            .into_any_element()
    });
    let mut second = vec![
        match generated {
            Some(_) => labeled_dot(
                "Expression",
                text_field(&f.expression, true),
                220.,
                changed(|n| {
                    n.generated
                        .as_ref()
                        .map(|g| g.expression.clone())
                        .unwrap_or_default()
                }),
                p,
            ),
            None => labeled_dot(
                "Default",
                text_field(&f.default, true),
                220.,
                changed(|n| n.default.clone()),
                p,
            ),
        }
        .into_any_element(),
    ];
    second.push(if look.oracle && editing {
        labeled("Position", fixed_field("stays in place".into(), p), 160., p).into_any_element()
    } else {
        labeled_dot(
            "Position",
            Select::new(&f.position).h(row_height()),
            160.,
            changed(|n| format!("{:?}", n.position)),
            p,
        )
        .into_any_element()
    });
    if !look.oracle {
        second.push(if structure::is_text_type(&c.data_type) {
            labeled_dot(
                "Collation",
                collation_field(&f.collation, "table default"),
                160.,
                changed(|n| n.collation.clone().unwrap_or_default()),
                p,
            )
            .into_any_element()
        } else {
            let none = format!("none for {}", c.data_type);
            labeled("Collation", fixed_field(none, p), 160., p).into_any_element()
        });
    }
    if let Some(g) = generated {
        let kind = if g.stored { "stored" } else { "virtual" };
        second.push(labeled("Generated", fixed_field(kind.into(), p), 120., p).into_any_element());
    }
    let mut checks = div().flex().flex_wrap().gap(px(16.));
    if generated.is_none() || !look.mariadb {
        checks = checks.child(toggle("column-nullable", "Nullable", f.nullable, |f, v| {
            f.nullable = v
        }));
    }
    if generated.is_none() {
        checks = checks.child(toggle(
            "column-auto-increment",
            "Auto increment",
            f.auto_increment,
            |f, v| f.auto_increment = v,
        ));
    }
    if !look.oracle && generated.is_none() && structure::is_on_update_type(&c.data_type) {
        checks = checks.child(toggle(
            "column-on-update",
            "On update CURRENT_TIMESTAMP",
            f.on_update,
            |f, v| f.on_update = v,
        ));
    }
    let mut fields = vec![
        fields_row(vec![
            labeled_dot(
                "Name",
                text_field(&f.name, true),
                160.,
                changed(|n| n.name.clone()),
                p,
            )
            .into_any_element(),
            labeled_dot(
                "Type",
                Select::new(&f.data_type).h(row_height()),
                160.,
                changed(|n| n.data_type.clone()),
                p,
            )
            .into_any_element(),
            labeled_dot(
                "Length or members",
                text_field(&f.length, true),
                120.,
                changed(|n| n.length.clone()),
                p,
            )
            .into_any_element(),
        ]),
        fields_row(second),
        fields_row(vec![
            labeled_dot(
                "Comment",
                text_field(&f.comment, false),
                220.,
                changed(|n| n.comment.clone()),
                p,
            )
            .into_any_element(),
        ]),
        checks.into_any_element(),
    ];
    if generated.is_some() {
        fields.push(hint(
            "Stored and virtual cannot be switched: drop and add the column.",
        ));
    }
    if look.oracle
        && matches!(
            c.data_type.split_whitespace().next(),
            Some("date" | "timestamp")
        )
    {
        fields.push(hint(
            "On update: Oracle has no such clause. Use a before update trigger to set the column.",
        ));
    }
    fields.extend(
        look.hints
            .iter()
            .map(|h| warning_line(h.clone(), p).into_any_element()),
    );
    if look.rows {
        let body = div()
            .flex()
            .flex_wrap()
            .gap_x(px(4.))
            .child("The type changes on a table with rows.")
            .child(div().font_weight(FontWeight::SEMIBOLD).child(MAY_TRUNCATE))
            .child("Apply asks first.");
        fields.push(warning_band(body, p).into_any_element());
    }
    form_box(title, fields, sql, busy, p, cx)
}

fn index_form(
    f: &IndexForm,
    sql: &Result<Vec<String>, String>,
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
                    .clickable(cx.listener(move |this, _, _, cx| {
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
                    .clickable(cx.listener(move |this, _, _, cx| {
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
    form_box(None, fields, sql, busy, p, cx)
}

/// The Add foreign key form, as the design's `ForeignKeyCheck` lays it out: each of
/// this table's columns on one row with the column it references. `hints` are what
/// the server will likely refuse; Apply stays on.
#[allow(
    clippy::too_many_arguments,
    reason = "one form, drawn from the tab's state"
)]
fn fk_form(
    f: &FkForm,
    data: &TableStructure,
    sql: &Result<Vec<String>, String>,
    hints: &[String],
    note: Option<String>,
    busy: bool,
    p: &Palette,
    cx: &mut Context<QueryTab>,
) -> Div {
    let tables = if f.loading {
        "Loading tables…"
    } else {
        "Pick a table…"
    };
    let loaded = f.target.is_some();
    let pairs = f.pairs.iter().enumerate().map(|(i, (column, pick))| {
        let ty = data
            .columns
            .iter()
            .find(|c| &c.name == column)
            .map(|c| c.column_type.clone())
            .unwrap_or_default();
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .justify_between()
                    .gap(px(6.))
                    .h(row_height())
                    .px(px(8.))
                    .border_1()
                    .border_color(p.line2)
                    .rounded(RADIUS)
                    .bg(p.panel)
                    .font_family(MONO_FONT)
                    .text_size(data_size())
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(column.clone())
                    .child(div().text_color(p.tx3).child(ty)),
            )
            .child(icon(Icon::ArrowRight, p.tx3))
            .child(
                div().flex_1().min_w_0().child(
                    Select::new(pick)
                        .h(row_height())
                        .placeholder(if loaded {
                            "Referenced column…"
                        } else {
                            "Pick the table first"
                        })
                        .disabled(!loaded),
                ),
            )
            .child(
                button_at("pair-remove", i, cx)
                    .child("Remove")
                    .clickable(cx.listener(move |this, _, _, cx| {
                        if let Some(f) = this.fk_form_mut()
                            && i < f.pairs.len()
                        {
                            f.pairs.remove(i);
                        }
                        cx.notify();
                    })),
            )
            .into_any_element()
    });
    let small = |text: String, color| {
        div()
            .text_size(small_size())
            .text_color(color)
            .line_height(px(18.))
            .child(text)
    };
    let on_update = match &f.on_update {
        Some(u) => labeled("On update", Select::new(u).h(row_height()), 160., p).into_any_element(),
        None => small(
            "Oracle offers no action, cascade and set null, and has no on update.".into(),
            p.tx3,
        )
        .flex_grow(1.)
        .flex_basis(px(160.))
        .pt(px(20.))
        .into_any_element(),
    };
    let mut fields = vec![
        fields_row(vec![
            labeled("Name", text_field(&f.name, true), 220., p).into_any_element(),
            labeled(
                "References table",
                Select::new(&f.ref_table)
                    .h(row_height())
                    .placeholder(tables)
                    .disabled(f.loading),
                220.,
                p,
            )
            .into_any_element(),
        ]),
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .text_size(small_size())
                    .text_color(p.tx2)
                    .child("Columns, and the columns they reference, in order"),
            )
            .children(pairs)
            .child(
                div().max_w(px(280.)).child(
                    Select::new(&f.add)
                        .h(row_height())
                        .placeholder("Add a column…"),
                ),
            )
            .into_any_element(),
        fields_row(vec![
            labeled(
                "On delete",
                Select::new(&f.on_delete).h(row_height()),
                160.,
                p,
            )
            .into_any_element(),
            on_update,
        ]),
    ];
    fields.extend(
        hints
            .iter()
            .map(|h| warning_line(h.clone(), p).into_any_element()),
    );
    fields.extend(note.map(|n| small(n, p.tx3).into_any_element()));
    form_box(None, fields, sql, busy, p, cx)
}

fn check_form(
    f: &CheckForm,
    sql: &Result<Vec<String>, String>,
    busy: bool,
    p: &Palette,
    cx: &mut Context<QueryTab>,
) -> Div {
    let fields = vec![fields_row(vec![
        labeled("Name", text_field(&f.name, true), 220., p).into_any_element(),
        labeled("Expression", text_field(&f.condition, true), 360., p).into_any_element(),
    ])];
    form_box(None, fields, sql, busy, p, cx)
}

/// A Column form checkbox: `set` writes `on` into the form at `at`.
fn form_check(
    id: impl Into<ElementId>,
    label: Option<&'static str>,
    on: bool,
    at: FormAt,
    set: fn(&mut ColumnForm, bool),
    cx: &Context<QueryTab>,
) -> AnyElement {
    let check = Checkbox::new(id)
        .checked(on)
        .cursor_pointer()
        .on_click(cx.listener(move |this, checked: &bool, _, cx| {
            if let Some(f) = this.structure_mut().and_then(|s| column_form_mut(s, at)) {
                set(f, *checked);
            }
            cx.notify();
        }));
    match label {
        Some(l) => check.label(l),
        None => check,
    }
    .into_any_element()
}

/// An outline button for row `i` of a form's column list.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn key(from: &str, column: &str, on_delete: &str) -> ForeignKey {
        let t = |table: &str| TableRef {
            database: "w".into(),
            table: table.into(),
        };
        ForeignKey {
            name: format!("{from}_fk"),
            from: t(from),
            from_columns: vec![column.into()],
            to: t("orders"),
            to_columns: vec!["id".into()],
            on_delete: on_delete.into(),
            on_update: "restrict".into(),
        }
    }

    #[test]
    fn table_dialogs_list_other_tables_keys_and_preview() {
        let t = TableRef {
            database: "w".into(),
            table: "orders".into(),
        };
        let mut s = TableStructure {
            referenced_by: vec![
                key("items", "order_id", "cascade"),
                // The table's own key does not stop a drop.
                key("orders", "parent_id", "restrict"),
            ],
            ..TableStructure::default()
        };
        s.facts.rows = Some(48_200);
        let ask = table_ask(TableAction::Drop, &t, s.clone(), 0, false);
        assert!(ask.danger && ask.check.is_none());
        assert_eq!(
            ask.text.as_deref(),
            Some("Deletes the table, its indexes and about 48,200 rows. It cannot be undone.")
        );
        let (label, rows) = ask.list.clone().unwrap();
        assert_eq!(
            label,
            "1 foreign key points here. MySQL refuses the drop while they exist."
        );
        assert_eq!(
            rows,
            vec![("items.order_id".into(), "on delete cascade".into())]
        );
        let preview =
            |ask: &crate::dialog::Ask, to: &str, on: bool| (ask.preview.clone().unwrap().0)(to, on);
        assert_eq!(
            preview(&ask, "", false).unwrap(),
            "drop table `w`.`orders`;"
        );

        let ask = table_ask(TableAction::Rename, &t, s.clone(), 2, true);
        assert!(!ask.danger && ask.list.is_none());
        assert!(preview(&ask, "orders", false).is_err());
        assert_eq!(
            preview(&ask, "sales", false).unwrap(),
            "rename table `w`.`orders` to `w`.`sales`;"
        );
        assert!(
            ask.warning
                .as_deref()
                .is_some_and(|w| w.ends_with("Open editors still use the old name."))
        );
        let text = ask.text.unwrap();
        assert!(text.contains("open transaction") && text.contains("2 pending changes"));

        s.oracle = true;
        let ask = table_ask(TableAction::Drop, &t, s, 0, false);
        assert_eq!(
            ask.check.as_deref(),
            Some("Cascade constraints: also drop this key")
        );
        assert_eq!(
            preview(&ask, "", true).unwrap(),
            r#"drop table "w"."orders" cascade constraints;"#
        );
    }

    #[test]
    fn collation_lists_follow_the_charset_and_keep_the_value() {
        let list: Vec<(String, String)> = [
            ("latin1_swedish_ci", "latin1"),
            ("utf8mb4_bin", "utf8mb4"),
            ("utf8mb4_general_ci", "utf8mb4"),
        ]
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .into();
        assert_eq!(
            collation_choices(&list, Some("utf8mb4"), Some("utf8mb4_bin")),
            ["utf8mb4_bin", "utf8mb4_general_ci"]
        );
        assert_eq!(collation_choices(&list, None, None).len(), 3);
        // A value the list lacks (another server, an old name) stays first.
        assert_eq!(
            collation_choices(&list, Some("utf8mb4"), Some("utf8mb4_0900_ai_ci")),
            ["utf8mb4_0900_ai_ci", "utf8mb4_bin", "utf8mb4_general_ci"]
        );
        assert_eq!(collation_choices(&[], None, Some("x_bin")), ["x_bin"]);
        assert_eq!(charset_of(&list, "UTF8MB4_BIN"), Some("utf8mb4"));
        assert_eq!(charset_of(&list, "nope"), None);
    }

    #[test]
    fn reference_tables_of_other_schemas_are_qualified() {
        let t = |database: &str, table: &str| TableRef {
            database: database.into(),
            table: table.into(),
        };
        assert_eq!(table_label(&t("shop", "orders"), "shop"), "orders");
        assert_eq!(
            table_label(&t("billing", "invoices"), "shop"),
            "billing.invoices"
        );
    }

    #[test]
    fn primary_key_order_is_its_own() {
        let rows = |order: &[u32]| -> Vec<(u32, String)> {
            order.iter().map(|&i| (i, format!("c{i}"))).collect()
        };
        let mut key = vec![3, 1];
        assert_eq!(key_names(&rows(&[1, 2, 3]), &key), ["c3", "c1"]);
        // Moving rows does not move the key.
        assert_eq!(key_names(&rows(&[3, 2, 1]), &key), ["c3", "c1"]);
        move_key(&mut key, 1, true);
        assert_eq!(key, [1, 3]);
        move_key(&mut key, 0, true);
        move_key(&mut key, 1, false);
        move_key(&mut key, 9, true);
        assert_eq!(key, [1, 3]);
        move_key(&mut key, 0, false);
        assert_eq!(key_names(&rows(&[1, 2, 3]), &key), ["c3", "c1"]);
        // A removed row leaves the key.
        assert_eq!(key_names(&rows(&[1, 2]), &key), ["c1"]);
    }

    #[test]
    fn column_forms_with_typing_ask_before_closing() {
        let before = NewColumn {
            name: "total".into(),
            data_type: "int".into(),
            ..NewColumn::default()
        };
        assert!(!column_typed(&before.clone(), Some(&before)));
        let renamed = NewColumn {
            name: "sum".into(),
            ..before.clone()
        };
        assert!(column_typed(&renamed, Some(&before)));
        assert!(!column_typed(&NewColumn::default(), None));
        assert!(!column_typed(
            &NewColumn {
                name: "  ".into(),
                ..NewColumn::default()
            },
            None
        ));
        assert!(column_typed(&renamed, None));
    }

    #[test]
    fn keys_name_the_tables_they_point_to_and_from() {
        let t = |table: &str| TableRef {
            database: "gui".into(),
            table: table.into(),
        };
        let key = |from: &str, to: &str| structure::ForeignKey {
            name: "k".into(),
            from: t(from),
            from_columns: vec!["a".into()],
            to: t(to),
            to_columns: vec!["b".into()],
            on_delete: "restrict".into(),
            on_update: "restrict".into(),
        };
        let d = TableStructure {
            foreign_keys: vec![key("orders", "customers")],
            referenced_by: vec![key("items", "orders")],
            ..TableStructure::default()
        };
        assert!(keys_name(&d, &t("customers")));
        assert!(keys_name(&d, &t("items")));
        assert!(!keys_name(&d, &t("audit_log")));
    }

    #[test]
    fn previews_break_long_statements_outside_quotes() {
        let sql = [
            "alter table `gui`.`orders` change column `total` `total` decimal(12,2) null \
             default 0.00 comment 'Order total, it''s \\' kept' after `customer_id`"
                .to_owned(),
            "comment on column t.c is 'x'".to_owned(),
            "create table t (\n  `a` int,\n  `b` int\n)".to_owned(),
        ];
        assert_eq!(
            wrap_statements(&sql, 40),
            "alter table `gui`.`orders` change column\n  `total` `total` decimal(12,2) null\n  \
             default 0.00 comment\n  'Order total, it''s \\' kept' after\n  `customer_id`;\n\
             comment on column t.c is 'x';\ncreate table t (\n  `a` int,\n  `b` int\n);"
        );
    }

    #[test]
    fn key_and_check_forms_with_typing_ask_before_closing() {
        let texts = |t: &[&str]| t.iter().map(|&t| t.to_owned()).collect::<Vec<_>>();
        assert!(!form_typed(&texts(&["", " "]), false));
        assert!(form_typed(&texts(&["", "qty >= 0"]), false));
        assert!(form_typed(&texts(&["fk_orders"]), false));
        // A picked column or referenced table counts as typing.
        assert!(form_typed(&texts(&[""]), true));
    }

    #[test]
    fn partial_failure_names_what_ran_and_what_failed() {
        let labels = ["rename column", "modify", "comment"].map(str::to_owned);
        assert_eq!(
            partial_failure(&labels, 1, "ORA-54031: in use"),
            "Ran 1 of 3 (rename column). Statement 2 (modify) failed: ORA-54031: in use"
        );
        assert_eq!(
            partial_failure(&labels, 2, "e"),
            "Ran 2 of 3 (rename column, modify). Statement 3 (comment) failed: e"
        );
    }
}
