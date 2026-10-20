//! What every page shares: the layout context (top bar, scan banner, sidebar) and
//! pagination.

use axum::http::{HeaderMap, header};
use serde::Serialize;

use super::profile::CurrentProfile;
use super::{AppState, Urls};
use crate::scanner::ScanPhase;

/// The cookie that remembers the sidebar mode. It has to be a cookie, not
/// `localStorage`: the server renders the sidebar. Cookies ignore the port, so all
/// libraries served from one host share this preference.
pub(crate) const SIDEBAR_COOKIE: &str = "shelfgauge-sidebar";

/// How the sidebar groups the library.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum SidebarMode {
    /// Alphabetical authors, each expanding to their books.
    #[default]
    Authors,
    /// The directory tree, with zip archives as folders.
    Folders,
}

impl SidebarMode {
    /// The mode stored in the request's cookie (default: by author).
    pub(crate) fn from_headers(headers: &HeaderMap) -> Self {
        let folders = headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .any(|(name, value)| name == SIDEBAR_COOKIE && value == "folders");
        if folders {
            Self::Folders
        } else {
            Self::Authors
        }
    }

    /// Whether this is the by-folder mode (for templates).
    pub(crate) fn is_folders(self) -> bool {
        self == Self::Folders
    }
}

/// The rendered sidebar.
#[derive(Debug, Clone, Default)]
pub(crate) struct SidebarView {
    /// The mode it was rendered in.
    pub(crate) mode: SidebarMode,
    /// The author name filter (authors mode).
    pub(crate) filter: String,
    /// The tree (our own template output).
    pub(crate) html: String,
}

/// Scan progress, as rendered into the banner and returned by `/scan/status`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ScanStatus {
    /// Whether a scan is running.
    pub(crate) running: bool,
    /// Books indexed so far.
    pub(crate) done: u64,
    /// Books to index (0 while the scan is still looking for changes).
    pub(crate) total: u64,
    /// The banner text.
    pub(crate) message: String,
}

impl ScanStatus {
    /// The current status of the library's background scan.
    pub(crate) fn of(state: &AppState) -> Self {
        let scan = state.scan();
        let progress = scan.progress();
        let running = scan.is_running() && progress.phase != ScanPhase::Done;
        let total = if progress.phase == ScanPhase::Indexing {
            progress.total
        } else {
            0
        };
        let message = if running {
            if total > 0 {
                format!(
                    "Scanning the library: {} of {total} books indexed.",
                    progress.done
                )
            } else {
                "Scanning the library: looking for changes…".to_owned()
            }
        } else {
            match scan.last_result() {
                Some(Ok(s)) if s.cancelled => "The scan was cancelled.".to_owned(),
                Some(Ok(s)) => format!(
                    "Scan finished: {} added, {} updated, {} moved, {} removed, {} errors.",
                    s.added, s.updated, s.moved, s.removed, s.errors
                ),
                Some(Err(e)) => format!("The scan failed: {e}"),
                None => String::new(),
            }
        };
        Self {
            running,
            done: progress.done,
            total,
            message,
        }
    }
}

/// The `data-r*` attribute of each reader setting on `<html>` (see `theme.js`).
const READER_ATTRS: &[(&str, &str)] = &[
    ("font", "rfont"),
    ("size", "rsize"),
    ("leading", "rleading"),
    ("width", "rwidth"),
    ("align", "ralign"),
    ("status", "rstatus"),
];

/// The profile as the layout shows it: the attributes of `<html>` and the avatar menu.
pub(crate) struct LayoutProfile {
    /// Whether there are several profiles: the avatar menu is shown.
    pub(crate) several: bool,
    /// The name.
    pub(crate) name: String,
    /// The avatar's initials.
    pub(crate) initials: String,
    /// The palette index (`data-pcolor`, omitted for 0).
    pub(crate) color: u8,
    /// Whether the gear, Rescan and the admin menu entries are shown (an admin, and on
    /// another device only with `remote_admin`).
    pub(crate) admin: bool,
    /// Whether the page is shown on another device: the avatar menu is shown with
    /// *Sign out* instead of *Switch profile*.
    pub(crate) remote: bool,
    /// The theme (`data-theme`); empty for the system theme.
    pub(crate) theme: String,
    /// The reader settings as `data-*` attributes: `(attribute, value)`.
    pub(crate) reader: Vec<(&'static str, &'static str)>,
    /// Whether the page asks `theme.js` to adopt the settings saved in this browser
    /// before 0.15.0 (`data-adopt`): the owner, settings never saved.
    pub(crate) adopt: bool,
}

impl LayoutProfile {
    /// The view of the request's profile.
    pub(crate) fn of(state: &AppState, current: &CurrentProfile) -> Self {
        let profile = &current.profile;
        let settings = profile.settings.clone().unwrap_or_default();
        Self {
            several: current.several(),
            name: profile.name.clone(),
            initials: profile.initials(),
            color: profile.color,
            admin: current.admin_here(state),
            remote: current.remote,
            theme: settings.get("theme").unwrap_or("").to_owned(),
            reader: READER_ATTRS
                .iter()
                .filter_map(|(name, attr)| settings.get(name).map(|v| (*attr, v)))
                .collect(),
            adopt: profile.id.is_owner() && profile.settings.is_none(),
        }
    }

    /// For a page without a profile (the picker, an error page without a session): no
    /// menu, no admin controls, the system theme.
    pub(crate) fn none() -> Self {
        Self {
            several: false,
            name: String::new(),
            initials: String::new(),
            color: 0,
            admin: false,
            remote: false,
            theme: String::new(),
            reader: Vec::new(),
            adopt: false,
        }
    }
}

/// The context of the base layout; every page template has it as `layout`.
pub(crate) struct Layout {
    /// The URL helper.
    pub(crate) urls: Urls,
    /// The page title.
    pub(crate) title: String,
    /// The text in the search field.
    pub(crate) query: String,
    /// Scan progress for the banner.
    pub(crate) scan: ScanStatus,
    /// The sidebar.
    pub(crate) sidebar: SidebarView,
    /// The language of the page's main content (`<html lang>`).
    pub(crate) lang: String,
    /// The request's profile.
    pub(crate) profile: LayoutProfile,
}

impl Layout {
    /// The layout for a page of `profile`.
    pub(crate) fn new(
        state: &AppState,
        profile: &CurrentProfile,
        title: impl Into<String>,
        sidebar: SidebarView,
    ) -> Self {
        Self::with_profile(state, LayoutProfile::of(state, profile), title, sidebar)
    }

    fn with_profile(
        state: &AppState,
        profile: LayoutProfile,
        title: impl Into<String>,
        sidebar: SidebarView,
    ) -> Self {
        Self {
            urls: state.urls().clone(),
            title: title.into(),
            query: String::new(),
            scan: ScanStatus::of(state),
            sidebar,
            lang: "en".to_owned(),
            profile,
        }
    }

    /// The layout for an error page: an empty sidebar, so nothing is read from the
    /// index; the profile's theme if it is known.
    pub(crate) fn without_sidebar(
        state: &AppState,
        profile: Option<&CurrentProfile>,
        title: impl Into<String>,
    ) -> Self {
        let profile = profile.map_or_else(LayoutProfile::none, |p| LayoutProfile::of(state, p));
        Self::with_profile(state, profile, title, SidebarView::default())
    }

    /// The layout for a page that needs no profile (the picker and the PIN page).
    pub(crate) fn without_profile(state: &AppState, title: impl Into<String>) -> Self {
        Self::with_profile(state, LayoutProfile::none(), title, SidebarView::default())
    }

    /// Sets the text of the search field.
    pub(crate) fn with_query(mut self, query: &str) -> Self {
        self.query = query.to_owned();
        self
    }

    /// Sets the page language; `None` keeps English (the language of the UI).
    pub(crate) fn with_lang(mut self, lang: Option<String>) -> Self {
        if let Some(lang) = lang {
            self.lang = lang;
        }
        self
    }
}

/// A 1-based page of a list of known length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Paging {
    /// The page, clamped to `1..=pages`.
    pub(crate) page: u64,
    /// The number of pages (at least 1).
    pub(crate) pages: u64,
    /// Entries per page.
    pub(crate) limit: u32,
}

impl Paging {
    /// The page `requested` of `total` entries, `limit` per page; out-of-range pages are
    /// clamped.
    pub(crate) fn new(requested: u64, total: u64, limit: u32) -> Self {
        let limit = limit.max(1);
        let pages = total.div_ceil(u64::from(limit)).max(1);
        Self {
            page: requested.clamp(1, pages),
            pages,
            limit,
        }
    }

    /// The index of the first entry on the page.
    pub(crate) fn offset(self) -> u64 {
        (self.page - 1) * u64::from(self.limit)
    }

    /// The pager for this page; `link` builds the URLs of a page number.
    pub(crate) fn pager(self, link: impl Fn(u64) -> PageLink) -> Option<Pager> {
        (self.pages > 1).then(|| Pager {
            prev: (self.page > 1).then(|| link(self.page - 1)),
            next: (self.page < self.pages).then(|| link(self.page + 1)),
            label: format!("Page {} of {}", self.page, self.pages),
        })
    }
}

/// The page number from a `page` query value: invalid or missing values mean 1.
pub(crate) fn page_number(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(1)
        .max(1)
}

/// Links to the previous and next page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pager {
    /// The previous page.
    pub(crate) prev: Option<PageLink>,
    /// The next page.
    pub(crate) next: Option<PageLink>,
    /// "Page 2 of 7".
    pub(crate) label: String,
}

/// A link to a page: the page URL and, in the sidebar, the fragment URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PageLink {
    /// The URL of the full page.
    pub(crate) href: String,
    /// The URL of the sidebar fragment (empty outside the sidebar).
    pub(crate) src: String,
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn paging_clamps_and_counts() {
        let p = Paging::new(1, 0, 10);
        assert_eq!((p.page, p.pages, p.offset()), (1, 1, 0));
        let p = Paging::new(2, 10, 10);
        assert_eq!((p.page, p.pages), (1, 1), "exactly one full page");
        let p = Paging::new(2, 11, 10);
        assert_eq!((p.page, p.pages, p.offset()), (2, 2, 10));
        let p = Paging::new(u64::MAX, 11, 10);
        assert_eq!((p.page, p.offset()), (2, 10));
        assert_eq!(Paging::new(0, 11, 10).page, 1);
    }

    #[test]
    fn pager_links() {
        let link = |n: u64| PageLink {
            href: format!("/p{n}"),
            src: String::new(),
        };
        assert_eq!(Paging::new(1, 5, 10).pager(link), None);
        let pager = Paging::new(2, 30, 10).pager(link).unwrap();
        assert_eq!(pager.prev.unwrap().href, "/p1");
        assert_eq!(pager.next.unwrap().href, "/p3");
        assert_eq!(pager.label, "Page 2 of 3");
        let pager = Paging::new(3, 30, 10).pager(link).unwrap();
        assert!(pager.next.is_none());
    }

    #[test]
    fn page_numbers() {
        assert_eq!(page_number(None), 1);
        assert_eq!(page_number(Some("3")), 3);
        assert_eq!(page_number(Some("0")), 1);
        assert_eq!(page_number(Some("-2")), 1);
        assert_eq!(page_number(Some("x")), 1);
        assert_eq!(page_number(Some("99999999999999999999999")), 1);
    }

    #[test]
    fn sidebar_mode_cookie() {
        let mut headers = HeaderMap::new();
        assert_eq!(SidebarMode::from_headers(&headers), SidebarMode::Authors);
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=b; shelfgauge-sidebar=folders"),
        );
        assert_eq!(SidebarMode::from_headers(&headers), SidebarMode::Folders);
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("shelfgauge-sidebar=nonsense"),
        );
        assert_eq!(SidebarMode::from_headers(&headers), SidebarMode::Authors);
    }
}
