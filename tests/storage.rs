//! Storage layer tests against a real database file.

use rusqlite::Connection;
use shelfgauge::container::ContainerKind;
use shelfgauge::format::model::{Person, Series};
use shelfgauge::storage::authors::{books_by_author, count_authors, list_authors};
use shelfgauge::storage::books::{
    BookRecord, BookStatus, EntryRecord, book_by_id, books_in_container, find_by_content_hash,
    find_by_entry_identity, find_live_at, mark_removed, relocate, upsert_book,
};
use shelfgauge::storage::containers::{ContainerRecord, upsert_container};
use shelfgauge::storage::folders::{FolderKind, FolderPath, folder_children};
use shelfgauge::storage::search::search;
use shelfgauge::storage::{open_read_pool, open_writer};

struct Db {
    _dir: tempfile::TempDir,
    conn: Connection,
}

fn db() -> Db {
    let dir = tempfile::tempdir().unwrap();
    let conn = open_writer(&dir.path().join("library.db")).unwrap();
    Db { _dir: dir, conn }
}

fn person(last: &str, first: &str) -> Person {
    Person {
        last_name: Some(last.to_owned()),
        first_name: Some(first.to_owned()),
        ..Person::default()
    }
}

fn container(conn: &Connection, path: &str) -> i64 {
    let kind = ContainerKind::of(path.as_ref());
    upsert_container(
        conn,
        &ContainerRecord {
            path: path.to_owned(),
            kind,
            size: 100,
            mtime_ns: 1,
            content_hash: (kind == ContainerKind::File).then(|| path.as_bytes().to_vec()),
        },
    )
    .unwrap()
}

fn entry(path: &str, crc32: u32) -> EntryRecord {
    EntryRecord {
        name: path.as_bytes().to_vec(),
        path: path.to_owned(),
        index: 0,
        size: 1000 + u64::from(crc32),
        crc32,
    }
}

fn book(
    container_id: i64,
    entry: Option<EntryRecord>,
    title: &str,
    authors: Vec<Person>,
) -> BookRecord {
    BookRecord {
        container_id,
        content_hash: entry
            .is_none()
            .then(|| format!("hash-{title}").into_bytes()),
        entry,
        format: "fb2".to_owned(),
        title: title.to_owned(),
        authors,
        series: Vec::new(),
        genres: vec!["prose".to_owned()],
        annotation: String::new(),
        language: Some("ru".to_owned()),
        cover_mime: None,
        status: BookStatus::Ok,
        error_message: None,
    }
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn titles(books: &[shelfgauge::storage::books::BookSummary]) -> Vec<&str> {
    books.iter().map(|b| b.title.as_str()).collect()
}

#[test]
fn upsert_is_idempotent() {
    let db = db();
    let c = container(&db.conn, "war.fb2");
    let mut record = book(c, None, "Война и мир", vec![person("Толстой", "Лев")]);
    record.series = vec![Series {
        name: "Classics".to_owned(),
        index: Some("1".to_owned()),
    }];
    let id = upsert_book(&db.conn, &record).unwrap();
    assert_eq!(upsert_book(&db.conn, &record).unwrap(), id);

    record.title = "Война и мир (2-е изд.)".to_owned();
    assert_eq!(upsert_book(&db.conn, &record).unwrap(), id);

    for (sql, expected) in [
        ("SELECT count(*) FROM books", 1),
        ("SELECT count(*) FROM book_authors", 1),
        ("SELECT count(*) FROM authors", 1),
        ("SELECT count(*) FROM book_series", 1),
        ("SELECT count(*) FROM book_genres", 1),
        ("SELECT count(*) FROM books_fts", 1),
    ] {
        assert_eq!(count(&db.conn, sql), expected, "{sql}");
    }
    let details = book_by_id(&db.conn, id).unwrap().unwrap();
    assert_eq!(details.title, "Война и мир (2-е изд.)");
    assert_eq!(details.authors[0].display_name, "Толстой Лев");
    assert_eq!(details.series[0].number.as_deref(), Some("1"));
    assert_eq!(details.genres, ["prose"]);
    assert_eq!(details.container_path, "war.fb2");
    assert_eq!(details.entry, None);
    assert_eq!(book_by_id(&db.conn, id + 1).unwrap(), None);
}

#[test]
fn zip_entries_are_separate_books_in_one_container() {
    let db = db();
    let zip = container(&db.conn, "lib/books.zip");
    let a = upsert_book(&db.conn, &book(zip, Some(entry("a.fb2", 1)), "A", vec![])).unwrap();
    let b = upsert_book(&db.conn, &book(zip, Some(entry("b.fb2", 2)), "B", vec![])).unwrap();
    assert_ne!(a, b);
    let located = books_in_container(&db.conn, zip).unwrap();
    assert_eq!(located.len(), 2);
    let details = book_by_id(&db.conn, b).unwrap().unwrap();
    assert_eq!(details.container_kind, ContainerKind::Zip);
    assert_eq!(details.entry.unwrap().crc32, 2);
}

#[test]
fn authors_are_deduplicated_by_normalized_name() {
    let db = db();
    let c1 = container(&db.conn, "1.fb2");
    let c2 = container(&db.conn, "2.fb2");
    let c3 = container(&db.conn, "3.fb2");
    upsert_book(
        &db.conn,
        &book(c1, None, "Ёлка", vec![person("Толстой", "Лев")]),
    )
    .unwrap();
    upsert_book(
        &db.conn,
        &book(c2, None, "Б", vec![person(" толстой ", "ЛЕВ")]),
    )
    .unwrap();
    upsert_book(
        &db.conn,
        &book(c3, None, "В", vec![person("Толстой", "Алексей")]),
    )
    .unwrap();
    assert_eq!(count(&db.conn, "SELECT count(*) FROM authors"), 2);
    let authors = list_authors(&db.conn, "", 10, 0).unwrap();
    let names: Vec<(&str, u64)> = authors
        .iter()
        .map(|a| (a.display_name.as_str(), a.book_count))
        .collect();
    assert_eq!(names, [("Толстой Алексей", 1), ("Толстой Лев", 2)]);
}

#[test]
fn author_list_prefix_paging_and_order() {
    let db = db();
    let names = [
        ("Толстой", "Лев"),
        ("Тургенев", "Иван"),
        ("Ёсида", "Ко"),
        ("Есенин", "Сергей"),
        ("Asimov", "Isaac"),
        ("Чехов", "Антон"),
    ];
    for (i, (last, first)) in names.iter().enumerate() {
        let c = container(&db.conn, &format!("{i}.fb2"));
        upsert_book(&db.conn, &book(c, None, "T", vec![person(last, first)])).unwrap();
    }
    let all: Vec<String> = list_authors(&db.conn, "", 100, 0)
        .unwrap()
        .into_iter()
        .map(|a| a.display_name)
        .collect();
    assert_eq!(
        all,
        [
            "Asimov Isaac",
            "Есенин Сергей",
            "Ёсида Ко",
            "Толстой Лев",
            "Тургенев Иван",
            "Чехов Антон"
        ],
        "ё sorts as е"
    );
    let page = |prefix: &str, limit, offset| -> Vec<String> {
        list_authors(&db.conn, prefix, limit, offset)
            .unwrap()
            .into_iter()
            .map(|a| a.display_name)
            .collect()
    };
    assert_eq!(page("т", 10, 0), ["Толстой Лев", "Тургенев Иван"]);
    assert_eq!(page("ТУР", 10, 0), ["Тургенев Иван"]);
    assert_eq!(page("ё", 10, 0), ["Есенин Сергей", "Ёсида Ко"]);
    assert_eq!(page("", 2, 2), ["Ёсида Ко", "Толстой Лев"]);
    assert!(page("", 10, 100).is_empty());
    assert!(page("%", 10, 0).is_empty(), "no LIKE wildcards");
    assert_eq!(count_authors(&db.conn, "").unwrap(), 6);
    assert_eq!(count_authors(&db.conn, "т").unwrap(), 2);
}

#[test]
fn books_by_author_are_grouped_by_series() {
    let db = db();
    let author = person("Иванов", "Иван");
    let specs: [(&str, Option<(&str, &str)>); 5] = [
        ("Отдельная книга", None),
        ("Вторая", Some(("Цикл", "2"))),
        ("Первая", Some(("Цикл", "1"))),
        ("Десятая", Some(("Цикл", "10"))),
        ("Альфа", Some(("Алфавит", "1.5"))),
    ];
    for (i, (title, series)) in specs.iter().enumerate() {
        let c = container(&db.conn, &format!("{i}.fb2"));
        let mut record = book(c, None, title, vec![author.clone()]);
        record.series = series
            .iter()
            .map(|(name, number)| Series {
                name: (*name).to_owned(),
                index: Some((*number).to_owned()),
            })
            .collect();
        upsert_book(&db.conn, &record).unwrap();
    }
    let author_id = list_authors(&db.conn, "", 1, 0).unwrap()[0].id;
    let books = books_by_author(&db.conn, author_id).unwrap();
    assert_eq!(
        titles(&books),
        ["Альфа", "Первая", "Вторая", "Десятая", "Отдельная книга"]
    );
    assert_eq!(
        books[2].series.as_ref().unwrap().number.as_deref(),
        Some("2")
    );
    assert_eq!(books[4].series, None);
}

#[test]
fn removed_books_free_their_location_and_disappear_from_lists() {
    let db = db();
    let c = container(&db.conn, "dir/book.fb2");
    let id = upsert_book(
        &db.conn,
        &book(c, None, "Маяк", vec![person("Сидоров", "Иван")]),
    )
    .unwrap();
    mark_removed(&db.conn, id).unwrap();

    assert_eq!(find_live_at(&db.conn, c, b"").unwrap(), None);
    assert!(list_authors(&db.conn, "", 10, 0).unwrap().is_empty());
    assert_eq!(count_authors(&db.conn, "").unwrap(), 0);
    assert!(search(&db.conn, "маяк", 10, 0).unwrap().is_empty());
    assert_eq!(
        folder_children(&db.conn, &FolderPath::Dir(String::new())).unwrap(),
        Default::default()
    );
    let details = book_by_id(&db.conn, id).unwrap().unwrap();
    assert_eq!(details.status, BookStatus::Removed);

    // A new book at the same location gets a new row.
    let new_id = upsert_book(&db.conn, &book(c, None, "Другая", vec![])).unwrap();
    assert_ne!(new_id, id);
}

#[test]
fn relocation_keeps_the_id_and_restores_search() {
    let db = db();
    let old = container(&db.conn, "old/book.fb2");
    let id = upsert_book(
        &db.conn,
        &book(old, None, "Маяк", vec![person("Сидоров", "Иван")]),
    )
    .unwrap();
    mark_removed(&db.conn, id).unwrap();

    let new = container(&db.conn, "new/книга.fb2");
    relocate(&db.conn, id, new, None).unwrap();
    let details = book_by_id(&db.conn, id).unwrap().unwrap();
    assert_eq!(details.status, BookStatus::Ok);
    assert_eq!(details.container_path, "new/книга.fb2");
    assert_eq!(search(&db.conn, "сидоров", 10, 0).unwrap()[0].id, id);

    let zip = container(&db.conn, "z.zip");
    relocate(&db.conn, id, zip, Some(&entry("in/book.fb2", 7))).unwrap();
    let details = book_by_id(&db.conn, id).unwrap().unwrap();
    assert_eq!(details.entry.unwrap().path, "in/book.fb2");
    assert_eq!(count(&db.conn, "SELECT count(*) FROM books_fts"), 1);
}

#[test]
fn matcher_queries() {
    let db = db();
    let file = container(&db.conn, "a.fb2");
    let zip = container(&db.conn, "b.zip");
    let f = upsert_book(&db.conn, &book(file, None, "F", vec![])).unwrap();
    let z = upsert_book(&db.conn, &book(zip, Some(entry("x.fb2", 42)), "Z", vec![])).unwrap();

    let by_hash = find_by_content_hash(&db.conn, b"hash-F").unwrap();
    assert_eq!(by_hash.iter().map(|b| b.id).collect::<Vec<_>>(), [f]);
    assert!(find_by_content_hash(&db.conn, b"nope").unwrap().is_empty());

    let by_identity = find_by_entry_identity(&db.conn, 1042, 42).unwrap();
    assert_eq!(by_identity.len(), 1);
    assert_eq!(by_identity[0].id, z);
    assert_eq!(by_identity[0].entry_name, b"x.fb2");
    assert!(
        find_by_entry_identity(&db.conn, 1042, 43)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn error_books_keep_their_location() {
    let db = db();
    let c = container(&db.conn, "broken.fb2");
    let record = BookRecord::failed(c, None, None, "fb2", "broken", "invalid XML: oops");
    let id = upsert_book(&db.conn, &record).unwrap();
    assert_eq!(find_live_at(&db.conn, c, b"").unwrap(), Some(id));
    let details = book_by_id(&db.conn, id).unwrap().unwrap();
    assert_eq!(details.status, BookStatus::Error);
    assert_eq!(details.error_message.as_deref(), Some("invalid XML: oops"));
    mark_removed(&db.conn, id).unwrap();
    relocate(&db.conn, id, c, None).unwrap();
    assert_eq!(
        book_by_id(&db.conn, id).unwrap().unwrap().status,
        BookStatus::Error
    );
}

#[test]
fn folder_tree() {
    let db = db();
    let books = [
        ("a/b/c.fb2", None, "C"),
        ("a/d.fb2", None, "D"),
        ("a/x.zip", Some("Маяк/1.fb2"), "One"),
        ("a/x.zip", Some("Маяк/Глубже/3.fb2"), "Three"),
        ("a/x.zip", Some("2.fb2"), "Two"),
        ("a0/z.fb2", None, "Z"),
        ("e_f%/g.fb2", None, "G"),
        ("a_b.fb2", None, "AB"),
        ("Б.fb2", None, "Б"),
    ];
    for (i, (path, inner, title)) in books.iter().enumerate() {
        let c = container(&db.conn, path);
        let e = inner.map(|p| entry(p, i as u32));
        upsert_book(&db.conn, &book(c, e, title, vec![])).unwrap();
    }
    let removed_c = container(&db.conn, "gone/only.fb2");
    let removed = upsert_book(&db.conn, &book(removed_c, None, "Gone", vec![])).unwrap();
    mark_removed(&db.conn, removed).unwrap();

    let list = |path: FolderPath| folder_children(&db.conn, &path).unwrap();
    let folder_names =
        |listing: &shelfgauge::storage::folders::FolderListing| -> Vec<(String, FolderKind, u64)> {
            listing
                .folders
                .iter()
                .map(|f| (f.name.clone(), f.kind, f.book_count))
                .collect()
        };

    let root = list(FolderPath::Dir(String::new()));
    assert_eq!(
        folder_names(&root),
        [
            ("a".to_owned(), FolderKind::Directory, 5),
            ("a0".to_owned(), FolderKind::Directory, 1),
            ("e_f%".to_owned(), FolderKind::Directory, 1),
        ]
    );
    assert_eq!(titles(&root.books), ["AB", "Б"]);

    let a = list(FolderPath::Dir("a".to_owned()));
    assert_eq!(
        folder_names(&a),
        [
            ("b".to_owned(), FolderKind::Directory, 1),
            ("x.zip".to_owned(), FolderKind::Archive, 3),
        ]
    );
    assert_eq!(titles(&a.books), ["D"]);
    assert_eq!(
        a.folders[1].path,
        FolderPath::Archive {
            archive: "a/x.zip".to_owned(),
            inner: String::new()
        }
    );

    let special = list(FolderPath::Dir("e_f%".to_owned()));
    assert_eq!(titles(&special.books), ["G"]);
    assert!(list(FolderPath::Dir("e".to_owned())).books.is_empty());

    let zip = list(FolderPath::Archive {
        archive: "a/x.zip".to_owned(),
        inner: String::new(),
    });
    assert_eq!(
        folder_names(&zip),
        [("Маяк".to_owned(), FolderKind::ArchiveDirectory, 2)]
    );
    assert_eq!(titles(&zip.books), ["Two"]);

    let inner = list(FolderPath::Archive {
        archive: "a/x.zip".to_owned(),
        inner: "Маяк".to_owned(),
    });
    assert_eq!(
        folder_names(&inner),
        [("Глубже".to_owned(), FolderKind::ArchiveDirectory, 1)]
    );
    assert_eq!(titles(&inner.books), ["One"]);
    assert_eq!(
        inner.folders[0].path,
        FolderPath::Archive {
            archive: "a/x.zip".to_owned(),
            inner: "Маяк/Глубже".to_owned()
        }
    );
}

#[test]
fn full_text_search() {
    let db = db();
    let specs = [
        ("Война и мир", person("Толстой", "Лев"), "Роман-эпопея."),
        ("Ёлка", person("Достоевский", "Фёдор"), "Святочный рассказ."),
        ("Мой дом", person("Иванов", "Иван"), ""),
        ("Мои книги", person("Петров", "Пётр"), ""),
        (
            "Café society",
            person("Smith", "John"),
            "A story about a café.",
        ),
    ];
    for (i, (title, author, annotation)) in specs.iter().enumerate() {
        let c = container(&db.conn, &format!("{i}.fb2"));
        let mut record = book(c, None, title, vec![author.clone()]);
        record.annotation = (*annotation).to_owned();
        if i == 0 {
            record.series = vec![Series {
                name: "Русская классика".to_owned(),
                index: None,
            }];
        }
        upsert_book(&db.conn, &record).unwrap();
    }
    let found = |q: &str| -> Vec<String> {
        search(&db.conn, q, 10, 0)
            .unwrap()
            .into_iter()
            .map(|b| b.title)
            .collect()
    };
    assert_eq!(found("война"), ["Война и мир"]);
    assert_eq!(found("ВОЙН"), ["Война и мир"], "prefix and case");
    assert_eq!(
        found("толстой мир"),
        ["Война и мир"],
        "all terms, across columns"
    );
    assert_eq!(found("классика"), ["Война и мир"], "series");
    assert_eq!(found("эпопея"), ["Война и мир"], "annotation");
    assert_eq!(found("елка"), ["Ёлка"], "е finds ё");
    assert_eq!(found("ёлка"), ["Ёлка"]);
    assert_eq!(found("федор"), ["Ёлка"], "ё in author names");
    assert_eq!(found("мой"), ["Мой дом"], "й is not и");
    assert_eq!(found("мои"), ["Мои книги"]);
    assert_eq!(found("cafe"), ["Café society"], "Latin diacritics fold");
    assert!(found("толстой ёлка").is_empty());
    for hostile in [
        "",
        "   ",
        "\"",
        "\"\"",
        "*",
        "-",
        "a:b",
        "^x",
        "(",
        ")",
        "AND",
        "OR",
        "NOT",
        "NEAR",
        "NEAR(a b)",
        "война\"",
        "'; DROP TABLE books; --",
        "\u{0}",
    ] {
        let result = search(&db.conn, hostile, 10, 0);
        assert!(result.is_ok(), "{hostile:?}: {result:?}");
    }
    assert_eq!(found("война\""), ["Война и мир"]);
    assert_eq!(
        search(&db.conn, "м", 10, 0).unwrap().len(),
        3,
        "one-letter prefix"
    );
    assert_eq!(search(&db.conn, "м", 1, 0).unwrap().len(), 1, "limit");
    assert_eq!(search(&db.conn, "м", 10, 2).unwrap().len(), 1, "offset");
}

#[test]
fn read_pool_queries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    let writer = open_writer(&path).unwrap();
    let c = container(&writer, "x.fb2");
    upsert_book(
        &writer,
        &book(c, None, "Маяк", vec![person("Сидоров", "Иван")]),
    )
    .unwrap();
    let pool = open_read_pool(&path, 4).unwrap();
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let pool = pool.clone();
            std::thread::spawn(move || {
                let conn = pool.get().unwrap();
                search(&conn, "маяк", 10, 0).unwrap().len()
            })
        })
        .collect();
    for handle in handles {
        assert_eq!(handle.join().unwrap(), 1);
    }
}

#[test]
fn record_from_parsed_metadata() {
    use shelfgauge::format::model::{Annotation, Block, BookMetadata, Image};
    let metadata = BookMetadata {
        title: "  ".to_owned(),
        annotation: Annotation::new(vec![Block::paragraph("One."), Block::paragraph("Two.")]),
        cover: Some(Image {
            mime: "image/png".to_owned(),
            data: vec![1],
        }),
        ..BookMetadata::default()
    };
    let record = BookRecord::parsed(1, None, None, "fb2", &metadata, "file-name");
    assert_eq!(record.title, "file-name");
    assert_eq!(record.annotation, "One.\nTwo.");
    assert_eq!(record.cover_mime.as_deref(), Some("image/png"));
    assert_eq!(record.status, BookStatus::Ok);
}
