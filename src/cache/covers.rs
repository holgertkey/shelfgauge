//! Cover images, cached as `.shelfgauge/covers/<book id>.<ext>` during scans.

use std::path::{Path, PathBuf};

use super::write_atomic;
use crate::format::model::Image;

/// File extensions used for covers, by MIME type; anything else is stored as `.bin`.
const EXTENSIONS: &[(&str, &str)] = &[
    ("image/jpeg", "jpg"),
    ("image/png", "png"),
    ("image/gif", "gif"),
    ("image/webp", "webp"),
    ("image/bmp", "bmp"),
];

/// The file extension for a cover of MIME type `mime`.
pub fn extension(mime: &str) -> &'static str {
    EXTENSIONS
        .iter()
        .find(|(m, _)| *m == mime)
        .map_or("bin", |(_, ext)| ext)
}

/// The canonical MIME type for a cover of type `mime`, if it is a known image type.
///
/// The stored type comes from the book file, so it is matched against this allowlist
/// instead of being sent back as is.
pub fn known_mime(mime: &str) -> Option<&'static str> {
    EXTENSIONS
        .iter()
        .find(|(m, _)| m.eq_ignore_ascii_case(mime.trim()))
        .map(|(m, _)| *m)
}

/// The path of book `id`'s cover of MIME type `mime` in `dir`.
pub fn path(dir: &Path, id: i64, mime: &str) -> PathBuf {
    dir.join(format!("{id}.{}", extension(mime)))
}

/// Stores book `id`'s cover, replacing a cover of any other type.
pub fn store(dir: &Path, id: i64, image: &Image) -> std::io::Result<()> {
    let target = path(dir, id, &image.mime);
    write_atomic(&target, &image.data)?;
    remove_except(dir, id, Some(&target))
}

/// Removes book `id`'s cover, if any.
pub fn remove(dir: &Path, id: i64) -> std::io::Result<()> {
    remove_except(dir, id, None)
}

fn remove_except(dir: &Path, id: i64, keep: Option<&Path>) -> std::io::Result<()> {
    for ext in EXTENSIONS.iter().map(|(_, ext)| *ext).chain(["bin"]) {
        let candidate = dir.join(format!("{id}.{ext}"));
        if Some(candidate.as_path()) == keep {
            continue;
        }
        match std::fs::remove_file(&candidate) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(mime: &str, data: &[u8]) -> Image {
        Image {
            mime: mime.to_owned(),
            data: data.to_vec(),
        }
    }

    #[test]
    fn known_mime_is_an_allowlist() {
        assert_eq!(known_mime("image/jpeg"), Some("image/jpeg"));
        assert_eq!(known_mime(" IMAGE/PNG "), Some("image/png"));
        assert_eq!(known_mime("text/html"), None);
        assert_eq!(known_mime("image/svg+xml"), None);
    }

    #[test]
    fn store_replace_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let covers = dir.path().join("covers");
        store(&covers, 7, &image("image/jpeg", b"jpg")).unwrap();
        assert_eq!(std::fs::read(covers.join("7.jpg")).unwrap(), b"jpg");

        store(&covers, 7, &image("image/png", b"png")).unwrap();
        assert!(!covers.join("7.jpg").exists(), "old type removed");
        assert_eq!(std::fs::read(covers.join("7.png")).unwrap(), b"png");

        store(&covers, 8, &image("image/x-unknown", b"?")).unwrap();
        assert!(covers.join("8.bin").exists());

        remove(&covers, 7).unwrap();
        assert!(!covers.join("7.png").exists());
        assert!(covers.join("8.bin").exists());
        remove(&covers, 99).unwrap();
    }
}
