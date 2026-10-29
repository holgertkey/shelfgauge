//! The `.sgbook` container: a publication in one file (concept §11 "Book files").
//!
//! A zip archive without compression, in a fixed layout: the entry `mimetype` first
//! (content [`MIME`], so the type sits at byte 38 as EPUB's does), then `sg.json`, then
//! the blobs at their hub paths (`manifest`, `p/<n>`, `i/<n>`), already compressed,
//! padded and encrypted. No extra fields, no data descriptors, no zip64, no comments.
//!
//! The file is untrusted (anyone can hand one over), so this reader is strict: it
//! accepts only that layout and needs only the end of the file and the central
//! directory. The entries are found from the central directory alone: each local
//! header is `30` bytes plus the name, and the entries follow each other without
//! gaps, so a reader cuts entries out of a large file by offset without holding it in
//! memory (the browser slices a `File`). The blobs authenticate themselves when they
//! are opened; `mimetype` is compared and `sg.json` is checked by its reader.
//!
//! # Examples
//!
//! ```
//! use shelfgauge_core::publication::container::{self, Name};
//!
//! // The smallest file: `mimetype` and `sg.json` (a real one has blobs too).
//! let file = container::tests_support::write(&[
//!     ("mimetype", container::MIME.as_bytes()),
//!     ("sg.json", b"{}"),
//! ]);
//! let entries = container::read(&file).unwrap();
//! assert_eq!(entries[0].0, Name::Mimetype);
//! assert_eq!(entries[1], (Name::SgJson, &b"{}"[..]));
//! assert_eq!(&file[38..38 + container::MIME.len()], container::MIME.as_bytes());
//! ```

use super::BlobPath;

/// The media type of a book file, the content of its `mimetype` entry.
pub const MIME: &str = "application/vnd.shelfgauge.book";

/// The file extension books are written with.
pub const EXTENSION: &str = "sgbook";

/// The extensions a reader accepts when opening (a shortened name opens too).
pub const OPEN_EXTENSIONS: &[&str] = &["sgbook", "sgb"];

/// The version of the container (`sg.json` and this layout). Version 2 (shelfgauge
/// 0.21) names the publishing library and the generation of its key in `sg.json`
/// instead of an account; a reader tells a version 1 file ("made by an older
/// shelfgauge") from a newer one.
pub const CONTAINER_VERSION: u16 = 2;

/// The container version of the book files of shelfgauge 0.19–0.20, which do not
/// open any more (their book keys were sealed under accounts that no longer exist).
pub const OLD_CONTAINER_VERSION: u16 = 1;

/// The name of the entry with the file's keys and metadata.
pub const SG_JSON: &str = "sg.json";

/// The length of the end of central directory record (no comment).
pub const END_LEN: usize = 22;

/// The length of a local file header before its name.
pub const LOCAL_HEADER_LEN: u64 = 30;

/// The length of a central directory header before its name.
const CENTRAL_HEADER_LEN: usize = 46;

/// The most entries a file may have (parts and images of a very large book fit).
pub const MAX_ENTRIES: usize = 200_000;

/// The largest central directory accepted.
pub const MAX_DIRECTORY: u64 = 32 * 1024 * 1024;

/// The largest `sg.json` accepted (it holds a snapshot of the book's marks).
pub const MAX_SG_JSON: u64 = 64 * 1024 * 1024;

const END_SIGNATURE: u32 = 0x0605_4b50;
const CENTRAL_SIGNATURE: u32 = 0x0201_4b50;

/// The length of the start of a book file that [`check_head`] reads: the local header
/// of `mimetype` and its content.
pub const HEAD_LEN: usize = 38 + MIME.len();

/// Flags that change how an entry's data must be read: encryption, a data
/// descriptor, strong encryption, a masked directory.
const REFUSED_FLAGS: u16 = 0x0001 | 0x0008 | 0x0040 | 0x2000;

/// The name of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Name {
    /// `mimetype`.
    Mimetype,
    /// `sg.json`.
    SgJson,
    /// A blob at its hub path.
    Blob(BlobPath),
}

impl Name {
    /// The name of an entry, if the container allows it.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "mimetype" => Some(Self::Mimetype),
            SG_JSON => Some(Self::SgJson),
            _ => name.parse().ok().map(Self::Blob),
        }
    }
}

impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mimetype => f.write_str("mimetype"),
            Self::SgJson => f.write_str(SG_JSON),
            Self::Blob(path) => path.fmt(f),
        }
    }
}

/// Why a file is not a book file this reader opens.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContainerError {
    /// Not a zip file of the expected layout (or cut short).
    #[error("this is not a shelfgauge book file ({0})")]
    NotABookFile(&'static str),
    /// A zip file, but not in the layout this reader accepts.
    #[error("the book file is damaged or was changed ({0})")]
    Malformed(String),
}

fn malformed(what: impl Into<String>) -> ContainerError {
    ContainerError::Malformed(what.into())
}

fn u16_at(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().expect("four bytes"))
}

/// Where the central directory is, from the end of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Directory {
    /// Its offset in the file.
    pub offset: u64,
    /// Its length.
    pub size: u64,
    /// The number of entries.
    pub count: usize,
}

/// Reads the end of central directory record: `tail` is the last [`END_LEN`] bytes of
/// a file of `file_len` bytes.
pub fn end_of_directory(tail: &[u8], file_len: u64) -> Result<Directory, ContainerError> {
    if tail.len() != END_LEN || file_len < END_LEN as u64 {
        return Err(ContainerError::NotABookFile("too short"));
    }
    if u32_at(tail, 0) != END_SIGNATURE {
        return Err(ContainerError::NotABookFile("no zip directory at the end"));
    }
    let (disk, cd_disk) = (u16_at(tail, 4), u16_at(tail, 6));
    let (on_disk, total) = (u16_at(tail, 8), u16_at(tail, 10));
    let (size, offset) = (u32_at(tail, 12), u32_at(tail, 16));
    if disk != 0 || cd_disk != 0 || on_disk != total {
        return Err(malformed("a multi-part archive"));
    }
    if total == u16::MAX || size == u32::MAX || offset == u32::MAX {
        return Err(malformed("zip64"));
    }
    if u16_at(tail, 20) != 0 {
        return Err(malformed("an archive comment"));
    }
    let (offset, size) = (u64::from(offset), u64::from(size));
    if offset.checked_add(size) != Some(file_len - END_LEN as u64) {
        return Err(malformed("the directory is not where the end record says"));
    }
    if size > MAX_DIRECTORY {
        return Err(malformed("the directory is too large"));
    }
    if total == 0 {
        return Err(ContainerError::NotABookFile("an empty archive"));
    }
    Ok(Directory {
        offset,
        size,
        count: usize::from(total),
    })
}

/// An entry found in the central directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// Its name.
    pub name: Name,
    /// The offset of its data in the file.
    pub offset: u64,
    /// The length of its data.
    pub size: u64,
}

/// Reads the central directory `data` (the bytes at [`Directory::offset`]) and checks
/// the layout: `mimetype` first at offset 0, then `sg.json`, the entries stored, in
/// order and without gaps up to the directory; names unique and allowed.
pub fn entries(data: &[u8], dir: &Directory) -> Result<Vec<Entry>, ContainerError> {
    if data.len() as u64 != dir.size {
        return Err(malformed("the directory is cut short"));
    }
    if dir.count > MAX_ENTRIES {
        return Err(malformed("too many entries"));
    }
    let mut out = Vec::with_capacity(dir.count);
    let mut seen = std::collections::HashSet::with_capacity(dir.count);
    let mut at = 0usize;
    let mut next = 0u64;
    for _ in 0..dir.count {
        let header = data
            .get(at..at + CENTRAL_HEADER_LEN)
            .ok_or_else(|| malformed("the directory is cut short"))?;
        if u32_at(header, 0) != CENTRAL_SIGNATURE {
            return Err(malformed("a broken directory entry"));
        }
        let flags = u16_at(header, 8);
        let method = u16_at(header, 10);
        let (compressed, size) = (u32_at(header, 20), u32_at(header, 24));
        let name_len = usize::from(u16_at(header, 28));
        let extra_len = usize::from(u16_at(header, 30));
        let comment_len = usize::from(u16_at(header, 32));
        let disk = u16_at(header, 34);
        let local = u32_at(header, 42);
        if flags & REFUSED_FLAGS != 0 {
            return Err(malformed("an encrypted entry or a data descriptor"));
        }
        if method != 0 || compressed != size {
            return Err(malformed("a compressed entry"));
        }
        if extra_len != 0 || comment_len != 0 || disk != 0 {
            return Err(malformed("extra fields or comments"));
        }
        if size == u32::MAX || local == u32::MAX {
            return Err(malformed("zip64"));
        }
        let name = data
            .get(at + CENTRAL_HEADER_LEN..at + CENTRAL_HEADER_LEN + name_len)
            .ok_or_else(|| malformed("the directory is cut short"))?;
        let name = std::str::from_utf8(name)
            .ok()
            .and_then(Name::parse)
            .ok_or_else(|| malformed("an unknown entry"))?;
        if !seen.insert(name) {
            return Err(malformed(format!("{name} appears twice")));
        }
        match (out.len(), name) {
            (0, Name::Mimetype) | (1, Name::SgJson) => {}
            (0, _) => return Err(ContainerError::NotABookFile("no mimetype entry first")),
            (1, _) => return Err(malformed("no sg.json after mimetype")),
            (_, Name::Mimetype | Name::SgJson) => return Err(malformed("entries out of order")),
            _ => {}
        }
        let local = u64::from(local);
        if local != next {
            return Err(malformed("entries with gaps or out of order"));
        }
        let offset = local + LOCAL_HEADER_LEN + name_len as u64;
        let size = u64::from(size);
        if name == Name::SgJson && size > MAX_SG_JSON {
            return Err(malformed("sg.json is too large"));
        }
        next = offset + size;
        if next > dir.offset {
            return Err(malformed("an entry runs into the directory"));
        }
        out.push(Entry { name, offset, size });
        at += CENTRAL_HEADER_LEN + name_len;
    }
    if at != data.len() {
        return Err(malformed("bytes after the last directory entry"));
    }
    if next != dir.offset {
        return Err(malformed("bytes between the entries and the directory"));
    }
    if out.len() < 2 {
        return Err(malformed("no sg.json"));
    }
    Ok(out)
}

/// Checks the first [`HEAD_LEN`] bytes of a file: a zip whose first entry is
/// `mimetype`, stored, without an extra field, with the content [`MIME`]. Any other
/// file (an EPUB too) is refused here as not a book file, before its directory is read.
pub fn check_head(head: &[u8]) -> Result<(), ContainerError> {
    let ours = head.len() == HEAD_LEN
        && head[..4] == *b"PK\x03\x04"
        && u16_at(head, 8) == 0
        && u16_at(head, 26) == 8
        && u16_at(head, 28) == 0
        && &head[30..38] == b"mimetype";
    if !ours {
        return Err(ContainerError::NotABookFile("no mimetype entry first"));
    }
    check_mimetype(&head[38..])
}

/// Checks the data of the `mimetype` entry.
pub fn check_mimetype(data: &[u8]) -> Result<(), ContainerError> {
    if data == MIME.as_bytes() {
        Ok(())
    } else {
        Err(ContainerError::NotABookFile("another file type"))
    }
}

/// Reads a whole file held in memory: every entry with its data, `mimetype` checked.
pub fn read(file: &[u8]) -> Result<Vec<(Name, &[u8])>, ContainerError> {
    check_head(file.get(..HEAD_LEN).unwrap_or(file))?;
    let len = file.len() as u64;
    let tail = file
        .get(file.len().saturating_sub(END_LEN)..)
        .filter(|t| t.len() == END_LEN)
        .ok_or(ContainerError::NotABookFile("too short"))?;
    let dir = end_of_directory(tail, len)?;
    let directory = &file[dir.offset as usize..(dir.offset + dir.size) as usize];
    let found = entries(directory, &dir)?;
    let out: Vec<(Name, &[u8])> = found
        .iter()
        .map(|e| {
            (
                e.name,
                &file[e.offset as usize..(e.offset + e.size) as usize],
            )
        })
        .collect();
    check_mimetype(out[0].1)?;
    Ok(out)
}

/// A minimal writer of the layout, for examples and tests of readers (the instance
/// writes files with the `zip` crate). Not for real files: it writes CRC32 as zero.
#[doc(hidden)]
pub mod tests_support {
    /// A zip of `entries` in the container's layout (CRC32 left zero).
    pub fn write(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let offset = out.len() as u32;
            let header = |sig: u32| {
                let mut h = Vec::new();
                h.extend_from_slice(&sig.to_le_bytes());
                h
            };
            let mut local = header(0x0403_4b50);
            local.extend_from_slice(&[10, 0, 0, 0, 0, 0, 0, 0, 0x21, 0]);
            local.extend_from_slice(&0u32.to_le_bytes());
            local.extend_from_slice(&(data.len() as u32).to_le_bytes());
            local.extend_from_slice(&(data.len() as u32).to_le_bytes());
            local.extend_from_slice(&(name.len() as u16).to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&local);
            out.extend_from_slice(data);
            let mut c = header(super::CENTRAL_SIGNATURE);
            c.extend_from_slice(&[20, 0, 10, 0, 0, 0, 0, 0, 0, 0, 0x21, 0]);
            c.extend_from_slice(&0u32.to_le_bytes());
            c.extend_from_slice(&(data.len() as u32).to_le_bytes());
            c.extend_from_slice(&(data.len() as u32).to_le_bytes());
            c.extend_from_slice(&(name.len() as u16).to_le_bytes());
            c.extend_from_slice(&[0; 12]);
            c.extend_from_slice(&offset.to_le_bytes());
            c.extend_from_slice(name.as_bytes());
            central.extend_from_slice(&c);
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&super::END_SIGNATURE.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::write;
    use super::*;

    fn book() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("mimetype", MIME.as_bytes().to_vec()),
            ("sg.json", b"{}".to_vec()),
            ("manifest", vec![1; 40]),
            ("p/0", vec![2; 300]),
            ("i/0", vec![3; 7]),
        ]
    }

    fn zip(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let refs: Vec<(&str, &[u8])> = entries.iter().map(|(n, d)| (*n, d.as_slice())).collect();
        write(&refs)
    }

    #[test]
    fn a_book_file_is_read() {
        let file = zip(&book());
        let entries = read(&file).unwrap();
        let names: Vec<String> = entries.iter().map(|(n, _)| n.to_string()).collect();
        assert_eq!(names, ["mimetype", "sg.json", "manifest", "p/0", "i/0"]);
        assert_eq!(entries[3].1, &[2; 300][..]);
        assert_eq!(entries[1].0, Name::SgJson);
        assert_eq!(entries[2].0, Name::Blob(BlobPath::Manifest));
    }

    #[test]
    fn the_directory_alone_finds_the_entries() {
        let file = zip(&book());
        let tail = &file[file.len() - END_LEN..];
        let dir = end_of_directory(tail, file.len() as u64).unwrap();
        assert_eq!(dir.count, 5);
        let cd = &file[dir.offset as usize..(dir.offset + dir.size) as usize];
        let found = entries(cd, &dir).unwrap();
        assert_eq!(found[0].offset, 38);
        assert_eq!(found[0].size, MIME.len() as u64);
    }

    fn refused(file: &[u8]) -> ContainerError {
        read(file).unwrap_err()
    }

    #[test]
    fn the_head_alone_tells_a_book_file() {
        let file = zip(&book());
        assert_eq!(check_head(&file[..HEAD_LEN]), Ok(()));
        let mut epub = book();
        epub[0].1 = b"application/epub+zip".to_vec();
        let epub = zip(&epub);
        assert!(check_head(&epub[..HEAD_LEN]).is_err());
        assert!(check_head(&file[..HEAD_LEN - 1]).is_err());
        let mut extra = file.clone();
        extra[28] = 4;
        assert!(check_head(&extra[..HEAD_LEN]).is_err());
    }

    #[test]
    fn other_files_are_not_book_files() {
        assert!(matches!(refused(b""), ContainerError::NotABookFile(_)));
        assert!(matches!(
            refused(&[0u8; 100]),
            ContainerError::NotABookFile(_)
        ));
        let mut epub = book();
        epub[0].1 = b"application/epub+zip".to_vec();
        assert!(matches!(
            refused(&zip(&epub)),
            ContainerError::NotABookFile(_)
        ));
        let mut first = book();
        first.swap(0, 1);
        assert!(matches!(
            refused(&zip(&first)),
            ContainerError::NotABookFile(_)
        ));
    }

    #[test]
    fn unknown_or_repeated_names_are_refused() {
        for name in [
            "index.html",
            "p/01",
            "p/x",
            "../manifest",
            "/manifest",
            "i/",
        ] {
            let mut entries = book();
            entries[3].0 = name;
            assert!(
                matches!(refused(&zip(&entries)), ContainerError::Malformed(_)),
                "{name}"
            );
        }
        let mut twice = book();
        twice[4].0 = "p/0";
        assert!(matches!(
            refused(&zip(&twice)),
            ContainerError::Malformed(_)
        ));
        let mut late = book();
        late[4].0 = "sg.json";
        assert!(matches!(refused(&zip(&late)), ContainerError::Malformed(_)));
        let mut no_json = book();
        no_json.remove(1);
        assert!(matches!(
            refused(&zip(&no_json)),
            ContainerError::Malformed(_)
        ));
        assert!(matches!(
            refused(&zip(&book()[..1])),
            ContainerError::Malformed(_)
        ));
    }

    /// The offset of central header `n` (0-based) in `file`.
    fn central(file: &[u8], n: usize) -> usize {
        let dir = end_of_directory(&file[file.len() - END_LEN..], file.len() as u64).unwrap();
        let mut at = dir.offset as usize;
        for _ in 0..n {
            at += CENTRAL_HEADER_LEN + usize::from(u16_at(file, at + 28));
        }
        at
    }

    #[test]
    fn compressed_encrypted_or_extended_entries_are_refused() {
        // (offset in the central header, value)
        for (at, value) in [(10usize, 8u16), (8, 1), (8, 8), (30, 4), (32, 1), (34, 1)] {
            let mut file = zip(&book());
            let header = central(&file, 3);
            file[header + at..header + at + 2].copy_from_slice(&value.to_le_bytes());
            assert!(
                matches!(refused(&file), ContainerError::Malformed(_)),
                "field at {at} = {value}"
            );
        }
        // Compressed size different from the size.
        let mut file = zip(&book());
        let header = central(&file, 3);
        file[header + 20] ^= 1;
        assert!(matches!(refused(&file), ContainerError::Malformed(_)));
    }

    #[test]
    fn offsets_that_do_not_add_up_are_refused() {
        // A local header offset moved.
        let mut file = zip(&book());
        let header = central(&file, 2);
        file[header + 42] += 1;
        assert!(matches!(refused(&file), ContainerError::Malformed(_)));
        // A size that runs into the directory.
        let mut file = zip(&book());
        let header = central(&file, 4);
        for at in [20, 24] {
            file[header + at..header + at + 4].copy_from_slice(&1000u32.to_le_bytes());
        }
        assert!(matches!(refused(&file), ContainerError::Malformed(_)));
        // Bytes appended (the end record is no longer at the end), or a comment.
        let mut file = zip(&book());
        file.push(0);
        assert!(matches!(refused(&file), ContainerError::NotABookFile(_)));
        let mut file = zip(&book());
        let n = file.len();
        file[n - 2] = 1;
        assert!(matches!(refused(&file), ContainerError::Malformed(_)));
        // A directory offset that points elsewhere.
        let mut file = zip(&book());
        let n = file.len();
        file[n - 6] += 1;
        assert!(matches!(refused(&file), ContainerError::Malformed(_)));
        // More entries than the directory holds.
        let mut file = zip(&book());
        let n = file.len();
        file[n - 12] += 1;
        file[n - 14] += 1;
        assert!(matches!(refused(&file), ContainerError::Malformed(_)));
        // Every truncation fails without panicking.
        let file = zip(&book());
        for len in 0..file.len() {
            assert!(read(&file[..len]).is_err(), "{len}");
        }
    }

    #[test]
    fn every_byte_flip_fails_or_reads_the_same_layout() {
        let file = zip(&book());
        for at in 0..file.len() {
            let mut bad = file.clone();
            bad[at] ^= 0x5a;
            // Never a panic; a success has the same names (only data bytes changed).
            if let Ok(entries) = read(&bad) {
                let names: Vec<Name> = entries.iter().map(|(n, _)| *n).collect();
                let expected: Vec<Name> = read(&file).unwrap().iter().map(|(n, _)| *n).collect();
                assert_eq!(names, expected, "flip at {at}");
            }
        }
    }
}
