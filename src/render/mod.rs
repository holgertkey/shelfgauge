//! Reading mode: loading a book's content, splitting it into parts and rendering a part
//! as HTML.
//!
//! Everything here works on [`BookContent`](crate::format::model::BookContent) only,
//! never on a format's source, so other formats and the V2 reader can reuse it.
//! [`split`], [`html`] and [`outline`] live in the core crate (`shelfgauge-core`), which also builds
//! for the reading device; [`load`] (files, caches, storage) stays here.

pub mod load;

pub use shelfgauge_core::render::{html, outline, split};
