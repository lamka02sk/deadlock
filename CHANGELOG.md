# Changelog

All notable changes are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to follow
[Semantic Versioning](https://semver.org/) once it reaches 1.0.

## [Unreleased]

### Added

- The connection menu lists every schema (database) with a checkbox: tick or untick to
  show or hide them, several in one visit. Show all brings every hidden one back. Hide
  in a schema's own menu stays.
- Connection setting "Show empty object groups in the sidebar", off by default: groups
  with nothing in them (Views 0, Sequences 0…) are left out of the tree.
- Edit in the sidebar menu of tables (opens the Structure view), views, routines and
  other objects.
- Scratch files: New scratch file in the Files panel menu makes a `.sql` file in
  `.deadlock/scratches/`, listed first as Scratches, outside the project's own folders.
- Interface and code fonts in Settings → Appearance: any installed font, searchable,
  for every window at once. Default keeps Inter and JetBrains Mono.
- Search in Settings: filters settings, help texts and shortcuts (by name, group or keys)
  across all sections.
- The New / Edit connection tab closes like other tabs: its close button, a middle click
  and `Ctrl+W`.
- Stop button in the data view's pager while rows load: cancels the query on the server
  and keeps the rows already on screen.
- The `where` and `order by` fields show a spinner while the load their Enter started
  runs.

### Changed

- The transposed data view (`Ctrl+T`) is now the data grid itself, turned: fields down,
  rows across, field names fixed on the left. Everything the grid does works there too:
  inline editing, sorting by a click on a field's name (Shift adds it), the cell menu,
  drag selection with autoscroll at the edges, Shift+arrows, copy, paste and the pending
  change marks. The cursor stays on the same value when switching. Lines between the
  records keep them apart, since their fields line up differently. The data view has
  the same lines between its columns.
- A click on a column header shows the new sort arrow at once, with a spinner until
  the sorted rows arrive (it used to appear only after the reload).
- Leaf objects in the sidebar (columns, routines, sequences) start at the chevron's place,
  so they are less indented.
- The project button in the toolbar has a subtle background at rest.

### Fixed

- The sidebar keeps the width you drag it to after a restart (in `.deadlock/state.json`).
- The sidebar remembers which nodes were open after a restart (in `.deadlock/state.json`),
  painted from the schema cache until the connection connects. Opening a node under a
  connection that is not connected yet connects it.
- The sidebar header and the Files and History panel headers line up with the tab strip.
- Hover in the sidebar tree and the Files panel stuck on the first row the pointer entered.
- Empty object groups were indented a few pixels more than their siblings with a chevron.
- Dragging a selection to the left or right edge of a grid scrolls it sideways, starting
  32 px inside the edge, so it works when the grid ends at the screen's edge.
- New files and folders did not show in the Files panel in big projects: the scan used up
  its entry limit inside deep folders (`vendor/`, `node_modules/`) first. It now lists
  folders level by level, so entries near the root always come first.
- The Structure view's definition is a full-height side panel: it stays put while the
  columns and indexes scroll, scrolls on its own both ways (Shift+wheel sideways), and its
  text can be selected and copied with the mouse.
- Read-only SQL can be selected with the mouse and copied everywhere it shows: an
  object's Source tab (sequences, triggers…), the object and Structure form previews,
  pending changes, confirmation dialogs, server configuration, session and deadlock
  statements, and Redis command previews.

## [0.1.5] - 2026-11-26

### Added

- Autocomplete in the data view's `where` and `order by` fields, the table's columns first.
- Refresh button in the data view's pager: reloads the page and keeps the scroll position.
  Its icon spins while rows load, and the time the last load took shows beside it.
- Transposed data view (`Ctrl+T` or the pager button): one row as a column / value list,
  stepped with the arrow keys; edits go through the cell dialog into the pending changes.
- Copy query button in the data view: the `select` the where and order by fields make.
- Resized column widths are remembered per table.
- `Ctrl+W` closes the tab in front; `Ctrl+Shift+T` reopens the last of 25 closed tabs.
- The schema menu's New items each have their own icon.

### Changed

- Copying several cells in the data view joins the values with `, ` on one line.
- The Delete row button is gone from the data view toolbar; the `Delete` key and the row
  menu still delete rows. Rows per page moved next to the page buttons.

### Fixed

- The data view no longer sorts by the table's key when the order by field is empty. On a
  big Oracle table that sort read every row through the key's index (seconds instead of
  milliseconds); rows now come in storage order, as in DataGrip. A typed order by still
  ends with the key, so pages do not overlap.
- Dragging a selection past the top or bottom of a grid scrolls it.
- Shortcuts stopped working after closing a tab until something was clicked.

## [0.1.4] - 2026-11-26

### Added

- Schema picker for consoles and SQL files in the toolbar: switches the tab's session,
  follows a typed `USE`, and is remembered per tab after a restart.
- The console opens on the connection and schema last clicked in the sidebar or tab strip.
- Ctrl+click (Cmd+click on macOS) on a table, view or routine name in SQL opens it.
- Reorder connections in Settings → Project.

### Changed

- Dark theme text is a soft grey instead of white.
- Tabs behind the front one no longer have a bottom border in the connection's color.

### Fixed

- Run icons in the editor gutter move with the text while scrolling.
- Switching projects from the toolbar menu or closing the window sometimes did nothing
  while a tab's session was busy or unreachable.

## [0.1.3] - 2026-11-25

### Added

- Table editing: edit columns, table actions, keys and checks, and create new tables.
- Project management: new project, name, color and pattern, connections, delete project.

## [0.1.2] - 2026-11-24

### Fixed

- Linux packages (`.deb`, AppImage) are built on Ubuntu 24.04, so they install and run
  there. The 0.1.1 packages were built on Ubuntu 26.04 and need glibc 2.43 or newer.

## [0.1.1] - 2026-11-24

The Linux packages of this release need Ubuntu 26.04 or newer; use 0.1.2 on older systems.

### Added

- Global search (double Shift): objects, columns and source text per engine, with hidden
  schemas excluded in the query.
- SQL autocomplete for objects, columns and keywords in files, the console and object
  editors (Ctrl+Space).
- Files panel (Ctrl+Alt+B): the project's `.sql` tree with rename, new file, delete to
  trash and a file watcher.
- Form-based editors for views, routines, triggers, events, sequences, synonyms,
  materialized views, packages and types, with a safe apply (check, swap, restore) per
  engine.
- Data grid: explicit paging with page size and a count-all-rows button, row numbers that
  continue across pages, multi-column sort (Shift+click), multi-cell selection, a richer
  context menu (copy, paste, duplicate, delete, set NULL, INSERT, CSV, JSON), a JSON
  editor and a date-time picker.
- Edit and duplicate connections, per-connection colors, hiding schemas, and a project
  switcher in the toolbar.
- PROD and STAGE badges on tabs, middle-click to close tabs, a Settings button on the app
  icon.

### Changed

- Transactions on MySQL and MariaDB start lazily, so no empty transaction is left open.
- Oracle needs fewer round trips per page and counts rows with a bounded automatic count.
- Menus have padding and icons, clickable elements show a pointer cursor, and the sidebar
  search matches object names only.
- The welcome screen closes when a project opens.

### Fixed

- Connections recover from a lost session, and a session can no longer kill itself.

## [0.1.0] - 2026-11-23

First tagged version.

### Added

- Native desktop app on GPUI for MySQL, MariaDB, Oracle (Thin and Thick drivers) and
  Redis, with one window per project.
- Projects as folders: connections in `.deadlock/project.toml`, passwords in the OS
  keychain, SSH tunnels, TLS modes, environments and read-only connections.
- Schema browser, SQL editor with inline results, console, transactions and safety
  prompts, table data and structure editors.
- Explain plans, sessions and locks, server configuration, schema diagram.
- Redis key browser and editors for all key types.
- CSV import, export to CSV, TSV, JSON and SQL, query history, settings with rebindable
  shortcuts.
- Linux packages (`.deb`, AppImage) and a macOS `.dmg`, attached to the release by CI.

[Unreleased]: https://github.com/lamka02sk/deadlock/compare/v0.1.5...HEAD
[0.1.5]: https://github.com/lamka02sk/deadlock/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/lamka02sk/deadlock/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/lamka02sk/deadlock/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/lamka02sk/deadlock/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/lamka02sk/deadlock/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/lamka02sk/deadlock/releases/tag/v0.1.0
