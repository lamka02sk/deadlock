# Changelog

All notable changes are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to follow
[Semantic Versioning](https://semver.org/) once it reaches 1.0.

## [Unreleased]

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
