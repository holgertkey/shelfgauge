//! The one place where URLs are built: every route and every generated link goes
//! through [`Urls`], so `server.base_path` is applied consistently.

use super::assets;

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
    fn asset_version_changes_with_content() {
        let urls = Urls::new("/");
        let css = urls.asset("style.css");
        let js = urls.asset("app.js");
        let version = |url: &str| url.split_once("?v=").unwrap().1.to_owned();
        assert_eq!(version(&css).len(), 16);
        assert_ne!(version(&css), version(&js));
    }
}
