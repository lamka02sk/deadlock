# Contributing to Deadlock

Thanks for helping. Bug reports, small fixes and focused features are all welcome. For a
large change, open an issue first so we can agree on the approach.

## Build and run

See [Build from source](README.md#build-from-source) for the system libraries. Then:

```sh
cargo run -p deadlock                # debug build
cargo run -p deadlock -- <folder>    # open a project folder
```

## Before you open a pull request

CI runs these on every push; run them locally first:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`unsafe` code is forbidden in the workspace (`unsafe_code = "forbid"`), and `dbg!` and
`todo!` raise warnings, which CI treats as errors.

### Engine tests

Tests that need a real database are `#[ignore]`d. If you touch a driver or SQL
generation, run them against the containers in `docker-compose.yml`:

```sh
docker compose up -d --wait
cargo test -p deadlock-core --test engines -- --ignored --skip big_schema
cargo test -p deadlock-core --test redis -- --ignored --test-threads 1
docker compose down
```

The Oracle tests use the Thin driver. Run them on the Thick driver (needs Oracle Instant
Client) with `DEADLOCK_TEST_ORACLE=thick`. Say in the pull request which engines you
tested; CI does not run the ignored tests.

## Code layout

- `crates/deadlock-core`: everything that has no UI (drivers, project files, search,
  completion, import/export). Keep GPUI types out of it.
- `crates/deadlock`: the GPUI application.
- `vendor/`: patched copies of upstream crates. Do not edit them casually. A change
  there must be recorded in `vendor/gpui-base/PATCHES.md` and marked `Deadlock patch`.

Keep the UI thread free of blocking I/O; talk to `deadlock-core` over channels.

## Commits and pull requests

- One logical change per commit. The subject is a short imperative sentence without a
  trailing period, for example `Add macOS package (.dmg) and macOS CI check` or
  `CI: install desktop-file-utils for the AppImage`. Use a prefix such as `CI:` when it
  helps. Explain the why in the body when it is not obvious.
- Fill in the pull request template: what changed, how you tested it, which engines.
- Add an entry to the `Unreleased` section of [CHANGELOG.md](CHANGELOG.md) for
  user-visible changes.
- Screenshots help for UI changes.

## Reporting bugs

Use the [bug report form](https://github.com/lamka02sk/deadlock/issues/new?template=bug_report.yml).
Include the Deadlock version, OS and display server (X11 or Wayland), database engine
and version, and steps to reproduce. Remove passwords, hostnames and data you do not want
public from logs and screenshots.

Security problems do not belong in public issues; see [SECURITY.md](SECURITY.md).

## Conduct

By taking part you agree to the [Code of Conduct](CODE_OF_CONDUCT.md).

## License

Deadlock is licensed under the [MIT license](LICENSE). By contributing, you agree that your
contribution is licensed under the same terms.
