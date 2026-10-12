# shelfgauge v0.2.0

A single-binary tool that indexes a folder tree of ebooks — FB2 files, plain or inside
`.zip` archives — into a local SQLite database and serves a browsable, searchable web
library on `localhost`, with an in-browser reader.

**Current version:** 0.2.0

## Status

Early development. `shelfgauge scan` indexes a library of FB2 books (plain files and
`.zip` archives) into `.shelfgauge/library.db`; `shelfgauge [PATH]` starts the web
server (layout skeleton only) and scans in the background. The library browser and the
reader are not implemented yet. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for
the target design.

## Building

```sh
cargo build --release
```

## Usage

```sh
shelfgauge [PATH]        # serve the library at PATH (default: current directory)
shelfgauge scan [PATH]   # update the library index and exit
```

Options: `--port <PORT>`, `--bind <ADDR>`, `--no-scan`, `-v`/`-vv` (more logging; otherwise
`RUST_LOG` is used), `-V` (version).

A new library is created only when PATH is given explicitly (e.g. `shelfgauge scan .`):
its working data goes into `.shelfgauge/` with a commented `config.toml`. Without PATH,
the current directory must already be a library. Command-line options override the
config.

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for the version history.

## License

MIT — see [LICENSE](LICENSE).

---

_shelfgauge v0.2.0 — MIT License_
