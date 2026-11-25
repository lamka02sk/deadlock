# Deadlock — build plan

Database IDE for MySQL, MariaDB, Oracle and Redis. Rust, native UI.
Design: https://claude.ai/artifact/RGYCGMf4iayRAkej7r2wns (8 screens × light/dark, plus Toolbar, Sidebar, StatusBar components and the app icon).

Phases run back to back. Each one ends in a build you can use, and the next phase starts from it.

## Decisions

| Topic | Decision | Why |
|---|---|---|
| UI framework | **GPUI + gpui-component**, both pinned to a git rev | Ships the three hard widgets: virtualized table, tree-sitter code editor, dock/tabs/tree. iced 0.14 has multi-window, but the data grid and editor would be ours to write. |
| Fallback | iced, decided at the end of Phase 0 only | The Phase 0 spike is the gate. No switching after it. |
| Crates | 2 to start: `deadlock-core` (no UI deps), `deadlock` (GPUI app) | Keeps the core testable headless and makes the UI replaceable. Split drivers out only if compile times force it. |
| Async | `deadlock-core` owns a tokio runtime; UI talks to it over channels | DB drivers need tokio, GPUI has its own executor. The UI thread never blocks on I/O. |
| MySQL + MariaDB | One driver (`mysql_async`), flavor detected from the server version | Same wire protocol. Differences live in a small dialect table. |
| Oracle | Pure-Rust thin driver, chosen by spike in Phase 2: `oracledb` (26.0.0-beta.1) vs `oracle-rs`; `oracle` (ODPI-C) is the fallback | Thin means no Instant Client to install. Both thin options are young. |
| Redis | `redis` crate | Standard, async, RESP3. |
| Secrets | OS keychain (`keyring`), never in project files | Design: "Keep the password in the system keychain". |
| Local data | SQLite (`rusqlite`) for query history and schema cache | Both need search. |
| Other | `russh` (SSH tunnel), `sqlformat` (Format), `tree-sitter-sequel` (highlighting) | |

### Project and windows

- A **project is a folder**. `.deadlock/project.toml` holds connections without secrets, `.deadlock/consoles/` holds console buffers, `.deadlock/state.json` holds open tabs and layout.
- One process, many windows. A registry maps canonical project path → window.
- Opening a project that is already open **focuses its window**. A second `deadlock <path>` launch forwards the path to the running process over a local socket and exits.

### Driver boundary

```rust
trait Driver {
    fn capabilities(&self) -> Capabilities;              // tx, explain, structure, sessions, config, diagram
    async fn execute(&self, text: &str) -> ResultStream; // rows arrive in pages
    async fn cancel(&self) -> Result<()>;
    async fn children(&self, node: &ObjectPath) -> Result<Vec<ObjectNode>>; // lazy sidebar tree
}
```

Toolbar buttons enable and disable from `capabilities()` and the active tab, the same way the design's toolbar `context` (query / data / none) does. Redis reports no tx, explain, structure or diagram.

## Design gaps to close

The mock uses PostgreSQL sample data. Every feature below maps to the engine's own equivalent. Each gap is added to the design canvas and reviewed before its phase starts.

| Gap | Needed by |
|---|---|
| **Sidebar search icon** (next to "+") | Phase 3 |
| **Console button** in the toolbar, and **inline results** under a statement | Phase 4 |
| Project picker / welcome window, project name in the title bar | Phase 1 |
| Connection type list is PostgreSQL, SQLite, SQL Server, ClickHouse, MongoDB… → MySQL, MariaDB, Oracle, Redis. Oracle needs service name / SID, Redis needs DB index | Phase 2 |
| No project files list in the sidebar (only Connections). v1 uses Open file, Ctrl+O | Phase 4 |
| History panel, Tx mode menu, context menus, error and empty states | Phases 4–5 |
| **Redis has no screens**: key browser, value viewers | Phase 12 |
| Import / Export dialogs, settings | Phase 14 |

Designed and approved 2026-11-22: console button, sidebar search icon, open-project window (`Welcome` artboard). Published 2026-11-22, awaiting review: four-engine type list on New connection, sidebar search open state (`SidebarSearch` artboard). Still to design: the console tab and inline results (Phase 4, after the Phase 0 answer), Redis screens (Phase 12).

## Phases

Size: S ≈ days, M ≈ 1–2 weeks, L ≈ 3+ weeks, for one developer.

### Milestone 1 — Foundation

**Phase 0 — Spike and skeleton (S, gate)**
- Workspace with the two crates, CI (fmt, clippy, test), `docker-compose` with MySQL 8.4, MariaDB 11, Oracle Database Free, Redis 8.
- Spike in GPUI: 1M-row virtual table, 50k-line SQL editor with highlighting, two windows, **a grid embedded between editor lines**.
- Port design tokens (light/dark colors, Inter 13, JetBrains Mono 12.5, radius 6, heights 44/36/32/28), bundle both fonts, export the design's SVG icons.
- Done when: the spike scrolls smoothly, and the inline-result question has an answer: supported, patchable upstream, or use the fallback (results in a split directly under the editor, one tab per statement, gutter marker on each statement).

**Phase 1 — Shell, projects, windows (M)**
- Static shell from the design: toolbar, 264 px sidebar, tab strip, status bar, light/dark following the OS.
- Project model, welcome window with recent projects, File → Open project.
- Multi-window: one window per project, single-instance socket, focus-if-open.
- Restore window bounds and open tabs per project.
- Done when: two projects are open in two windows, and opening one of them again focuses its window instead of creating a third.

### Milestone 2 — SQL core on MySQL and MariaDB

**Phase 2 — Connections (M)**
- `Driver` trait and value model. MySQL/MariaDB driver.
- New connection tab from the design: name, host, port, database, user, password to keychain, environment, encryption mode, read-only, SSH tunnel, address preview, Test connection.
- Connection switcher in the toolbar, connection state in the status bar.
- Oracle driver spike, then a headless connect-and-select smoke test for **all four engines**. This proves the trait before any UI depends on it.
- Done when: MySQL and MariaDB connect from the UI, and `cargo test` connects to all four in Docker.

**Phase 3 — Schema browser and search (M)**
- Lazy tree: connection → database → Tables / Views / Routines → columns with PK/FK icons and types. Counts on groups.
- Refresh (F5). Schema cache in SQLite so the tree paints before the server answers.
- **Search**: icon in the sidebar header opens a filter field. Fuzzy match on names, matches expand automatically, cached objects are included even when their node was never opened, Esc clears.
- Done when: a 5 000-table schema opens and filters without a visible stall.

**Phase 4 — Query workspace, console, inline results (L)**
- SQL file tabs: open, save, unsaved dot, line/column in the status bar, Format.
- Statement splitter per dialect (strings, comments, backticks, `DELIMITER`). Unit-tested.
- **Run parts**: Ctrl+Enter runs the selection, or the statement under the caret. A gutter run icon sits on each statement. Run file runs everything. Stop cancels with `KILL QUERY` on a side connection.
- Results: virtual grid, streamed pages, `null` styling, Result / Messages tabs, "214 rows in 38 ms". Multiple result sets.
- **Inline results**: each result renders under its statement, collapsible, with a setting to send results to the bottom panel instead.
- **Console**: toolbar button opens the console for the active connection. One persistent buffer per connection, auto-saved to `.deadlock/consoles/`, same editor and run behavior as files.
- History (Ctrl+H), searchable, per project.
- Done when: in a `.sql` file with 10 statements, running the third shows its result under the third, and the console survives an app restart.

**Phase 5 — Transactions and safety (S)**
- Tx mode Auto / Manual, Commit, Roll back, open-transaction indicator.
- Read-only connections refuse writes. Production connections get a warning color and a confirm on writes. `UPDATE` / `DELETE` without `WHERE` asks first.
- Warn that MySQL DDL commits implicitly.
- Done when: a manual transaction survives across statements and rolls back cleanly.

**Phase 6 — Table data editor (L)**
- Data tab: `where` and `order by` fields, paging, click-to-sort.
- Inline cell edit, Add row, Delete row, M / D / A marks, changed-cell highlight.
- Pending changes panel with the generated SQL, Submit and Discard. Generator is unit-tested.
- Tables without a primary or unique key open read-only.
- Done when: the design's 4-change scenario (2 updates, 1 delete, 1 insert) submits in one transaction.

### Milestone 3 — DBA tools on MySQL and MariaDB

Each phase defines an engine-neutral model first (`TableStructure`, `PlanNode`, `Session`, `ConfigParam`), so Oracle and Redis later add queries, not UI.

**Phase 7 — Table structure (M)**
- Columns, indexes, foreign keys, referenced-by, with links to the other table.
- Definition pane from `SHOW CREATE TABLE`, Copy.
- Add column / Add index: form → `ALTER` preview → apply.

**Phase 8 — Explain plan (M)**
- Plan tab, Tree and Text views, summary tiles, slowest step, step detail, "Show in query".
- MySQL: `EXPLAIN FORMAT=JSON` and `EXPLAIN ANALYZE`. MariaDB: `ANALYZE FORMAT=JSON`.

**Phase 9 — Sessions and locks (M)**
- Session list with filters (All / Active / Waiting / Idle in transaction), 5 s refresh, pause.
- Blocker → waiter banner from `performance_schema.data_lock_waits` (MySQL) or `information_schema.INNODB_LOCK_WAITS` (MariaDB).
- Cancel statement (`KILL QUERY`), End session (`KILL`). Status bar shows "N sessions waiting on a lock".
- Last detected deadlock from `SHOW ENGINE INNODB STATUS`.

**Phase 10 — Server configuration (M)**
- Variables by category, filters (All / Modified / Pending / Needs restart), detail pane with allowed range and default.
- Pending changes panel with SQL preview. MySQL: `SET GLOBAL` / `SET PERSIST`. MariaDB: `SET GLOBAL` only, with a note that it does not survive a restart.

Done when (Milestone 3): all four screens match the design against MySQL 8.4 and MariaDB 11.

### Milestone 4 — Remaining engines

**Phase 11 — Oracle (L)**
- Connection form: service name / SID. Schemas are users.
- Splitter: PL/SQL blocks ending in `/`. Bind variables prompt. `DBMS_OUTPUT` in Messages.
- Introspection from `ALL_*` views, DDL from `DBMS_METADATA`, data editing by `ROWID`.
- Plan from `DBMS_XPLAN`, sessions from `V$SESSION` / `V$LOCK`, config from `V$PARAMETER` with `ALTER SYSTEM … SCOPE=`.
- Missing privileges disable the screen with a reason, never an error wall.
- Done when: every Milestone 2 and 3 scenario passes against Oracle Database Free.

**Phase 12 — Redis (L)**
- Needs its screens designed first.
- Connection form: host, port, DB index, ACL user/password, TLS.
- Sidebar: db → key namespaces split on `:`, loaded with `SCAN`. Sidebar search becomes `SCAN MATCH`.
- Value tab per type: string, hash, list, set, sorted set, stream, JSON. TTL view and edit.
- Console runs commands, one per line, results inline.
- Sessions from `CLIENT LIST` / `CLIENT KILL` plus `SLOWLOG`. Config from `CONFIG GET` / `CONFIG SET`.
- Done when: a 1M-key instance browses without `KEYS *` and without blocking the server.

### Milestone 5 — Finish

**Phase 13 — Schema diagram (M)**
- Canvas with table cards and FK edges, auto-arrange, Add table, zoom, Export image.

**Phase 13b — Transposed view (S)**
- Toggle on a result block and on the table Data tab: show the selected row (or first row) as a two-column Column / Value list instead of a grid. Arrow keys or a row stepper move between rows.
- Same cell rendering as the grid (`null` styling, long-value truncation). In the Data tab, edits use the same pending-changes path as grid edits.
- Choice kept per tab, not saved across restarts.
- Done when: a 60-column row is readable without horizontal scroll and an edit made in the transposed view appears in the pending-changes SQL.

**Phase 14 — Import, export, settings, packaging (M)**
- Export grid or table to CSV / JSON / SQL inserts. Import CSV with column mapping.
- Settings, keymap, theme override.
- Packages: Linux (AppImage, deb), macOS (dmg), Windows (msi).

### Milestone 6 — Table editing (MySQL, MariaDB, Oracle)

Today the Structure view adds columns and indexes and drops columns, indexes, foreign keys and (Oracle only) checks. Nothing edits an existing column, and nothing acts on a whole table. Everything below builds on `deadlock-core/src/structure.rs` (SQL text, unit tested) and `deadlock/src/query/structure.rs` (forms, `run_ddl`), and every statement goes through the existing `confirm` prompts (production, implicit commit, forced reason). Read-only connections disable every action, the same way the object menu does.

**Shared groundwork (in Phase 15):**
- `run_ddl` takes `Vec<String>` (`confirm` already takes a list). Oracle column edits and comments need 2–3 statements. MySQL DDL commits on its own, so if statement 2 fails the notice says which ones ran.
- Split the column-definition part of `add_column_sql` / `oracle_add_column` into `column_definition()`. Add column, Edit column and Create table all use it.

**Phase 15 — Edit column, rename, move, ON UPDATE (M)**
- Model: `StructureColumn` gains `charset` and `collation` (read from `COLUMNS.CHARACTER_SET_NAME` / `COLLATION_NAME` in `mysql.rs`) and `on_update: Option<String>`, taken out of `extra` by the fill function at `mysql.rs:842`. Without the collation, a `CHANGE` would quietly reset the column to the table default.
- `NewColumn` gains `on_update: bool` and `collation: Option<String>`. `NewColumn::from_existing(&StructureColumn)` prefills the form: it splits `column_type` into type + length (`decimal(10,2) unsigned zerofill`, `enum('a','b')`, `datetime(3)`), and keeps types missing from the `column_types` list as free text.
- **ON UPDATE (MySQL / MariaDB):** an "On update CURRENT_TIMESTAMP" checkbox on the Column form, shown for `timestamp` / `datetime` only. It writes `on update current_timestamp(N)` with the same precision as the column, otherwise MySQL rejects it. Oracle has no such clause: the checkbox is hidden and the hint says to use a trigger.
- `modify_column_sql(t, old, new, s)`:
  - MySQL / MariaDB: a single `alter table … change column old new <full definition>`, restating type, collation, null, default, on update, auto_increment, comment, generation expression and position (`first` / `after x`). `CHANGE` works on MySQL 5.7 and every MariaDB, so rename uses it too instead of `RENAME COLUMN`. An auto_increment column that is already a key gets no extra `primary key`.
  - Oracle: `rename column`, then `modify (c type default d [null|not null])` with only the parts that changed (Oracle errors ORA-01442 / ORA-01451 when nullability is restated unchanged), then `comment on column`. Reordering is refused with a reason. Add column with a comment then also works (it becomes 2 statements instead of today's refusal).
  - Generated columns: name, expression and comment can be edited. Switching between stored and virtual is refused in the form (MySQL cannot do it in place).
- UI: "Edit column…" in the column row menu and on double-click opens the Column form prefilled, titled "Edit column x", with the button reading Apply. The live SQL preview stays. "Move up" / "Move down" in the row menu run the same `CHANGE` with a new `after`.
- Safety: a type or length change on a table with rows gets a forced reason: "Existing values may be truncated or rejected."
- Tests: a round-trip unit test (each fixture column → `from_existing` → `modify_column_sql` with no edits gives a definition identical to the column's line in `SHOW CREATE TABLE`), plus `engines.rs` cases per engine: rename, type change, null ↔ not null, default, on update, comment, move, then re-read and compare.
- Done when: every attribute of an existing column can be changed without losing another one, on MySQL 8.4, MariaDB 11 and Oracle Free.

**Phase 16 — Table actions: rename, drop, truncate (S)**
- A context menu on sidebar table nodes (sidebar tables have no menu today: `sidebar.rs:846`) and on the table tab (next to Import CSV…): Rename…, Truncate…, Drop….
- SQL in `structure.rs`: MySQL `rename table a.b to a.c`, Oracle `alter table s.t rename to n`; `truncate table`; `drop table` (Oracle: plain drop, which goes to the recycle bin, with an optional "cascade constraints" checkbox).
- Truncate and Drop are always confirmed as danger, showing the statement. The dialog lists the tables whose foreign keys point here (`referenced_by`) because the server will refuse or cascade.
- Afterwards: reload the sidebar node, retitle open tabs (rename) or close them (drop), and rename or remove the table's entry in `diagrams.json`. Rename warns that views and routines that use the old name break (Oracle marks them invalid).
- Done when: all three work from both menus on the three engines, and no tab or diagram card is left pointing at a missing table.

**Phase 17 — Add foreign key, add and list checks (M)**
- `NewForeignKey { name, columns, ref_table, ref_columns, on_delete, on_update }` → `add_foreign_key_sql`. MySQL: `add constraint … foreign key (…) references … on delete … on update …`. Oracle has no ON UPDATE and only `cascade` / `set null` on delete, so the form offers only those.
- Form: the table's columns (multi-pick), the referenced table (schema cache list), then that table's columns (its structure loads on pick). Column counts must match. Type mismatches and referenced columns that are not a primary or unique key are shown as hints before the server rejects them.
- Checks on MySQL 8.0.16+ / MariaDB 10.2+: read `information_schema.CHECK_CONSTRAINTS` so the Checks section and Drop work there too (today "MySQL's stay in the DDL"). Drop syntax: MySQL `drop check`, MariaDB `drop constraint`.
- `add_check_sql`: `add constraint n check (expr)` on all three engines. Existing rows that fail it show the server error as it is.
- Done when: an FK and a check can be added and dropped from Structure on the three engines, and both appear in the sections without a reload.

**Phase 18 — Create table (M)**
- "New table…" in the database node menu and on the Tables group (`object_menu` already has the `New …` pattern).
- Opens a table tab in draft state: a Structure view with Name, a column list (one Column form row per column, reusing `column_definition`), primary key columns, and on MySQL engine, collation and comment. The SQL preview updates live, and Create runs it. The tab then becomes a normal table tab, and indexes and FKs are added with the Phase 15–17 forms.
- `NewTable` → `create_table_sql`. Oracle: identity columns, and comments as extra `comment on` statements.
- Done when: a table with 5 columns, a composite primary key, a default, an `on update` timestamp (MySQL) and comments is created on each engine and opens with matching structure.

**Design gaps (add to the canvas, review before each phase starts):** Edit column state of the Column form and the On update checkbox (15); the table context menu and the Rename / Drop / Truncate dialogs (16); the Add foreign key and Add check forms (17); the New table draft tab (18).

**Out of scope here:** editing an existing FK or index (drop and add again), partitions, MySQL table options beyond engine / collation / comment, and Oracle column reordering.

### Milestone 7 — Project management

A project stays a folder. Its name, color and pattern live in `.deadlock/project.toml`; deleting a project deletes Deadlock's data for the folder, never the folder. Brief: `docs/plans/phase-19-20-project-management.md`.

**Phase 19 — New project, name, color and pattern (M)**
- Title bar project dropdown gains New project… (native folder picker, pick or create a folder, then Settings → Project opens) and Project settings….
- Settings gets a Project section: Name (display name, folder is not renamed because keychain, history and cache are keyed by its path), Color (`color::PALETTE`), Pattern (None, Stripes, Checks, Dots).
- The title bar paints the color as a left-to-right gradient with the pattern over its left part. Recent list entries carry name and color, so the dropdown and welcome window show them.
- Done when: create, rename, color and pattern work from the dropdown and show everywhere at once and after a restart, in both themes.

**Phase 20 — Connections and Delete project (M)**
- Project section lists connections with New, Edit, Duplicate and Delete. Delete connection is new (also in the sidebar menu): removes the config entry, keychain secrets, cache rows, diagrams and console buffer, and closes its tabs (file tabs stay, unbound).
- Delete project… in a Danger zone: confirm, run the close checks, remove keychain secrets, history, cache, `.deadlock/` and the recent entry, then close the window.
- Done when: nothing is left behind for a deleted connection or project, and the folder's own files stay.

**Design gaps:** dropdown rows, Settings Project section, title bar patterns (19); Connections block, Danger zone, both delete dialogs (20).

## Phase 0 result (2026-11-22): go on GPUI

- Pins: `gpui-kit` (re-exports gpui and gpui-component) at git rev `1c7e7f7c6c8d6585da909804496614da01bb8663`; gpui itself is `gpui-pre = "=0.3.12"` from crates.io.
- Table (1M rows) and multi-window work. Table scrolls at 9 ms p50 frame interval on real hardware.
- Editor (50k lines) draws in 2.9 ms, **but** with an accessibility client active it copies the whole document every frame (146 ms). Fix: patch `crates/base/src/input/base/a11y_text.rs` in gpui-component. Needed before Phase 4.
- **Inline results: not supported today, possible with a patch** (about 1–2 weeks): generalise the editor's inline-completion "ghost lines" into block widgets between rows. Carry it in a fork through `[patch]`, offer it upstream. Phase 4 does this; the split-panel fallback stays as the escape hatch.
- **Wayland focus:** a second launch cannot focus the existing window, the compositor only lets it request attention, unless GPUI is patched to accept the second launch's `XDG_ACTIVATION_TOKEN`. Phase 1 ships the attention request.
- Update 2026-11-22: the user allows real windows on their desktop again, so the real Wayland session is the default for GUI checks. Off-screen command, for when a run must not take focus:
  `env -u WAYLAND_DISPLAY VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.json xvfb-run -a -s "-screen 0 1920x1080x24" timeout 10 cargo run -p deadlock --example <name>`
- Dev DBs: MariaDB is on host port 3307, Oracle image is `gvenzl/oracle-free:23-slim`.

## Phase 1 result (2026-11-22): done

- Shell window, project model, open-project window, one window per project, single instance (Unix socket), bounds restore. 15 tests.
- Run: `cargo run -p deadlock` (open-project window) or `cargo run -p deadlock -- <folder>`.
- Open project is Ctrl+Shift+O (Ctrl+O stays Open file). Single instance is Unix-only for now. Errors go to stderr only, no UI notification yet.
- Not verified: real Wayland and macOS, the native folder picker, clicks, hover states.

## Phase 2 result (2026-11-22): done, except the SSH tunnel

- `Driver` trait, value model, MySQL/MariaDB, Oracle and Redis drivers, connection model with keychain, New connection tab, sidebar / switcher / status bar wiring. Created MySQL and MariaDB connections through the real UI.
- Oracle driver: `oracledb` 26.0.0-beta.6 (pure Rust, blocking, runs on tokio's blocking pool). Cancel is `ALTER SYSTEM CANCEL SQL` from a side session and needs that privilege.
- Integration tests: `docker compose up -d --wait && cargo test -p deadlock-core --test engines -- --ignored` (6 pass, all four engines).
- **Open:** SSH tunnel (checkbox disabled; needs host-key checking and an SSH server in Docker to test). No password prompt for connections saved without the keychain. TLS Require / Verify untested for Oracle and Redis. MySQL multi-statement returns only the first result set until Phase 4.
- The connection id is derived from the name and never changes: it keys the keychain entry.

## Phase 3 result (2026-11-22): done

- Schema tree for MySQL/MariaDB (four bulk `information_schema` queries per database), virtualized sidebar tree, F5 refresh, SQLite schema cache, fuzzy search, password prompt, one-row toolbar that drops labels when narrow.
- 5,000 tables, release build: load 326–388 ms, search keystroke 2–7 ms typical, 22 ms worst case.
- **Open:** tree loading shares the server connection with queries, so from Phase 4 a long query blocks tree loads: give queries their own session. Expanded nodes are not saved across restarts.

## Phase 4 split

- **4a:** editor tabs, statement splitter, run parts, results in the bottom panel, console, history, the a11y editor patch, a query session separate from the tree's.
- **4b:** inline results (the editor block-widget patch). Design (`Inline` artboard) approved 2026-11-22. Runs after Phase 8, because both edit `query.rs` in the same checkout.

## Phase 4a result (2026-11-22): done

- SQL file tabs, statement splitter (MySQL/MariaDB, Oracle, Redis; 14 tests), run selection / statement / file, Stop, one server session per tab, paged result grid with Result / Messages tabs and multiple result sets, console per connection, Format.
- Editor a11y fix: `vendor/gpui-base/` (copy of upstream `crates/base` at the pinned rev) wired through `[patch]`; changes listed in `vendor/gpui-base/PATCHES.md`. 50k lines with an a11y client: 247 ms → 4.8 ms draw. Phase 4b's block-widget patch goes into the same vendored crate.
- 1M-row result: memory stays flat (209 → 211 MB); pages load on scroll.
- **Open:** only the last statement's result keeps paging, earlier ones stop at 500 rows. The save prompt is GPUI's unstyled dialog. Native file pickers and the real KDE desktop were not exercised. History panel not designed or built.
- Dev DB leftovers in the MySQL container: `big1m` table, `two_sets` procedure. The `mysql_big_schema_timings` test needs a hand-made `big` database.

## Phase 5 result (2026-11-22): done

- Statement classifier (`classify.rs`: read / write / DDL / tx control, dangerous, implicit commit), Auto / Manual per tab, Commit / Roll back, open-transaction state read from the server, one Commit / Roll Back / Cancel prompt on close / quit / switch, read-only enforced on server and client, Production marker and confirmation, dangerous-statement confirmation.
- Integration tests: `cargo test -p deadlock-core --test engines -- --ignored --skip big_schema` (19 pass).
- **Open:** a killed process rolls back with no prompt. A failed Oracle auto-commit only shows as the marker. Tx mode is not saved across restarts. All dialogs are GPUI's unstyled built-in one: a styled dialog component is still owed (design gap). In Manual mode a plain SELECT on InnoDB shows "Uncommitted changes", because the server reports a transaction.

## Phase 6 result (2026-11-22): done (MySQL, MariaDB)

- Table tab with filter, sort, paging, in-place editing with type checks, Add / Delete row, undo, pending-changes panel, Submit in one transaction, read-only for keyless tables and views. SQL generator and pending model in `edit.rs`; every UPDATE / DELETE must match exactly one row or the submit rolls back.
- Integration tests: 25 pass across the four engines.
- **Open:** the WHERE uses only the key, so a concurrent change to another column of the same row is overwritten silently. Paging is `LIMIT … OFFSET`. Server warnings after submit are not checked. Views are always read-only. Status bar counts pending added rows ("Rows 1 to 215 of 214").
- Milestone 2 is complete except Phase 4b (inline results, waiting on design review) and the History panel.

## Phase 7 result (2026-11-22): done (MySQL, MariaDB)

- `TableStructure` model and `ALTER TABLE` generator in `structure.rs`, `Driver::structure()`, Structure view with Columns / Indexes / Foreign keys / Referenced by / Definition, Add column, Add index, drop column / index / foreign key, refresh of tree, cache and Data view after an ALTER. 27 integration tests pass.
- **Open:** no in-place edit of existing columns. The type field only accepts types from the server's list. Production and implicit-commit prompts for DDL were not exercised in the app. The Data / Structure choice is not saved across restarts.

## Phase 8 result (2026-11-22): done (MySQL, MariaDB)

- `PlanNode` model and parsers in `plan.rs`, `Driver::explain(statement, analyze)`, 48 captured plan fixtures, Plan tab with tiles, tree, Text view, step detail, Estimated / Measured switch. Explain is Ctrl+Shift+E, measured is Ctrl+Alt+Shift+E. 29 integration tests pass.
- **Open:** MySQL cannot measure single-table UPDATE / DELETE. Read-only connections cannot explain writes at all (the server refuses). `serde_json` `preserve_order` is now on for the whole workspace.

## Phase 4b result (2026-11-23): done

- Editor block widgets as patch 2 in `vendor/gpui-base/` (`block_map.rs`, `blocks.rs`, about 740 lines, see `PATCHES.md`). Inline result blocks in query and console tabs (`query/inline.rs`): expanded, collapsed, error, running, outdated; Open in panel; Inline | Panel switch saved per project. No measurable editor slowdown, also with 50 blocks.
- **Open:** Page Up/Down and scrollbar markers ignore block heights. Word wrap, folds, IME and a11y near blocks untested. Inline runs only tried on MySQL. Result blocks are not restored after a restart. Export is disabled until Phase 14. History panel still not designed or built.
- Build profile: dev debuginfo cut to line tables (none for dependencies); `target/` went from 37 GB to 4 GB.

## Phase 9 result (2026-11-23): done (MySQL, MariaDB)

- `Session` / lock-wait model, blocking chains and the InnoDB deadlock-report parser in `sessions.rs`; MySQL and MariaDB queries in `mysql/sessions.rs`; Sessions and locks tab with 5 s refresh, banner, filters, detail pane, Cancel statement, End session, last deadlock; status-bar and sidebar "N waiting" indicators. 39 integration tests pass.
- **Open:** on TLS connections `mysql_async` drops the program name, so Deadlock's own sessions are only labelled within one process. MariaDB needs the `metadata_lock_info` plugin to name a metadata-lock holder. Row locks are capped at 5,000. Sessions tabs are not restored after a restart. Stopping the background check on window close is untested.

## Phase 10 result (2026-11-23): done (MySQL, MariaDB). Milestone 3 complete.

- `ConfigParam` model, categories, units, validation and statement generator in `config.rs`; MySQL (`SET PERSIST` / `PERSIST_ONLY` / `GLOBAL`, `RESET PERSIST`) and MariaDB (`SET GLOBAL` plus copyable option-file lines) in `mysql/config.rs`; Server configuration tab with filters, categories, search, detail editors and the pending-changes panel. 48 integration tests pass.
- **Open:** MySQL reports no variable types, so its booleans and enums are free text. MySQL restart-only flags come from a one-time probe of 8.4.11 (`config/mysql-8.4.tsv`), not the server. Privilege checks read direct grants only (roles ignored); MariaDB requires SUPER. Closing the tab with pending changes does not ask. Server warnings on adjusted values are not shown.

## Phase 11 result (2026-11-23): done (Oracle)

- Oracle implementations in `oracle/` (schema, table, structure, plan, sessions, config) plus bind-variable dialog, `DBMS_OUTPUT`, `DESC`, compilation errors, rows by PK / unique key / ROWID, check constraints, Service name / SID choice. 60 integration tests pass across the four engines; MySQL and MariaDB unchanged.
- A plain user sees own objects; measured plans need `SELECT_CATALOG_ROLE`, sessions and config name the missing privilege.
- **`oracledb` beta limits:** user queries that select LOBs with binds can fail (ORA-01008 / ORA-01002) after a reparse; time-zone names are lost (values shown in UTC); JSON key order is lost; LOBs load whole into memory; no OUT binds.
- **Open:** TNS aliases, wallets and TCPS untested or unsupported. First Structure load on a cold server takes about 20 s. Oracle has no last-deadlock view.

## Phase order change

Phase 13 (schema diagram) runs before Phase 12 (Redis), because Redis waits on its screens being designed and reviewed.

## Phase 13 result (2026-11-23): done (MySQL, MariaDB, Oracle)

- Diagram model, card sizing, line routing, layered auto layout, layout persistence and SVG generator in `diagram.rs`; `Driver::diagram(schema)`; Schema diagram tab with pan, zoom, drag, Arrange, Add / remove table, export to SVG and PNG (`resvg`, already in the tree). Layout saved in `.deadlock/diagrams.json`. 63 integration tests pass.
- 300 tables / 400 relationships: pan 2–8 ms per frame, card drag about 1 ms (debug build, software rendering).
- **Open:** some long lines pass behind cards in big schemas. Export needs a save-dialog portal. Two windows on one project can overwrite each other's `diagrams.json`. Header tooltips overlap open menus (shared `button()` helper). Diagram tabs are not reopened after a restart.

Redis design (`Redis` artboard) approved 2026-11-23. `History`, `Dialog`, `Import` artboards approved 2026-11-23 (Phase 14b). Settings design (`Settings` artboard) approved 2026-11-23; queued behind 14b in the same subagent thread. Production connections always ask and always start in Manual; that is not a setting.

## Phase 14 split

- **14a (no design needed):** export of results and tables, SSH tunnel (left open by Phase 2), Linux packaging.
- **14b (needs design):** import, settings, History panel, a styled dialog, the Tx menu.

## Phase 14a result (2026-11-23): done, except the AppImage

- Export (`export.rs`): CSV, TSV, JSON, JSON Lines, SQL INSERT; "All rows" re-runs a read and streams page by page; cancel removes the partial file. 1M rows to CSV in 4.4 s with flat memory (debug build).
- SSH tunnel (`ssh.rs`, `russh`): key file, agent or password; host key checked against `~/.ssh/known_hosts` (read only) and Deadlock's own file; changed keys refused. `ssh` service added to `docker-compose.yml`. 71 integration tests pass.
- Packaging: `packaging/linux/package.sh [deb|appimage|all]` → `target/package/`. The `.deb` (16.7 MB) is built and runs. **The AppImage is not built:** the script downloads `appimagetool` from GitHub and auto mode blocked that; run `packaging/linux/package.sh appimage` by hand. `README.md` and a tag-push CI job exist. No macOS or Windows packaging.
- **Open:** `@revoked` / `@cert-authority` known_hosts lines are ignored; `~/.ssh/config` is not read; TLS "Verify certificate" fails through a tunnel; each tab opens its own SSH connection. The toolbar Import button does nothing (14b). Opening a `.sql` from the desktop makes its folder a project.

## Oracle Thick driver (2026-11-23): done, confirmed by the user on their real server

- Why: the user's real Oracle account has only a 10G password verifier (`0x939`), which the Thin `oracledb` driver cannot use, and they cannot change the server.
- Oracle connections have a Driver choice: Thin (default) or Thick (`oracle` crate 0.6.3 on ODPI-C, needs Oracle Instant Client; `thick = true` in `project.toml`). All of `oracle/` is shared; only `oracle/conn.rs` (thin) and `oracle/thick.rs` differ. Run the Oracle tests on Thick with `DEADLOCK_TEST_ORACLE=thick`.
- Thin's old-verifier error now explains itself and offers "Switch this connection to the Thick driver". `DEADLOCK_ORACLE_CLIENT_DIR` points at a client folder.
- **Not reproduced:** the 26ai container refuses to store a 10G verifier. If Thick still fails on the real server, check `SQLNET.ALLOWED_LOGON_VERSION_CLIENT` for Instant Client 21 first.
- Thick cannot read native JSON or VECTOR columns; it keeps time-zone offsets and cancels without `ALTER SYSTEM`.

## Phase 12 result (2026-11-23): done (Redis)

- Key browser (`SCAN`, namespaces on `:`, Load more), server-side search, key tabs for string / hash / list / set / sorted set / stream / JSON, pending changes in `MULTI` / `EXEC` under `WATCH`, key actions, Clients and slow log, Server configuration, export. A `MONITOR` test fails if a blocking command is sent. Tests: `cargo test -p deadlock-core --test redis -- --ignored --test-threads 1` (10 pass).
- 1,000,000 keys (debug build): first paint 6 ms, load more 5–8 ms, whole-keyspace search 0.35–0.57 s, nothing over 10 ms in the server's slow log.
- **Open:** Cluster and Sentinel are refused with a message. No command completion. Multi-select delete and Production confirmations not clicked through in the app. Namespaces nest, where the design shows them flat.
- All 15 planned phases are built except 14b (import, History panel, styled dialog; designs published, awaiting review) and the AppImage.

## Phase 14b and Settings result (2026-11-23): done. The plan is complete except the AppImage.

- Styled dialog (`dialog.rs`) hooked into GPUI's prompt builder, so every prompt uses it. History store (`history.rs`, `history.sqlite3`, newest 10,000 per project, secrets masked) and panel (Ctrl+H). CSV import (`import.rs`): whole-file check first, then one transaction; 1M rows in about 11.5 s with flat memory. 77 engine and 10 Redis integration tests pass.
- Settings (`settings.rs`, `settings_tab.rs`, `keymap.rs`): `settings.toml` in the config directory, Ctrl+, opens the tab; theme, text sizes, editor, results, safety, rebindable shortcuts (42 actions). Production always asks and always starts in Manual.
- **Open:** a table tab in Manual mode does not show imported rows until its transaction ends. Ctrl+H is History, so the editor's Replace is only in the Ctrl+F bar. Twelve converted prompts were not triggered in the app. `redis` test `million_key_timings` needs a hand-loaded data set. No macOS or Windows packaging.

## Risks

1. **Inline results in the editor.** gpui-component's editor documents no block widgets between lines. Answered in Phase 0, with the split-panel fallback already defined.
2. **GPUI is pre-1.0.** Pin the rev, upgrade deliberately, keep all logic in `deadlock-core`.
3. **Oracle thin drivers are young** (one beta, one 0.1.x). The ODPI-C crate is the fallback and costs an Instant Client install.
4. **Trait shaped by MySQL only.** Covered by the four-engine smoke test in Phase 2 and the engine-neutral models in Milestone 3.
5. **Redis does not fit a SQL-shaped UI.** It gets its own screens and reports no SQL capabilities.

## Settled (2026-11-22)

1. A project is a folder with `.deadlock/` inside it.
2. Connections are per project.
3. Engine order stays: MySQL/MariaDB, DBA tools, Oracle, Redis.
4. Design gaps are added to the design canvas and reviewed before the phase that needs them starts.

## How phases run

Each phase is handed to a subagent with this file as its brief. A phase starts when the previous one is done and its design gaps are reviewed.

Rust code follows the `rust-skills` skill in `.claude/skills/rust-skills/`. Every phase brief tells the subagent to load it.

Subagent models: Opus, Sonnet or Haiku only, with Fable as advisor when a subagent is stuck or a decision is not settled here. Opus for phase implementation and spikes, Sonnet for mechanical work, Haiku for lookups.

## Phase 15 result (2026-11-25): done (MySQL, MariaDB, Oracle)

- `structure.rs`: `StructureColumn` gains `charset`, `collation`, `on_update` (cut out of `extra`); `TableStructure.mariadb`; `NewColumn` gains `on_update`, `collation`, `generated`, `invisible`. `column_definition()` is shared by Add and Edit; `add_column_sql` returns a list (Oracle comment = 2nd statement); `NewColumn::from_existing` splits types; `modify_column_sql`: one `change column` on MySQL / MariaDB, only the changed parts on Oracle (rename, modify, expression, drop identity, comment). Expression defaults read back as `(uuid())`.
- Structure view: Edit column… (row menu, double-click) opens the prefilled form titled "Edit column x" with changed dots, Collation (MySQL), On update CURRENT_TIMESTAMP, Expression for generated columns, the forced-reason band, "Apply N statements"; Move up / Move down; `run_ddl` runs a list and reports "Ran k of n…" on a partial failure; read-only disables every action. `menu_item_disabled` moved to `assets.rs`.
- Tests: 230 core unit tests; new `mysql_columns_edit`, `mariadb_columns_edit`, `oracle_columns_edit` (also on Thick) pass with the existing structure tests; whole engine suite 81 pass (Redis not started, tunnel and timings skipped).
- Settled: MariaDB rejects `null` after a generated column (1064), so it is left out there; Oracle `modify (c as (expr))` works on a virtual column; MySQL restates a table-default collation implicitly, else `SHOW CREATE TABLE` changes.
- **Gaps closed (Batch A):** MariaDB column-level checks (`LEVEL = 'Column'`, or the DDL before 10.5.10) and MySQL `SRS_ID` are read into `StructureColumn.check` / `srid` and restated on CHANGE (a rename renames the column inside the check); `enum` members are quoted literals in Length (`'a,b','it''s'`). A row estimate of 0 is checked with `select 1 … limit 1` (Oracle: `rownum = 1`) in the driver, and `None` (not known) when rows exist. After "Ran k of n (rename column). Statement k+1 (modify) failed: …" (`statement_label`) the form stays open and edits the reloaded column (by its new name if the rename ran). `modify_column_hints` shows the ORA-54031 line before Apply. Tests: `enum_members_round_trip`, `modify_keeps_srid_and_column_check`, `oracle_hints_name_the_virtual_column`, `statements_are_labelled`, `partial_failure_names_what_ran_and_what_failed`; `*_columns_edit` round-trip `SHOW CREATE TABLE` byte-identical with them; the MariaDB suites also passed on a throwaway MariaDB 10.5.9.
- **Gaps closed (Batch B):** Collation (Edit column, Add column, New table rows and table) is a searchable list of the column's character set's collations (`Driver::collations`: `COLLATIONS`, or MariaDB 10.10+'s `COLLATION_CHARACTER_SET_APPLICABILITY` full names), read once per connection; a value missing from the list stays; clearing it means the table default. The row menu's Edit column… shows "Double-click" (`menu_item_hint`, an element item: gpui-component's popup menu only prints the key binding of an action, not free text). Closing the window or the tab asks before a Column form with changes is lost. Tests: `collation_lists_follow_the_charset_and_keep_the_value`, `column_forms_with_typing_ask_before_closing`, engine `*_collations_cover_column_collations`.
- **Open:** Oracle refuses to modify a column a virtual column reads (ORA-54031): the server forbids it, the form says so before Apply. A `set` member cannot hold a comma (server error 1367).

## Phase 16 result (2026-11-25): done (MySQL, MariaDB, Oracle)

- `structure.rs`: `rename_table_sql` (MySQL `rename table a.b to a.c`, Oracle `alter table … rename to n`, unqualified), `truncate_table_sql`, `drop_table_sql` (`cascade constraints` on Oracle only). `diagram::rename_table` renames or removes a saved card; no file, no card: no write.
- `dialog::Ask` gains an optional text field, warning band, labelled row list, checkbox and a live `Preview` (statement from field + checkbox; `Err` shows "To finish: …" and disables the default action); `ask_inputs` reads the field and checkbox back. Space types into the field.
- Sidebar table rows get a menu (Open, Copy name, Import CSV…, Rename…, Truncate…, Drop table…), the table tab menu gets the last three after Import CSV…; disabled on read-only, only Drop is red. Both open the tab on Structure, which asks once the structure has loaded: the designed dialogs with the badge, row estimate (`TableFacts`), keys of other tables pointing here, the open-transaction and pending-changes lines. That dialog replaces the run prompts (no second "Run this statement?"); pending data changes are discarded after it.
- Afterwards: rename retitles the tab (`Kind` follows, state saves), reloads structure, Data and the sidebar; drop closes the tab; truncate reloads the Data view. Diagram cards follow both (the open diagram of that schema reloads).
- Tests: 232 core unit tests, `table_dialogs_list_other_tables_keys_and_preview` in the app; new `mysql_table_actions`, `mariadb_table_actions`, `oracle_table_actions` pass, with the Phase 15 `*_columns_edit` and `*_table_structure`.
- **Gaps closed (Batch B):** the sidebar table menu has Copy CREATE statement (the Structure DDL, through the shared `copy_create_with`) and "Enter" on Open. After a rename, open trigger tabs on that table follow it (`ObjectDraft::follow_table_rename`, unapplied edits kept, then re-read), and the schema reload refreshes the sidebar, its cache and completion. The rename dialog adds "Open editors still use the old name." Dialog warning bands and form hints carry the new warning triangle (`Icon::Warning`, `ui::warning_band` / `warning_line`). Tests: `triggers_follow_a_renamed_table_of_their_schema`, the rename case of `table_dialogs_list_other_tables_keys_and_preview`, `every_icon_and_the_logo_load`.
- **Open:** none. Settled: SQL typed in editors is not rewritten after a rename (rewriting someone's text is unsafe; the dialog says so). A `;` in a new name is quoted into the name, which is valid SQL.

## Phase 17 result (2026-11-25): done (MySQL, MariaDB, Oracle)

- `structure.rs`: `FK_ACTIONS`, `ORACLE_FK_ACTIONS`, `NewForeignKey` → `add_foreign_key_sql` (Oracle: no ON UPDATE, `no action` writes no clause), `foreign_key_hints` (base type + `unsigned`, exact PK / unique key set, Oracle wants a constraint, `set null` on not null), `add_check_sql`; `drop_constraint_sql` writes `drop constraint` on MariaDB; `TableStructure.checks_unsupported`.
- `mysql.rs`: checks read from `TABLE_CONSTRAINTS` + `CHECK_CONSTRAINTS` (MySQL) or `CHECK_CONSTRAINTS … LEVEL = 'Table'` (MariaDB); a failed query sets `checks_unsupported` and the section hides.
- Structure view: Add foreign key and Add check in the section titles (off on read-only); the FK form pairs each column with its referenced column on one row, References table is searchable, Oracle shows On delete only with the hint; hints in the warn color never block Apply; "No foreign keys yet." / "No check constraints yet."; the Check constraints section shows on every table of a supporting server. Referenced tables come from the sidebar's cached `Tables` children (new `TabEvent::NeedTables` → `QueryTab::reference_tables`), else a `children([schema, "Tables"])` round trip.
- Tests: 235 core unit tests (`foreign_key_sql_per_engine`, `foreign_key_hints_flag_likely_refusals`, `check_sql_and_drop_syntax`); new `mysql_keys_and_checks`, `mariadb_keys_and_checks`, `oracle_keys_and_checks` pass with the `*_columns_edit`, `*_table_actions`, `*_table_structure` tests.
- Settled: MariaDB column-level checks (a JSON column's `json_valid`) refuse `drop constraint` (1091), so they are not listed; they belong to the column and Edit column restates them.
- **Gap closed (Batch A):** MariaDB before 10.5.10 (no `LEVEL`) reads the checks without it, and a column's own check is the one named after a column whose DDL line ends with it (`column_check`, `column_checks_without_level_come_from_the_ddl`; the MariaDB suites passed on 10.5.9).
- **Gaps closed (Batch B):** References table lists the schema's own tables, read from the server when the form opens and again whenever a schema of the connection reloads (`SchemaChanged`), then other shown schemas' cached tables as `schema.table` (`Tree::cached_tables`); the statement qualifies the referenced table. Referenced-column choices show the type and key dimmed (`RefColumn`). `foreign_key_hints` flags text columns whose collations differ (MySQL 3780, MariaDB 1005, both checked on the servers); `foreign_key_index_note` adds the "has no index yet: MySQL adds one with the key's name" line. Tests: `foreign_key_hints_compare_text_collations`, `foreign_key_index_note_when_no_index_leads_with_the_columns`, `cached_tables_of_other_shown_databases`, `reference_tables_of_other_schemas_are_qualified`; `*_keys_and_checks` add a key into another schema on all three engines, check the index the note announces, and the collation refusal.
- **Open:** none. Settled: an Oracle key into another schema needs that schema's `REFERENCES` grant; without it Oracle hides the table (ORA-00942), shown as it comes.

## Phase 18 result (2026-11-25): done (MySQL, MariaDB, Oracle). Milestone 6 complete.

- `structure.rs`: `NewTable`, `table_engines` (MariaDB adds `Aria`), `create_table_sql`: one `create table` with a line per column (`column_definition`), `primary key (…)`, then `engine = … collate = … comment = …` when set; Oracle adds `comment on table` / `comment on column` statements. Refuses no name, no columns, a repeated name (case-insensitive on MySQL), a key column not in the list, a MySQL auto increment column outside the key, non-name engine / collation; column errors read `column 3 (status): choose a type`.
- New table… first in the database node menu and on the Tables group (off on read-only) opens a draft table tab ("New table", grey dot, tooltip `New table in schema · conn`, no tab menu, left out of `state.json`): Data disabled ("Create the table first"), `schema.name` with a Draft chip, Table fields (Name; Engine and Collation on MySQL / MariaDB; Comment), a column grid of Column form rows (#, Name, Type, Length, Collation on text types, Null, AI / Identity on integer types, Default, On update on datetime / timestamp, Comment, remove; row menu Move up / Move down / Remove column), Primary key (columns in row order, Remove, Add a column… picker), the live statement with Copy, the To finish line, "No primary key yet" as a note, Discard and Create in a footer. Closing a typed draft asks Discard / Cancel. Create runs behind the run prompts; the tab then becomes the table's tab (structure reloads, sidebar schema reloads, state saves, an open diagram reloads); a failure keeps the draft and shows the error above the footer.
- Checkbox setters take `FormAt` (`Form` / `Draft(i)`), shared by the Column form and the draft rows; `new_column_form` serves draft rows (no position, no type yet).
- Tests: 238 core unit tests (`create_table_mysql`, `create_table_oracle`, `create_table_refuses_bad_input`), `tables_groups_offer_new_table` in the app; new `mysql_create_table`, `mariadb_create_table`, `oracle_create_table` pass with every `*_columns_edit`, `*_table_actions`, `*_keys_and_checks`, `*_table_structure`.
- **Gap closed (Batch A):** a MySQL / MariaDB draft reads `@@SESSION.sql_mode` on its session once it opens and quotes literals for it (Create waits: "To finish: wait for the server's sql_mode"); `mysql_create_table` / `mariadb_create_table` create on a `NO_BACKSLASH_ESCAPES` session.
- **Gaps closed (Batch B):** after Create the sidebar opens the schema's Tables and selects the new table once the reload has it (`Tree::reveal`). Closing the window or quitting with a typed draft asks (the unapplied-changes prompt, which also lists Column forms with changes). The Primary key list keeps its own order, with up / down, independent of the rows (`key_names`, `move_key`). Collation is the searchable list (Phase 15). The one-statement guard scans literals in the session's escaping mode (`split::split_in_mode`), so a value ending in `\` before one holding `;` passes under `NO_BACKSLASH_ESCAPES`; the data view's filter guard does the same. Tests: `reveal_opens_the_ancestors_of_a_loaded_node`, `primary_key_order_is_its_own`, `one_statement_guard_follows_backslash_mode`, `no_backslash_escapes_ends_a_string_at_its_quote`, the filter case in `selects_wrap_user_text_and_end_with_the_key`; `mysql_create_table` / `mariadb_create_table` create a `(line, id)` key and both values on a `NO_BACKSLASH_ESCAPES` session.
- **Open:** none.

## Milestone 6 click-through (2026-11-25)

The real app under Xvfb (software Vulkan), driven by xdotool against the docker MySQL 8.4, MariaDB 11.8 and Oracle Free, with a MySQL Production and a MySQL read-only connection. Screenshots: `/tmp/claude-1000/-home-lamka02sk-Code-deadlock/d97b1f4a-a095-4256-9a25-28d76a85257c/scratchpad/gui/shots/`.

- **Clicked:** Edit column from the row menu (with the "Double-click" hint) and by double-click, typed changes with the changed dots, the forced reason on a type change, Apply and the run prompts; Move up / Move down (Move up off on the first row; Oracle explains why both are off); Oracle rename + modify + comment in one Apply (3 statements); MariaDB collation search and pick; Add index, Add foreign key (a `gui2.regions` pick, column pairs with dimmed types, the type / key / index hints and the server refusal shown as is, then a working key; Oracle On delete only), Add check on all three engines; Rename, Truncate and Drop from the sidebar and the tab menu, the referenced-by list, Oracle's cascade constraints checkbox, the Production badge; the diagram following a rename and a drop; trigger tabs following a rename (MySQL, Oracle) and closing after a drop, asking first when edited; New table from the database and Tables menus through Create (MySQL with collation, auto increment, on update, comments with `\` and `'`, a composite key reordered with up / down, a server refusal kept in the draft; Oracle identity and `comment on`), the new table selected in the sidebar; the discard prompts on closing a draft, an Add index form and an Add check form, and on Quit; read-only disabling (menus, Add buttons, row menu, double-click); Production prompts; restart restoring table tabs.
- **Bugs fixed:** the Add index, foreign key and check forms asked nothing before a tab, window or Quit closed them (`form_unapplied`, `key_and_check_forms_with_typing_ask_before_closing`); trigger tabs of a dropped table stayed open (`close_objects_on_table`, `ObjectDraft::is_on_table`); a Structure tab whose keys point to or from a renamed or dropped table kept showing the old table (`other_table_changed`, `keys_name_the_tables_they_point_to_and_from`); Oracle trigger source ends with a NUL after Oracle rewrites it for a table rename, so every open trigger tab showed a "Body" change (`source_text` trims it; `oracle_table_actions` reads the body back); MySQL counted a transaction as open after DDL in Manual mode, so the status bar said "Uncommitted changes" and Quit asked to commit after Edit column (`run` clears the write mark after an implicit-commit DDL; `*_manual_transactions` cover it); a search in a Type list picked the wrong type (the form read the filtered index; now the value, and Structure lists are `SearchableVec`, which filters); long form previews ran off the right edge (`wrap_statements`, `previews_break_long_statements_outside_quotes`); a second Add form replaced an open one with its typing (Add buttons are off while a form is open); "PK FK" wrapped in the Key column; "about 2 and resets" and "New table have edits" wording; a disabled Drop item was still red; the run and table dialogs cut a long statement at their width (`wrap_sql`, shared with the form previews, `long_lines_wrap_keeping_their_indent_and_short_lines_as_typed`); Oracle keys showed "no action" under On update (now empty, `dictionary_rows_become_a_structure`); an empty Referenced by section showed only its header ("No other tables reference this one."); object tabs were not restored on restart (`TabState::Object`, `tabs_round_trip_through_state`; they reopen behind the front tab once their connection connects, one password prompt at a time, and only an object the server says is gone (`DriverError::NotFound`) is left out; any other failed read still opens the tab with its error, so a dropped link loses no tab; `*_manages_objects` assert `NotFound`).
- **Not covered:** closing the window by its title-bar button (Xvfb runs no window manager; Quit runs the same `should_close` path and was clicked).

## Phase 19 result (2026-11-25): done

- `project.rs`: `ProjectConfig` gains `name`, `color` and `pattern` (`Pattern`: none, stripes, checks, dots), written before `connections` and left out while unset, so an old `project.toml` reads and writes as before. `Project::name()` and `RecentProject::label()` share `display_name`: the trimmed name, else the folder's name. `recent.rs`: `RecentProject` gains `name` and `color` (`serde(default)`); `set_look`; `touch` keeps a known entry's look.
- Settings has a **Project** section, first, titled with the project's name: Name (placeholder = folder name; saved on Enter, when focus moves to another control, or with any Color or Pattern click), Color (None + the 12 `PALETTE` swatches, the connection form's ring and check), Pattern (four 64 × 32 tiles in the chosen color; grey and "Shows once the project has a color." without one). Every change goes through `Shell::set_look`: `project.toml` first (a failed write puts the old config back and shows the error in the section), then the window title, the recent entry (`app::set_recent_look`, saved) and a redraw of every window. The section ends after its one fieldset, so Phase 20's Connections block and Danger zone go below it.
- Title bar (`title_band`): a `linear_gradient` from the color at 22 % over the panel to the panel at 55 % of the width; the pattern over the left 40 % (`pattern_slash` 1 px / 6 px at 18 %, `checkerboard` 6 px at 10 %, Dots: a `canvas` of 2 px round quads on an 8 px grid at 26 %; ×1.6 on the dark theme), then a fade to the gradient's own tint at 40 %. Worst label contrast measured on the screenshots (`tx2` labels against every background pixel above and below the buttons, 4 patterns × Red, Blue, Slate × both themes at 1600 px, plus 900 px): 4.78:1 (dark, Blue, Dots); `tx` text ≥ 11:1.
- Project dropdown: rows show the recent name and an 8 px color dot (every row keeps the dot's place, so names line up); under a line, **New project…** (`Icon::Plus`, Ctrl/Cmd+click opens in a new window) and **Project settings…** (`Icon::Config`). The welcome window shows the name and tints the folder icon with the color, and follows changes while it is open.
- `app::new_project`: the folder picker with "Create project", then `Shell::pick_project` (focus if open elsewhere, close checks, switch, or a new window). When the folder had no `.deadlock/`, `Registry.name_next` makes `register` open Settings on Project once the window has the project.
- Tests: 253 core unit tests (`name_color_and_pattern_round_trip`, `blank_name_falls_back_to_the_folder`, `existing_config_is_kept` now also checks an old file has no look, `look_survives_touch_and_old_files_load`); app tests unchanged (72). The app crate has no GPUI test harness (all its tests are pure functions), so the Settings section was checked in the real app instead.
- Checked in the real app under Xvfb with xdotool: the dropdown, Project settings…, Blue + Dots + rename to "Payments" updating the toolbar, window title, Settings, dropdown and the open welcome window at once; a blank name going back to the folder name; restart keeping all of it; both themes. New project… under an isolated D-Bus session: the **GTK** portal (Create Folder button in the picker) and the **KDE** portal (right-click → Create New → Folder…; no toolbar button) both made a new folder, the window switched to it and Settings opened on Project. Screenshots: `/tmp/claude-1000/-home-lamka02sk-Code-deadlock/3053626f-53db-4f5b-9920-818c5c330e24/scratchpad/phase19/`.
- **Open:** macOS and Windows pickers not checked (no machine here). Ctrl+click on New project… was not clicked (same `pick_project` path as the recent rows). Clicking blank space does not take focus from the Name field in GPUI; fixed in Phase 20: the field saves on every change (`project.toml`, window title and recent entry per key), so a typed name survives closing the window. `Icon::Config` stands in for the brief's `Icon::Settings`, which does not exist.

## Phase 20 result (2026-11-25): done

- Core: `keychain::ssh_entry` moved from the app into `connection.rs`; `SchemaCache::forget(project, Some(conn) | None)`; `diagram::forget(root, conn)` (nothing saved for it: no write); `RecentProjects::remove`; `project::delete(root, ids, history, cache)`: keychain entries (password and `#ssh`) of every connection, history rows, cache rows, then `.deadlock/`; the first failure stops it (`Error::Keychain`, `Error::Cache` added). The keychain step is a parameter of the private `delete_with`, so the tests run without a keychain.
- `shell/delete.rs`: **Delete connection…** (Project section and the end of the sidebar connection menu, after a separator, in the danger color). A tab of it with an open transaction or pending changes refuses with "Commit or roll back in X first."; else the danger dialog (Production badge when it is one) says what goes and how many tabs close. `project.toml` is written first; a failed write puts the connection back and shows the error, nothing else changes. Then its console, table, Sessions, configuration, diagram, object and import tabs close (console buffers without a last save), file tabs stay with `connection: None`, the driver drops on the runtime, passwords, prompts, key searches, restored objects and tree state of it go, the sidebar rebuilds, and on the blocking pool the keychain entries, cache rows, saved diagrams and console buffer are removed (logged on failure). The pool work starts from a task, after closed diagram tabs saved their layout on release. A connect or schema load finishing for a deleted connection is dropped (no driver, no cache row).
- **Delete project…** (Danger zone): the danger dialog with the connection count and folder, then the normal close checks (`CloseThen { Close, Switch, Delete }` replaces `switch_to`; Cancel in any prompt cancels the delete), then `project::delete` on the blocking pool. Meanwhile nothing writes into `.deadlock/` (`Shell.deleted`: state, console flush, cache saves; diagram tabs close first). Success: the recent entry goes (`app::forget_recent`, saved), the open-project window opens when no other project window is left (before this window goes: the app quits with its last window), the window closes. Failure: the window stays and a dialog shows the error ("What was removed before this step stays removed; Delete project again to remove the rest.").
- Settings → Project: **Connections** block (New connection, then each connection in sidebar order: color dot, name, Production badge, Read-only tag, `Engine · user@host:port` (Redis and a blank user leave the user out), Edit, Duplicate, Delete; "No connections yet." when empty), redrawn after every form save or delete; **Danger zone** with Delete project…. The Name field saves on every change (Phase 19 Open item).
- Tests: 258 core unit tests (`delete_removes_deadlock_data_and_keeps_the_folder_files` (also checks secrets go before `.deadlock/` and a second delete passes), `delete_stops_at_the_first_failure`, `forget_drops_one_connection_or_the_whole_project`, `forget_removes_one_connection_and_keeps_the_others`, `remove_drops_only_that_folder`); 73 app tests (`addresses_leave_out_a_redis_or_blank_user`). fmt, clippy `-D warnings` and `cargo test --workspace` pass.
- Checked in the real app under Xvfb with xdotool, against the docker MySQL 8.4 and MariaDB 11.8, with an isolated D-Bus session running its own unlocked gnome-keyring (items listed with `gdbus` `SearchItems service=deadlock`; `secret-tool` is not installed): the Connections block with a Production and a read-only connection, light and dark; Edit, Duplicate (saved as a new connection, the block and sidebar followed) and New from it; typing a name without Enter wrote `project.toml`, the window title and the recent entry; Delete connection from the sidebar menu with its console holding typed text, a table tab, Sessions and a diagram open (4 tabs closed, `query.sql` stayed unbound, its keychain item, cache rows, `diagrams.json` key and console buffer gone, the other connection's kept), and from the block; Delete project with an unsaved file (the save prompt came first, Cancel kept everything), then Don't Save and Save: `.deadlock/` gone, `query.sql` kept (with the saved text), keychain, history and cache empty, recent list empty, the open-project window shown; a failing delete (read-only `.deadlock/`) kept the window and showed the error, a retry finished. Screenshots: `/tmp/claude-1000/-home-lamka02sk-Code-deadlock/3053626f-53db-4f5b-9920-818c5c330e24/scratchpad/phase20/`.
- **Bugs fixed while checking:** the app quit after Delete project (the open-project window was opened after the last window closed); the failure path panicked (history panels were refreshed inside the shell's own update).
- **Open:** the refusal for a tab with an open transaction or pending changes was not clicked (it reads the tab's `tx_open` and `pending_changes`, as the status bar does). With no keychain service at all (`NoDefaultStore` / `PlatformFailure`), Delete project skips the secrets and deletes the rest, since nothing could have been saved there; a locked keychain still stops it (`delete_without_a_keychain_service_still_deletes`, unit test only). macOS and Windows not checked.

Milestone 7 complete.
