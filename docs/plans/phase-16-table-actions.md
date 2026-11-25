# Phase 16 — Table actions: rename, drop, truncate (S)

Brief for the implementer. Scope is PLAN.md "Milestone 6 → Phase 16". Do not widen it. Starts from the finished Phase 15 (`run_ddl(Vec<String>)`, `TableStructure.mariadb`, `menu_item_disabled`, read-only disabling).
Before writing Rust, load `.claude/skills/rust-skills/SKILL.md` and follow it.
Design: artboard **`TableActions`** on https://claude.ai/artifact/RGYCGMf4iayRAkej7r2wns (table context menu, Rename / Drop / Truncate dialogs). The artboard decides looks; this file decides behaviour.

## 1. Goal and done-when

- Context menu on sidebar **table** nodes (none today: `sidebar.rs` returns early at ≈l. 846) and on the table tab (after "Import CSV…"): `Rename…`, `Truncate…`, `Drop…`. Disabled on read-only connections.
- Truncate and Drop are always confirmed as danger, showing the statement, and list the tables whose foreign keys point here (`referenced_by`).
- Afterwards: sidebar node reloads; rename retitles the open tab, drop closes it; the table's entry in `.deadlock/diagrams.json` is renamed or removed (and an open diagram of that schema reloads). Rename warns that views and routines using the old name break (Oracle marks them invalid).
- **Done when:** all three work from both menus on MySQL 8.4, MariaDB 11 and Oracle Free, and no tab or diagram card is left pointing at a missing table.

## 2. Design of the flow (one path for both menus)

Both menus call `Shell::table_action(conn, TableRef, TableAction, window, cx)`, which opens (or brings forward) the table's tab on its Structure view with the existing `open_table(conn, table, View::Structure, …)`, then `tab.update(cx, |t, cx| t.start_table_action(action, window, cx))`. This is the same "open the editor, then act" pattern `object_menu` uses for routines (`open_object(.., Some(Then::Drop))`), and it reuses the tab's session, `after_discard` (pending data edits), `run_ddl` → `confirm` (production, implicit commit, forced danger) and the loaded `TableStructure` (for `oracle`, `referenced_by`).

The action is a form in the Structure view (`Form::Table`), rendered above the Columns section with the existing `form_box` (fields + live `preview` + Cancel/Apply). Apply → `run_ddl(vec![sql], forced, …)` → the danger dialog. No change to `dialog::Ask` is needed. If `TableActions` shows these as modal dialogs with a text field / checkbox, still build them as this in-view form and say so in the report (extending `dialog::Ask` with inputs is out of scope).

## 3. File-by-file changes

### `crates/deadlock-core/src/structure.rs`
```rust
/// `rename table a.b to a.c` (MySQL, MariaDB) or `alter table s.t rename to n` (Oracle).
/// # Errors  An empty name, the same name, or text that ends the statement.
pub fn rename_table_sql(t: &TableRef, to: &str, s: &TableStructure) -> Result<String, String>;
/// `truncate table a.b`.
pub fn truncate_table_sql(t: &TableRef, s: &TableStructure) -> String;
/// `drop table a.b`; Oracle with `cascade` → `drop table s.t cascade constraints`.
pub fn drop_table_sql(t: &TableRef, cascade: bool, s: &TableStructure) -> String;
```
Use `s.qualified(t)` / `s.ident(..)` (both private methods today, same module: fine). `to` is trimmed; Oracle's new name is **not** schema-qualified (ORA-14047). `cascade` is ignored off Oracle. Run `one_statement_in(.., engine)` on the rename.

### `crates/deadlock-core/src/diagram.rs`
```rust
/// Renames (`to = Some`) or removes (`None`) table `from` in the saved layout of `schema` on
/// `connection`. Nothing saved, or no such card: no write.
/// # Errors  `.deadlock/diagrams.json` cannot be written.
pub fn rename_table(root: &Path, connection: &str, schema: &str, from: &str, to: Option<&str>) -> Result<()>;
```
Same `read_json` / `write_json` as `save`.

### `crates/deadlock/src/diagram.rs`
`impl DiagramTab { pub fn table_changed(&mut self, from: &str, to: Option<&str>, cx: &mut Context<Self>) }`: `self.save_now()` (flush pending moves), `diagram::rename_table(&self.root, &self.connection.id, &self.schema, from, to)` (`eprintln!("deadlock: {e}")` on error, like `save_now`), then `self.reload(cx)`.

### `crates/deadlock/src/query.rs`
- `TabEvent` gains `TableRenamed(TableRef, TableRef)` (from, to) and `TableDropped(TableRef)`.
- `mod structure` is private: add `pub(crate) use structure::TableAction;` next to the `pub use table::{…}` line so the shell can name it.

### `crates/deadlock/src/query/structure.rs`
- `pub(crate) enum TableAction { Rename, Truncate, Drop }`.
- `Form::Table(TableForm { action: TableAction, name: Option<Entity<InputState>> /* Rename */, cascade: bool /* Oracle Drop */ })`.
- `pub(crate) fn start_table_action(&mut self, action, window, cx)`: build the form (name input prefilled with the current name via `default_value`, focused, Enter = Apply like `object.rs::start_rename`), `form_subs`, `s.notice = None`. Works before the structure has loaded: the form shows once `data` arrives (render already waits for it). If `data.view` → notice "Views are renamed and dropped from the sidebar's view menu." and no form.
- `form_sql` arm: Rename → `rename_table_sql`; Truncate → `truncate_table_sql`; Drop → `drop_table_sql(.., f.cascade, ..)`.
- `apply_form` for `Form::Table`: `forced` = 
  - Truncate: `"Deletes every row of {table}. It cannot be rolled back."` + refs line;
  - Drop: `"Drops table {table} and every row in it."` + (Oracle) `" Oracle keeps it in the recycle bin (FLASHBACK TABLE … TO BEFORE DROP)."` + refs line;
  - Rename: `None` (the warning is in the form; production/implicit-commit prompts still apply).
  - refs line, when `referenced_by` has keys from **other** tables: `"Referenced by {t1} ({fk1}), {t2} ({fk2}): the server refuses this while those keys exist."`; Oracle Drop with `cascade`: `"…: cascade constraints drops those keys."`.
- `run_ddl` gains `after: After` (replace `done: &'static str` by it or add it next to `done`): `enum After { Reload, Renamed(String), Dropped }`. Existing callers pass `After::Reload`.
- `ddl_done` on success:
  - `Reload` → as today (truncate uses it: structure facts and Data grid reload via `columns_changed`).
  - `Renamed(to)` → `let from = t.table.clone(); t.table.table = to; self.kind = Kind::Table(t.table.clone());` (same move as `key.rs::renamed`), emit `TableRenamed(from, new)`, then the Reload path (structure, `columns_changed`, `SchemaChanged`), emit `Changed`.
  - `Dropped` → emit `SchemaChanged(database)` and `TableDropped(t.table.clone())`; no reload.
- Form fields per action (render with `labeled`, `text_field`, `Checkbox`, hint lines in `p.tx3`):
  - Rename: "New name"; hint "Views, routines and triggers in other tables that use the old name stop working." + Oracle: " Oracle marks them invalid." (MySQL keeps the table's own triggers, FKs and grants.)
  - Truncate: the refs hint, no fields.
  - Drop: the refs hint; Oracle only: Checkbox "Cascade constraints" (shown when `referenced_by` has other tables).

### `crates/deadlock/src/shell/objects.rs` (object menus live here)
- `pub(super) fn table_menu(&self, row: &Row, menu: PopupMenu, shell: Entity<Shell>, cx: &App) -> PopupMenu`: for `row.id = [conn, schema, "Tables", table]`, items `Rename…` (`Icon::Rename`), `Truncate…` (`Icon::Minus`), separator, `Drop…` (`menu_item_danger`, `Icon::Trash`), each `.disabled(read_only)` (`self.config(conn).is_some_and(|c| c.read_only)`), on click `s.table_action(&conn, TableRef { database: schema, table }, action, window, cx)`.
- `pub(super) fn table_action(&mut self, conn: &str, table: TableRef, action: TableAction, window, cx)`: `self.open_table(conn, table.clone(), View::Structure, window, cx)`, find the tab (same predicate as `open_table`), `tab.update(cx, |t, cx| t.start_table_action(action, window, cx))`.
- `fn table_changed(&mut self, conn: &str, from: &TableRef, to: Option<&str>, cx)`: if a diagram tab of `conn` shows `from.database` → `tab.update(|d, cx| d.table_changed(&from.table, to, cx))`; else `deadlock_core::diagram::rename_table(self.project.root(), conn, &from.database, &from.table, to)` (`eprintln!` on error).

### `crates/deadlock/src/shell/sidebar.rs` (≈l. 843–882)
- `let table_menu = row.kind == RowKind::Node(NodeKind::Table) && row.id.len() == 4 && row.id[2] == "Tables" && shell.flavor(&row.id[0]).is_some();` Include it in the early-return condition and, inside `context_menu`, chain `shell_entity.read(cx).table_menu(&row, menu, shell_entity.clone(), cx)` after the object menu.

### `crates/deadlock/src/shell/tabs.rs`
- `on_tab_event`: `TableRenamed(from, to)` → `self.table_changed(&conn, from, Some(&to.table), cx); self.schedule_save(cx); cx.notify()`; `TableDropped(t)` → `self.table_changed(&conn, t, None, cx)`, then `if let Some(i) = self.tabs.iter().position(|(x, _)| x == tab) { self.remove_tab(i, cx) }` (no prompts: pending edits were settled by `after_discard`, and the DDL committed).
- Tab menu (≈l. 994): keep `Import CSV…`, then separator and the same three items (shared builder: call `shell.read(cx).table_menu_items(..)` or inline three `menu_item`s calling `s.table_action`), disabled when `c.read_only`. Redis key tabs (`key`) keep only what they have today (nothing).

## 4. SQL per engine

| | MySQL 8.4 | MariaDB 11 | Oracle Free 23 |
|---|---|---|---|
| Rename | `` rename table `d`.`a` to `d`.`b` `` | same | `alter table "S"."A" rename to "B"` |
| Truncate | `` truncate table `d`.`a` `` (error 1701 if another table's FK references it) | same | `truncate table "S"."A"` (ORA-02266 if referenced by enabled FKs) |
| Drop | `` drop table `d`.`a` `` (error 3730 if referenced) | same (error 1451) | `drop table "S"."A"` → recycle bin (RECYCLEBIN on by default); `… cascade constraints` drops the referencing FKs (else ORA-02449) |

All are DDL with an implicit commit (classified by `confirm`). Server errors are shown as they are. Not offered: `purge`, `truncate … cascade` (Oracle), cross-schema rename, `if exists`.

## 5. Tests

Unit (`cargo test -p deadlock-core`):
- structure.rs `table_actions_sql`: MySQL rename quotes both sides (`` `my db`.`or``ders` ``), empty / same / `;` name → Err; Oracle rename target unqualified; truncate; drop; Oracle drop with and without cascade; cascade ignored on MySQL.
- diagram.rs `renamed_and_dropped_tables_follow_in_saved_layouts` (tempdir, like `layouts_save_per_connection_and_schema`): rename keeps x/y, removal drops the card, other schemas and connections untouched, missing file → `Ok` and no file written.

Integration (ignored; `docker compose up -d --wait mysql mariadb oracle && cargo test -p deadlock-core --test engines -- --ignored table_actions`):
- `mod table_structure`: `fn table_actions(engine, port)` + `mysql_table_actions` / `mariadb_table_actions`: tables `p16_parent` (1 row) and `p16_child` referencing it. Truncate parent → Err (server refuses: referenced); truncate child → 0 rows; rename child → `p16_kid` and re-read structure (FK still there); drop parent → Err; drop kid then parent → ok, `structure()` of each now errors.
- `mod oracle`: `oracle_table_actions`: same flow; drop parent with `cascade = true` succeeds while the child still exists and the child's FK is gone; cleanup with `PURGE`.
- CI set: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- Manual (`cargo run -p deadlock -- <project>`): each action from the sidebar menu and from the tab menu on the three engines; an open diagram of the schema follows a rename and a drop; a read-only connection shows the items disabled; restart and check `state.json` restores the renamed tab, not the old one.

## 6. Ordered tasks

1. Load the rust-skills skill; read `TableActions` on the canvas.
2. structure.rs: the three SQL functions + unit test.
3. core diagram.rs: `rename_table` + unit test.
4. engines.rs: `table_actions` (MySQL, MariaDB) and `oracle_table_actions`; run against compose.
5. query.rs: the two `TabEvent` variants.
6. query/structure.rs: `TableAction`, `Form::Table`, `start_table_action`, form_sql/apply_form arms, `After` through `run_ddl` / `ddl_done`, form render.
7. app diagram.rs: `DiagramTab::table_changed`.
8. shell: `table_action`, `table_changed`, `table_menu`; sidebar hook; tab menu items; `on_tab_event` arms.
9. CI commands; manual check on the three engines.

## 7. Do not

- No view, routine or Redis key actions (views keep their object menu); no Oracle `purge` / flashback UI; no multi-table actions.
- No changes to `dialog::Ask`, no new dependencies, no new icons.
- No commits, no edits to PLAN.md.

## 8. Risks / open questions

- If the artboard demands true modal dialogs (with a name field / checkbox), this plan's in-view form deviates; the parent must accept it or approve extending `dialog::Ask`.
- Opening the table tab to act on it means a sidebar Drop first opens a tab (and a session) that then closes; accepted because it reuses `confirm`, `after_discard` and the structure for `referenced_by`.
- Object tabs of the table's triggers and SQL text in editors still name the old table after a rename; out of scope (PLAN only covers table tabs and diagram cards).

## 9. Decisions after design review (parent, 2026-11-25)

- **Real modal dialogs, as `TableActions` shows.** This overrides §2 and §7. Extend `dialog::Ask` minimally with one optional single-line text field (Rename: the new name, with a live statement preview and the button disabled until the name is new and not empty) and one optional checkbox (Oracle Drop: "Cascade constraints", unchecked by default, shown only when keys point here). Keep the opening-the-tab flow from §2 so `after_discard`, `confirm` and `referenced_by` are reused. The Drop and Truncate dialogs list `referenced_by` as the artboard shows.
- Row counts in the dialogs come from `TableFacts` estimates ("about N rows"), never `count(*)`.
- Only Drop table… is red in the menus.
