# shelfgauge-core

The shared core of [shelfgauge](https://github.com/holgertkey/shelfgauge): the
format-independent book model, the split of a book into reading parts, HTML rendering
of a part, the rendered text that annotations are anchored in, and search within a
book.

It does no IO and parses no book files (the parsers are in the `shelfgauge` crate),
and it builds for `wasm32-unknown-unknown`, so a book renders the same on a reading
device as on the server. Its API follows the needs of `shelfgauge` and is not meant
to be stable on its own; `shelfgauge` re-exports these modules at the same paths.

License: MIT.
