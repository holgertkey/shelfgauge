//! Walking the library directory tree.
//!
//! Symbolic links are not followed (they are skipped), `.shelfgauge/` and paths matching
//! `[scan].exclude` (gitignore syntax, relative to the library root) are skipped, and
//! entries are visited in file-name order so scans are deterministic.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use walkdir::WalkDir;

use super::ScanError;
use crate::container::ContainerKind;
use crate::format::Registry;
use crate::library::DATA_DIR_NAME;

/// A book file or archive found by the walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Path relative to the library root, `/`-separated.
    pub path: String,
    /// Absolute path.
    pub abs_path: PathBuf,
    /// Plain file or archive.
    pub kind: ContainerKind,
    /// Size in bytes at the time of the walk.
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch at the time of the walk.
    pub mtime_ns: i64,
}

/// The result of a walk.
#[derive(Debug, Default)]
pub struct Walk {
    /// Book files and archives, in walk order.
    pub found: Vec<Found>,
    /// Relative paths of directories that could not be read completely; books below
    /// them must not be considered removed.
    pub unreadable_dirs: Vec<String>,
    /// Whether the whole tree was walked (no unattributable errors, not cancelled).
    pub complete: bool,
    /// Files skipped because their path is not valid UTF-8.
    pub skipped_non_utf8: u64,
}

/// Compiles `[scan].exclude` patterns (gitignore syntax) relative to `root`.
pub fn excludes(root: &Path, patterns: &[String]) -> Result<Gitignore, ScanError> {
    let mut builder = GitignoreBuilder::new(root);
    for pattern in patterns {
        builder
            .add_line(None, pattern)
            .map_err(|e| ScanError::InvalidExclude {
                pattern: pattern.clone(),
                message: e.to_string(),
            })?;
    }
    builder.build().map_err(|e| ScanError::InvalidExclude {
        pattern: patterns.join(", "),
        message: e.to_string(),
    })
}

/// Walks the library at `root`, collecting plain files the `registry` can parse and
/// `.zip` archives.
pub fn walk(root: &Path, excludes: &Gitignore, registry: &Registry, cancel: &AtomicBool) -> Walk {
    let mut walk = Walk {
        complete: true,
        ..Walk::default()
    };
    let entries = WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            let Ok(relative) = entry.path().strip_prefix(root) else {
                return true;
            };
            if relative.as_os_str().is_empty() {
                return true;
            }
            if relative == Path::new(DATA_DIR_NAME) {
                return false;
            }
            !excludes
                .matched(relative, entry.file_type().is_dir())
                .is_ignore()
        });
    for entry in entries {
        if cancel.load(Ordering::Relaxed) {
            walk.complete = false;
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                match error.path().and_then(|p| relative_path(root, p)) {
                    Some(dir) => {
                        tracing::warn!("cannot read {dir:?}: {error}");
                        walk.unreadable_dirs.push(dir);
                    }
                    None => {
                        tracing::warn!("error while walking the library: {error}");
                        walk.complete = false;
                    }
                }
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let kind = ContainerKind::of(entry.path());
        if kind == ContainerKind::File && registry.for_path(entry.path()).is_none() {
            continue;
        }
        let Some(path) = relative_path(root, entry.path()) else {
            tracing::warn!(
                "skipping {}: the path is not valid UTF-8",
                entry.path().display()
            );
            walk.skipped_non_utf8 += 1;
            continue;
        };
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                tracing::warn!("cannot read metadata of {path:?}: {error}");
                // Treat it like an unreadable directory: keep its books.
                walk.unreadable_dirs.push(path);
                continue;
            }
        };
        walk.found.push(Found {
            path,
            abs_path: entry.path().to_owned(),
            kind,
            size: metadata.len(),
            mtime_ns: metadata.modified().map(mtime_ns).unwrap_or(0),
        });
    }
    walk
}

/// Nanoseconds since the Unix epoch (negative before it, saturating).
pub fn mtime_ns(time: std::time::SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_nanos()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_nanos()).map_or(i64::MIN + 1, |n| -n),
    }
}

/// `path` relative to `root`, `/`-separated; `None` if it is not under `root` or not
/// valid UTF-8.
fn relative_path(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in relative.components() {
        parts.push(component.as_os_str().to_str()?);
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(root: &Path, path: &str) {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, b"x").unwrap();
    }

    fn paths(walk: &Walk) -> Vec<&str> {
        walk.found.iter().map(|f| f.path.as_str()).collect()
    }

    #[test]
    fn finds_books_and_archives_in_name_order() {
        let dir = tempfile::tempdir().unwrap();
        for path in [
            "b.fb2",
            "a/z.FB2",
            "a/y.zip",
            "a/notes.md",
            ".shelfgauge/hidden.fb2",
            "skip/x.fb2",
            "keep/tmp.fb2",
            "keep/book.fb2",
        ] {
            touch(dir.path(), path);
        }
        let excludes = excludes(dir.path(), &["skip/".to_owned(), "tmp.*".to_owned()]).unwrap();
        let walk = walk(
            dir.path(),
            &excludes,
            &Registry::with_builtin_formats(),
            &AtomicBool::new(false),
        );
        assert!(walk.complete);
        assert_eq!(
            paths(&walk),
            ["a/y.zip", "a/z.FB2", "b.fb2", "keep/book.fb2"]
        );
        assert_eq!(walk.found[0].kind, ContainerKind::Zip);
        assert_eq!(walk.found[0].size, 1);
        assert!(walk.found[0].mtime_ns > 0);
    }

    #[test]
    fn cancelled_walk_is_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.fb2");
        let walk = walk(
            dir.path(),
            &excludes(dir.path(), &[]).unwrap(),
            &Registry::with_builtin_formats(),
            &AtomicBool::new(true),
        );
        assert!(!walk.complete);
    }

    #[test]
    fn invalid_exclude_names_the_pattern() {
        let err = excludes(Path::new("/lib"), &["a/{b".to_owned()]).unwrap_err();
        assert!(err.to_string().contains("a/{b"), "{err}");
    }

    #[test]
    fn mtime_conversion() {
        use std::time::Duration;
        // Multiples of 100 ns: Windows `SystemTime` has 100 ns resolution.
        assert_eq!(mtime_ns(UNIX_EPOCH + Duration::from_nanos(1_500)), 1_500);
        assert_eq!(mtime_ns(UNIX_EPOCH - Duration::from_nanos(1_500)), -1_500);
        assert_eq!(mtime_ns(UNIX_EPOCH), 0);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_and_symlinks_are_skipped() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "real.fb2");
        let bad = std::ffi::OsStr::from_bytes(b"bad\xff.fb2");
        // macOS file systems refuse non-UTF-8 names (EILSEQ); elsewhere this must work.
        let created = std::fs::write(dir.path().join(bad), b"x").is_ok();
        assert!(created || cfg!(target_os = "macos"));
        std::os::unix::fs::symlink(dir.path().join("real.fb2"), dir.path().join("link.fb2"))
            .unwrap();
        let walk = walk(
            dir.path(),
            &excludes(dir.path(), &[]).unwrap(),
            &Registry::with_builtin_formats(),
            &AtomicBool::new(false),
        );
        assert_eq!(paths(&walk), ["real.fb2"]);
        assert_eq!(walk.skipped_non_utf8, u64::from(created));
    }
}
