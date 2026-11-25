# Phases 19–20 — Project management in the app

Brief for the implementer. Scope is PLAN.md "Milestone 7 → Phases 19–20". Do not widen it.
Before writing Rust, load `.claude/skills/rust-skills/SKILL.md` and follow it.
Design: no artboards (Claude Design login was rejected on 2026-11-25, see §6). Build from the app's existing pieces and look: the switcher popover rows, Settings rows and `small_button`, the sidebar Color submenu swatches (`color::PALETTE`), `dialog::ask`. This file decides behaviour.

A project stays **a folder** (PLAN.md "Project and windows"). Nothing here changes that: a project's name, color and pattern are settings stored in its `.deadlock/project.toml`, and deleting a project deletes Deadlock's data, never the user's folder.

## 1. Goal and done-when

**Phase 19 — New project, name, color and pattern (M)**
- The project dropdown in the title bar (`switcher`, `shell.rs:1284`) gets two rows under the recent list: **New project…** and **Project settings…**.
- Settings gets a **Project** section with Name, Color (swatches) and Pattern.
- The title bar shows the project color as a gradient from the left edge, with the pattern laid over it.
- **Done when:** a new project can be created from the dropdown, renamed, given a color and a pattern, and the title bar, window title, dropdown and welcome window all show the change at once and after a restart, in light and dark theme.

**Phase 20 — Connections and Delete project (M)**
- The Project section lists the project's connections with New, Edit, Duplicate and Delete.
- Delete connection exists for the first time (also added to the sidebar connection menu).
- A Danger zone with **Delete project…**.
- **Done when:** a connection can be deleted with no secret, cache row, diagram, console buffer or tab left behind, and a project can be deleted with no `.deadlock/` folder, keychain entry, history row, cache row or recent entry left behind, while the folder and its `.sql` files stay.

## 2. Phase 19 — file by file

### `crates/deadlock-core/src/project.rs`
```rust
/// The pattern laid over the project color in the title bar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pattern { #[default] None, Stripes, Checks, Dots }

pub struct ProjectConfig {
    /// Shown instead of the folder name. `None` or blank: the folder name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `#rrggbb`, one of `color::PALETTE`. `None`: no gradient.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Pattern::is_none")]
    pub pattern: Pattern,
    #[serde(default)]
    pub connections: Vec<ConnectionConfig>,   // stays last: TOML values before the array of tables
}
```
- `Project::name()` returns the trimmed `config.name` when it is not blank, else `folder_name(&root)` (today's behaviour).
- The project is renamed in `project.toml` only. The folder is **not** renamed, because keychain entries are keyed by the folder path (`connection.rs:279`) and so are history and the schema cache.
- Tests: an old `project.toml` with only `connections` still parses; a full config round-trips through `save_config` / `open`; a blank name falls back to the folder name.

### `crates/deadlock-core/src/recent.rs`
- `RecentProject` gains `name: Option<String>` and `color: Option<String>` (`#[serde(default)]`), so the dropdown and the welcome window show them without reading every `project.toml`.
- `touch(path, opened_at)` keeps its signature. Add `set_look(path, name, color)`, called on register and on every Project settings save.
- Test: an old `recent.json` without the new fields loads.

### `crates/deadlock/src/app.rs`
- `register()` copies `project.config.name` / `color` into the recent entry.
- New `pub(crate) fn new_project(handle: AnyWindowHandle, cx)`: the native folder picker with `prompt: Some("Create project".into())`. The user picks a folder or makes a new one in the picker. Then:
  - folder already has `.deadlock/`: it opens like a recent pick (focus if open elsewhere, else switch this window after the close checks);
  - otherwise `Project::open` creates `.deadlock/` (today's code), the window switches to it, and Settings opens on the Project section so the name and color are set right away.
- Ctrl+click (Cmd+click) on New project… opens the new project in a new window, the same rule as the recent rows.
- Window title: `format!("{} — Deadlock", project.name())` already exists in two places. A rename calls `window.set_window_title` again.

### `crates/deadlock/src/shell.rs`
- `switcher()`: rows show `RecentProject.name` (fallback folder name) and a 8 px color dot when `color` is set. Below the list a separator, then **New project…** (`Icon::Plus`) and **Project settings…** (`Icon::Settings`).
- `toolbar()`: when `project.config.color` is set, the toolbar row paints, back to front:
  1. `linear_gradient(90°, stop(color::over(c, 0.22, p.panel), 0%), stop(p.panel, 55%))` as its background (`gpui-pre` 0.3.12 has `linear_gradient`, `pattern_slash`, `checkerboard` in `color.rs`);
  2. an absolute overlay over the left 40 %: `Stripes` = `pattern_slash(c @ 0.18, 1., 6.)`, `Checks` = `checkerboard(c @ 0.10, 6.)`, `Dots` = a `canvas` that paints 2 px round quads on an 8 px grid (the only pattern GPUI does not ship);
  3. a fade overlay over the same area, `linear_gradient` from transparent to `p.panel`, so the pattern ends softly;
  4. the toolbar items, unchanged, on top.
  Alphas are starting values: tune them on the artboard so every toolbar label and the PROD badge keep 4.5:1 contrast on both themes.
- The dropdown's Project settings row calls `open_settings` with the Project section in front.

### `crates/deadlock/src/settings_tab.rs`
- `Section::Project`, first in the list, titled with the project's name. `SettingsTab` already holds the project `root`; it also needs the `Shell` handle (weak) to read and write `project.config`.
- Rows: **Name** (input, placeholder = folder name, saved on blur or Enter), **Color** (the 12 `color::PALETTE` swatches plus None; the same swatches the sidebar Color submenu uses), **Pattern** (four small preview tiles: None, Stripes, Checks, Dots, drawn with the selected color).
- Every change: update `shell.project.config`, `save_config()`, `recent.set_look(...)` + save, `set_window_title`, `cx.notify()` on the shell. A failed write shows the error in the section and keeps the old value in the file (`write_atomic`).

## 3. Phase 20 — file by file

### `crates/deadlock-core/src/connection.rs` (`keychain`)
- Move `ssh_entry(id)` from `shell.rs:1221` here as `pub fn ssh_entry`, so core code can delete both entries.

### `crates/deadlock-core/src/cache.rs`, `history.rs`, `diagram.rs`, `recent.rs`
- `SchemaCache::forget(project, connection: Option<&str>)`: deletes one connection's rows, or the whole project's.
- `History::clear(project)` exists. Leave history of a deleted connection in place (it is project history; rows keep their connection name).
- `diagram::forget(root, connection)`: removes the connection's key from `diagrams.json`.
- `RecentProjects::remove(path)`.

### `crates/deadlock-core/src/project.rs`
```rust
/// Removes everything Deadlock stored for the project at `root`, never the folder itself:
/// keychain entries of `connections` (password and SSH secret), history rows, schema
/// cache rows, then `.deadlock/`. Secrets go first and `.deadlock/` last, so a failure
/// part way leaves a project that still opens and can be deleted again.
/// # Errors  The first step that fails; nothing after it runs.
pub fn delete(root: &Path, connections: &[String], history: &mut History, cache: &SchemaCache) -> Result<()>;
```
- Test (temp dir, no keychain): `.deadlock/` is gone, a `query.sql` next to it is still there.

### `crates/deadlock/src/shell.rs` — Delete connection
`Shell::delete_connection(id, window, cx)`, reached from the Project section and from a new **Delete connection…** row at the end of the sidebar connection menu (`sidebar.rs:823`, after a separator).
1. Refuse while a tab on that connection has an open transaction or pending edits (`pending_changes`): a plain dialog says "Commit or roll back in tab X first."
2. Confirm with `dialog::ask(Dialog::new("Delete connection {name}?", &["Delete connection", "Cancel"]).danger(true).text(...))`. The text says the saved password is removed and how many tabs close.
3. Drop the driver (`self.conns.remove`), close its console, table, sessions, config, diagram and object tabs. File tabs stay and lose their connection (`connection: None`), so no text is lost.
4. Remove it from `project.config.connections`, `save_config()`. If the save fails, stop here and show the error: nothing else is deleted.
5. On the blocking pool: `keychain::delete` for `id` and `ssh_entry(id)`, `SchemaCache::forget(root, Some(id))`, `diagram::forget(root, id)`, remove `console_path(root, id)`. These failures are logged, not shown: the connection is already gone.
6. Reload the sidebar tree.

### `crates/deadlock/src/shell.rs` — Delete project
- `switch_to: Option<PathBuf>` becomes `after_close: AfterClose { Close, Switch(PathBuf), Delete }`, read in `finish_close` (`shell.rs:316`).
- **Delete project…** confirms with a danger dialog: "Delete project {name}?", text "Removes Deadlock's data for this folder: {n} connections and their saved passwords, query history, schema cache, diagrams and console buffers. The folder {path} and your files stay.", buttons `["Delete project", "Cancel"]`.
- Then the normal close checks run (open transactions, unsaved files). Cancel there cancels the delete.
- Then `project::delete` on the blocking pool, `recent.remove(root)` + save, the window closes, and the welcome window opens if no project window is left (`show_welcome`). If `project::delete` fails, the window stays open and the dialog shows the error.

### `crates/deadlock/src/settings_tab.rs`
- **Connections** block in the Project section: a **New connection** button (calls `Shell::new_connection`), then one row per connection in sidebar order: color dot, name, engine, `user@host:port` (Redis without user), PROD badge (`dialog::production_badge`), read-only tag. Row buttons: Edit (`edit_connection(id, true)`), Duplicate (`edit_connection(id, false)`), Delete (`delete_connection`). Empty list: "No connections yet."
- **Danger zone** block at the bottom: the Delete project… button in the danger style.
- The list re-renders from `project.config` after every form save or delete (the shell notifies the tab).

## 4. Order of work

1. Phase 19 core: `Pattern`, `ProjectConfig` fields, `name()`, recent fields, tests.
2. Settings Project section with Name / Color / Pattern; window title and recent update.
3. Title bar gradient and the three patterns; check contrast on both themes.
4. Dropdown rows: New project…, Project settings…; `new_project` in `app.rs`.
5. Phase 20 core: `ssh_entry` move, `forget` functions, `project::delete`, tests.
6. Delete connection (shell + sidebar menu), then the Connections block.
7. `AfterClose`, Delete project, Danger zone.

## 5. Tests and manual checks

- Unit: config round trip and old-file compatibility (19), recent compatibility (19), `project::delete` keeps user files (20), `SchemaCache::forget` deletes only the given connection's rows (20), `diagram::forget` keeps other connections (20).
- Manual, per phase, on Linux and on one other OS:
  - the folder picker lets the user create a new folder (GTK portal, macOS, Windows). **If one platform cannot, stop and report**: the fallback is a Name + Location form in the dropdown, which is a bigger change;
  - each pattern in light and dark theme with three colors (Red, Blue, Slate), window at 900 px and 1600 px wide;
  - Delete connection while its console has text and a table tab is open; check the keychain (`secret-tool search service deadlock` on Linux) afterwards;
  - Delete project with an unsaved file open: the unsaved-file prompt comes first, Cancel keeps everything.

## 6. Design gaps (add to the canvas, review before each phase starts)

- Phase 19: the dropdown with New project… / Project settings… and color dots; the Settings Project section (Name, Color, Pattern tiles); the title bar with each pattern in light and dark.
- Phase 20: the Connections block rows; the Danger zone; the Delete connection and Delete project dialogs.

## 7. Out of scope

Renaming or moving the project folder, moving the folder to the trash, reordering connections, an Open folder… row in the dropdown, per-project app settings overrides, importing connections from another project, a custom color picker beyond `PALETTE`.
