//! shelfgauge core: the book model, the reader split, HTML rendering and search within
//! a book, and what the hub's reader app shares with the library besides: account
//! addresses, the keys derived from a hub password, and a library's catalog with its
//! search.
//!
//! This is the part of shelfgauge that runs both on the server and on the reading
//! device: it builds for `wasm32-unknown-unknown`, so a book renders the same in the
//! hub's reader as on the instance. It never parses a book file and does no IO; the
//! parsers, storage and the web server are in the `shelfgauge` crate, which re-exports
//! these modules at the same paths (`shelfgauge::format::model`, ...).

#![warn(missing_docs)]

pub mod address;
pub mod annotations;
pub mod catalog;
pub mod config;
pub mod fold;
pub mod format;
pub mod password;
pub mod publication;
pub mod render;
