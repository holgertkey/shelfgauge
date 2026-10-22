//! The one place where URLs are built: every route and every generated link goes
//! through [`Urls`], so `server.base_path` is applied consistently.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use super::assets;
use crate::storage::folders::FolderPath;
use crate::storage::stats::BookFilter;

/// Characters percent-encoded in query values: everything except unreserved characters
/// and `/` (so `+`, `&`, `#`, `%`, spaces and non-ASCII text are always encoded).
const QUERY_VALUE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'/');

/// Characters percent-encoded in a path segment: everything except unreserved
/// characters (so `/`, `?`, `#` and `%` are encoded too).
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Builds URLs under the configured `server.base_path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Urls {
    /// The normalized base path without a trailing slash; empty for `/`.
    prefix: String,
}

impl Urls {
    /// Creates the helper for a base path normalized by
    /// [`Config::validate`](crate::config::Config::validate) (`/` or `/a/b`).
    pub fn new(base_path: &str) -> Self {
        Self {
            prefix: base_path.trim_end_matches('/').to_owned(),
        }
    }

    /// The URL of an absolute application path such as `/` or `/book/12`.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::web::Urls;
    ///
    /// assert_eq!(Urls::new("/").path("/"), "/");
    /// assert_eq!(Urls::new("/").path("/scan"), "/scan");
    /// assert_eq!(Urls::new("/books").path("/"), "/books/");
    /// assert_eq!(Urls::new("/books").path("/scan"), "/books/scan");
    /// ```
    pub fn path(&self, path: &str) -> String {
        debug_assert!(path.starts_with('/'), "{path:?} must start with `/`");
        format!("{}{path}", self.prefix)
    }

    /// The URL of the application root (the library page).
    pub fn home(&self) -> String {
        self.path("/")
    }

    /// The base path without a trailing slash, or `None` when it is `/`.
    pub fn prefix(&self) -> Option<&str> {
        (!self.prefix.is_empty()).then_some(self.prefix.as_str())
    }

    /// The URL of an embedded asset, versioned by its content hash so it can be cached
    /// for good.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::web::Urls;
    ///
    /// let url = Urls::new("/books").asset("style.css");
    /// assert!(url.starts_with("/books/assets/style.css?v="), "{url}");
    /// ```
    pub fn asset(&self, name: &str) -> String {
        let url = self.path(&format!("/assets/{name}"));
        match assets::version(name) {
            Some(version) => format!("{url}?v={version}"),
            None => {
                debug_assert!(false, "unknown asset {name:?}");
                url
            }
        }
    }

    /// The author index, filtered by a name `prefix`, at 1-based `page`.
    pub fn authors(&self, prefix: &str, page: u64) -> String {
        self.with_query("/authors", &[("q", prefix)], page)
    }

    /// An author's page.
    pub fn author(&self, id: i64) -> String {
        self.path(&format!("/author/{id}"))
    }

    /// The root folder's page.
    pub fn folders(&self) -> String {
        self.folder(&FolderPath::Dir(String::new()), 1)
    }

    /// A folder's page at 1-based `page` of its books.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::storage::folders::FolderPath;
    /// use shelfgauge::web::Urls;
    ///
    /// let urls = Urls::new("/");
    /// assert_eq!(urls.folder(&FolderPath::Dir(String::new()), 1), "/folder");
    /// assert_eq!(urls.folder(&FolderPath::Dir("a b/c".into()), 2), "/folder?path=a%20b/c&page=2");
    /// let zip = FolderPath::Archive { archive: "x.zip".into(), inner: "d".into() };
    /// assert_eq!(urls.folder(&zip, 1), "/folder?zip=x.zip&in=d");
    /// ```
    pub fn folder(&self, folder: &FolderPath, page: u64) -> String {
        self.with_query("/folder", &folder_params(folder), page)
    }

    /// A book's card.
    pub fn book(&self, id: i64) -> String {
        self.path(&format!("/book/{id}"))
    }

    /// Reading mode: 1-based `part` of a book.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::web::Urls;
    ///
    /// assert_eq!(Urls::new("/").read(3, 1), "/book/3/read");
    /// assert_eq!(Urls::new("/").read(3, 2), "/book/3/read?part=2");
    /// ```
    pub fn read(&self, id: i64, part: usize) -> String {
        let url = self.path(&format!("/book/{id}/read"));
        if part > 1 {
            format!("{url}?part={part}")
        } else {
            url
        }
    }

    /// Reading mode at a block: the server redirects to the part that contains it.
    pub fn read_at(&self, id: i64, block: crate::format::model::BlockId) -> String {
        self.path(&format!("/book/{id}/read?at={block}"))
    }

    /// Reading mode at a bookmark or mark: like [`read_at`](Self::read_at), with
    /// `from=mark` so that this page view does not move the reading position.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::format::model::BlockId;
    /// use shelfgauge::web::Urls;
    ///
    /// let b = BlockId::new(5).unwrap();
    /// assert_eq!(Urls::new("/").read_mark(3, b), "/book/3/read?at=b5&from=mark");
    /// ```
    pub fn read_mark(&self, id: i64, block: crate::format::model::BlockId) -> String {
        self.path(&format!("/book/{id}/read?at={block}&from=mark"))
    }

    /// Reading mode: 1-based `part`, opened from a bookmark or mark (`from=mark`).
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::web::Urls;
    ///
    /// assert_eq!(Urls::new("/").read_from_mark(3, 1), "/book/3/read?from=mark");
    /// assert_eq!(Urls::new("/").read_from_mark(3, 2), "/book/3/read?part=2&from=mark");
    /// ```
    pub fn read_from_mark(&self, id: i64, part: usize) -> String {
        let url = self.read(id, part);
        let separator = if url.contains('?') { '&' } else { '?' };
        format!("{url}{separator}from=mark")
    }

    /// Search within a book: the results for `query` as an HTML fragment.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::web::Urls;
    ///
    /// assert_eq!(Urls::new("/").find(3, "a b"), "/book/3/find?q=a%20b");
    /// ```
    pub fn find(&self, id: i64, query: &str) -> String {
        self.with_query(&format!("/book/{id}/find"), &[("q", query)], 1)
    }

    /// Reading mode: 1-based `part` with the matches of `query` highlighted and listed
    /// in the Search tab; `hit` is the 1-based number of the result followed.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::web::Urls;
    ///
    /// let urls = Urls::new("/");
    /// assert_eq!(urls.read_find(3, 1, "ёж", None), "/book/3/read?find=%D1%91%D0%B6");
    /// assert_eq!(urls.read_find(3, 2, "x&y", Some(7)), "/book/3/read?part=2&find=x%26y&hit=7");
    /// ```
    pub fn read_find(&self, id: i64, part: usize, query: &str, hit: Option<usize>) -> String {
        let part = if part > 1 {
            part.to_string()
        } else {
            String::new()
        };
        let hit = hit.map_or_else(String::new, |hit| hit.to_string());
        self.with_query(
            &format!("/book/{id}/read"),
            &[("part", &part), ("find", query), ("hit", &hit)],
            1,
        )
    }

    /// A book's annotations: `POST` creates one (JSON), `GET` is the reader's lists as
    /// an HTML fragment.
    pub fn annotations(&self, id: i64) -> String {
        self.path(&format!("/book/{id}/annotations"))
    }

    /// Edits an annotation's note (`POST`, JSON or form).
    pub fn annotation_note(&self, id: &str) -> String {
        self.path(&format!(
            "/annotation/{}/note",
            utf8_percent_encode(id, PATH_SEGMENT)
        ))
    }

    /// Deletes an annotation (`POST`, JSON or form).
    pub fn annotation_delete(&self, id: &str) -> String {
        self.path(&format!(
            "/annotation/{}/delete",
            utf8_percent_encode(id, PATH_SEGMENT)
        ))
    }

    /// Marks a book as finished, taking it off the "Currently reading" list (`POST`).
    pub fn finished(&self, id: i64) -> String {
        self.path(&format!("/book/{id}/finished"))
    }

    /// Clears the "Currently reading" list (`POST`).
    pub fn clear_reading(&self) -> String {
        self.path("/reading/clear")
    }

    /// Saves a book's reading position (`POST`, JSON).
    pub fn position(&self, id: i64) -> String {
        self.path(&format!("/book/{id}/position"))
    }

    /// An image of a book's content, versioned by the book's `updated_at`.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::web::Urls;
    ///
    /// assert_eq!(Urls::new("/").image(3, "a/b #1.jpg", 9), "/book/3/image/a%2Fb%20%231.jpg?v=9");
    /// ```
    pub fn image(&self, id: i64, image_id: &str, version: i64) -> String {
        self.path(&format!(
            "/book/{id}/image/{}?v={version}",
            utf8_percent_encode(image_id, PATH_SEGMENT)
        ))
    }

    /// A book's cover, versioned by the book's `updated_at`.
    pub fn cover(&self, id: i64, version: i64) -> String {
        self.path(&format!("/cover/{id}?v={version}"))
    }

    /// Search results for `query` at 1-based `page`.
    pub fn search(&self, query: &str, page: u64) -> String {
        self.with_query("/search", &[("q", query)], page)
    }

    /// The library statistics.
    pub fn stats(&self) -> String {
        self.path("/stats")
    }

    /// The readable books behind a figure of the statistics, at 1-based `page`.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::storage::stats::BookFilter;
    /// use shelfgauge::web::Urls;
    ///
    /// let urls = Urls::new("/");
    /// assert_eq!(urls.books(&BookFilter::Genre("a b&c".into()), 2), "/books?genre=a%20b%26c&page=2");
    /// assert_eq!(urls.books(&BookFilter::WithoutCover, 1), "/books?missing=cover");
    /// ```
    pub fn books(&self, filter: &BookFilter, page: u64) -> String {
        let param = match filter {
            BookFilter::Format(format) => ("format", format.as_str()),
            BookFilter::Language(language) => ("language", language.as_str()),
            BookFilter::Genre(code) => ("genre", code.as_str()),
            BookFilter::WithoutCover => ("missing", "cover"),
            BookFilter::WithoutAuthor => ("missing", "author"),
        };
        self.with_query("/books", &[param], page)
    }

    /// The groups of identical copies (`probable`: of probable duplicates) at 1-based
    /// `page`.
    pub fn duplicates(&self, probable: bool, page: u64) -> String {
        let kind = if probable { "probable" } else { "" };
        self.with_query("/duplicates", &[("kind", kind)], page)
    }

    /// The books that could not be read.
    pub fn errors(&self) -> String {
        self.path("/errors")
    }

    /// The rescan action (`POST`).
    pub fn scan(&self) -> String {
        self.path("/scan")
    }

    /// The settings page (`GET`; `POST` saves the config form).
    pub fn settings(&self) -> String {
        self.path("/settings")
    }

    /// The settings page's maintenance action `action` (`POST`): `reread`,
    /// `content-cache` or `export`.
    pub fn settings_action(&self, action: &str) -> String {
        self.path(&format!("/settings/{action}"))
    }

    /// The profile picker, returning to `next` (a local path) after a choice.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::web::Urls;
    ///
    /// let urls = Urls::new("/books");
    /// assert_eq!(urls.profiles(None), "/books/profiles");
    /// assert_eq!(urls.profiles(Some("/books/book/1?part=2")), "/books/profiles?next=/books/book/1%3Fpart%3D2");
    /// ```
    pub fn profiles(&self, next: Option<&str>) -> String {
        match next {
            Some(next) => format!(
                "{}?next={}",
                self.path("/profiles"),
                utf8_percent_encode(next, QUERY_VALUE)
            ),
            None => self.path("/profiles"),
        }
    }

    /// A profile action of the picker (`POST`): `select` or `pin` (also `GET`: the PIN
    /// page).
    pub fn profile_action(&self, id: impl std::borrow::Borrow<i64>, action: &str) -> String {
        self.path(&format!("/profiles/{}/{action}", id.borrow()))
    }

    /// Ends the current profile session (`POST`).
    pub fn leave_profile(&self) -> String {
        self.path("/profiles/leave")
    }

    /// The login of other devices, with an optional local page to return to.
    pub fn login(&self, next: Option<&str>) -> String {
        match next {
            Some(next) => format!(
                "{}?next={}",
                self.path("/login"),
                utf8_percent_encode(next, QUERY_VALUE)
            ),
            None => self.path("/login"),
        }
    }

    /// The password form of a profile, with an optional local page to return to.
    pub fn login_profile(&self, id: impl std::borrow::Borrow<i64>, next: Option<&str>) -> String {
        let path = self.path(&format!("/login/{}", id.borrow()));
        match next {
            Some(next) => format!("{path}?next={}", utf8_percent_encode(next, QUERY_VALUE)),
            None => path,
        }
    }

    /// Logs out (`POST`): ends the session.
    pub fn logout(&self) -> String {
        self.path("/logout")
    }

    /// My profile (`GET`; `POST` saves it).
    pub fn my_profile(&self) -> String {
        self.path("/profile")
    }

    /// The current profile's hub account: Phone and other devices (`GET`; `POST`
    /// registers, joins, pairs, revokes or leaves).
    pub fn profile_hub(&self) -> String {
        self.path("/profile/hub")
    }

    /// A book's place on the hub shelf (`POST`: publish or remove).
    pub fn book_hub(&self, id: i64) -> String {
        self.path(&format!("/book/{id}/hub"))
    }

    /// The current profile's recovery code while it waits for "I saved it" (`GET`).
    pub fn profile_hub_recovery(&self) -> String {
        self.path("/profile/hub/recovery")
    }

    /// Saves the current profile's theme and reader settings (`POST`, JSON).
    pub fn profile_settings(&self) -> String {
        self.path("/profile/settings")
    }

    /// The admin's profile management: `POST` adds a profile.
    pub fn manage_profiles(&self) -> String {
        self.path("/settings/profiles")
    }

    /// An admin action on profile `id` (`POST`; `GET` for the delete confirmation).
    pub fn manage_profile(&self, id: impl std::borrow::Borrow<i64>, action: &str) -> String {
        self.path(&format!("/settings/profiles/{}/{action}", id.borrow()))
    }

    /// The scan progress (JSON).
    pub fn scan_status(&self) -> String {
        self.path("/scan/status")
    }

    /// The sidebar fragment with the author list.
    pub fn sidebar_authors(&self, prefix: &str, page: u64) -> String {
        self.with_query("/sidebar/authors", &[("q", prefix)], page)
    }

    /// The sidebar fragment with an author's books.
    pub fn sidebar_author(&self, id: i64) -> String {
        self.path(&format!("/sidebar/author/{id}"))
    }

    /// The sidebar fragment with a folder's contents.
    pub fn sidebar_folder(&self, folder: &FolderPath, page: u64) -> String {
        self.with_query("/sidebar/folder", &folder_params(folder), page)
    }

    /// `path` with the non-empty `params` and a `page` above 1 as the query string.
    fn with_query(&self, path: &str, params: &[(&str, &str)], page: u64) -> String {
        let page = page.to_string();
        let pairs: Vec<String> = params
            .iter()
            .copied()
            .chain((page != "1" && page != "0").then_some(("page", page.as_str())))
            .filter(|(_, value)| !value.is_empty())
            .map(|(name, value)| format!("{name}={}", utf8_percent_encode(value, QUERY_VALUE)))
            .collect();
        let url = self.path(path);
        if pairs.is_empty() {
            url
        } else {
            format!("{url}?{}", pairs.join("&"))
        }
    }
}

/// The query parameters naming a folder (see [`Urls::folder`]).
fn folder_params(folder: &FolderPath) -> Vec<(&'static str, &str)> {
    match folder {
        FolderPath::Dir(dir) => vec![("path", dir.as_str())],
        FolderPath::Archive { archive, inner } => {
            vec![("zip", archive.as_str()), ("in", inner.as_str())]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_base_path_adds_nothing() {
        let urls = Urls::new("/");
        assert_eq!(urls.prefix(), None);
        assert_eq!(urls.home(), "/");
        assert_eq!(urls.path("/book/1"), "/book/1");
        assert!(urls.asset("style.css").starts_with("/assets/style.css?v="));
    }

    #[test]
    fn sub_path_prefixes_every_url() {
        let urls = Urls::new("/library/books");
        assert_eq!(urls.prefix(), Some("/library/books"));
        assert_eq!(urls.home(), "/library/books/");
        assert_eq!(urls.path("/book/1"), "/library/books/book/1");
        assert!(
            urls.asset("app.js")
                .starts_with("/library/books/assets/app.js?v=")
        );
    }

    #[test]
    fn query_values_are_encoded() {
        let urls = Urls::new("/books");
        assert_eq!(urls.authors("", 1), "/books/authors");
        assert_eq!(
            urls.authors("Тол", 3),
            "/books/authors?q=%D0%A2%D0%BE%D0%BB&page=3"
        );
        assert_eq!(
            urls.search("a+b & c#1 %", 1),
            "/books/search?q=a%2Bb%20%26%20c%231%20%25"
        );
        assert_eq!(urls.book(7), "/books/book/7");
        assert_eq!(urls.cover(7, 123), "/books/cover/7?v=123");
        assert_eq!(urls.sidebar_author(7), "/books/sidebar/author/7");
        let zip = FolderPath::Archive {
            archive: "a/x.zip".into(),
            inner: String::new(),
        };
        assert_eq!(
            urls.sidebar_folder(&zip, 2),
            "/books/sidebar/folder?zip=a/x.zip&page=2"
        );
    }

    #[test]
    fn asset_version_changes_with_content() {
        let urls = Urls::new("/");
        let css = urls.asset("style.css");
        let js = urls.asset("app.js");
        let version = |url: &str| url.split_once("?v=").unwrap().1.to_owned();
        assert_eq!(version(&css).len(), 16);
        assert_ne!(version(&css), version(&js));
    }
}
