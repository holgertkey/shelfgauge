//! Error types of the shelfgauge engine.

/// Errors returned by the shelfgauge engine.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An I/O operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// A `Result` with the engine's [`Error`] type.
pub type Result<T, E = Error> = std::result::Result<T, E>;
