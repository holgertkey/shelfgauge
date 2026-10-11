//! shelfgauge engine: indexes a folder of ebooks and serves it as a local web library.
//!
//! The binary (`src/main.rs`) is a thin wrapper around this library; see
//! `docs/ARCHITECTURE.md` for the overall design.

#![warn(missing_docs)]

pub mod error;

pub use error::{Error, Result};

/// The program version as shown to users (a `+000` build suffix is omitted).
pub const VERSION: &str = env!("SHELFGAUGE_VERSION");
