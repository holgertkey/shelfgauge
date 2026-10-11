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
  and a missing `CHANGELOG.md` section header is added; a `+000` build suffix is not
  displayed.
- `README.md` and `CHANGELOG.md`.
- GitHub Actions CI (fmt, clippy, test, doc) on Linux, Windows and macOS.
