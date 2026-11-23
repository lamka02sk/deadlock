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
