# Milestone 6 — closing every gap

The user's bar: production users must be able to trust these features, so every **Open** item in the Phase 15–18 result sections of `PLAN.md` gets fixed. Nothing is left open silently. If an item truly cannot be fixed (the server forbids it), the app must say so clearly *before* Apply, and the result section says why.

Read the Phase 15–18 result sections of `PLAN.md` and the four phase plans in this folder first. Designs: `/tmp/claude-1000/-home-lamka02sk-Code-deadlock/d97b1f4a-a095-4256-9a25-28d76a85257c/scratchpad/design/project/{EditColumn,TableActions,ForeignKeyCheck,NewTable}.dc.html`.

## Batch A — data correctness (deadlock-core first, then UI wiring)

1. **MariaDB JSON `json_valid` lost on edit.** `CHANGE` drops a JSON column's column-level check. Read column-level checks (`CHECK_CONSTRAINTS.LEVEL = 'Column'`, or the DDL on older servers) and restate them in the column definition, so a no-op edit keeps `SHOW CREATE TABLE` byte-identical. Turn the pinned test into a passing one.
2. **MySQL spatial `SRID` not restated.** Read `COLUMNS.SRS_ID` (MySQL 8.0+) and restate `srid N` on edit. Add it to the round-trip engine test.
3. **`enum` / `set` members with commas.** Parse and write members as quoted SQL literals, so `'a,b'` and `'it''s'` round-trip. Unit and engine tests.
4. **Backslash escaping in Create table.** The draft must quote literals in the server's actual mode: read `@@sql_mode` (the tab's session once it connects, or a one-off query) before building the statement, and rebuild the preview when it arrives. Test with `NO_BACKSLASH_ESCAPES` on.
5. **Row estimate 0 skips the forced reason.** InnoDB estimates can be 0 on a non-empty table. When the estimate is 0, check for rows with `select 1 from t limit 1` before deciding. Keep that check off the UI thread.
6. **Partial failure closes the form.** After "Ran k of n", reload the structure and keep the form open, rebased on the reloaded column (by its new name if the rename ran), so Apply again builds correct SQL for the rest.
7. **"Ran k of n" names nothing.** Name what ran and what failed: "Ran 1 of 3 (rename column). Statement 2 (modify) failed: ORA-…".
8. **Oracle ORA-54031.** Before Apply, when another column's generation expression references the edited column and its type changes, show the hint "Oracle cannot change a column that virtual column X reads: drop or change X first." Apply stays enabled.
9. **MariaDB older than 10.5.10 lists no checks.** Fall back to the query without `LEVEL`, and leave out rows whose name equals a column name with a `json_valid`-style column check. Cover it with a unit test of the row filter.

## Batch B — UI, safety and design parity

10. **Closing the window with drafts open.** Closing the window or quitting while a New table draft (or Edit column form) has typed content asks, using the same prompt pattern as dirty object tabs (`dirty_objects`).
11. **Rename leaves old names behind.** After a table rename: open trigger and object tabs for that table follow the new name, and the completion / schema cache refreshes. User-typed SQL in editors is **not** rewritten, because rewriting someone's text is unsafe. The rename dialog already warns; add "Open editors still use the old name" to it.
12. **Sidebar doesn't select the new table.** After Create, select the new table node in the sidebar.
13. **The FK table list is a snapshot.** Refresh the referenced-table list when the form opens and after `SchemaChanged`.
14. **Collation as a dropdown.** Edit column, Add column and the New table grid get a searchable list of the column's character-set collations (`information_schema.COLLATIONS`, loaded once per connection). Keep the current value even if it's missing from the list.
15. **Copy CREATE statement** in the sidebar table menu, as in `TableActions` (reuse `copy_create`).
16. **Warning icon** in the dialog's warning band: add a warning triangle to the icon set (inline SVG asset, same stroke style) and use it there and in the form bands.
17. **"Double-click" hint** on Edit column… in the row menu. Add a hint slot to the house menu-item helper if the popup menu allows one. If it really can't, say why in the result.
18. **FK form parity:** the "no index yet: MySQL adds one with the key's name" info line, type-styled referenced-column labels (type dimmed), a charset/collation mismatch hint for text columns, and **cross-schema references** (the table list includes other schemas as `schema.table`; the SQL qualifies the referenced table; Oracle needs `REFERENCES` privilege, so show the server error as is).
19. **New table primary-key order.** The Primary key list's order is the key order, independent of row order. Add up/down on the list.

20. **One-statement check under `NO_BACKSLASH_ESCAPES`.** The guard that refuses a second statement can wrongly refuse a value ending in `\` when a later value contains `;` (left over from Batch A). Make it scan literals in the session's escaping mode. Unit test both modes.

`;` in a new table name is quoted into the name. That is valid and safe SQL, so it stays.

## Batch C — click through the real app

Nothing in Milestone 6 has been clicked through yet. Run the app under `xvfb-run` (software rendering) against the docker engines. Drive it with `xdotool` and take a screenshot with `import -window root` at each step. Walk every UI path the result sections list as not clicked:
- Edit, Move and the forced-reason prompt.
- Table actions from both menus, including a diagram following a rename and a drop.
- FK and check forms.
- The New table draft through Create, the discard prompts, read-only disabling, and a restart restoring tabs.

Compare each screen with its artboard. Fix every bug found. Keep the screenshots in the scratchpad and list them.

## Rules for every batch

- Load `rust-skills`. No new dependencies. No commits.
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` must be clean.
- Run all Milestone 6 engine tests on MySQL, MariaDB and Oracle:
  `cargo test -p deadlock-core --test engines -- --ignored --test-threads 1 create_table columns_edit table_actions keys_and_checks table_structure`
- Update that phase's **Open** line in `PLAN.md`: remove each fixed item, and keep only what the server truly forbids, with the reason.
