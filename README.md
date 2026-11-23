# Deadlock

A native database IDE for MySQL, MariaDB, Oracle and Redis, written in Rust with
[GPUI](https://www.gpui.rs) and gpui-component. Query editor with inline results, table
data editor, explain plans, sessions and locks, server configuration, schema diagrams,
export to CSV / TSV / JSON / SQL, and SSH tunnels. A project is a folder; its
connections live in `.deadlock/project.toml`, passwords in the system keychain.

`PLAN.md` is the build plan and records what each phase delivered.

## Build and run

Needs a stable Rust toolchain and, on Linux, the GPUI system libraries
(Debian / Ubuntu: `clang libfontconfig-dev libwayland-dev libxkbcommon-x11-dev
libx11-xcb-dev libssl-dev libzstd-dev libasound2-dev libvulkan1`).

```sh
cargo run -p deadlock              # open-project window
cargo run -p deadlock -- <folder>  # open a project folder (or a .sql file in its folder)
```

## Test

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The engine tests need the databases and the SSH server in `docker-compose.yml`:

```sh
docker compose up -d --wait
cargo test -p deadlock-core --test engines -- --ignored --skip big_schema
docker compose down
```

## Package (Linux)

```sh
packaging/linux/package.sh
```

Builds the release binary and writes `target/package/deadlock_<version>_amd64.deb`
and `target/package/Deadlock-<version>-x86_64.AppImage`. Needs `dpkg-deb` and
`dpkg-shlibdeps` (package `dpkg-dev`) and `curl`; it downloads `appimagetool` into
`target/package-tools/` once. No root needed. CI builds both on every tag push.

At run time Deadlock needs glibc, libxcb, libxkbcommon(-x11), a Vulkan driver
(`libvulkan1` plus Mesa or a vendor driver), and fontconfig / freetype; on Wayland
also libwayland-client. The `.deb` declares these; the AppImage takes them from the
host. Packages are built on the build machine's glibc, so they run on systems with
that glibc or newer.

macOS and Windows packages are not built yet.
