//! Genre codes.

use rusqlite::Connection;

use super::StorageError;

/// The id of the genre `code`, creating it if needed.
pub(crate) fn upsert_genre(conn: &Connection, code: &str) -> Result<i64, StorageError> {
    let id = conn.query_row(
        "INSERT INTO genres (code) VALUES (?1)
         ON CONFLICT (code) DO UPDATE SET code = excluded.code
         RETURNING id",
        [code],
        |row| row.get(0),
    )?;
    Ok(id)
}

/// The genre codes of a book.
pub(crate) fn book_genres(conn: &Connection, book_id: i64) -> Result<Vec<String>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT g.code FROM book_genres bg JOIN genres g ON g.id = bg.genre_id
         WHERE bg.book_id = ?1 ORDER BY g.code",
    )?;
    let rows = stmt.query_map([book_id], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests::memory_db;

    #[test]
    fn genres_are_deduplicated() {
        let conn = memory_db();
        let a = upsert_genre(&conn, "sf").unwrap();
        assert_eq!(upsert_genre(&conn, "sf").unwrap(), a);
        assert_ne!(upsert_genre(&conn, "prose").unwrap(), a);
    }
}
