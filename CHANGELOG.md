# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Build numbers (`+NNN`) do not get their own sections.

## [Unreleased]

## [0.1.0] - 2026-10-11

### Added

- Library + binary crate layout (`src/lib.rs` engine with `#![warn(missing_docs)]`,
  thin `src/main.rs`).
- Base dependencies: `anyhow`, `thiserror`, `tracing`, `tracing-subscriber`, `clap`.
- Minimal CLI with `--version` and `--help`.
- `build.rs` version sync: the version from `Cargo.toml` is written into `README.md`
  and a missing `CHANGELOG.md` section header is added. The `+BUILD` counter is a local
  development aid: only debug builds show it in `--version`.
- `README.md` and `CHANGELOG.md`.
- GitHub Actions CI (fmt, clippy, test, doc) on Linux, Windows and macOS.
- Library root: positional `PATH` argument (default: the current directory); working
  data lives in `.shelfgauge/` inside it.
- Config file `.shelfgauge/config.toml` with `[server]`, `[scan]`, `[library]` and
  `[reader]` sections, created with commented defaults on first run. Missing keys use
  defaults, unknown keys are logged as warnings, invalid values fail with an error
  naming the key; `server.base_path` is normalized.
- CLI: `shelfgauge [PATH]` (serve), `shelfgauge scan [PATH]`, `--port`, `--bind`,
  `--no-scan` and `-v`/`-vv`; CLI values override the config file. Serve and scan are
  stubs that print the effective configuration.
- Logging to stderr via `tracing`; level from `-v` flags, else `RUST_LOG`.
