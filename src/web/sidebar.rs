//! The sidebar tree: authors or folders, loaded lazily one level at a time.
//!
//! A level is a [`Listing`] (`<div class="listing">`): branches (authors, folders) that
//! expand to the URL in `data-src`, books, and a pager. The same template renders the
//! fragments that the browser fetches and the sidebar that is part of every page.
//!
//! On a page about an author, folder or book, the server renders the path to it already
//! expanded ("reveal"): askama has no recursion, so each level is rendered to a string
//! and inserted into its parent's open branch.

use askama::Template;
use rusqlite::Connection;

use super::layout::{PageLink, Pager, Paging, SidebarMode, SidebarView};
use super::{AppError, AppState};
use crate::container::ContainerKind;
use crate::storage::authors::{author_index, books_by_author, count_authors, list_authors};
use crate::storage::books::{BookDetails, BookSummary};
use crate::storage::folders::{FolderKind, FolderPath, folder_book_index, folder_children};

/// One entry of a sidebar level.
pub(crate) enum Item {
    /// An expandable entry.
    Branch {
        /// The text.
        label: String,
        /// The page about it.
        href: String,
        /// The number of books below it.
        count: u64,
        /// The fragment with its children.
        src: String,
        /// CSS class: `author`, `folder` or `archive`.
        kind: &'static str,
        /// Its children, pre-rendered, when it is expanded.
        open: Option<String>,
        /// Whether the current page is about this entry.
        active: bool,
    },
    /// A book.
    Book {
        /// The title.
        title: String,
        /// Authors or the number in a series.
        detail: String,
        /// The book card.
        href: String,
        /// Whether the current page is this book.
        active: bool,
    },
    /// A series heading in an author's books.
    Heading(String),
}

/// One level of the sidebar tree.
#[derive(Template)]
#[template(path = "sidebar/listing.html")]
pub(crate) struct Listing {
    items: Vec<Item>,
    pager: Option<Pager>,
    empty: &'static str,
}

/// What the current page is about, to be revealed in the sidebar.
#[derive(Debug, Clone, Default)]
pub(crate) enum Focus {
    /// Nothing in particular.
    #[default]
    None,
    /// An author.
    Author(i64),
    /// A folder.
    Folder(FolderPath),
    /// A book: its first author and its folder.
    Book {
        /// The book id.
        id: i64,
        /// Its first author.
        author: Option<i64>,
        /// The folder it is in.
        folder: FolderPath,
    },
}

impl Focus {
    /// The focus for a book card.
    pub(crate) fn book(book: &BookDetails) -> Self {
        Self::Book {
            id: book.id,
            author: book.authors.first().map(|a| a.id),
            folder: book_folder(book),
        }
    }
}

/// The folder a book is listed in.
pub(crate) fn book_folder(book: &BookDetails) -> FolderPath {
    match (&book.entry, book.container_kind) {
        (Some(entry), ContainerKind::Zip) => FolderPath::Archive {
            archive: book.container_path.clone(),
            inner: parent(&entry.path.replace('\\', "/")).to_owned(),
        },
        _ => FolderPath::Dir(parent(&book.container_path).to_owned()),
    }
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// The folders from the root down to `folder` (both included).
pub(crate) fn ancestors(folder: &FolderPath) -> Vec<FolderPath> {
    fn dirs(path: &str, out: &mut Vec<FolderPath>) {
        out.push(FolderPath::Dir(String::new()));
        let mut current = String::new();
        for segment in path.split('/').filter(|s| !s.is_empty()) {
            if !current.is_empty() {
                current.push('/');
            }
            current.push_str(segment);
            out.push(FolderPath::Dir(current.clone()));
        }
    }
    let mut chain = Vec::new();
    match folder {
        FolderPath::Dir(path) => dirs(path, &mut chain),
        FolderPath::Archive { archive, inner } => {
            dirs(parent(archive), &mut chain);
            chain.push(FolderPath::Archive {
                archive: archive.clone(),
                inner: String::new(),
            });
            let mut current = String::new();
            for segment in inner.split('/').filter(|s| !s.is_empty()) {
                if !current.is_empty() {
                    current.push('/');
                }
                current.push_str(segment);
                chain.push(FolderPath::Archive {
                    archive: archive.clone(),
                    inner: current.clone(),
                });
            }
        }
    }
    chain
}

/// The whole sidebar for a page in `mode`, with `focus` revealed.
pub(crate) fn build(
    conn: &Connection,
    state: &AppState,
    mode: SidebarMode,
    focus: &Focus,
) -> Result<SidebarView, AppError> {
    let html = match mode {
        SidebarMode::Authors => {
            let (author, book) = match *focus {
                Focus::Author(id) => (Some(id), None),
                Focus::Book { id, author, .. } => (author, Some(id)),
                _ => (None, None),
            };
            match author {
                Some(author) => reveal_author(conn, state, author, book)?,
                None => authors(conn, state, "", 1, None)?,
            }
        }
        SidebarMode::Folders => match focus {
            Focus::Folder(folder) => reveal_folder(conn, state, folder, None)?,
            Focus::Book { id, folder, .. } => reveal_folder(conn, state, folder, Some(*id))?,
            _ => folder(conn, state, &FolderPath::Dir(String::new()), 1, None, None)?,
        },
    };
    Ok(SidebarView {
        mode,
        filter: String::new(),
        html,
    })
}

/// The author list filtered by `prefix` at `page`; `open` is an author whose books are
/// shown expanded (with the pre-rendered children).
pub(crate) fn authors(
    conn: &Connection,
    state: &AppState,
    prefix: &str,
    page: u64,
    open: Option<(i64, String)>,
) -> Result<String, AppError> {
    let urls = state.urls();
    let paging = Paging::new(page, count_authors(conn, prefix)?, state.page_size());
    let mut open = open;
    let items = list_authors(conn, prefix, paging.limit, paging.offset())?
        .into_iter()
        .map(|author| {
            let children = open
                .take_if(|(id, _)| *id == author.id)
                .map(|(_, html)| html);
            Item::Branch {
                label: author.display_name,
                href: urls.author(author.id),
                count: author.book_count,
                src: urls.sidebar_author(author.id),
                kind: "author",
                active: children.is_some(),
                open: children,
            }
        })
        .collect();
    let pager = paging.pager(|n| PageLink {
        href: urls.authors(prefix, n),
        src: urls.sidebar_authors(prefix, n),
    });
    render(items, pager, "No authors.")
}

/// An author's books, grouped by series.
pub(crate) fn author_books(
    conn: &Connection,
    state: &AppState,
    author: i64,
    active: Option<i64>,
) -> Result<String, AppError> {
    let urls = state.urls();
    let mut items = Vec::new();
    // Books without a series come last; they get an "Other books" heading only after
    // series headings.
    let mut series: Option<String> = None;
    for book in books_by_author(conn, author)? {
        let name = book.series.as_ref().map(|s| s.name.clone());
        if name != series {
            items.push(Item::Heading(
                name.clone().unwrap_or_else(|| "Other books".to_owned()),
            ));
            series = name;
        }
        let detail = book
            .series
            .as_ref()
            .and_then(|s| s.number.as_ref())
            .map(|n| format!("#{n}"))
            .unwrap_or_default();
        items.push(book_item(urls, book, detail, active));
    }
    render(items, None, "No books.")
}

/// A folder's sub-folders and one page of its books; `open` is a sub-folder shown
/// expanded (with the pre-rendered children), `active` the current book.
pub(crate) fn folder(
    conn: &Connection,
    state: &AppState,
    folder: &FolderPath,
    page: u64,
    open: Option<(FolderPath, String)>,
    active: Option<i64>,
) -> Result<String, AppError> {
    let urls = state.urls();
    let limit = state.page_size();
    let first = folder_children(conn, folder, limit, 0)?;
    let paging = Paging::new(page, first.book_total, limit);
    let listing = if paging.page == 1 {
        first
    } else {
        folder_children(conn, folder, limit, paging.offset())?
    };
    let mut open = open;
    let mut items: Vec<Item> = listing
        .folders
        .into_iter()
        .map(|entry| {
            let children = open
                .take_if(|(path, _)| *path == entry.path)
                .map(|(_, html)| html);
            Item::Branch {
                label: entry.name,
                href: urls.folder(&entry.path, 1),
                count: entry.book_count,
                src: urls.sidebar_folder(&entry.path, 1),
                kind: match entry.kind {
                    FolderKind::Archive => "archive",
                    FolderKind::Directory | FolderKind::ArchiveDirectory => "folder",
                },
                active: false,
                open: children,
            }
        })
        .collect();
    items.extend(listing.books.into_iter().map(|book| {
        let detail = book.authors.clone();
        book_item(urls, book, detail, active)
    }));
    let pager = paging.pager(|n| PageLink {
        href: urls.folder(folder, n),
        src: urls.sidebar_folder(folder, n),
    });
    render(items, pager, "No books.")
}

/// The author list at the page of `author`, with the author's books expanded.
fn reveal_author(
    conn: &Connection,
    state: &AppState,
    author: i64,
    book: Option<i64>,
) -> Result<String, AppError> {
    let Some(index) = author_index(conn, author)? else {
        return authors(conn, state, "", 1, None);
    };
    let page = index / u64::from(state.page_size()) + 1;
    let books = author_books(conn, state, author, book)?;
    authors(conn, state, "", page, Some((author, books)))
}

/// The folder tree expanded down to `target`, at the page of `book` if given.
fn reveal_folder(
    conn: &Connection,
    state: &AppState,
    target: &FolderPath,
    book: Option<i64>,
) -> Result<String, AppError> {
    let chain = ancestors(target);
    let page = match book {
        Some(id) => {
            folder_book_index(conn, target, id)?.map_or(1, |i| i / u64::from(state.page_size()) + 1)
        }
        None => 1,
    };
    let mut html = folder(conn, state, target, page, None, book)?;
    for pair in chain.windows(2).rev() {
        let [parent, child] = pair else {
            unreachable!()
        };
        html = folder(conn, state, parent, 1, Some((child.clone(), html)), None)?;
    }
    Ok(html)
}

fn book_item(urls: &super::Urls, book: BookSummary, detail: String, active: Option<i64>) -> Item {
    Item::Book {
        href: urls.book(book.id),
        active: active == Some(book.id),
        title: book.title,
        detail,
    }
}

fn render(items: Vec<Item>, pager: Option<Pager>, empty: &'static str) -> Result<String, AppError> {
    Ok(Listing {
        items,
        pager,
        empty,
    }
    .render()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(path: &str) -> FolderPath {
        FolderPath::Dir(path.to_owned())
    }

    fn zip(archive: &str, inner: &str) -> FolderPath {
        FolderPath::Archive {
            archive: archive.to_owned(),
            inner: inner.to_owned(),
        }
    }

    #[test]
    fn ancestor_chains() {
        assert_eq!(ancestors(&dir("")), [dir("")]);
        assert_eq!(ancestors(&dir("a/b")), [dir(""), dir("a"), dir("a/b")]);
        assert_eq!(ancestors(&zip("x.zip", "")), [dir(""), zip("x.zip", "")]);
        assert_eq!(
            ancestors(&zip("a/x.zip", "m/n")),
            [
                dir(""),
                dir("a"),
                zip("a/x.zip", ""),
                zip("a/x.zip", "m"),
                zip("a/x.zip", "m/n")
            ]
        );
    }
}
