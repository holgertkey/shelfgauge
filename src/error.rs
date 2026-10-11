//! Error types of the shelfgauge engine.

use std::path::PathBuf;

/// Errors returned by the shelfgauge engine.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The library root directory does not exist.
    #[error("library directory {} does not exist", .0.display())]
    LibraryNotFound(PathBuf),

    /// The library root path exists but is not a directory.
    #[error("library path {} is not a directory", .0.display())]
    NotADirectory(PathBuf),

    /// An I/O operation on `path` failed.
    #[error("{}: {error}", path.display())]
    Io {
        /// The file or directory the operation was performed on.
        path: PathBuf,
        /// The underlying I/O error (part of the message, not a separate `source()`).
        error: std::io::Error,
    },

    /// The config file is not valid TOML or does not match the expected structure.
    #[error("invalid config file {}: {message}", path.display())]
    ConfigSyntax {
        /// Path of the config file.
        path: PathBuf,
        /// Parser error message, including the location in the file.
        message: String,
    },

    /// A value in the config file is out of range or otherwise invalid.
    #[error("invalid config file {}: {error}", path.display())]
    InvalidConfig {
        /// Path of the config file.
        path: PathBuf,
        /// What is wrong with which key (part of the message, not a separate `source()`).
        error: crate::config::ConfigError,
    },

    /// A value given on the command line is invalid.
    #[error("invalid command-line option: {0}")]
    InvalidOption(crate::config::ConfigError),
}

impl Error {
    /// Creates an [`Error::Io`] for an operation on `path`.
    pub fn io(path: impl Into<PathBuf>, error: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            error,
        }
    }
}

/// A `Result` with the engine's [`Error`] type.
pub type Result<T, E = Error> = std::result::Result<T, E>;
