# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Build numbers (`+NNN`) do not get their own sections.

## [Unreleased]

### Added

- Format-independent book model (`format::model`): `BookMetadata` (title, authors,
  series list with a verbatim index, genres, annotation, language, cover) and
  `BookContent` (bodies → section tree → blocks, images by id), with inline markup and
  links. Every block gets a deterministic `BlockId` (`b<n>`) in document order; the
  numbering rule is documented and pinned by a golden test. `PARSER_VERSION` = 1.
- `BookParser` trait and an extension → parser `Registry` (case-insensitive).
- FB2 encoding detection (`format::fb2::encoding`): BOM → UTF-16 pattern → XML
  declaration → valid UTF-8 → `chardetng` guess. Lying declarations are handled
  (declared UTF-8 but invalid → guessed; declared legacy but valid non-ASCII UTF-8 →
  UTF-8); mostly-valid UTF-8 with a few stray bytes stays UTF-8 (with replacement
  characters) instead of being misread as a legacy encoding; malformed input is reported
  instead of failing.
- FB2 parser (`format::fb2::Fb2Parser`, registered by `Registry::with_builtin_formats`):
  `<title-info>` metadata (title, authors, nested sequences, genres, annotation, language,
  cover) and full content (all bodies and FB2 block/inline elements, binaries with
  lenient base64 and MIME sniffing). Elements are matched by local name; unknown
  elements keep their text; trailing content after subsections is wrapped into an
  untitled section; a `<body>` image before the title is allowed. Never panics: non-XML/non-FB2 input, DTDs and nesting deeper than
  256 levels are errors; nesting beyond 64 levels is flattened to text.
- `format::PARSER_STACK_SIZE` (16 MB) and `run_with_parser_stack`: threads running
  parsers need this much stack.
- Container layer (`container`): `BookLocation` (path relative to the library root +
  optional zip entry identified by raw name bytes, index, size, CRC32) and `read_book`.
  Zip archives are listed from the central directory without decompression; entry
  display names are decoded as UTF-8, CP866, windows-1251 or CP437; directories,
  encrypted entries and nested archives are skipped. Reads check the recorded size and
  CRC32 (changed/missing entries report `needs_rescan`), stop at the declared size and
  refuse books over `ReadLimits::max_book_size` (256 MiB by default). Supported
  compression: stored, deflate, deflate64, bzip2, LZMA (all pure Rust).

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
