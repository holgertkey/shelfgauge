# shelfgauge v0.1.0+001

A single-binary tool that indexes a folder tree of ebooks — FB2 files, plain or inside
`.zip` archives — into a local SQLite database and serves a browsable, searchable web
library on `localhost`, with an in-browser reader.

**Current version:** 0.1.0+001

## Status

Early development. The project foundation is in place; library scanning, the web UI and
the reader are not implemented yet. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for
the target design.

## Building

```sh
cargo build --release
```

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for the version history.

## License

MIT — see [LICENSE](LICENSE).

---

_shelfgauge v0.1.0+001 — MIT License_
