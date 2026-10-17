//! Scanner tests on temporary libraries.

mod common;

use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use rusqlite::Connection;
use shelfgauge::container::ReadLimits;
use shelfgauge::format::Registry;
use shelfgauge::library::Library;
use shelfgauge::scanner::{ScanProgress, ScanSummary, Scanner, UNFINISHED_MTIME};
use shelfgauge::storage::books::{BookStatus, all_books, book_by_id};
use shelfgauge::storage::open_writer;

/// A small FB2 book with a 1×1 PNG cover.
fn fb2(title: &str, author: &str) -> Vec<u8> {
    format!(
        r##"<?xml version="1.0" encoding="UTF-8"?>
<FictionBook xmlns:l="http://www.w3.org/1999/xlink"><description><title-info>
<author><last-name>{author}</last-name></author><book-title>{title}</book-title>
<coverpage><image l:href="#c"/></coverpage><lang>ru</lang></title-info></description>
<body><section><p>{title}</p></section></body>
<binary id="c" content-type="image/png">iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==</binary>
</FictionBook>"##
    )
    .into_bytes()
}

struct TestLibrary {
    dir: tempfile::TempDir,
    library: Library,
    conn: Connection,
}

impl TestLibrary {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        let conn = open_writer(&library.db_path()).unwrap();
        Self { dir, library, conn }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn write(&self, path: &str, data: &[u8]) {
        let full = self.root().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, data).unwrap();
    }

    fn zip(&self, path: &str, entries: &[(&str, Vec<u8>)]) {
        let full = self.root().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        let mut writer = zip::ZipWriter::new(std::fs::File::create(full).unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap();
    }

    fn scan_with(&mut self, exclude: &[String], cancel: bool) -> ScanSummary {
        let defaults = Registry::with_formats(shelfgauge::format::DEFAULT_FORMATS).unwrap();
        self.scan_registry(&defaults, exclude, cancel)
    }

    fn scan_registry(
        &mut self,
        registry: &Registry,
        exclude: &[String],
        cancel: bool,
    ) -> ScanSummary {
        let progress = ScanProgress::default();
        let cancel = AtomicBool::new(cancel);
        let scanner = Scanner {
            library: &self.library,
            exclude,
            registry,
            limits: ReadLimits::default(),
            threads: 2,
            progress: &progress,
            cancel: &cancel,
        };
        scanner.run(&mut self.conn).unwrap()
    }

    fn scan(&mut self) -> ScanSummary {
        self.scan_with(&[], false)
    }

    /// Scans with `limits`; returns the summary and the final progress.
    fn scan_limited(
        &mut self,
        limits: ReadLimits,
    ) -> (ScanSummary, shelfgauge::scanner::ProgressSnapshot) {
        let registry = Registry::with_formats(shelfgauge::format::DEFAULT_FORMATS).unwrap();
        let progress = ScanProgress::default();
        let cancel = AtomicBool::new(false);
        let summary = Scanner {
            library: &self.library,
            exclude: &[],
            registry: &registry,
            limits,
            threads: 2,
            progress: &progress,
            cancel: &cancel,
        }
        .run(&mut self.conn)
        .unwrap();
        (summary, progress.snapshot())
    }

    /// The recorded CRC32 of the plain file `path`.
    fn file_crc32(&self, path: &str) -> Option<u32> {
        shelfgauge::storage::containers::container_by_path(&self.conn, path)
            .unwrap()
            .unwrap()
            .record
            .crc32
    }

    /// `(title, status)` of all books, ordered by id.
    fn books(&self) -> Vec<(i64, String, BookStatus)> {
        all_books(&self.conn)
            .unwrap()
            .into_iter()
            .map(|b| {
                let details = book_by_id(&self.conn, b.id).unwrap().unwrap();
                (b.id, details.title, details.status)
            })
            .collect()
    }

    fn live_titles(&self) -> Vec<String> {
        let mut titles: Vec<String> = self
            .books()
            .into_iter()
            .filter(|(_, _, status)| *status != BookStatus::Removed)
            .map(|(_, title, _)| title)
            .collect();
        titles.sort();
        titles
    }

    fn id_of(&self, title: &str) -> i64 {
        self.books()
            .into_iter()
            .find(|(_, t, s)| t == title && *s != BookStatus::Removed)
            .unwrap_or_else(|| panic!("no live book {title:?}"))
            .0
    }

    fn path_of(&self, id: i64) -> String {
        let book = book_by_id(&self.conn, id).unwrap().unwrap();
        match book.entry {
            Some(entry) => format!("{}::{}", book.container_path, entry.path),
            None => book.container_path,
        }
    }

    /// Sets a file's mtime explicitly (content writes within one clock tick may keep it).
    fn set_mtime(&self, path: &str, secs: u64) {
        let file = std::fs::File::options()
            .write(true)
            .open(self.root().join(path))
            .unwrap();
        file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
            .unwrap();
    }
}

fn summary(
    added: u64,
    updated: u64,
    moved: u64,
    removed: u64,
    unchanged: u64,
    parsed: u64,
) -> ScanSummary {
    ScanSummary {
        added,
        updated,
        moved,
        removed,
        unchanged,
        parsed,
        ..ScanSummary::default()
    }
}

#[test]
fn first_scan_and_rescan_without_changes() {
    let mut lib = TestLibrary::new();
    lib.write("a/one.fb2", &fb2("Один", "Иванов"));
    lib.write("b.fb2", &fb2("Два", "Петров"));
    lib.zip(
        "c.zip",
        &[
            ("x/three.fb2", fb2("Три", "Сидоров")),
            ("four.fb2", fb2("Четыре", "Сидоров")),
            ("readme.txt", b"x".to_vec()),
        ],
    );
    lib.write(".shelfgauge/ignored.fb2", &fb2("Скрытая", "Никто"));
    lib.write("notes.txt", b"not a book");

    // The two text files are read, but they are not books.
    assert_eq!(
        lib.scan(),
        ScanSummary {
            not_books: 2,
            ..summary(4, 0, 0, 0, 0, 6)
        }
    );
    assert_eq!(lib.live_titles(), ["Два", "Один", "Три", "Четыре"]);
    let id = lib.id_of("Три");
    assert_eq!(lib.path_of(id), "c.zip::x/three.fb2");
    let covers = lib.library.covers_dir();
    assert!(covers.join(format!("{id}.png")).exists(), "cover cached");

    assert_eq!(
        lib.scan(),
        summary(0, 0, 0, 0, 4, 0),
        "no-change rescan parses nothing"
    );
}

#[test]
fn modified_and_touched_files() {
    let mut lib = TestLibrary::new();
    lib.write("book.fb2", &fb2("Старое", "Автор"));
    lib.scan();
    let id = lib.id_of("Старое");

    lib.write("book.fb2", &fb2("Новое название", "Автор"));
    lib.set_mtime("book.fb2", 1_000_000);
    assert_eq!(lib.scan(), summary(0, 1, 0, 0, 0, 1));
    assert_eq!(
        lib.id_of("Новое название"),
        id,
        "same location keeps its id"
    );

    // Only the mtime changes: the file is hashed but not parsed.
    lib.set_mtime("book.fb2", 2_000_000);
    assert_eq!(lib.scan(), summary(0, 0, 0, 0, 1, 0));
    assert_eq!(lib.scan(), summary(0, 0, 0, 0, 1, 0));
}

#[test]
fn moved_and_renamed_file_keeps_its_id() {
    let mut lib = TestLibrary::new();
    lib.write("old/book.fb2", &fb2("Маяк", "Автор"));
    lib.scan();
    let id = lib.id_of("Маяк");

    std::fs::create_dir_all(lib.root().join("new")).unwrap();
    std::fs::rename(
        lib.root().join("old/book.fb2"),
        lib.root().join("new/renamed.fb2"),
    )
    .unwrap();
    assert_eq!(lib.scan(), summary(0, 0, 1, 0, 0, 0));
    assert_eq!(lib.id_of("Маяк"), id);
    assert_eq!(lib.path_of(id), "new/renamed.fb2");
}

#[test]
fn book_moved_between_archives_keeps_its_id() {
    let mut lib = TestLibrary::new();
    lib.zip(
        "a.zip",
        &[("1.fb2", fb2("Первая", "А")), ("2.fb2", fb2("Вторая", "Б"))],
    );
    lib.zip("b.zip", &[("3.fb2", fb2("Третья", "В"))]);
    lib.scan();
    let id = lib.id_of("Вторая");

    lib.zip("a.zip", &[("1.fb2", fb2("Первая", "А"))]);
    lib.zip(
        "b.zip",
        &[
            ("3.fb2", fb2("Третья", "В")),
            ("moved/2.fb2", fb2("Вторая", "Б")),
        ],
    );
    lib.set_mtime("a.zip", 1_000_000);
    lib.set_mtime("b.zip", 1_000_000);
    assert_eq!(lib.scan(), summary(0, 0, 1, 0, 2, 0));
    assert_eq!(lib.id_of("Вторая"), id);
    assert_eq!(lib.path_of(id), "b.zip::moved/2.fb2");
}

#[test]
fn renamed_archive_is_not_parsed_again() {
    let mut lib = TestLibrary::new();
    let entries: Vec<(String, Vec<u8>)> = (0..150)
        .map(|i| (format!("{i:03}.fb2"), fb2(&format!("Книга {i}"), "Автор")))
        .collect();
    let refs: Vec<(&str, Vec<u8>)> = entries
        .iter()
        .map(|(n, d)| (n.as_str(), d.clone()))
        .collect();
    lib.zip("library.zip", &refs);
    assert_eq!(lib.scan().added, 150);
    let ids: Vec<i64> = lib.books().iter().map(|b| b.0).collect();

    std::fs::rename(
        lib.root().join("library.zip"),
        lib.root().join("renamed.zip"),
    )
    .unwrap();
    assert_eq!(lib.scan(), summary(0, 0, 150, 0, 0, 0));
    let after: Vec<i64> = lib.books().iter().map(|b| b.0).collect();
    assert_eq!(after, ids);
    assert!(lib.books().iter().all(|b| b.2 == BookStatus::Ok));
}

#[test]
fn duplicate_copies_are_not_merged() {
    let mut lib = TestLibrary::new();
    lib.write("copy1.fb2", &fb2("Дубль", "Автор"));
    lib.write("copy2.fb2", &fb2("Дубль", "Автор"));
    assert_eq!(lib.scan().added, 2);

    // Moving both copies is ambiguous: no guessing, new rows; old ones removed.
    std::fs::create_dir(lib.root().join("moved")).unwrap();
    for name in ["copy1.fb2", "copy2.fb2"] {
        std::fs::rename(lib.root().join(name), lib.root().join("moved").join(name)).unwrap();
    }
    let s = lib.scan();
    assert_eq!((s.added, s.moved, s.removed), (2, 0, 2));
    assert_eq!(lib.live_titles(), ["Дубль", "Дубль"]);

    // Deleting one copy removes exactly one book.
    std::fs::remove_file(lib.root().join("moved/copy2.fb2")).unwrap();
    assert_eq!(lib.scan(), summary(0, 0, 0, 1, 1, 0));
}

#[test]
fn deleted_and_restored_book() {
    let mut lib = TestLibrary::new();
    lib.write("keep.fb2", &fb2("Остаётся", "А"));
    lib.write("gone.fb2", &fb2("Исчезает", "Б"));
    lib.scan();
    let id = lib.id_of("Исчезает");
    let saved = std::fs::read(lib.root().join("gone.fb2")).unwrap();

    std::fs::remove_file(lib.root().join("gone.fb2")).unwrap();
    assert_eq!(lib.scan(), summary(0, 0, 0, 1, 1, 0));
    let book = book_by_id(&lib.conn, id).unwrap().unwrap();
    assert_eq!(book.status, BookStatus::Removed);

    // The same file reappearing elsewhere is matched to the removed book.
    lib.write("back/again.fb2", &saved);
    assert_eq!(lib.scan(), summary(0, 0, 1, 0, 1, 0));
    assert_eq!(lib.id_of("Исчезает"), id);
}

#[test]
fn broken_book_is_recorded_and_not_reparsed() {
    let mut lib = TestLibrary::new();
    lib.write("good.fb2", &fb2("Хорошая", "А"));
    lib.write(
        "broken.fb2",
        b"<FictionBook><body a=1></body></FictionBook>",
    );
    lib.zip(
        "mixed.zip",
        &[
            ("bad.fb2", b"not xml".to_vec()),
            ("ok.fb2", fb2("Целая", "Б")),
        ],
    );
    let s = lib.scan();
    assert_eq!((s.added, s.errors, s.parsed), (4, 2, 4));
    let broken = lib
        .books()
        .into_iter()
        .find(|b| b.1 == "broken")
        .expect("failed book titled after its file");
    assert_eq!(broken.2, BookStatus::Error);
    let details = book_by_id(&lib.conn, broken.0).unwrap().unwrap();
    assert!(details.error_message.unwrap().contains("invalid XML"));

    assert_eq!(lib.scan(), summary(0, 0, 0, 0, 4, 0));
}

#[test]
fn changed_entry_reparses_only_that_entry() {
    let mut lib = TestLibrary::new();
    let books: Vec<(String, Vec<u8>)> = (0..10)
        .map(|i| (format!("{i}.fb2"), fb2(&format!("Том {i}"), "А")))
        .collect();
    let mut refs: Vec<(&str, Vec<u8>)> =
        books.iter().map(|(n, d)| (n.as_str(), d.clone())).collect();
    lib.zip("set.zip", &refs);
    lib.scan();
    let id = lib.id_of("Том 3");

    refs[3].1 = fb2("Том 3 исправленный", "А");
    lib.zip("set.zip", &refs);
    lib.set_mtime("set.zip", 1_000_000);
    assert_eq!(lib.scan(), summary(0, 1, 0, 0, 9, 1));
    assert_eq!(lib.id_of("Том 3 исправленный"), id);
}

#[test]
fn interrupted_archive_is_completed_by_the_next_scan() {
    let mut lib = TestLibrary::new();
    lib.zip(
        "a.zip",
        &[
            ("1.fb2", fb2("Один", "А")),
            ("2.fb2", fb2("Два", "Б")),
            ("3.fb2", fb2("Три", "В")),
        ],
    );
    lib.scan();
    // Simulate a crash after the archive was started and two of its books written.
    let lost = lib.id_of("Два");
    lib.conn
        .execute_batch(&format!(
            "DELETE FROM books_fts WHERE rowid = {lost};
             DELETE FROM books WHERE id = {lost};
             UPDATE containers SET mtime_ns = {UNFINISHED_MTIME} WHERE path = 'a.zip';"
        ))
        .unwrap();
    assert_eq!(lib.scan(), summary(1, 0, 0, 0, 2, 1));
    assert_eq!(lib.live_titles(), ["Два", "Один", "Три"]);
    assert_eq!(lib.scan(), summary(0, 0, 0, 0, 3, 0));
}

#[test]
fn corrupt_archive_keeps_its_books() {
    let mut lib = TestLibrary::new();
    lib.zip("a.zip", &[("1.fb2", fb2("Один", "А"))]);
    lib.scan();
    lib.write("a.zip", b"this is no longer a zip file");
    let s = lib.scan();
    assert_eq!((s.removed, s.errors), (0, 1));
    assert_eq!(lib.live_titles(), ["Один"]);
}

#[test]
fn cancelled_scan_removes_nothing() {
    let mut lib = TestLibrary::new();
    lib.write("a.fb2", &fb2("Один", "А"));
    lib.scan();
    std::fs::remove_file(lib.root().join("a.fb2")).unwrap();
    let s = lib.scan_with(&[], true);
    assert!(s.cancelled);
    assert_eq!(s.removed, 0);
    assert_eq!(lib.live_titles(), ["Один"]);
    assert_eq!(lib.scan().removed, 1);
}

#[test]
fn excludes_are_applied() {
    let mut lib = TestLibrary::new();
    lib.write("keep/a.fb2", &fb2("Видимая", "А"));
    lib.write("drafts/b.fb2", &fb2("Черновик", "Б"));
    lib.write("keep/c.tmp.fb2", &fb2("Временная", "В"));
    let exclude = ["drafts/".to_owned(), "*.tmp.fb2".to_owned()];
    lib.scan_with(&exclude, false);
    assert_eq!(lib.live_titles(), ["Видимая"]);
}

#[test]
fn invalid_exclude_is_an_error() {
    let lib = TestLibrary::new();
    let registry = Registry::with_builtin_formats();
    let progress = ScanProgress::default();
    let cancel = AtomicBool::new(false);
    let exclude = ["{unclosed".to_owned()];
    let scanner = Scanner {
        library: &lib.library,
        exclude: &exclude,
        registry: &registry,
        limits: ReadLimits::default(),
        threads: 1,
        progress: &progress,
        cancel: &cancel,
    };
    let mut conn = open_writer(&lib.library.db_path()).unwrap();
    let err = scanner.run(&mut conn).unwrap_err();
    assert!(err.to_string().contains("exclude"), "{err}");
}

#[cfg(unix)]
#[test]
fn unreadable_directory_keeps_its_books() {
    use std::os::unix::fs::PermissionsExt;
    let mut lib = TestLibrary::new();
    lib.write("locked/a.fb2", &fb2("Заперта", "А"));
    lib.write("open/b.fb2", &fb2("Открыта", "Б"));
    lib.scan();
    let locked = lib.root().join("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read_dir(&locked).is_ok() {
        // Running as root: permissions are not enforced, nothing to test.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    let s = lib.scan();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(s.removed, 0);
    assert_eq!(lib.live_titles(), ["Заперта", "Открыта"]);
}

#[test]
fn progress_reports_completion() {
    let mut lib = TestLibrary::new();
    lib.write("a.fb2", &fb2("Один", "А"));
    lib.write("b.fb2", &fb2("Два", "Б"));
    let registry = Registry::with_builtin_formats();
    let progress = ScanProgress::default();
    let cancel = AtomicBool::new(false);
    let scanner = Scanner {
        library: &lib.library,
        exclude: &[],
        registry: &registry,
        limits: ReadLimits::default(),
        threads: 0,
        progress: &progress,
        cancel: &cancel,
    };
    scanner.run(&mut lib.conn).unwrap();
    let snapshot = progress.snapshot();
    assert_eq!(snapshot.phase, shelfgauge::scanner::ScanPhase::Done);
    assert_eq!((snapshot.total, snapshot.done, snapshot.errors), (2, 2, 0));
}

/// Books that failed with an older parser are parsed again once (for example after the
/// parser learned to repair unbalanced tags); books that parsed are left alone.
#[test]
fn error_books_from_an_older_parser_are_parsed_again() {
    const UNBALANCED: &[u8] = include_bytes!("fixtures/fb2/unbalanced.fb2");
    let mut lib = TestLibrary::new();
    lib.write("unbalanced.fb2", UNBALANCED);
    lib.write("good.fb2", &fb2("Хорошая", "А"));
    lib.zip("pack.zip", &[("inner.fb2", UNBALANCED.to_vec())]);
    let s = lib.scan();
    assert_eq!(
        (s.added, s.errors),
        (3, 0),
        "the current parser repairs the book"
    );

    // Pretend the books were written by an older parser: the unbalanced ones failed.
    lib.conn
        .execute_batch(
            "UPDATE books SET parser_version = 0;
             UPDATE books SET status = 'error', error_message = 'invalid XML'
              WHERE title = 'Лишний тег';",
        )
        .unwrap();
    let s = lib.scan();
    assert_eq!(
        (s.updated, s.parsed, s.unchanged, s.errors),
        (2, 2, 1, 0),
        "{s:?}"
    );
    for (_, title, status) in lib.books() {
        assert_eq!(status, BookStatus::Ok, "{title}");
    }
    // Retried once: now they carry the current parser version.
    assert_eq!(lib.scan(), summary(0, 0, 0, 0, 3, 0));
}

/// Regression: read errors stored for the UI used to contain the absolute path of the
/// library (revealing the server's directory layout).
#[test]
fn error_messages_do_not_reveal_absolute_paths() {
    let mut lib = TestLibrary::new();
    lib.write("big.fb2", &fb2("Большая", "А"));
    let registry = Registry::with_builtin_formats();
    let progress = ScanProgress::default();
    let cancel = AtomicBool::new(false);
    Scanner {
        library: &lib.library,
        exclude: &[],
        registry: &registry,
        limits: ReadLimits { max_book_size: 10 },
        threads: 1,
        progress: &progress,
        cancel: &cancel,
    }
    .run(&mut lib.conn)
    .unwrap();
    let (id, _, status) = lib.books().remove(0);
    assert_eq!(status, BookStatus::Error);
    let message = book_by_id(&lib.conn, id)
        .unwrap()
        .unwrap()
        .error_message
        .unwrap();
    assert!(message.contains("exceeds the limit"), "{message}");
    let root = lib.root().display().to_string();
    assert!(!message.contains(&root), "{message}");
}

#[test]
fn epub_books_plain_inside_archives_and_protected() {
    let mut lib = TestLibrary::new();
    lib.write("books/war.epub", &common::epub3());
    lib.zip(
        "more.zip",
        &[
            ("novels/pp.epub", common::epub2()),
            ("pp.fb2", fb2("Гордость", "Остин")),
        ],
    );
    let mut locked = common::epub2_entries();
    locked.push(("META-INF/license.lcpl", b"{}".to_vec()));
    lib.write("locked.epub", &common::zip(&locked));
    lib.write("broken.epub", b"not a zip");

    let s = lib.scan();
    assert_eq!(
        (s.added, s.errors),
        (5, 1),
        "a protected book is not an error"
    );
    let status = |title: &str| {
        lib.books()
            .into_iter()
            .filter(|(_, t, _)| t == title)
            .map(|(_, _, status)| status)
            .collect::<Vec<_>>()
    };
    assert_eq!(status("Война и мир"), [BookStatus::Ok]);
    assert_eq!(status("broken"), [BookStatus::Error]);
    let mut pp = status("Pride and Prejudice");
    pp.sort_by_key(|s| format!("{s:?}"));
    assert_eq!(pp, [BookStatus::Ok, BookStatus::Protected]);
    let war = book_by_id(&lib.conn, lib.id_of("Война и мир"))
        .unwrap()
        .unwrap();
    assert_eq!(war.format, "epub");
    assert!(war.cover_mime.is_some());
    let in_zip = lib
        .books()
        .into_iter()
        .map(|(id, _, _)| lib.path_of(id))
        .find(|p| p.ends_with(".epub") && p.contains("::"))
        .unwrap();
    assert_eq!(in_zip, "more.zip::novels/pp.epub");
    let locked = lib
        .books()
        .into_iter()
        .find(|(id, _, _)| lib.path_of(*id) == "locked.epub")
        .unwrap();
    let details = book_by_id(&lib.conn, locked.0).unwrap().unwrap();
    assert_eq!(details.drm.as_deref(), Some("Readium LCP"));

    assert_eq!(
        lib.scan(),
        summary(0, 0, 0, 0, 5, 0),
        "nothing parsed again"
    );
}

/// Regression: an archive indexed before a format was supported is listed again (its
/// stat is unchanged), so its books of the new format are found; its other books are
/// not parsed again.
#[test]
fn archives_are_listed_again_when_formats_are_added() {
    let mut lib = TestLibrary::new();
    lib.zip(
        "mixed.zip",
        &[
            ("a.fb2", fb2("Старая", "Автор")),
            ("b.epub", common::epub2()),
        ],
    );
    let mut fb2_only = Registry::new();
    fb2_only.register(Box::new(shelfgauge::format::fb2::Fb2Parser));
    let s = lib.scan_registry(&fb2_only, &[], false);
    assert_eq!((s.added, s.parsed), (1, 1));
    assert_eq!(lib.live_titles(), ["Старая"]);

    let s = lib.scan();
    assert_eq!((s.added, s.parsed, s.unchanged), (1, 1, 1), "{s:?}");
    assert_eq!(lib.live_titles(), ["Pride and Prejudice", "Старая"]);
    assert_eq!(
        lib.scan(),
        summary(0, 0, 0, 0, 2, 0),
        "listed once, then unchanged"
    );
}

/// A `.note` format for scanner tests: a file starting with `skip` is not a book, any
/// other file is a book titled with its first line.
struct NoteParser;

impl shelfgauge::format::BookParser for NoteParser {
    fn format(&self) -> &'static str {
        "note"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["note"]
    }

    fn parse_metadata(
        &self,
        data: &[u8],
    ) -> Result<shelfgauge::format::model::BookMetadata, shelfgauge::format::ParseError> {
        let text = String::from_utf8_lossy(data);
        if text.starts_with("skip") {
            return Err(shelfgauge::format::ParseError::not_a_book("a note"));
        }
        Ok(shelfgauge::format::model::BookMetadata {
            title: text.lines().next().unwrap_or_default().to_owned(),
            ..Default::default()
        })
    }

    fn parse_content(
        &self,
        _: &[u8],
    ) -> Result<shelfgauge::format::model::BookContent, shelfgauge::format::ParseError> {
        Err(shelfgauge::format::ParseError::new("not needed"))
    }
}

fn notes_registry() -> Registry {
    let mut registry = Registry::with_builtin_formats();
    registry.register(Box::new(NoteParser));
    registry
}

#[test]
fn files_that_are_not_books_are_skipped_silently() {
    let mut lib = TestLibrary::new();
    let registry = notes_registry();
    lib.write("a/readme.note", b"skip me");
    lib.write("a/book.note", b"A Book");
    lib.zip(
        "pack.zip",
        &[
            ("x.note", b"skip too".to_vec()),
            ("y.note", b"Zipped".to_vec()),
        ],
    );
    let s = lib.scan_registry(&registry, &[], false);
    assert_eq!(
        (s.added, s.parsed, s.errors, s.not_books),
        (2, 4, 0, 2),
        "{s:?}"
    );
    assert_eq!(lib.live_titles(), ["A Book", "Zipped"]);
    assert_eq!(
        lib.books().len(),
        2,
        "nothing recorded for the skipped files"
    );

    let s = lib.scan_registry(&registry, &[], false);
    assert_eq!(s, summary(0, 0, 0, 0, 2, 0), "the rescan stays silent");
}

#[test]
fn a_book_that_becomes_not_a_book_is_removed_and_back() {
    let mut lib = TestLibrary::new();
    let registry = notes_registry();
    lib.write("book.note", b"Once a book");
    lib.zip("pack.zip", &[("in.note", b"Zipped book".to_vec())]);
    lib.scan_registry(&registry, &[], false);
    let id = lib.id_of("Once a book");

    lib.write("book.note", b"skip now");
    lib.set_mtime("book.note", 1_000_000);
    lib.zip("pack.zip", &[("in.note", b"skip as well".to_vec())]);
    lib.set_mtime("pack.zip", 1_000_000);
    let s = lib.scan_registry(&registry, &[], false);
    assert_eq!((s.removed, s.not_books, s.errors), (2, 2, 0), "{s:?}");
    assert!(lib.live_titles().is_empty());
    assert_eq!(
        lib.scan_registry(&registry, &[], false),
        summary(0, 0, 0, 0, 0, 0)
    );

    lib.write("book.note", b"A book again");
    lib.set_mtime("book.note", 2_000_000);
    let s = lib.scan_registry(&registry, &[], false);
    assert_eq!((s.added, s.not_books), (1, 0), "{s:?}");
    assert_eq!(lib.live_titles(), ["A book again"]);
    assert_ne!(
        lib.id_of("A book again"),
        id,
        "a removed book is not revived"
    );
}

/// A zip of `(name, data)` entries in memory.
fn zipped(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, data) in entries {
        writer
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(data).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

#[test]
fn zipped_fb2_books() {
    let mut lib = TestLibrary::new();
    lib.write("plain.fbz", &zipped(&[("book.fb2", fb2("Сжатая", "А"))]));
    lib.write(
        "really-zip.fb2",
        &zipped(&[("x.fb2", fb2("Тайный архив", "Б"))]),
    );
    lib.write("empty.fbz", &zipped(&[("notes.txt", b"no book".to_vec())]));
    lib.zip(
        "pack.zip",
        &[("inner.FBZ", zipped(&[("in.fb2", fb2("Внутри", "В"))]))],
    );
    let s = lib.scan();
    assert_eq!((s.added, s.errors), (4, 1), "{s:?}");
    assert_eq!(
        lib.live_titles(),
        ["empty", "Внутри", "Сжатая", "Тайный архив"]
    );
    let broken = lib
        .books()
        .into_iter()
        .find(|(_, title, _)| title == "empty")
        .unwrap();
    assert_eq!(broken.2, BookStatus::Error);
    let details = book_by_id(&lib.conn, lib.id_of("Внутри")).unwrap().unwrap();
    assert_eq!(details.format, "fb2");
    assert_eq!(lib.path_of(details.id), "pack.zip::inner.FBZ");
}

#[test]
fn text_books_take_title_and_author_from_the_name() {
    let mut lib = TestLibrary::new();
    let book = common::txt_libru();
    let (cp1251, _, _) = encoding_rs::WINDOWS_1251.encode(&book);
    lib.write("ru/Пушкин - Дубровский.txt", &cp1251);
    lib.write("ru/readme.txt", book.as_bytes());
    lib.zip(
        "mixed.zip",
        &[
            ("a.fb2", fb2("Из архива", "Г")),
            ("b.epub", common::epub3()),
            ("Austen - Pride.txt", common::txt_gutenberg().into_bytes()),
            ("note.txt", b"too short".to_vec()),
        ],
    );
    let s = lib.scan();
    assert_eq!((s.added, s.errors, s.not_books), (4, 0, 2), "{s:?}");
    let id = lib.id_of("Дубровский");
    let details = book_by_id(&lib.conn, id).unwrap().unwrap();
    assert_eq!(details.format, "txt");
    assert_eq!(details.authors.len(), 1);
    assert_eq!(details.language, None);
    assert!(lib.live_titles().contains(&"Pride".to_owned()));
}

#[test]
fn formats_taken_off_the_list_are_removed_and_come_back() {
    let mut lib = TestLibrary::new();
    lib.write("a.fb2", &fb2("Книга", "А"));
    lib.write("Автор - Текст.txt", common::txt_libru().as_bytes());
    lib.zip(
        "pack.zip",
        &[("in.txt", common::txt_gutenberg().into_bytes())],
    );
    let all = Registry::with_formats(&["fb2", "epub", "txt"]).unwrap();
    let no_txt = Registry::with_formats(&["fb2", "epub"]).unwrap();
    assert_eq!(lib.scan_registry(&all, &[], false).added, 3);
    let text_id = lib.id_of("Текст");

    let s = lib.scan_registry(&no_txt, &[], false);
    assert_eq!((s.removed, s.unchanged), (2, 1), "{s:?}");
    assert_eq!(lib.live_titles(), ["Книга"]);

    let s = lib.scan_registry(&all, &[], false);
    assert_eq!((s.moved, s.added, s.unchanged), (2, 0, 1), "{s:?}");
    assert_eq!(lib.id_of("Текст"), text_id, "the same book comes back");
}

#[test]
fn html_books_are_indexed_only_when_enabled() {
    let mut lib = TestLibrary::new();
    lib.write("site/Пушкин.html", common::html_libru().as_bytes());
    lib.write("site/index.htm", common::html_saved_page().as_bytes());
    lib.zip(
        "pages.zip",
        &[("war.xhtml", common::html_gutenberg().into_bytes())],
    );
    lib.write("book.fb2", &fb2("Книга", "А"));
    let s = lib.scan();
    assert_eq!((s.added, s.not_books), (1, 0), "off by default: {s:?}");

    let with_html = Registry::with_formats(&["fb2", "epub", "txt", "html"]).unwrap();
    let s = lib.scan_registry(&with_html, &[], false);
    assert_eq!((s.added, s.not_books, s.errors), (2, 1, 0), "{s:?}");
    assert_eq!(lib.live_titles(), ["War and Peace", "Дубровский", "Книга"]);
    let details = book_by_id(&lib.conn, lib.id_of("Дубровский"))
        .unwrap()
        .unwrap();
    assert_eq!(details.format, "html");
    assert_eq!(
        lib.scan_registry(&with_html, &[], false),
        summary(0, 0, 0, 0, 3, 0)
    );

    let s = lib.scan();
    assert_eq!(s.removed, 2, "turned off again: {s:?}");
}

/// Schema version 4 records a CRC32 for plain files: the first scan after the upgrade
/// reads every plain file once more (without parsing it), later scans read nothing.
#[test]
fn plain_files_get_their_crc32_once() {
    let mut lib = TestLibrary::new();
    let one = fb2("Один", "Иванов");
    lib.write("a.fb2", &one);
    lib.write("b.fb2", &fb2("Два", "Петров"));
    lib.zip("c.zip", &[("three.fb2", fb2("Три", "Сидоров"))]);
    let big = fb2(&"Длинная ".repeat(500), "Толстой");
    lib.write("big.fb2", &big);
    let limits = ReadLimits {
        max_book_size: (big.len() - 1) as u64,
    };
    let (first, _) = lib.scan_limited(limits);
    assert_eq!((first.added, first.errors), (4, 1), "{first:?}");
    assert_eq!(lib.file_crc32("a.fb2"), Some(crc32fast::hash(&one)));
    assert_eq!(lib.file_crc32("c.zip"), None, "archives have none");
    assert_eq!(lib.file_crc32("big.fb2"), None, "not read");
    let scan = shelfgauge::storage::scans::last_scan(&lib.conn)
        .unwrap()
        .expect("the scan is recorded");
    assert_eq!((scan.added, scan.errors, scan.cancelled), (4, 1, false));
    assert!(scan.finished_at >= scan.started_at && scan.started_at > 0);

    // As after the migration: no CRC32 recorded yet.
    lib.conn
        .execute("UPDATE containers SET crc32 = NULL", [])
        .unwrap();
    let (upgrade, progress) = lib.scan_limited(limits);
    assert_eq!(upgrade, summary(0, 0, 0, 0, 4, 0), "nothing is parsed");
    assert_eq!(progress.total, 2, "the two readable plain files are read");
    assert_eq!(lib.file_crc32("a.fb2"), Some(crc32fast::hash(&one)));
    assert!(lib.file_crc32("b.fb2").is_some());
    assert_eq!(lib.file_crc32("big.fb2"), None);

    let (later, progress) = lib.scan_limited(limits);
    assert_eq!(later, summary(0, 0, 0, 0, 4, 0));
    assert_eq!(progress.total, 0, "nothing is read again");
    let generation = shelfgauge::storage::scans::scan_generation(&lib.conn).unwrap();
    assert_eq!(generation, 3, "every scan is recorded");
}

/// A moved plain file keeps its CRC32 at the new path.
#[test]
fn moved_files_keep_their_crc32() {
    let mut lib = TestLibrary::new();
    let one = fb2("Один", "Иванов");
    lib.write("a.fb2", &one);
    lib.scan();
    std::fs::rename(lib.root().join("a.fb2"), lib.root().join("moved.fb2")).unwrap();
    assert_eq!(lib.scan(), summary(0, 0, 1, 0, 0, 0));
    assert_eq!(lib.file_crc32("moved.fb2"), Some(crc32fast::hash(&one)));
}
