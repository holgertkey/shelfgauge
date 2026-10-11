//! On-disk caches in `.shelfgauge/`: cover images and parsed book content.

use std::io::Write;
use std::path::Path;

pub mod content;
pub mod covers;

/// Writes `data` to `path` atomically: a temporary file in the same directory is
/// renamed over the target, so readers never see a partial file.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(data)?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
