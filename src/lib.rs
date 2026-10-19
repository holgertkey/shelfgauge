//! shelfgauge engine: indexes a folder of ebooks and serves it as a local web library.
//!
//! The binary (`src/main.rs`) is a thin wrapper around this library; see
//! `docs/ARCHITECTURE.md` for the overall design.

#![warn(missing_docs)]

pub mod annotations;
pub mod cache;
pub mod config;
pub mod container;
pub mod error;
pub mod format;
pub mod library;
pub mod logging;
pub mod profiles;
pub mod render;
pub mod scanner;
pub mod storage;
pub mod web;

pub use error::{Error, Result};

/// The program version: `MAJOR.MINOR.PATCH`, plus a non-zero `+BUILD` in debug builds.
pub const VERSION: &str = env!("SHELFGAUGE_VERSION");
