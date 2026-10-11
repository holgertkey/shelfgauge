//! Zip archives: enumerating book entries and reading one entry.
//!
//! Entries are listed from the central directory only (no decompression). An entry is
//! identified by its raw name bytes; the display name is decoded for the UI:
//!
//! 1. the UTF-8 flag (general purpose bit 11) is set, or the name is valid UTF-8 → UTF-8;
//! 2. every non-ASCII byte is a Cyrillic letter in CP866 → CP866 (DOS archivers),
//!    unless the name is also all-letters in windows-1251 and uses none of CP866's
//!    `а`–`п` (0x80–0xAF): lowercase windows-1251 text falls in CP866's `р`–`я` range;
//! 3. every non-ASCII byte is a Cyrillic letter in windows-1251 → windows-1251 (some
//!    Windows archivers store the ANSI code page);
//! 4. otherwise CP437, the encoding the zip specification prescribes.
//!
//! Nested archives and other non-book entries are skipped.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use ::zip::ZipArchive;
use ::zip::result::ZipError;
use encoding_rs::{IBM866, WINDOWS_1251};

use super::{ContainerError, EntryRef, ReadLimits};

/// General purpose flag bit 11: the name and comment are UTF-8.
const UTF8_FLAG: u16 = 1 << 11;

/// A book entry found in an archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipEntry {
    /// How to find the entry again.
    pub entry: EntryRef,
    /// The entry name decoded for display (see the [module docs](self)).
    pub display_name: String,
    /// Compressed size in bytes.
    pub compressed_size: u64,
}

/// Lists the entries of the archive at `path` whose name ends with one of `extensions`
/// (lowercase, without the dot; matched case-insensitively).
///
/// Directories, encrypted entries and entries with other extensions (including nested
/// archives) are skipped.
///
/// # Examples
///
/// ```no_run
/// use shelfgauge::container::zip::list_books;
///
/// for book in list_books("library/books.zip".as_ref(), &["fb2"]).unwrap() {
///     println!("{} ({} bytes)", book.display_name, book.entry.size);
/// }
/// ```
pub fn list_books(path: &Path, extensions: &[&str]) -> Result<Vec<ZipEntry>, ContainerError> {
    let archive = open(path)?;
    let mut books = Vec::new();
    for index in 0..archive.len() {
        let file = archive
            .by_index_data(index)
            .map_err(|e| zip_error(path, e))?;
        let raw = file.name_raw();
        if file.is_dir() || !has_extension(raw, extensions) {
            continue;
        }
        let display_name = display_name(raw, file.flags().as_u16() & UTF8_FLAG != 0);
        if file.encrypted() {
            tracing::debug!(archive = %path.display(), "skipping encrypted entry {display_name:?}");
            continue;
        }
        books.push(ZipEntry {
            entry: EntryRef {
                name: raw.to_vec(),
                index,
                size: file.size(),
                crc32: file.crc32(),
            },
            display_name,
            compressed_size: file.compressed_size(),
        });
    }
    Ok(books)
}

/// Reads one entry, checking that it still matches `entry` and respecting the limits.
///
/// The entry is looked up at `entry.index` first and by raw name otherwise. Its size and
/// CRC32 must equal the recorded ones ([`ContainerError::EntryChanged`] otherwise); the
/// data is verified against the CRC32 while reading, and decompression stops at the
/// declared size, so lying headers cannot inflate memory use.
pub fn read_entry(
    path: &Path,
    entry: &EntryRef,
    limits: &ReadLimits,
) -> Result<Vec<u8>, ContainerError> {
    let mut archive = open(path)?;
    let name = || display_name(&entry.name, false);
    let index = find(&archive, entry).ok_or_else(|| ContainerError::EntryNotFound {
        path: path.to_owned(),
        name: name(),
    })?;
    {
        let file = archive
            .by_index_data(index)
            .map_err(|e| zip_error(path, e))?;
        if file.size() != entry.size || file.crc32() != entry.crc32 {
            return Err(ContainerError::EntryChanged {
                path: path.to_owned(),
                name: name(),
            });
        }
        if file.encrypted() {
            return Err(ContainerError::Encrypted {
                path: path.to_owned(),
                name: name(),
            });
        }
        if file.size() > limits.max_book_size {
            return Err(ContainerError::TooLarge {
                path: path.to_owned(),
                size: file.size(),
                limit: limits.max_book_size,
            });
        }
    }
    let file = archive.by_index(index).map_err(|e| zip_error(path, e))?;
    let capacity = usize::try_from(entry.size).unwrap_or(0);
    let mut data = Vec::with_capacity(capacity);
    file.take(limits.max_book_size + 1)
        .read_to_end(&mut data)
        .map_err(|e| ContainerError::Zip {
            path: path.to_owned(),
            message: format!("cannot read entry {:?}: {e}", name()),
        })?;
    if data.len() as u64 != entry.size {
        return Err(ContainerError::Zip {
            path: path.to_owned(),
            message: format!("entry {:?} has an unexpected size", name()),
        });
    }
    Ok(data)
}

/// Decodes an entry name for display (see the [module docs](self)).
pub fn display_name(raw: &[u8], utf8_flag: bool) -> String {
    if utf8_flag {
        return String::from_utf8_lossy(raw).into_owned();
    }
    if let Ok(name) = std::str::from_utf8(raw) {
        return name.to_owned();
    }
    let high = || raw.iter().copied().filter(|b| !b.is_ascii());
    let cp866 = high().all(is_cp866_letter);
    let cp1251 = high().all(is_cp1251_letter);
    if cp866 && !(cp1251 && !high().any(|b| (0x80..=0xaf).contains(&b))) {
        return IBM866.decode_without_bom_handling(raw).0.into_owned();
    }
    if cp1251 {
        return WINDOWS_1251.decode_without_bom_handling(raw).0.into_owned();
    }
    decode_cp437(raw)
}

/// Cyrillic letters in CP866: `А`–`п`, `р`–`я`, `Ё`, `ё`.
fn is_cp866_letter(b: u8) -> bool {
    matches!(b, 0x80..=0xaf | 0xe0..=0xf1)
}

/// Cyrillic letters in windows-1251: `А`–`я`, `Ё`, `ё`.
fn is_cp1251_letter(b: u8) -> bool {
    matches!(b, 0xc0..=0xff | 0xa8 | 0xb8)
}

/// The upper half (0x80–0xFF) of code page 437.
const CP437_HIGH: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»\
    ░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀\
    αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{a0}";

/// Decodes CP437 (not provided by `encoding_rs`).
fn decode_cp437(raw: &[u8]) -> String {
    raw.iter()
        .map(|&b| match b {
            0..=0x7f => char::from(b),
            _ => CP437_HIGH
                .chars()
                .nth(usize::from(b - 0x80))
                .unwrap_or('\u{fffd}'),
        })
        .collect()
}

/// Whether a raw name ends with `.<ext>` for one of `extensions` (ASCII, any case).
fn has_extension(raw: &[u8], extensions: &[&str]) -> bool {
    extensions.iter().any(|ext| {
        raw.len() > ext.len() + 1
            && raw[raw.len() - ext.len() - 1] == b'.'
            && raw[raw.len() - ext.len()..].eq_ignore_ascii_case(ext.as_bytes())
            && !matches!(raw[raw.len() - ext.len() - 2], b'/' | b'\\')
    })
}

/// The index of `entry`: the recorded index if the name matches there, else by name.
fn find<R: Read + std::io::Seek>(archive: &ZipArchive<R>, entry: &EntryRef) -> Option<usize> {
    let name_at = |i| {
        archive
            .by_index_data(i)
            .ok()
            .map(|f| f.name_raw() == entry.name)
    };
    if entry.index < archive.len() && name_at(entry.index) == Some(true) {
        return Some(entry.index);
    }
    (0..archive.len()).find(|&i| name_at(i) == Some(true))
}

fn open(path: &Path) -> Result<ZipArchive<BufReader<File>>, ContainerError> {
    let file = File::open(path).map_err(|error| ContainerError::Io {
        path: path.to_owned(),
        error,
    })?;
    ZipArchive::new(BufReader::new(file)).map_err(|e| zip_error(path, e))
}

fn zip_error(path: &Path, error: ZipError) -> ContainerError {
    match error {
        ZipError::Io(error) => ContainerError::Io {
            path: path.to_owned(),
            error,
        },
        other => ContainerError::Zip {
            path: path.to_owned(),
            message: other.to_string(),
        },
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write;

    use super::*;

    /// One entry for [`build_zip`]: stored (uncompressed) data with a raw name.
    pub(crate) struct Entry<'a> {
        pub name: &'a [u8],
        pub data: &'a [u8],
        pub utf8_flag: bool,
        /// Overrides the declared uncompressed size (to build lying headers).
        pub declared_size: Option<u32>,
    }

    impl<'a> Entry<'a> {
        pub(crate) fn new(name: &'a [u8], data: &'a [u8]) -> Self {
            Self {
                name,
                data,
                utf8_flag: false,
                declared_size: None,
            }
        }
    }

    pub(crate) fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// Builds a zip archive of stored entries with full control over raw names and flags.
    pub(crate) fn build_zip(entries: &[Entry<'_>]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for e in entries {
            let offset = out.len() as u32;
            let flags: u16 = if e.utf8_flag { UTF8_FLAG } else { 0 };
            let crc = crc32(e.data);
            let size = e.data.len() as u32;
            let declared = e.declared_size.unwrap_or(size);
            let common = |buf: &mut Vec<u8>| {
                buf.extend_from_slice(&20u16.to_le_bytes()); // version needed
                buf.extend_from_slice(&flags.to_le_bytes());
                buf.extend_from_slice(&0u16.to_le_bytes()); // stored
                buf.extend_from_slice(&0u16.to_le_bytes()); // time
                buf.extend_from_slice(&0x21u16.to_le_bytes()); // date 1980-01-01
                buf.extend_from_slice(&crc.to_le_bytes());
                buf.extend_from_slice(&size.to_le_bytes()); // compressed
                buf.extend_from_slice(&declared.to_le_bytes()); // uncompressed
                buf.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
                buf.extend_from_slice(&0u16.to_le_bytes()); // extra length
            };
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            common(&mut out);
            out.extend_from_slice(e.name);
            out.extend_from_slice(e.data);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes()); // version made by
            common(&mut central);
            central.extend_from_slice(&0u16.to_le_bytes()); // comment length
            central.extend_from_slice(&0u16.to_le_bytes()); // disk
            central.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
            central.extend_from_slice(&0u32.to_le_bytes()); // external attributes
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(e.name);
        }
        let central_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&central_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn write_zip(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn names(books: &[ZipEntry]) -> Vec<&str> {
        books.iter().map(|b| b.display_name.as_str()).collect()
    }

    #[test]
    fn single_book_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_zip(
            dir.path(),
            "one.zip",
            &build_zip(&[Entry::new(b"book.fb2", b"<FictionBook/>")]),
        );
        let books = list_books(&path, &["fb2"]).unwrap();
        assert_eq!(names(&books), ["book.fb2"]);
        let entry = &books[0].entry;
        assert_eq!((entry.index, entry.size), (0, 14));
        assert_eq!(entry.crc32, crc32(b"<FictionBook/>"));
        let data = read_entry(&path, entry, &ReadLimits::default()).unwrap();
        assert_eq!(data, b"<FictionBook/>");
    }

    #[test]
    fn many_books_and_non_book_entries() {
        let dir = tempfile::tempdir().unwrap();
        let bodies: Vec<Vec<u8>> = (0..50).map(|i| format!("book {i}").into_bytes()).collect();
        let names_raw: Vec<String> = (0..50).map(|i| format!("dir/book{i:02}.FB2")).collect();
        let mut entries: Vec<Entry<'_>> = names_raw
            .iter()
            .zip(&bodies)
            .map(|(n, d)| Entry::new(n.as_bytes(), d))
            .collect();
        entries.push(Entry::new(b"dir/", b""));
        entries.push(Entry::new(b"readme.txt", b"text"));
        entries.push(Entry::new(b"nested.zip", b"PK"));
        entries.push(Entry::new(b"nested.fb2.zip", b"PK"));
        entries.push(Entry::new(b"dir/.fb2", b"hidden, no name"));
        entries.push(Entry::new(b"fb2", b"no extension"));
        let path = write_zip(dir.path(), "many.zip", &build_zip(&entries));
        let books = list_books(&path, &["fb2"]).unwrap();
        assert_eq!(books.len(), 50);
        assert_eq!(books[49].display_name, "dir/book49.FB2");
        let data = read_entry(&path, &books[17].entry, &ReadLimits::default()).unwrap();
        assert_eq!(data, b"book 17");
    }

    #[test]
    fn deflated_archive_written_by_the_zip_crate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deflated.zip");
        let text = "Старый маяк. ".repeat(1000);
        {
            let mut writer = ::zip::ZipWriter::new(File::create(&path).unwrap());
            let options = ::zip::write::SimpleFileOptions::default()
                .compression_method(::zip::CompressionMethod::Deflated);
            writer.start_file("Маяк/книга.fb2", options).unwrap();
            writer.write_all(text.as_bytes()).unwrap();
            writer.finish().unwrap();
        }
        let books = list_books(&path, &["fb2"]).unwrap();
        assert_eq!(names(&books), ["Маяк/книга.fb2"]);
        assert!(books[0].compressed_size < books[0].entry.size);
        let data = read_entry(&path, &books[0].entry, &ReadLimits::default()).unwrap();
        assert_eq!(data, text.as_bytes());
    }

    #[test]
    fn legacy_name_encodings() {
        let (cp866, _, _) = IBM866.encode("Толстой/Война и мир.fb2");
        let (cp1251, _, _) = WINDOWS_1251.encode("Толстой/Война и мир.fb2");
        let cp437 = b"caf\x82 cr\x8ame.fb2"; // "café crème" in CP437
        let dir = tempfile::tempdir().unwrap();
        let mut utf8 = Entry::new("Пушкин.fb2".as_bytes(), b"u");
        utf8.utf8_flag = true;
        let path = write_zip(
            dir.path(),
            "names.zip",
            &build_zip(&[
                Entry::new(&cp866, b"a"),
                Entry::new(&cp1251, b"b"),
                Entry::new(cp437, b"c"),
                utf8,
                Entry::new("Гоголь.fb2".as_bytes(), b"unflagged UTF-8"),
            ]),
        );
        let books = list_books(&path, &["fb2"]).unwrap();
        assert_eq!(
            names(&books),
            [
                "Толстой/Война и мир.fb2",
                "Толстой/Война и мир.fb2",
                // Western names are ambiguous with CP866 letters; CP866 is preferred.
                "cafВ crКme.fb2",
                "Пушкин.fb2",
                "Гоголь.fb2",
            ]
        );
        // The raw bytes, not the decoded name, identify the entry.
        assert_eq!(books[0].entry.name, cp866.as_ref());
        assert_eq!(
            read_entry(&path, &books[1].entry, &ReadLimits::default()).unwrap(),
            b"b"
        );
    }

    #[test]
    fn ambiguous_lowercase_names() {
        let (lower1251, _, _) = WINDOWS_1251.encode("война.fb2");
        assert_eq!(display_name(&lower1251, false), "война.fb2");
        let (lower866, _, _) = IBM866.encode("мир.fb2");
        assert_eq!(display_name(&lower866, false), "мир.fb2");
        // Known limitation: a CP866 name using only `р`–`я` reads as windows-1251.
        let (only_r_ya, _, _) = IBM866.encode("ряс.fb2");
        assert_eq!(display_name(&only_r_ya, false), "апб.fb2");
    }

    #[test]
    fn cp437_fallback_and_table() {
        assert_eq!(CP437_HIGH.chars().count(), 128);
        // 0xB0-0xBF are letters in neither CP866 nor windows-1251, so CP437 is used.
        assert_eq!(display_name(b"\xb0\xb1.fb2", false), "░▒.fb2");
        assert_eq!(decode_cp437(b"\x80\xff"), "Ç\u{a0}");
    }

    #[test]
    fn entry_moved_to_another_index_is_found_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_zip(
            dir.path(),
            "moved.zip",
            &build_zip(&[Entry::new(b"a.fb2", b"A"), Entry::new(b"b.fb2", b"B")]),
        );
        let mut entry = list_books(&path, &["fb2"]).unwrap()[1].entry.clone();
        entry.index = 0;
        assert_eq!(
            read_entry(&path, &entry, &ReadLimits::default()).unwrap(),
            b"B"
        );
        entry.index = 99;
        assert_eq!(
            read_entry(&path, &entry, &ReadLimits::default()).unwrap(),
            b"B"
        );
    }

    #[test]
    fn missing_or_changed_entries_need_rescan() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_zip(
            dir.path(),
            "x.zip",
            &build_zip(&[Entry::new(b"a.fb2", b"A")]),
        );
        let mut entry = list_books(&path, &["fb2"]).unwrap()[0].entry.clone();
        entry.crc32 ^= 1;
        let err = read_entry(&path, &entry, &ReadLimits::default()).unwrap_err();
        assert!(matches!(err, ContainerError::EntryChanged { .. }), "{err}");
        assert!(err.needs_rescan());
        entry.name = b"gone.fb2".to_vec();
        let err = read_entry(&path, &entry, &ReadLimits::default()).unwrap_err();
        assert!(matches!(err, ContainerError::EntryNotFound { .. }), "{err}");
        assert!(err.needs_rescan());
    }

    #[test]
    fn corrupted_archives_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let garbage = write_zip(dir.path(), "garbage.zip", b"definitely not a zip file");
        let err = list_books(&garbage, &["fb2"]).unwrap_err();
        assert!(matches!(err, ContainerError::Zip { .. }), "{err}");
        assert!(!err.needs_rescan());

        let good = build_zip(&[Entry::new(b"a.fb2", b"some book data")]);
        let truncated = write_zip(dir.path(), "truncated.zip", &good[..good.len() - 10]);
        assert!(list_books(&truncated, &["fb2"]).is_err());

        // Damaged data with an intact central directory: the CRC check catches it.
        let mut damaged = good.clone();
        let at = damaged.windows(4).position(|w| w == b"some").unwrap();
        damaged[at] = b'S';
        let damaged = write_zip(dir.path(), "damaged.zip", &damaged);
        let entry = list_books(&damaged, &["fb2"]).unwrap()[0].entry.clone();
        let err = read_entry(&damaged, &entry, &ReadLimits::default()).unwrap_err();
        assert!(err.to_string().contains("checksum"), "{err}");
    }

    #[test]
    fn entries_over_the_limit_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bomb.zip");
        {
            // 16 MB of zeros deflate to a few kilobytes.
            let mut writer = ::zip::ZipWriter::new(File::create(&path).unwrap());
            let options = ::zip::write::SimpleFileOptions::default()
                .compression_method(::zip::CompressionMethod::Deflated);
            writer.start_file("bomb.fb2", options).unwrap();
            let zeros = vec![0u8; 1024 * 1024];
            for _ in 0..16 {
                writer.write_all(&zeros).unwrap();
            }
            writer.finish().unwrap();
        }
        let books = list_books(&path, &["fb2"]).unwrap();
        let limits = ReadLimits {
            max_book_size: 1024 * 1024,
        };
        let err = read_entry(&path, &books[0].entry, &limits).unwrap_err();
        assert!(
            matches!(err, ContainerError::TooLarge { size, .. } if size == 16 * 1024 * 1024),
            "{err}"
        );
    }

    #[test]
    fn lying_size_header_cannot_inflate_reads() {
        let dir = tempfile::tempdir().unwrap();
        let mut entry = Entry::new(b"liar.fb2", b"0123456789");
        entry.declared_size = Some(4);
        let path = write_zip(dir.path(), "liar.zip", &build_zip(&[entry]));
        let books = list_books(&path, &["fb2"]).unwrap();
        assert_eq!(books[0].entry.size, 4);
        assert!(read_entry(&path, &books[0].entry, &ReadLimits::default()).is_err());
    }

    #[test]
    fn extension_matching() {
        assert!(has_extension(b"a.fb2", &["fb2"]));
        assert!(has_extension(b"dir/A.FB2", &["fb2"]));
        assert!(!has_extension(b".fb2", &["fb2"]));
        assert!(!has_extension(b"dir/.fb2", &["fb2"]));
        assert!(!has_extension(b"a.fb2.zip", &["fb2"]));
        assert!(!has_extension(b"afb2", &["fb2"]));
        assert!(!has_extension(b"a.fb2", &[]));
    }
}
