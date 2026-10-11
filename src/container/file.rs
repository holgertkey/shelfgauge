//! Plain book files.

use std::io::Read;
use std::path::Path;

use super::{ContainerError, ReadLimits};

/// Reads a plain book file, refusing files larger than the limit.
pub fn read(path: &Path, limits: &ReadLimits) -> Result<Vec<u8>, ContainerError> {
    let io = |error| ContainerError::Io {
        path: path.to_owned(),
        error,
    };
    let file = std::fs::File::open(path).map_err(io)?;
    let size = file.metadata().map_err(io)?.len();
    let too_large = |size| ContainerError::TooLarge {
        path: path.to_owned(),
        size,
        limit: limits.max_book_size,
    };
    if size > limits.max_book_size {
        return Err(too_large(size));
    }
    // The file may grow between the size check and reading: read at most limit + 1.
    let mut data = Vec::with_capacity(size as usize);
    file.take(limits.max_book_size + 1)
        .read_to_end(&mut data)
        .map_err(io)?;
    if data.len() as u64 > limits.max_book_size {
        return Err(too_large(data.len() as u64));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.fb2");
        std::fs::write(&path, b"content").unwrap();
        assert_eq!(read(&path, &ReadLimits::default()).unwrap(), b"content");
    }

    #[test]
    fn rejects_files_over_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.fb2");
        std::fs::write(&path, vec![b'x'; 11]).unwrap();
        let limits = ReadLimits { max_book_size: 10 };
        let err = read(&path, &limits).unwrap_err();
        assert!(
            matches!(
                err,
                ContainerError::TooLarge {
                    size: 11,
                    limit: 10,
                    ..
                }
            ),
            "{err}"
        );
        std::fs::write(&path, vec![b'x'; 10]).unwrap();
        assert_eq!(read(&path, &limits).unwrap().len(), 10);
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let err = read(Path::new("/nonexistent/book.fb2"), &ReadLimits::default()).unwrap_err();
        assert!(err.needs_rescan());
    }
}
