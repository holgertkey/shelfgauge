//! Reading mode: loading a book's content, splitting it into parts and rendering a part
//! as HTML.
//!
//! Everything here works on [`BookContent`](crate::format::model::BookContent) only,
//! never on a format's source, so other formats and the V2 reader can reuse it.

pub mod html;
pub mod load;
pub mod split;
