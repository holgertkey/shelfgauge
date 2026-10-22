//! Reading mode: splitting a book into parts and rendering a part as HTML.
//!
//! Everything here works on [`BookContent`](crate::format::model::BookContent) only,
//! never on a format's source.

pub mod html;
pub mod outline;
pub mod split;
