//! Container layer end to end: books as plain files and inside zip archives.

use std::io::Write;

use shelfgauge::container::zip::list_books;
use shelfgauge::container::{BookLocation, ReadLimits, read_book};
use shelfgauge::format::{Registry, run_with_parser_stack};

const MINIMAL: &[u8] = include_bytes!("fixtures/fb2/minimal.fb2");
const FULL: &[u8] = include_bytes!("fixtures/fb2/full.fb2");

#[test]
fn books_in_files_and_archives_parse_the_same() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("plain")).unwrap();
    std::fs::write(root.path().join("plain/full.fb2"), FULL).unwrap();

    let archive = root.path().join("Сборник.zip");
    {
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in [
            ("Маяк/full.fb2", FULL),
            ("minimal.FB2", MINIMAL),
            ("cover.jpg", &b"\xff\xd8\xff"[..]),
        ] {
            writer.start_file(name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap();
    }

    let registry = Registry::with_builtin_formats();
    let books = list_books(
        &archive,
        registry.for_extension("fb2").unwrap().extensions(),
    )
    .unwrap();
    let names: Vec<&str> = books.iter().map(|b| b.display_name.as_str()).collect();
    assert_eq!(names, ["Маяк/full.fb2", "minimal.FB2"]);

    let limits = ReadLimits::default();
    let plain = BookLocation {
        container_path: "plain/full.fb2".into(),
        entry: None,
    };
    let zipped = BookLocation {
        container_path: "Сборник.zip".into(),
        entry: Some(books[0].entry.clone()),
    };
    let plain_bytes = read_book(root.path(), &plain, &limits).unwrap();
    let zipped_bytes = read_book(root.path(), &zipped, &limits).unwrap();
    assert_eq!(plain_bytes, zipped_bytes);

    let parser = registry.for_extension("fb2").unwrap();
    let metadata = run_with_parser_stack(|| parser.parse_metadata(&zipped_bytes)).unwrap();
    assert_eq!(metadata.title, "Старый маяк");
}

#[test]
fn archive_removed_after_scan_needs_rescan() {
    let root = tempfile::tempdir().unwrap();
    let location = BookLocation {
        container_path: "gone.zip".into(),
        entry: Some(shelfgauge::container::EntryRef {
            name: b"a.fb2".to_vec(),
            index: 0,
            size: 1,
            crc32: 0,
        }),
    };
    let err = read_book(root.path(), &location, &ReadLimits::default()).unwrap_err();
    assert!(err.needs_rescan(), "{err}");
}
