//! shelfgauge core: the book model, the reader split, HTML rendering and search within
//! a book.
//!
//! This is the part of shelfgauge that runs both on the server and on the reading
//! device: it builds for `wasm32-unknown-unknown`, so a book renders the same in the
//! hub's reader as on the instance. It never parses a book file and does no IO; the
//! parsers, storage and the web server are in the `shelfgauge` crate, which re-exports
//! these modules at the same paths (`shelfgauge::format::model`, ...).

#![warn(missing_docs)]

pub mod annotations;
pub mod config;
pub mod format;
pub mod publication;
pub mod render;
