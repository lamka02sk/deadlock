# Phase 18 — Create table (M)

Brief for the implementer. Scope is PLAN.md "Milestone 6 → Phase 18". Do not widen it. Starts from finished Phases 15–17: `column_definition()`, `NewColumn` (with `on_update`, `collation`, `Default`), `ColumnForm` built by `new_column_form`, `run_ddl(Vec<String>, forced, After)`, `After::Renamed` retitling a tab, read-only disabling.
Before writing Rust, load `.claude/skills/rust-skills/SKILL.md` and follow it.
Design: artboard **`NewTable`** on https://claude.ai/artifact/RGYCGMf4iayRAkej7r2wns (the New table draft tab). The artboard decides looks; this file decides behaviour.

## 1. Goal and done-when

- "New table…" in the database node menu and on the Tables group (`object_menu` already has the `New …` pattern) opens a table tab in **draft** state: a Structure view with Name, a column list (one Column form row per column, `column_definition` underneath), primary key columns, table Comment on every engine, and on MySQL / MariaDB Engine and Collation. The SQL preview updates live; Create runs it. The tab then becomes a normal table tab (indexes and FKs come from the Phase 15–17 forms).
- **Done when:** a table with 5 columns, a composite primary key, a default, an `on update` timestamp (MySQL / MariaDB) and comments is created on MySQL 8.4, MariaDB 11 and Oracle Free, and the tab opens with matching structure.

## 2. File-by-file changes

### `crates/deadlock-core/src/structure.rs`
```rust
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewTable {
    pub name: String,
    pub columns: Vec<NewColumn>,      // position is ignored: list order is column order
    pub primary_key: Vec<String>,     // column names, in column order
    pub engine: Option<String>,       // MySQL / MariaDB; None = server default
    pub collation: Option<String>,    // MySQL / MariaDB; None = schema default
    pub comment: String,
}

/// The engines the form offers; MariaDB adds `Aria`.
pub fn table_engines(mariadb: bool) -> Vec<&'static str>; // ["InnoDB", "MyISAM", "MEMORY", "ARCHIVE", "CSV"] (+ "Aria")

/// `create table …` and, on Oracle, its `comment on` statements.
/// # Errors  No name, no columns, an empty or repeated column name, a primary key column
/// that is not in the list, an auto_increment column outside the primary key (MySQL),
/// `on update` on Oracle, engine/collation names that are not `[A-Za-z0-9_]+`, or text
/// that ends a statement. Column errors are prefixed with the column: `qty: choose a type`.
pub fn create_table_sql(database: &str, t: &NewTable, s: &TableStructure) -> Result<Vec<String>, String>;
```
`s` only carries the flags (`oracle`, `mariadb`, `backslash_escapes`); the caller builds it. Each column: `column_definition(c, None, s)?` (no key: the table-level `primary key (…)` covers auto_increment). Run `one_statement_in` on each statement.

### `crates/deadlock/src/query/structure.rs`
- `struct Draft { name: Entity<InputState>, engine: Option<Choice> /* MySQL */, collation: Entity<InputState>, comment: Entity<InputState>, columns: Vec<DraftColumn> }`, `struct DraftColumn { form: ColumnForm, primary: bool }`. `Structure` gains `draft: Option<Draft>` and `pub(super) fn is_draft(&self) -> bool`.
- `ColumnForm` rows for the draft come from `new_column_form(None, …)` with no position (make `positions` empty and skip the Position field when empty).
- Toggles (`Nullable`, `Auto increment`, `On update`, `PK`) currently write into `Form::Column`; make the setter target generic: `enum FormAt { Form, Draft(usize) }` + `fn column_form_mut(s: &mut Structure, at: FormAt) -> Option<&mut ColumnForm>`, used by the `toggle` closure in `column_form` / the draft row renderer.
- `pub(crate) fn start_draft(&mut self, window, cx)`: `t.view = View::Structure`; `s.data = None`; `s.draft = Some(Draft { … one empty column row … })`; focus Name; subscribe every input/choice with `form_subs` so the preview follows typing.
- Actions: `Add column` (push a `DraftColumn`), per row `Remove`, `PK` checkbox. `primary_key` = names of rows with `primary`, in row order.
- `draft_sql(&self, cx) -> Option<Result<Vec<String>, String>>`: build `NewTable` from the fields (empty Engine/Collation → `None`), flags `TableStructure { oracle: self.oracle(), mariadb: self.mariadb(), backslash_escapes: true, ..Default::default() }`, call `create_table_sql(&t.table.database, …)`.
- Create → `run_ddl(stmts, None, "Table created.", After::Created(name), …)`. `After::Created(name)` in `ddl_done` success: `t.table.table = name.trim()`, `self.kind = Kind::Table(t.table.clone())`, `s.draft = None`, then the Reload path (`load_structure`, `columns_changed`, `SchemaChanged`) and emit `Changed`. On error the draft stays as typed.
- Render: `structure::render` early branch for drafts: toolbar "New table in {schema}", the notes (read-only note, notice), then the draft per `NewTable`: Name; table fields (Engine `Select` and Collation on MySQL / MariaDB; Comment always); the column rows (compact row per column: Name, Type, Length, Default, Collation (MySQL, textual types), Nullable, PK, Auto increment, On update (MySQL, datetime/timestamp), Comment, Remove), `Add column`; `preview`; no Cancel (closing the tab discards the draft) and **Create** (primary, disabled while busy, read-only or the SQL is an Err). `form_box` gets the button label as a parameter (`"Apply"` everywhere else).

### `crates/deadlock/src/query/table.rs`
- `open_table`: for a draft, do not load anything; only ask for a session: `if t.structure.is_draft() { if matches!(self.session, Session::None) && !t.waiting { t.waiting = true; cx.emit(TabEvent::NeedSession) } return; }`.
- `table_session`: for a draft, clear `waiting`, keep any error as `structure.notice`, `cx.notify()`, return (no `load_view`).
- `render` (l. 2043–2050): a draft renders `structure::render(this, div(), cx)` (no Data/Structure switch).

### `crates/deadlock/src/shell/objects.rs`
- `has_object_menu`: `RowKind::Node(NodeKind::Group) => new_kind(&row.id).is_some() || is_tables_group(&row.id)` with `fn is_tables_group(id) -> bool { matches!(id, [_, _, g] if g == "Tables") }` (unit test next to `group_rows_offer_their_kind`).
- `object_menu`: Database branch: `New table…` (`Icon::Plus`, `.disabled(read_only)`) before the managed kinds; Group branch: for the Tables group the same item.
- `pub(super) fn new_table(&mut self, conn: &str, schema: &str, window, cx)`: `let Some(binding) = self.binding(Some(conn))`; `let i = self.add_tab(Kind::Table(TableRef { database: schema.into(), table: String::new() }), Some(binding), String::new(), window, cx)`; `self.tabs[i].0.update(cx, |t, cx| t.start_draft(window, cx))`; `self.show_tab(Front::Query(i), window, cx)`.

### `crates/deadlock/src/shell/tabs.rs`
- `tab_title`: `Kind::Table(r) if r.table.is_empty() => "New table".into()`; the tooltip (≈l. 928) reads `New table in {schema} · {connection}` for drafts.
- `sync_tabs`: skip draft tabs (`Kind::Table(r) if r.table.is_empty()`) so `state.json` never restores a nameless table tab (`filter_map`).
- Tab context menu: none for drafts (no Import CSV, no table actions).

## 3. SQL per engine

MySQL 8.4 / MariaDB 11 (one statement; newline + two-space indent so the preview reads like the Definition pane):
```sql
create table `shop`.`orders` (
  `id` bigint unsigned not null auto_increment comment 'row id',
  `line` int not null,
  `status` varchar(20) collate utf8mb4_bin not null default 'new',
  `created` datetime(3) not null default current_timestamp(3) on update current_timestamp(3),
  `note` text null comment 'free text',
  primary key (`id`, `line`)
) engine = InnoDB collate = utf8mb4_bin comment = 'orders'
```
- Table options only when set; `collate = X` alone sets the matching charset. No `primary key (…)` line when the list is empty.
- InnoDB needs the auto_increment column to be the **first** column of some index: with a composite PK, put it first or the server refuses (error 1075) — say so in the Err only if it is not in the PK at all; ordering is the server's call.

Oracle Free 23:
```sql
create table "APP"."orders" (
  "id" number generated by default as identity not null,
  "line" number(10) not null,
  "status" varchar2(20) default 'new' not null,
  "created" timestamp(3) default systimestamp not null,
  "note" clob,
  primary key ("id", "line")
)
comment on table "APP"."orders" is 'orders'
comment on column "APP"."orders"."id" is 'row id'
```
- Identity: `generated by default as identity` (12c+). `on update` → Err ("Oracle has no ON UPDATE: use a trigger"); Engine/Collation fields hidden. Names are quoted as typed, like Add column (so `orders` is lower-case and needs quotes later: see §8).

## 4. UI steps

1. Menu items through the existing `new_item`-style builder in `object_menu` (`menu_item(.., Icon::Plus).disabled(read_only)`).
2. Draft tab per `NewTable`: reuse `labeled`, `text_field`, `Select`, `Checkbox`, `button_at` (Remove), the `add(..)` icon button (Add column), `preview`, `form_box`'s button row with label "Create".
3. After Create: the same tab shows the normal Structure view of the new table (Data/Structure switch back), the sidebar schema reloads (`SchemaChanged`), and `state.json` now saves it.

## 5. Tests

Unit (`cargo test -p deadlock-core`):
- structure.rs `create_table_mysql`: the 5-column example above, byte-exact; `backslash_escapes` honoured in comments; no options → no trailing options.
- `create_table_oracle`: identity column, composite PK, comment statements in order (table first, then columns in order), only for non-empty comments.
- `create_table_refuses_bad_input`: no name, no columns, duplicate column, PK column not in list, MySQL AI column outside PK, Oracle `on_update`, engine `InnoDB; drop`, a column error prefixed with its name.
- objects.rs: `is_tables_group`.

Integration (ignored; `docker compose up -d --wait mysql mariadb oracle && cargo test -p deadlock-core --test engines -- --ignored create_table`):
- `mod table_structure`: `fn create_table(engine, port)` + `mysql_create_table` / `mariadb_create_table`: `DROP TABLE IF EXISTS p18_new`; run every statement of `create_table_sql` for the §3 MySQL table (name `p18_new`); re-read: 5 columns in order, PK `id, line` (`indexes[0].primary`, parts), `status` default `'new'` and collation `utf8mb4_bin`, `created.on_update` set with fsp 3, comments on `id` and `note`, `facts.comment == "orders"`, `facts.engine == Some("InnoDB")`. Drop it.
- `mod oracle`: `oracle_create_table`: the §3 Oracle table as `p18_new` (quoted lower-case: re-read with `TableRef { database: "DEADLOCK", table: "p18_new" }`); identity in `extra`, PK parts, default, comments; `DROP TABLE "p18_new" PURGE`.
- CI set: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- Manual (`cargo run -p deadlock -- <project>`): New table from the database menu and from the Tables group on each engine; Create; the tab turns into the table; add an index and an FK from the Structure view; restart: the created table's tab restores, a draft does not.

## 6. Ordered tasks

1. Load the rust-skills skill; read `NewTable` on the canvas.
2. structure.rs: `NewTable`, `table_engines`, `create_table_sql`; unit tests.
3. engines.rs: `create_table` (MySQL, MariaDB) and `oracle_create_table`; run against compose.
4. query/structure.rs: `FormAt` toggles; `Draft`; `start_draft`; `draft_sql`; draft render; `form_box` button label; `After::Created`.
5. query/table.rs: draft handling in `open_table`, `table_session`, `render`.
6. shell: `is_tables_group`, menu items, `new_table`; tabs.rs title/tooltip/`sync_tabs`.
7. CI commands; manual check on the three engines.

## 7. Do not

- No indexes, FKs, checks, partitions or generated columns inside the draft (added afterwards with the Phase 15–17 forms); no MySQL table options beyond engine / collation / comment; no Oracle tablespace or storage options; no `create table … like` / `as select`.
- No new dependencies; no commits; no edits to PLAN.md.

## 8. Risks / open questions

- The draft has no session-derived `sql_mode`, so string literals are quoted as if backslash escapes are on (`backslash_escapes: true`); on a server with `NO_BACKSLASH_ESCAPES` a `\` in a default or comment doubles. Rare; flagged rather than adding a query.
- Closing a draft tab discards what was typed without asking (`close_tab_then` never asks for table tabs). Adding a Discard prompt is a small extra the parent may want.
- Oracle names are quoted as typed (house convention from Add column), so lower-case names stay case-sensitive; upper-casing them would be a behaviour change to decide at milestone level.

## 9. Decisions after design review (parent, 2026-11-25)

- Follow `NewTable`: a footer with Discard and Create, and closing a draft tab with anything typed **asks before discarding** (this overrides §8's "discards without asking").
- Row order is the column order (Move up / Move down in the row menu); there is no Position field.
- A missing primary key is a note, not a blocker.
