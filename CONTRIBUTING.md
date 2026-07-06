# Contributing to bruh

## Build from source

Requires Rust 1.70+ and cargo.

```bash
git clone https://github.com/oboobotenefiok/bruh
cd bruh
cargo build          # debug
cargo build --release  # optimised
```

## Running tests

```bash
cargo test           # all tests
cargo test shell     # shell parser tests only
cargo test packages  # package manager tests only
```

## Code style

```bash
cargo fmt --all        # format
cargo clippy -- -D warnings  # lint
```

## Design decisions

You'll see me plant comments throughout the codebase. These are some stuff you'll have to pay attention to. Currently, I'm writing this based on the roadmap we've built. Hope this does.

| Decision | Rationale |
|---|---|
| **NDJSON buffer** instead of SQLite | No native compilation; human-readable; append-friendly; serde_json already in tree |
| **rustls-tls** instead of OpenSSL | Zero system library deps; critical for Termux (Android) |
| **Polling** instead of inotify/kqueue | Portable across Linux, macOS, Windows, Termux |
| **Manual CLI parsing** instead of clap | Keeps compile times low on constrained devices |
| **No buffer compression** | Buffer won't hit 10 MB in normal use; flate2 adds compile weight |

## Adding a package manager

1. Add a `poll_<name>()` async fn in `src/daemon/packages.rs`
2. Call it inside `poll_package_managers()` under the right platform gate (`#[cfg(windows)]` etc.)
3. Add a state file under `Config::data_dir()` using the existing cursor/diff pattern
4. Add tests in the `tests` module at the bottom of the file

## Platform gates

Use `#[cfg(windows)]` / `#[cfg(not(windows))]` / `#[cfg(unix)]` for platform-specific code.
Paths go through `crate::cli::config::{home_dir, data_dir, config_dir}` — never hardcode `~` or `%APPDATA%` directly.

## Submitting a PR

- One logical change per PR
- All tests must pass: `cargo test`
- No clippy warnings: `cargo clippy -- -D warnings`
- Code must be formatted: `cargo fmt --all --check`
