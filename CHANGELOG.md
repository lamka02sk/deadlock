# Changelog

All notable changes are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to follow
[Semantic Versioning](https://semver.org/) once it reaches 1.0.

## [Unreleased]

## [0.1.1] - 2026-11-24

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

[Unreleased]: https://github.com/lamka02sk/deadlock/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/lamka02sk/deadlock/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/lamka02sk/deadlock/releases/tag/v0.1.0
