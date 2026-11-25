# Milestone 8 — Console schema, navigation and fixes (release 0.1.4)

Branch `milestone-8-console-and-polish`. Two phases run side by side (separate worktrees)
and are merged at the end, so each phase keeps to its own files where it can.

Already done on the branch: dark theme text `tx` is `#E5E5E5` (was `#FAFAFA`); a tinted tab
behind the front one has its bottom border in its own background (`shell/tabs.rs::tinted`).

Rust code follows the `rust-skills` skill. Check every change in the real app under Xvfb
with xdotool against the docker MySQL / MariaDB (`docker-compose.yml`), as Phases 19–20 did,
in both themes. fmt, clippy `-D warnings` and `cargo test --workspace` pass at the end.

## Phase 21 — Console schema picker and "console follows the last click" (M)

Design: canvas artboard **Console schema picker (0.1.4)**,
https://claude.ai/artifact/RGYCGMf4iayRAkej7r2wns#artboard-SchemaPicker (light and dark).

1. **Schema chip in the toolbar**, between the separator after Save and Run (`shell.rs::toolbar`,
   a new `Item` like `Item::TxMode`): database icon (`--tx3`), schema name (`--tx`, cut at
   160 px, full name in the tooltip "Schema statements run in"), chevron. Shown for query tabs
   (files and consoles) bound to a connection; hidden for table tabs and unbound tabs;
   disabled while the tab runs and while a switch is in flight (spinner instead of the icon).
   Count its width in the toolbar's label-dropping maths.
2. **Menu** (280 px popover under the chip): filter field (focused, shown when more than 8
   schemas), rows of the connection's schemas from the schema cache / sidebar tree minus
   hidden schemas, a check on the current one, a `default` tag on the connection's configured
   `database`; ↑↓ Enter Esc; footer "Runs USE on this tab's session." (Oracle:
   `ALTER SESSION SET CURRENT_SCHEMA`, Redis: `SELECT n`, rows `db 0` … `db 15` or the
   databases the sidebar lists).
3. **Switching** runs the statement on the tab's own session (open it first if there is
   none). On success the chip shows the new schema; on failure the error goes to Messages like
   any statement and the chip keeps the old one. `heal.rs` already replays the last `USE` /
   `ALTER SESSION SET CURRENT_SCHEMA` after a reconnect: make sure a picked schema goes
   through the same path.
4. **Follow a typed `USE`**: after a run, the chip shows the session's schema (track the last
   successful `USE` / `ALTER SESSION SET CURRENT_SCHEMA` / Redis `SELECT`, or ask the server).
5. **Remember after a restart**: the tab's schema goes into `state.json` (`TabState`, with
   `serde(default)` so old files load) and is applied when the tab's session opens.
6. **No schema**: the chip reads "No schema" in `--tx3` when the connection has no default
   and none was picked.
7. **Console follows the last click** (`shell/tabs.rs::open_console`, `Shell::active`): today
   the front tab's connection wins over the sidebar selection. Track which came last: a click
   on a sidebar row (any row under a connection) or a tab becoming front by the user (strip
   click, keyboard tab switch, opening a tab). The Console button and Ctrl+Shift+N open the
   console of that connection, and **set the console's schema** to that context's schema when
   it has one (a table, view, routine… row or tab → its schema; a schema row → it; a
   connection row or a context without a schema → leave the console's schema as is). This
   applies to an already open console too. The Console button's enabled state uses the same
   rule.

Done when: the chip shows, switches, follows a typed `USE`, survives a restart, works on MySQL,
MariaDB and Oracle (Redis if the sidebar lists its databases), and the console opens on the
last clicked connection and schema.

Expose `QueryTab::schema(&self) -> Option<&str>` (the tab's current schema, falling back to
the connection's `database`) — Phase 22 resolves unqualified names with it after the merge.

## Phase 22 — Run icons, Ctrl+click, connection order, project switching and closing (M)

1. **Run icons lag the scroll** (`query.rs::render_editor`). The ▶ icons in the gutter follow
   scrolling late and jump to the right row after scrolling stops. They are absolutely
   positioned from `editor.range_to_bounds`, i.e. last frame's layout, and the tab re-renders
   only when the editor notifies. Make them paint in the same frame as the text (e.g. position
   them in a prepaint after the editor's, or draw them from the editor's own layout), still
   clickable with the tooltip. Check with fast wheel scrolling and scrollbar dragging, with
   inline result blocks on screen.
2. **Ctrl+click on an object name** in SQL (files, console, object editors; Cmd on macOS):
   with Ctrl held, an identifier under the pointer that names an object gets an underline
   (`--link`) and a pointing cursor; the click opens it the way the sidebar does (table/view →
   table tab, routine/trigger/… → object editor). Resolve `schema.name` and `name`; an
   unqualified name resolves in the tab's current schema (`QueryTab::schema`, Phase 21; until
   the merge use the connection's `database`), then in aliases' targets (`FROM orders o` →
   `o.` is `orders`). Columns are out of scope. Use the schema cache, as autocomplete does
   (`shell/complete.rs`). Nothing under the pointer or not found: no underline, the click
   places the caret as usual.
3. **Order of connections** in Settings → Project → Connections: drag rows (a grip on the
   left) and Move up / Move down in each row's menu or buttons, keyboard reachable. The order is
   the order in `project.toml`, which the sidebar already follows; save on drop.
4. **Picking another project in the toolbar dropdown sometimes does nothing**, and **the
   window sometimes will not close with the title bar X**. Both run `Shell::should_close`
   (`CloseThen::Switch` / `CloseThen::Close`). Lead: `Shell::settle` (`shell/tabs.rs`) awaits
   `check_tx` of every query tab before anything else; a tab whose session is busy, lost or
   reconnecting can leave that future pending forever, so no prompt shows and nothing happens.
   Also check `may_close`, `pending_settled`, `tx_settled`, `objects_settled` flags left set
   or cleared at the wrong moment by a cancelled prompt, and `switcher_open` / defer ordering
   in `pick_project`. Find the real cause (reproduce it: a running long statement, a stopped
   docker server, a tab mid-reconnect, a cancelled prompt and then a second try), fix it at
   the root (a bounded wait that treats "no answer" as "nothing to commit, say so", or
   whatever the cause needs), and keep the safety: an open transaction must still be asked
   about.

Done when: the icons stay on their rows during scrolling; Ctrl+click opens tables, views and
routines on MySQL, MariaDB and Oracle; connections reorder and the sidebar follows after a
restart; switching projects and closing work after each reproduction case.
