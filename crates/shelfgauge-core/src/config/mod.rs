//! The reader settings shared by the server and the reading device: the `[reader]`
//! section of a library's `config.toml`.

use serde::{Deserialize, Serialize};

/// The `[reader]` section: how books are split into parts in reading mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReaderConfig {
    /// The split mode.
    pub split: SplitMode,
    /// For [`SplitMode::Section`]: split at sections of this nesting depth (1 = top level).
    pub section_depth: u32,
    /// For [`SplitMode::Size`]: approximate number of characters per part.
    pub part_size: u32,
    /// The longest part in characters, in every mode; longer parts are cut into equal
    /// pieces at block boundaries. 0 means no limit.
    pub max_part_size: u32,
    /// Whether the reading position of each book is saved and offered on the book card.
    pub remember_position: bool,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        Self {
            split: SplitMode::Section,
            section_depth: 1,
            part_size: 50_000,
            max_part_size: 300_000,
            remember_position: true,
        }
    }
}

/// How a book is split into parts for reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SplitMode {
    /// The whole book is one part.
    None,
    /// One part per section at [`ReaderConfig::section_depth`].
    Section,
    /// Parts of about [`ReaderConfig::part_size`] characters, cut at block boundaries.
    Size,
}
