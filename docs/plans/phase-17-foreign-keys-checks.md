# Phase 17 — Add foreign key, add and list checks (M)

Brief for the implementer. Scope is PLAN.md "Milestone 6 → Phase 17". Do not widen it. Starts from finished Phases 15–16 (`run_ddl(Vec<String>, …, After)`, `TableStructure.mariadb`, read-only disabling, `form_sql` returning `Vec`).
Before writing Rust, load `.claude/skills/rust-skills/SKILL.md` and follow it.
Design: artboard **`ForeignKeyCheck`** on https://claude.ai/artifact/RGYCGMf4iayRAkej7r2wns (Add foreign key and Add check forms). The artboard decides looks; this file decides behaviour.

## 1. Goal and done-when

- Structure view: "Add foreign key" on the Foreign keys section, "Add check" on the Check constraints section (shown for every table, empty or not, on all three engines).
- MySQL 8.0.16+ / MariaDB 10.2+ checks are read from `information_schema`, listed and droppable (today only Oracle's are).
- **Done when:** an FK and a check can be added and dropped from Structure on MySQL 8.4, MariaDB 11 and Oracle Free, and both appear in their sections without a manual reload (the existing `ddl_done` → `load_structure`).

## 2. File-by-file changes

### `crates/deadlock-core/src/structure.rs`
```rust
/// Referential actions the Add foreign key form offers.
pub const FK_ACTIONS: [&str; 4] = ["restrict", "cascade", "set null", "no action"]; // MySQL, MariaDB
pub const ORACLE_FK_ACTIONS: [&str; 3] = ["no action", "cascade", "set null"];      // on delete only

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewForeignKey {
    pub name: String,            // empty: the server names it
    pub columns: Vec<String>,    // of this table, in order
    pub ref_table: TableRef,     // same schema as the table (see §7)
    pub ref_columns: Vec<String>,
    pub on_delete: String,       // one of the lists above
    pub on_update: String,       // MySQL / MariaDB only; ignored on Oracle
}

/// `alter table … add [constraint n] foreign key (…) references … (…) on delete … [on update …]`.
/// # Errors  No columns, counts differ, an action not in the engine's list, text that ends the statement.
pub fn add_foreign_key_sql(t: &TableRef, k: &NewForeignKey, s: &TableStructure) -> Result<String, String>;

/// What the server will likely refuse, as hints shown before Apply (never blocking):
/// a type mismatch per column pair (`pa is int, parent.a is bigint`), referenced columns
/// that are not exactly a primary key or unique index of `to`, `set null` on a NOT NULL column.
pub fn foreign_key_hints(k: &NewForeignKey, from: &TableStructure, to: &TableStructure) -> Vec<String>;

/// `alter table … add [constraint n] check (expr)`.
/// # Errors  An empty condition, text that ends the statement.
pub fn add_check_sql(t: &TableRef, name: &str, condition: &str, s: &TableStructure) -> Result<String, String>;
```
- Type comparison in hints: `column_type` lowercased, MySQL ignoring a display width (`int(11)` = `int`); string lengths may differ (MySQL allows it) — compare the base type and `unsigned` only. Key check: some `Index` with `primary || unique`, no expression parts, and the same column **set** as `ref_columns`.
- `TableStructure::drop_constraint_sql`: `verb = if self.oracle || self.mariadb { "constraint" } else { "check" }` (MariaDB has no `DROP CHECK`). Doc of `checks` field: "Check constraints (all three engines)".

### `crates/deadlock-core/src/mysql.rs` (`structure()`)
After the keys query, read checks into `s.checks` (set after `structure_from_rows`, like `s.ddl`):
- MySQL:
  ```sql
  SELECT tc.CONSTRAINT_NAME, cc.CHECK_CLAUSE, tc.ENFORCED
  FROM information_schema.TABLE_CONSTRAINTS tc
  JOIN information_schema.CHECK_CONSTRAINTS cc
    ON cc.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA AND cc.CONSTRAINT_NAME = tc.CONSTRAINT_NAME
  WHERE tc.TABLE_SCHEMA = ? AND tc.TABLE_NAME = ? AND tc.CONSTRAINT_TYPE = 'CHECK'
  ORDER BY tc.CONSTRAINT_NAME
  ```
  (`CHECK_CONSTRAINTS` has no `TABLE_NAME` on MySQL; `ENFORCED` is `YES`/`NO`.)
- MariaDB (`CHECK_CONSTRAINTS` has `TABLE_NAME`, no enforced flag):
  ```sql
  SELECT CONSTRAINT_NAME, CHECK_CLAUSE, 'YES' FROM information_schema.CHECK_CONSTRAINTS
  WHERE CONSTRAINT_SCHEMA = ? AND TABLE_NAME = ? ORDER BY CONSTRAINT_NAME
  ```
- Servers older than MySQL 8.0.16 / MariaDB 10.2.22 have no such view: treat a failed query as no checks (`unwrap_or_default()` with a `// shortcut: …` comment per the house rule). Map rows to `Check { name, condition: clause, enabled: enforced == "YES" }`. Skip for views.

### `crates/deadlock/src/query/structure.rs`
- `Form::ForeignKey(FkForm)` and `Form::Check(CheckForm)`:
  ```rust
  struct FkForm {
      name: Entity<InputState>,
      columns: Vec<String>, add_column: Choice,          // this table's columns, multi-pick in order
      tables: Option<Vec<String>>, ref_table: Choice,    // None while the list loads
      target: Option<TableStructure>,                    // the referenced table, loaded on pick
      ref_columns: Vec<String>, add_ref_column: Option<Choice>,
      on_delete: Choice, on_update: Option<Choice>,      // on_update None on Oracle
  }
  struct CheckForm { name: Entity<InputState>, condition: Entity<InputState> }
  ```
- `open_fk_form(window, cx)`: build the form; fetch the schema's tables with `driver.children(&ObjectPath(vec![schema, "Tables".into()]))` on the tab's session (both drivers walk the rest of the path: `mysql.rs` `for name in rest`, `oracle/schema.rs` l. 79), keep `NodeKind::Table` names, then `cx.spawn_in(window, …)` + `this.update_in(…)` (pattern: `object.rs:313`) to fill `ref_table`'s items. Picking a table (subscribe to `SelectEvent::Confirm`): if it is this table reuse `data`, else `driver.structure(&TableRef { database: schema, table })`, then build `add_ref_column` from its columns (again via `spawn_in`/`update_in`, since a `SelectState` needs the window). Column pickers and their Remove buttons reuse the index form's part list (`button_at`, `Select … placeholder("Add a column…")`).
- `form_sql` arms: FK → `add_foreign_key_sql`; Check → `add_check_sql`; both `.map(|s| vec![s])`. FK hints (`foreign_key_hints`, when `target` is loaded) render as `p.warn`/`p.tx3` lines above the preview.
- `apply_form`: done texts "Foreign key added." / "Check constraint added."; `After::Reload`.
- Render: section heads `Foreign keys` and `Check constraints` get `add(..)` buttons (`!busy && !read_only`, like Add index); the Checks section renders for every non-view table, with its rows and the existing Drop item.
- Engine-specific choices: on Oracle `on_delete` from `ORACLE_FK_ACTIONS` and no On update field (hint "Oracle has no ON UPDATE for foreign keys."); else `FK_ACTIONS` for both.

## 3. SQL per engine

| | MySQL 8.4 | MariaDB 11 | Oracle Free 23 |
|---|---|---|---|
| Add FK | `` alter table `d`.`c` add constraint `fk` foreign key (`pa`, `pb`) references `d`.`p` (`a`, `b`) on delete cascade on update restrict `` | same | `alter table "S"."C" add constraint "FK" foreign key ("PA") references "S"."P" ("A") on delete cascade` — `no action` writes no clause; no ON UPDATE |
| Unnamed FK | `… add foreign key (…) …` | same | same |
| Add check | `` alter table `d`.`t` add constraint `ck` check (qty >= 0) `` | same | `alter table "S"."T" add constraint "CK" check (qty >= 0)` |
| Drop check | `` alter table `d`.`t` drop check `ck` `` (8.0.16+) | `` … drop constraint `ck` `` | `… drop constraint "CK"` (exists) |
| Drop FK | exists (`drop foreign key`) | exists | exists (`drop constraint`) |

Server behaviour to expect (shown as the server's error, not pre-empted beyond the hints): MySQL 8.4's `restrict_fk_on_non_standard_key` (ON by default, new in 8.4.0) rejects FKs whose referenced columns are not a full PK/unique key; InnoDB needs matching types and signedness; MySQL creates the referencing index itself; Oracle needs a PK/unique constraint on exactly the referenced columns (ORA-02270); existing rows that violate a new check fail with the server's error (MySQL 3819, ORA-02293). **Verify:** MariaDB column-level checks (`LEVEL = 'Column'`, e.g. the `json_valid` check of a JSON column) drop with `drop constraint` too.

## 4. UI steps

1. `Add foreign key` / `Add check` buttons via the existing `add(id, label, tip, cx)` closure; forms in `form_box` under their section head, matching `ForeignKeyCheck`.
2. FK form: Name; Columns (multi-pick, ordered); References (searchable `Select` of the schema's tables; "Loading tables…" while `tables` is `None`); Referenced columns (enabled once `target` is loaded; count must match); On delete; On update (MySQL only); hints; preview; Cancel/Apply.
3. Check form: Name, Condition (`text_field(.., true)`); preview.
4. Read-only: buttons and Apply disabled.

## 5. Tests

Unit (`cargo test -p deadlock-core`):
- structure.rs `foreign_key_sql_per_engine`: composite MySQL FK with both actions and quoting; unnamed; Oracle without ON UPDATE and with `no action` omitted; count mismatch / no columns / `set default` → Err.
- `foreign_key_hints_flag_likely_refusals`: `int` vs `bigint`, `int(11)` vs `int` (no hint), unsigned mismatch, non-unique referenced columns, `set null` on NOT NULL.
- `check_sql_and_drop_syntax`: add check on the three engines; `drop_constraint_sql` gives `drop check` (MySQL), `drop constraint` (MariaDB: `TableStructure { mariadb: true, .. }`, Oracle).

Integration (ignored; `docker compose up -d --wait mysql mariadb oracle && cargo test -p deadlock-core --test engines -- --ignored keys_and_checks`):
- `mod table_structure`: `fn keys_and_checks(engine, port)` + `mysql_keys_and_checks` / `mariadb_keys_and_checks`: tables `p17_parent (a int, b varchar(20), primary key (a, b))`, `p17_child (id int primary key, pa int, pb varchar(20), qty int)`; add composite FK (named) with `on delete cascade on update set null` → re-read: in `foreign_keys` with both actions, parent's `referenced_by` lists it; add an unnamed FK → listed with a server name; add check `qty >= 0` → listed in `checks` (MySQL condition `(`qty` >= 0)`, MariaDB `` `qty` >= 0 ``); insert a violating row → Err; drop the check and both FKs via `drop_constraint_sql` / `drop_foreign_key_sql` → gone.
- `mod oracle`: `oracle_keys_and_checks`: same with `on delete set null`, check add/drop; `on_update` ignored in the SQL.
- Existing `structure_reads_and_alters` must still pass (its MySQL table now reports no checks; add a `CHECK` to `p7_child` only if it helps, not required).
- CI set: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. Manual check in `cargo run -p deadlock -- <project>` on the three engines.

## 6. Ordered tasks

1. Load the rust-skills skill; read `ForeignKeyCheck` on the canvas.
2. structure.rs: constants, `NewForeignKey`, `add_foreign_key_sql`, `foreign_key_hints`, `add_check_sql`, MariaDB `drop constraint`; unit tests.
3. mysql.rs: checks queries per flavor into `s.checks`.
4. engines.rs: `keys_and_checks` (MySQL, MariaDB) and `oracle_keys_and_checks`; run against compose.
5. query/structure.rs: Check form (small, do it first), then FK form with the async table list and target load, hints, buttons, always-shown Checks section.
6. CI commands; manual check.

## 7. Do not

- No editing of an existing FK, check or index (drop and add again), no cross-schema references, no `set default`, no enabling/disabling of checks or FKs, no Oracle `deferrable` / `novalidate` options.
- No new dependencies; no commits; no edits to PLAN.md.

## 8. Risks / open questions

- PLAN says the referenced table comes from the "schema cache list"; this plan asks the tab's own driver (`children([schema, "Tables"])`), which returns the same nodes the sidebar caches but is a fresh round trip (on MySQL it reads the whole schema's columns too). Cheaper alternative: the shell hands the tab its tree's `Tables` children through a new `TabEvent`. Parent to confirm.
- References limited to the table's own schema (matches "schema cache list"); cross-schema FKs need a schema picker — out of scope.
- Check condition text is sent as typed (single statement enforced); the server's parse error is the only validation.

## 9. Decisions after design review (parent, 2026-11-25)

- Referenced tables: prefer the shell passing its cached `Tables` children for the schema to the tab (a new `TabEvent`/method). Use the fresh `children()` lookup only if the cache is missing or not loaded. Say which one you built.
- Same-schema references only, as §7 says. The artboard's "other schemas as schema.table" is out of scope.
- Follow `ForeignKeyCheck` for layout: each column paired with its referenced column on one row, "No foreign keys yet." when empty, the Check constraints section always shown on supported servers (with "No check constraints yet."). Warnings never block Apply.
