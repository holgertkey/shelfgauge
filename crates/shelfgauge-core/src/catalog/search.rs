//! Searching and listing a catalog on the device, with the library's folding.

use std::cmp::Ordering;

use super::{BookId, Entry};
use crate::fold::{normalize_key, words};
use crate::format::genres::genre_label;

/// The most words of a query that are used (as the library's search).
const MAX_TERMS: usize = 16;

/// A book of the catalog with its folded words.
#[derive(Debug, Clone)]
struct Book {
    entry: Entry,
    /// [`normalize_key`] of the sort title.
    sort_key: String,
    /// The title's words.
    title_words: Vec<String>,
    /// The words of the authors, series and genre names.
    other_words: Vec<String>,
    /// [`normalize_key`] of each author.
    author_keys: Vec<String>,
    /// [`normalize_key`] of each series, in the entry's order.
    series_keys: Vec<String>,
}

impl Book {
    fn new(entry: Entry) -> Self {
        let mut other_words = Vec::new();
        for name in entry
            .author
            .iter()
            .chain(entry.series.iter().map(|s| &s.name))
        {
            other_words.extend(words(name));
        }
        for code in &entry.subject {
            other_words.extend(words(&genre_label(code)));
        }
        other_words.sort();
        other_words.dedup();
        Self {
            sort_key: normalize_key(if entry.sort_as.is_empty() {
                &entry.title
            } else {
                &entry.sort_as
            }),
            title_words: words(&entry.title),
            other_words,
            author_keys: entry.author.iter().map(|a| normalize_key(a)).collect(),
            series_keys: entry
                .series
                .iter()
                .map(|s| normalize_key(&s.name))
                .collect(),
            entry,
        }
    }

    /// The number of query words found in the title, or `None` if a word is found
    /// nowhere.
    fn matches(&self, terms: &[String]) -> Option<usize> {
        let mut in_title = 0;
        for term in terms {
            let prefix = |w: &String| w.starts_with(term.as_str());
            if self.title_words.iter().any(prefix) {
                in_title += 1;
            } else if !self.other_words.iter().any(prefix) {
                return None;
            }
        }
        Some(in_title)
    }
}

/// A name in a list with the number of its books.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct NameCount {
    /// The [`normalize_key`] of the name (what the list's filters take).
    pub key: String,
    /// The name as shown.
    pub name: String,
    /// The number of books.
    pub count: usize,
}

/// A genre in the list with the number of its books.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GenreCount {
    /// The code as stored in the entries.
    pub code: String,
    /// The readable name (the code when it has none).
    pub name: String,
    /// The number of books.
    pub count: usize,
}

/// A catalog held in memory: chunks of entries, searched and listed as the library
/// does on the computer. Each book is in one chunk (its [`chunk_of`]); entries are not
/// de-duplicated here.
///
/// [`chunk_of`]: super::chunk_of
///
/// # Examples
///
/// ```
/// use shelfgauge_core::catalog::{BookId, Catalog, Entry};
///
/// let entry = Entry {
///     id: BookId::new([1; 16]),
///     title: "Пикник на обочине".into(),
///     sort_as: "пикник на обочине".into(),
///     author: vec!["Аркадий Стругацкий".into(), "Борис Стругацкий".into()],
///     series: vec![],
///     subject: vec!["sf_social".into()],
///     language: Some("ru".into()),
///     published: Some(1972),
///     format: 1,
///     size: 300_000,
///     description: None,
///     added: 0,
///     publication: None,
///     cover: None,
/// };
/// let mut catalog = Catalog::new();
/// catalog.set_chunk(0, vec![entry]);
/// assert_eq!(catalog.search("пикн стругацк").len(), 1);
/// assert_eq!(catalog.search("social").len(), 1);
/// assert!(catalog.search("лем").is_empty());
/// assert_eq!(catalog.authors()[0].name, "Аркадий Стругацкий");
/// ```
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// By chunk number.
    chunks: Vec<Vec<Book>>,
}

impl Catalog {
    /// An empty catalog.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the entries of chunk `number`, replacing what it held; a number at or
    /// above [`MAX_CHUNKS`] is ignored.
    ///
    /// [`MAX_CHUNKS`]: super::MAX_CHUNKS
    pub fn set_chunk(&mut self, number: u32, entries: Vec<Entry>) {
        if number >= super::MAX_CHUNKS {
            return;
        }
        let at = number as usize;
        if self.chunks.len() <= at {
            self.chunks.resize_with(at + 1, Vec::new);
        }
        self.chunks[at] = entries.into_iter().map(Book::new).collect();
    }

    /// Drops the chunks numbered `count` and above (after the chunk count shrank).
    pub fn truncate(&mut self, count: u32) {
        self.chunks.truncate(count as usize);
    }

    /// Drops every chunk.
    pub fn clear(&mut self) {
        self.chunks.clear();
    }

    /// The number of books.
    pub fn len(&self) -> usize {
        self.books().count()
    }

    /// Whether the catalog has no books.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every book, in chunk order.
    fn books(&self) -> impl Iterator<Item = &Book> {
        self.chunks.iter().flatten()
    }

    /// The book with this id.
    pub fn get(&self, id: &BookId) -> Option<&Entry> {
        self.books().find(|b| b.entry.id == *id).map(|b| &b.entry)
    }

    /// Books matching `query`: every word must begin a word of the title, an author,
    /// a series or a genre name (folded as the library folds). Books with more words
    /// in the title come first, then by sort title. An empty query finds nothing.
    pub fn search(&self, query: &str) -> Vec<&Entry> {
        let terms: Vec<String> = words(query).into_iter().take(MAX_TERMS).collect();
        if terms.is_empty() {
            return Vec::new();
        }
        // One list per number of words in the title, each by title.
        let mut by_score: Vec<Vec<&Book>> = vec![Vec::new(); terms.len() + 1];
        for book in self.books() {
            if let Some(in_title) = book.matches(&terms) {
                by_score[in_title].push(book);
            }
        }
        by_score
            .into_iter()
            .rev()
            .flat_map(|books| sorted(books, &by_title))
            .collect()
    }

    /// Every book by sort title.
    pub fn all(&self) -> Vec<&Entry> {
        sorted(self.books().collect(), &by_title)
    }

    /// The books most recently added first, at most `limit`.
    pub fn recent(&self, limit: usize) -> Vec<&Entry> {
        let mut books = sorted(self.books().collect(), &|a, b| {
            b.entry
                .added
                .cmp(&a.entry.added)
                .then_with(|| by_title(a, b))
        });
        books.truncate(limit);
        books
    }

    /// The authors with their numbers of books, by [`normalize_key`].
    pub fn authors(&self) -> Vec<NameCount> {
        names(self.books().flat_map(|b| b.entry.author.iter()))
    }

    /// The series with their numbers of books, by [`normalize_key`].
    pub fn series(&self) -> Vec<NameCount> {
        names(
            self.books()
                .flat_map(|b| b.entry.series.iter().map(|s| &s.name)),
        )
    }

    /// The genres with their numbers of books, by name.
    pub fn genres(&self) -> Vec<GenreCount> {
        // Every book's codes once, then counted in runs.
        let mut codes: Vec<&str> = Vec::new();
        for book in self.books() {
            let mut own: Vec<&str> = book
                .entry
                .subject
                .iter()
                .map(|c| c.trim())
                .filter(|c| !c.is_empty())
                .collect();
            own.sort_unstable();
            own.dedup();
            codes.extend(own);
        }
        codes.sort_unstable();
        let mut genres: Vec<(String, GenreCount)> = Vec::new();
        for run in codes.chunk_by(|a, b| a == b) {
            let name = genre_label(run[0]);
            genres.push((
                normalize_key(&name),
                GenreCount {
                    name,
                    code: run[0].to_owned(),
                    count: run.len(),
                },
            ));
        }
        genres.sort_by(|(a, x), (b, y)| a.cmp(b).then_with(|| x.code.cmp(&y.code)));
        genres.into_iter().map(|(_, g)| g).collect()
    }

    /// The books of the author with this key ([`NameCount::key`]), by sort title.
    pub fn by_author(&self, key: &str) -> Vec<&Entry> {
        let books = self
            .books()
            .filter(|b| b.author_keys.iter().any(|a| a == key))
            .collect();
        sorted(books, &by_title)
    }

    /// The books of the series with this key, by their position in it, then by sort
    /// title.
    pub fn by_series(&self, key: &str) -> Vec<&Entry> {
        fn position<'a>(b: &'a Book, key: &str) -> Option<(u64, &'a str)> {
            let at = b.series_keys.iter().position(|s| s == key)?;
            position_key(b.entry.series[at].position.as_deref()?)
        }
        let books = self
            .books()
            .filter(|b| b.series_keys.iter().any(|s| s == key))
            .collect();
        sorted(books, &|a, b| {
            match (position(a, key), position(b, key)) {
                (Some(x), Some(y)) => x.cmp(&y),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            }
            .then_with(|| by_title(a, b))
        })
    }

    /// The books of the genre with this code, by sort title.
    pub fn by_genre(&self, code: &str) -> Vec<&Entry> {
        let books = self
            .books()
            .filter(|b| b.entry.subject.iter().any(|c| c.trim() == code))
            .collect();
        sorted(books, &by_title)
    }
}

/// `books` in `order` (one comparator type, so the sort is compiled once: it is large
/// in the WebAssembly module).
fn sorted<'a>(
    mut books: Vec<&'a Book>,
    order: &dyn Fn(&Book, &Book) -> Ordering,
) -> Vec<&'a Entry> {
    books.sort_by(|a, b| order(a, b));
    books.into_iter().map(|b| &b.entry).collect()
}

/// A series position (`"3"`, `"2.5"`, `"2,5"`) as its whole part and its fraction's
/// digits without trailing zeros, which order as the numbers do; `None` for anything
/// else. (No float parsing: it would add much to the WebAssembly module.)
fn position_key(text: &str) -> Option<(u64, &str)> {
    let text = text.trim();
    let (whole, fraction) = match text.find(['.', ',']) {
        Some(at) => (&text[..at], &text[at + 1..]),
        None => (text, ""),
    };
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() || !digits(whole) || !digits(fraction) {
        return None;
    }
    Some((whole.parse().ok()?, fraction.trim_end_matches('0')))
}

fn by_title(a: &Book, b: &Book) -> Ordering {
    a.sort_key
        .cmp(&b.sort_key)
        .then_with(|| a.entry.id.cmp(&b.entry.id))
}

/// Names merged by [`normalize_key`], each shown as its most frequent spelling (the
/// smallest on a tie).
fn names<'a>(all: impl Iterator<Item = &'a String>) -> Vec<NameCount> {
    let mut keyed: Vec<(String, &str)> = all
        .map(|name| (normalize_key(name), name.trim()))
        .filter(|(key, _)| !key.is_empty())
        .collect();
    keyed.sort_unstable();
    keyed
        .chunk_by(|a, b| a.0 == b.0)
        .map(|run| {
            // Spellings are sorted within the run: the longest run of one wins, the
            // first (smallest) of equal ones.
            let name = run
                .chunk_by(|a, b| a.1 == b.1)
                .fold(None::<&[(String, &str)]>, |best, same| match best {
                    Some(best) if best.len() >= same.len() => Some(best),
                    _ => Some(same),
                })
                .map_or("", |same| same[0].1);
            NameCount {
                key: run[0].0.clone(),
                name: name.to_owned(),
                count: run.len(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::SeriesRef;
    use super::*;

    fn book(n: u8, title: &str, authors: &[&str]) -> Entry {
        Entry {
            id: BookId::new([n; 16]),
            title: title.into(),
            sort_as: String::new(),
            author: authors.iter().map(|a| (*a).to_owned()).collect(),
            series: vec![],
            subject: vec![],
            language: None,
            published: None,
            format: 1,
            size: 1,
            description: None,
            added: u64::from(n),
            publication: None,
            cover: None,
        }
    }

    #[test]
    fn search_folds_as_the_library_does() {
        let mut catalog = Catalog::new();
        catalog.set_chunk(
            0,
            vec![
                book(1, "Ёжик в тумане", &["Сергей Козлов"]),
                book(2, "Les Misérables", &["Victor Hugo"]),
                book(3, "Йога для всех", &[]),
                book(4, "Иога", &[]),
            ],
        );
        let titles = |q: &str| -> Vec<String> {
            catalog.search(q).iter().map(|e| e.title.clone()).collect()
        };
        assert_eq!(titles("ежик"), ["Ёжик в тумане"]);
        assert_eq!(titles("ЁЖИК КОЗЛ"), ["Ёжик в тумане"]);
        assert_eq!(titles("miserables hugo"), ["Les Misérables"]);
        assert_eq!(titles("MISÉR"), ["Les Misérables"]);
        assert_eq!(titles("йог"), ["Йога для всех"]);
        assert_eq!(titles("иог"), ["Иога"]);
        assert!(titles("тумане лес").is_empty());
        assert!(titles(" -- ").is_empty());
    }

    #[test]
    fn title_hits_rank_first() {
        let mut catalog = Catalog::new();
        catalog.set_chunk(
            0,
            vec![
                book(1, "Азбука", &["Толстой"]),
                book(2, "Толстой и Достоевский", &["Мережковский"]),
            ],
        );
        let titles: Vec<&str> = catalog
            .search("толстой")
            .iter()
            .map(|e| e.title.as_str())
            .collect();
        assert_eq!(titles, ["Толстой и Достоевский", "Азбука"]);
    }

    #[test]
    fn genres_are_searched_and_listed_by_name() {
        let mut catalog = Catalog::new();
        let mut a = book(1, "A", &[]);
        a.subject = vec!["sf_fantasy".into(), "sf_fantasy".into()];
        let mut b = book(2, "B", &[]);
        b.subject = vec!["Fiction / Classics".into(), "sf_fantasy".into()];
        catalog.set_chunk(0, vec![a, b]);
        assert_eq!(catalog.search("fantasy").len(), 2);
        assert_eq!(catalog.search("classics").len(), 1);
        let genres = catalog.genres();
        assert_eq!(
            genres
                .iter()
                .map(|g| (g.name.as_str(), g.count))
                .collect::<Vec<_>>(),
            [("Fantasy", 2), ("Fiction / Classics", 1)]
        );
        assert_eq!(catalog.by_genre("sf_fantasy").len(), 2);
    }

    #[test]
    fn authors_merge_by_key_and_series_sort_by_position() {
        let mut catalog = Catalog::new();
        let mut entries = vec![
            book(1, "Третья", &["Пётр Иванов"]),
            book(2, "Первая", &["петр  иванов"]),
            book(3, "Вторая", &["Пётр Иванов"]),
            book(4, "Без номера", &[]),
        ];
        for (entry, position) in entries.iter_mut().zip(["10", "1", "2,5", ""]) {
            entry.series = vec![SeriesRef {
                name: "Серия".into(),
                position: (!position.is_empty()).then(|| position.to_owned()),
            }];
        }
        catalog.set_chunk(0, entries);
        let authors = catalog.authors();
        assert_eq!(authors.len(), 1);
        assert_eq!(
            (
                authors[0].key.as_str(),
                authors[0].name.as_str(),
                authors[0].count
            ),
            ("петр иванов", "Пётр Иванов", 3)
        );
        assert_eq!(catalog.by_author("петр иванов").len(), 3);
        let series: Vec<&str> = catalog
            .by_series("серия")
            .iter()
            .map(|e| e.title.as_str())
            .collect();
        assert_eq!(series, ["Первая", "Вторая", "Третья", "Без номера"]);
        assert_eq!(catalog.series()[0].count, 4);
    }

    #[test]
    fn positions_order_as_numbers() {
        let mut keys: Vec<_> = ["10", "2.5", "2.25", "2", "02.50", "x", "1."]
            .into_iter()
            .map(|p| (position_key(p), p))
            .collect();
        keys.sort();
        let order: Vec<&str> = keys.iter().map(|(_, p)| *p).collect();
        assert_eq!(order, ["x", "1.", "2", "2.25", "02.50", "2.5", "10"]);
        assert_eq!(position_key(" 3 "), Some((3, "")));
        assert_eq!(position_key("-1"), None);
        assert_eq!(position_key(".5"), None);
    }

    #[test]
    fn chunks_replace_and_truncate() {
        let mut catalog = Catalog::new();
        catalog.set_chunk(0, vec![book(1, "A", &[]), book(2, "B", &[])]);
        catalog.set_chunk(1, vec![book(3, "C", &[])]);
        assert_eq!(catalog.len(), 3);
        assert_eq!(
            catalog
                .recent(2)
                .iter()
                .map(|e| e.title.as_str())
                .collect::<Vec<_>>(),
            ["C", "B"]
        );
        catalog.set_chunk(0, vec![book(2, "B", &[])]);
        assert_eq!(catalog.len(), 2);
        assert!(catalog.get(&BookId::new([1; 16])).is_none());
        catalog.set_chunk(super::super::MAX_CHUNKS, vec![book(9, "Z", &[])]);
        assert_eq!(catalog.len(), 2);
        catalog.truncate(1);
        assert_eq!(catalog.len(), 1);
        catalog.truncate(0);
        assert!(catalog.is_empty());
        catalog.set_chunk(0, vec![book(1, "A", &[])]);
        catalog.clear();
        assert!(catalog.all().is_empty());
    }
}
