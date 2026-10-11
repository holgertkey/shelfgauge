//! Scanner tests on temporary libraries.

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
        let registry = Registry::with_builtin_formats();
        let progress = ScanProgress::default();
        let cancel = AtomicBool::new(cancel);
        let scanner = Scanner {
            library: &self.library,
            exclude,
            registry: &registry,
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

    assert_eq!(lib.scan(), summary(4, 0, 0, 0, 0, 4));
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
    lib.write("broken.fb2", b"<FictionBook><body>");
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
