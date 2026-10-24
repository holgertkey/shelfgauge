//! Book files (`.sgbook`): export on the book card, the container layout, `sg.json`,
//! opening a file with the account's keys (and refusing another account's without
//! decrypting anything), and `file` items on the shelf.

mod common;

use axum::http::{StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::hub::{Lib, TestHub};
use shelfgauge::hub::AccountId;
use shelfgauge::hub_client::book_file::{self, FileError, FileKeys, SgJson};
use shelfgauge::hub_client::keys::{self, AccountKey};
use shelfgauge::hub_client::store::{HubAccount, ShelfKind, ShelfState};
use shelfgauge::publication::format::container::{self, ContainerError, MIME, Name};
use shelfgauge::publication::format::{BlobPath, Manifest, PartBlob};

fn file_keys(account: &HubAccount) -> FileKeys<'_> {
    FileKeys {
        account: account.account,
        key: &account.key,
        key_id: account.key_id,
    }
}

fn sg_json(file: &[u8]) -> SgJson {
    let entries = container::read(file).unwrap();
    serde_json::from_slice(entries[1].1).unwrap()
}

/// Every blob of an opened file decrypts; the manifest.
fn open_all(opened: &book_file::Opened<'_>) -> Manifest {
    let mut manifest = None;
    for (path, blob) in &opened.blobs {
        let plain =
            shelfgauge::publication::open(&opened.id, &opened.book_key, *path, blob, 1 << 26)
                .unwrap();
        match path {
            BlobPath::Manifest => manifest = Some(Manifest::decode(&plain).unwrap()),
            BlobPath::Part(_) => {
                PartBlob::decode(&plain).unwrap();
            }
            BlobPath::Image(_) => {}
        }
    }
    manifest.expect("a manifest")
}

#[tokio::test]
async fn a_book_file_is_made_without_the_hub_and_opens_with_the_account_key() {
    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    let mark = lib.bookmark(book, "b2").await;
    let position = lib
        .json(
            &format!("/book/{book}/position"),
            serde_json::json!({ "block": "b3" }),
        )
        .await;
    assert_eq!(position.status, StatusCode::NO_CONTENT);
    let card = lib.get(&format!("/book/{book}")).await;
    assert!(card.body.contains("Download book file"), "{}", card.body);
    // A file needs no hub.
    hub.stop().await;

    let reply = lib.export(book).await;
    assert_eq!(reply.headers[header::CONTENT_TYPE], MIME);
    assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
    let disposition = reply.headers[header::CONTENT_DISPOSITION].to_str().unwrap();
    assert!(
        disposition.starts_with("attachment; filename=\"") && disposition.contains(".sgbook"),
        "{disposition}"
    );
    let file = reply.bytes;

    let account = lib.account().unwrap();
    let opened = book_file::open(&file, file_keys(&account)).unwrap();
    let manifest = open_all(&opened);
    let entry = &lib.shelf()[0];
    assert_eq!(manifest.content_key, entry.content_key);
    assert_eq!(
        opened.blobs.len(),
        1 + manifest.parts.len() + manifest.images.len()
    );

    // On the shelf as a file: the newest file's publication and key.
    assert_eq!(entry.kind, ShelfKind::File);
    assert_eq!(entry.state, ShelfState::Published);
    assert!(entry.own);
    assert_eq!(entry.publication, Some(opened.id));
    assert_eq!(entry.book_key.as_ref(), Some(&opened.book_key));
    assert_eq!(
        lib.outbox(),
        3,
        "shelf item, bookmark and position wait for a push"
    );

    // `sg.json`: what the device checks before decrypting, and the snapshot.
    let sg = &opened.sg;
    assert_eq!(sg.format, 1);
    assert_eq!(sg.mime, MIME);
    assert_eq!(sg.format_version, manifest.format_version);
    assert_eq!(sg.publication, opened.id.to_string());
    assert_eq!(sg.account, account.account.to_string());
    assert_eq!(sg.key_id, 1);
    let kinds: Vec<&str> = sg.snapshot.iter().map(|r| r.kind.as_str()).collect();
    assert_eq!(kinds, ["shelf_item", "annotation", "position"]);
    for record in &sg.snapshot {
        let data = URL_SAFE_NO_PAD.decode(&record.data).unwrap();
        let plain = keys::open_record(&account.key, &record.kind, &record.id, &data).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&plain).unwrap();
        match record.kind.as_str() {
            "shelf_item" => {
                assert_eq!(record.id, keys::shelf_id(&account.key, &entry.content_key));
                assert_eq!(value["kind"], "file");
                assert_eq!(value["publication"], opened.id.to_string());
                assert_eq!(
                    value["book_key"],
                    URL_SAFE_NO_PAD.encode(opened.book_key.as_bytes())
                );
            }
            "annotation" => assert_eq!(record.id, mark),
            _ => {
                assert_eq!(
                    record.id,
                    keys::position_id(&account.key, &entry.content_key)
                );
                assert_eq!(value["block"], 3);
            }
        }
    }

    let card = lib.get(&format!("/book/{book}")).await;
    assert!(
        card.body.contains("On your shelf as a book file"),
        "{}",
        card.body
    );
    assert!(!card.body.contains("Book link and QR code"));
}

#[tokio::test]
async fn the_container_has_the_fixed_layout() {
    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    let file = lib.export(book).await.bytes;

    // A local header of `mimetype` without extra fields: the type at byte 38, as EPUB.
    assert_eq!(&file[..4], b"PK\x03\x04");
    assert_eq!(&file[8..10], &[0, 0], "stored");
    assert_eq!(
        &file[26..30],
        &[8, 0, 0, 0],
        "name length 8, no extra field"
    );
    assert_eq!(&file[30..38], b"mimetype");
    assert_eq!(&file[38..38 + MIME.len()], MIME.as_bytes());

    let entries = container::read(&file).unwrap();
    let names: Vec<String> = entries.iter().map(|(n, _)| n.to_string()).collect();
    assert_eq!(names[..3], ["mimetype", "sg.json", "manifest"]);
    assert!(
        entries[3..]
            .iter()
            .all(|(n, _)| matches!(n, Name::Blob(BlobPath::Part(_) | BlobPath::Image(_))))
    );
    // Any other zip reader agrees.
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&file)).unwrap();
    assert_eq!(zip.len(), entries.len());
    for (i, name) in names.iter().enumerate() {
        let entry = zip.by_index(i).unwrap();
        assert_eq!(entry.compression(), zip::CompressionMethod::Stored);
        assert_eq!(entry.name().unwrap(), name.as_str());
    }
}

#[tokio::test]
async fn another_accounts_file_is_refused_before_anything_is_decrypted() {
    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    let file = lib.export(book).await.bytes;
    let account = lib.account().unwrap();

    // Another account, and the sealed key spoiled: still "another account", so the
    // key was never tried.
    let mut sg = sg_json(&file);
    sg.book_key = "AAAA".into();
    let spoiled = rewrite(&file, &sg);
    let other = AccountId::new([0x99; 16]);
    let key = AccountKey::new([5; 32]);
    let keys = FileKeys {
        account: other,
        key: &key,
        key_id: 1,
    };
    assert_eq!(
        book_file::open(&spoiled, keys).unwrap_err(),
        FileError::OtherAccount
    );
    // With the right account it is a damaged file, not another account's.
    assert!(matches!(
        book_file::open(&spoiled, file_keys(&account)).unwrap_err(),
        FileError::SgJson(_) | FileError::Decrypt
    ));

    // Another key id: refused before decrypting.
    let mut sg = sg_json(&file);
    sg.key_id = 2;
    sg.book_key = "AAAA".into();
    assert_eq!(
        book_file::open(&rewrite(&file, &sg), file_keys(&account)).unwrap_err(),
        FileError::UnknownKey(2)
    );

    // The sealed key moved to another publication id does not open.
    let mut sg = sg_json(&file);
    sg.publication = "ab".repeat(16);
    assert_eq!(
        book_file::open(&rewrite(&file, &sg), file_keys(&account)).unwrap_err(),
        FileError::Decrypt
    );

    // A newer container or blob format.
    for (format, version) in [(2, 1), (1, 99)] {
        let mut sg = sg_json(&file);
        sg.format = format;
        sg.format_version = version;
        assert_eq!(
            book_file::open(&rewrite(&file, &sg), file_keys(&account)).unwrap_err(),
            FileError::Unsupported
        );
    }

    // An EPUB (or anything else) is not a book file.
    let epub = common::epub3();
    assert!(matches!(
        book_file::open(&epub, file_keys(&account)).unwrap_err(),
        FileError::Container(ContainerError::NotABookFile(_))
    ));
}

/// The file with another `sg.json` (the blobs as they are).
fn rewrite(file: &[u8], sg: &SgJson) -> Vec<u8> {
    let entries = container::read(file).unwrap();
    let blobs: Vec<(BlobPath, Vec<u8>)> = entries[2..]
        .iter()
        .map(|(name, data)| match name {
            Name::Blob(path) => (*path, data.to_vec()),
            _ => unreachable!(),
        })
        .collect();
    book_file::write(sg, &blobs)
}

#[tokio::test]
async fn an_uploaded_book_keeps_its_shelf_item_and_each_file_is_new() {
    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    assert_eq!(lib.publish(book).await, "published");
    let uploaded = lib.shelf()[0].clone();

    let account = lib.account().unwrap();
    let first = lib.export(book).await.bytes;
    let first = book_file::open(&first, file_keys(&account)).unwrap();
    let after = lib.shelf()[0].clone();
    assert_eq!(after.kind, ShelfKind::Upload);
    assert_eq!(after.publication, uploaded.publication);
    assert_ne!(Some(first.id), uploaded.publication, "a new publication");
    let card = lib.get(&format!("/book/{book}")).await;
    assert!(card.body.contains("Book link and QR code"));
    assert!(card.body.contains("Download book file"));

    // The snapshot's shelf item is the upload's.
    let item = first
        .sg
        .snapshot
        .iter()
        .find(|r| r.kind == "shelf_item")
        .unwrap();
    let data = URL_SAFE_NO_PAD.decode(&item.data).unwrap();
    let plain = keys::open_record(&account.key, "shelf_item", &item.id, &data).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&plain).unwrap();
    assert_eq!(value["kind"], "upload");
    assert_eq!(
        value["publication"],
        uploaded.publication.unwrap().to_string()
    );
}

#[tokio::test]
async fn a_file_book_points_to_the_newest_file_and_can_be_uploaded() {
    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    let account = lib.account().unwrap();
    let first = lib.export(book).await.bytes;
    let second = lib.export(book).await.bytes;
    let first = book_file::open(&first, file_keys(&account)).unwrap();
    let second = book_file::open(&second, file_keys(&account)).unwrap();
    assert_ne!(first.id, second.id);
    assert_ne!(first.book_key, second.book_key);
    let entry = lib.shelf()[0].clone();
    assert_eq!(entry.publication, Some(second.id));
    // The older file still opens on its own.
    open_all(&first);

    // "Open on phone" uploads a new publication.
    assert_eq!(lib.publish(book).await, "published");
    let entry = lib.shelf()[0].clone();
    assert_eq!(entry.kind, ShelfKind::Upload);
    assert_eq!(entry.state, ShelfState::Published);
    assert_ne!(entry.publication, Some(second.id));
}

#[tokio::test]
async fn file_books_neither_expire_nor_are_deleted_on_the_hub() {
    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    lib.export(book).await;
    lib.sync().await;
    assert_eq!(lib.outbox(), 0);
    // The hub has no such publication: that is no expiry.
    lib.get("/profile/hub").await;
    lib.sync().await;
    let entry = lib.shelf()[0].clone();
    assert_eq!(entry.state, ShelfState::Published);
    assert_eq!(
        entry.kind,
        ShelfKind::File,
        "the echo of the shelf item keeps it"
    );

    // Removing it needs no delete on the hub (there is nothing to delete).
    assert_eq!(lib.shelf_action(book, "remove").await, "removed");
    assert!(lib.shelf().is_empty());
    let token = lib.account().unwrap().token;
    let records = hub.records(token.as_str()).await;
    assert!(records.iter().all(|(_, _, data)| data.is_none()));

    // Exporting again puts it back on the shelf.
    lib.export(book).await;
    assert_eq!(lib.shelf()[0].kind, ShelfKind::File);
}

#[tokio::test]
async fn drm_books_are_never_exported() {
    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("locked.epub");
    assert_eq!(lib.shelf_action(book, "export").await, "protected");
    assert!(lib.shelf().is_empty());
    let card = lib.get(&format!("/book/{book}")).await;
    assert!(!card.body.contains("Download book file"));
}

#[tokio::test]
async fn without_a_hub_account_there_is_no_export() {
    let lib = Lib::new("", "Home");
    let book = lib.book("book.txt");
    let reply = lib
        .post(&format!("/book/{book}/hub"), &[("action", "export")])
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_book_put_back_on_the_shelf_from_its_file_stays_this_librarys() {
    use axum::http::Method;

    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    let account = lib.account().unwrap();
    let file = lib.export(book).await.bytes;
    let opened = book_file::open(&file, file_keys(&account)).unwrap();
    let content_key = lib.shelf()[0].content_key.clone();
    assert_eq!(lib.shelf_action(book, "remove").await, "removed");

    // A device opens the file again and pushes the shelf item it makes (bookfile.js).
    let device_puts_back = |instance: &str, publication: String, updated_at: i64| {
        let item = serde_json::json!({
            "authors": "",
            "book_key": URL_SAFE_NO_PAD.encode(opened.book_key.as_bytes()),
            "content_key": URL_SAFE_NO_PAD.encode(&content_key),
            "format_version": 1,
            "instance": instance,
            "kind": "file",
            "publication": publication,
            "title": "Book",
            "updated_at": updated_at,
        });
        let id = keys::shelf_id(&account.key, &content_key);
        let data = keys::seal_record(
            &account.key,
            "shelf_item",
            &id,
            &serde_json::to_vec(&item).unwrap(),
        )
        .unwrap();
        serde_json::json!({ "records": [{ "kind": "shelf_item", "id": id, "data": URL_SAFE_NO_PAD.encode(data) }] })
    };
    let now = shelfgauge::annotations::now();
    let (status, _) = hub
        .api(
            Method::POST,
            "sync",
            Some(account.token.as_str()),
            Some(device_puts_back(
                "Home library",
                opened.id.to_string(),
                now + 10,
            )),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    lib.sync().await;
    let entry = lib.shelf()[0].clone();
    assert_eq!(entry.kind, ShelfKind::File);
    assert!(entry.own, "the file names this library");
    let card = lib.get(&format!("/book/{book}")).await;
    assert!(!card.body.contains("from Home library"), "{}", card.body);

    // A file of another library (same book, its own publication) is that library's.
    let (status, _) = hub
        .api(
            Method::POST,
            "sync",
            Some(account.token.as_str()),
            Some(device_puts_back("Office", "ab".repeat(16), now + 20)),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    lib.sync().await;
    assert!(!lib.shelf()[0].own);
}
