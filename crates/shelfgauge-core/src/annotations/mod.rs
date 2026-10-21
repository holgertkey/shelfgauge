//! The rendered text of a book, anchors in it and search within it.
//!
//! - [`text`]: the rendered text of a book, which offsets count into;
//! - [`find`]: search within a book's text;
//! - [`anchor`]: anchors, quotes and the fingerprint search.

pub mod anchor;
pub mod find;
pub mod text;
