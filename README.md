# shelfgauge v0.1.0+002

A single-binary tool that indexes a folder tree of ebooks — FB2 files, plain or inside
`.zip` archives — into a local SQLite database and serves a browsable, searchable web
library on `localhost`, with an in-browser reader.

**Current version:** 0.1.0+002

## Status

Early development. The project foundation is in place; library scanning, the web UI and
the reader are not implemented yet. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for
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
`RUST_LOG` is used). On first run, `.shelfgauge/config.toml` is created inside the library
with commented defaults; command-line options override it.

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for the version history.

## License

MIT — see [LICENSE](LICENSE).

---

_shelfgauge v0.1.0+002 — MIT License_
