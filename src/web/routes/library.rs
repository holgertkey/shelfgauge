//! Library pages: home, the author index, an author, a folder, the unreadable books.

use askama::Template;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{Html, Redirect};

use super::figures::{count, file_size};
use super::{BookRow, Params, Row, parse_id, render};
use crate::cache::covers;
use crate::storage::authors::{
    AuthorSummary, author_by_id, books_by_author, count_authors, list_authors,
};
use crate::storage::books::{
    count_books, count_unreadable, failed_books, library_size, protected_books, recent_books,
};
use crate::storage::folders::{FolderKind, FolderPath, folder_children};
use crate::web::layout::{Layout, PageLink, Pager, Paging, SidebarMode};
use crate::web::sidebar::{self, Focus};
use crate::web::{AppError, AppState};

/// The number of newest books on the library page.
const RECENT_BOOKS: u32 = 8;

#[derive(Template)]
#[template(path = "home.html")]
struct HomePage {
    layout: Layout,
    root: String,
    books: u64,
    /// "12 345 books · 2 345 authors · 4.2 GB".
    summary: String,
    /// "Currently reading".
    reading: Vec<super::annotations::ReadingItem>,
    /// "Recently added".
    recent: Vec<RecentCard>,
    /// The parts of the notice about books that need attention: text and link.
    attention: Vec<(String, String)>,
}

/// A book in the "Recently added" row.
struct RecentCard {
    title: String,
    authors: String,
    href: String,
    cover: Option<String>,
}

/// `GET /`: the library overview.
pub(crate) async fn home(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Html<String>, AppError> {
    let mode = SidebarMode::from_headers(&headers);
    let s = state.clone();
    let (books, authors, size, (errors, protected), identical, recent, sidebar) = state
        .db(move |conn| {
            Ok((
                count_books(conn)?,
                count_authors(conn, "")?,
                library_size(conn)?,
                count_unreadable(conn)?,
                s.identical_copies(conn)?.identical_groups,
                recent_books(conn, RECENT_BOOKS)?,
                sidebar::build(conn, &s, mode, &Focus::None)?,
            ))
        })
        .await?;
    let reading = state.user_db(super::annotations::reading_list).await?;
    let urls = state.urls();
    let mut attention = Vec::new();
    if errors > 0 {
        attention.push((
            format!("{} could not be read", count(errors, "book", "books")),
            urls.errors(),
        ));
    }
    if protected > 0 {
        let verb = if protected == 1 { "is" } else { "are" };
        attention.push((
            format!(
                "{} {verb} protected by DRM",
                count(protected, "book", "books")
            ),
            format!("{}#protected", urls.errors()),
        ));
    }
    if identical > 0 {
        let verb = if identical == 1 { "has" } else { "have" };
        attention.push((
            format!(
                "{} {verb} identical copies",
                count(identical, "book", "books")
            ),
            urls.duplicates(false, 1),
        ));
    }
    render(&HomePage {
        layout: Layout::new(&state, "Library", sidebar),
        root: state.library().root().display().to_string(),
        books,
        summary: format!(
            "{} · {} · {}",
            count(books, "book", "books"),
            count(authors, "author", "authors"),
            file_size(size)
        ),
        reading,
        recent: recent
            .into_iter()
            .map(|book| RecentCard {
                href: urls.book(book.id),
                cover: book
                    .cover_mime
                    .as_deref()
                    .and_then(covers::known_mime)
                    .map(|_| urls.cover(book.id, book.updated_at)),
                title: book.title,
                authors: book.authors,
            })
            .collect(),
        attention,
    })
}

#[derive(Template)]
#[template(path = "errors.html")]
struct ErrorsPage {
    layout: Layout,
    failed: Vec<UnreadableBook>,
    protected: Vec<UnreadableBook>,
}

/// A book on the errors page.
struct UnreadableBook {
    /// The path relative to the library root.
    path: String,
    href: String,
    /// Why it cannot be read.
    reason: String,
}

/// `GET /errors`: the books that could not be read, and those protected by DRM (the
/// web version of `shelfgauge errors`).
pub(crate) async fn errors(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Html<String>, AppError> {
    let mode = SidebarMode::from_headers(&headers);
    let s = state.clone();
    let (failed, protected, sidebar) = state
        .db(move |conn| {
            Ok((
                failed_books(conn)?,
                protected_books(conn)?,
                sidebar::build(conn, &s, mode, &Focus::None)?,
            ))
        })
        .await?;
    let urls = state.urls();
    render(&ErrorsPage {
        layout: Layout::new(&state, "Books that could not be read", sidebar),
        failed: failed
            .into_iter()
            .map(|book| UnreadableBook {
                href: urls.book(book.id),
                path: book.path,
                reason: book.message,
            })
            .collect(),
        protected: protected
            .into_iter()
            .map(|book| UnreadableBook {
                href: urls.book(book.id),
                path: book.path,
                reason: book.drm,
            })
            .collect(),
    })
}

/// `GET <base_path>` without the trailing slash: redirects to the library page.
pub(crate) async fn to_home(State(state): State<AppState>) -> Redirect {
    Redirect::temporary(&state.urls().home())
}

#[derive(Template)]
#[template(path = "authors.html")]
struct AuthorsPage {
    layout: Layout,
    prefix: String,
    authors: Vec<AuthorLink>,
    pager: Option<Pager>,
}

/// An author in the author index.
struct AuthorLink {
    name: String,
    href: String,
    count: u64,
}

/// `GET /authors?q=&page=`: the author index, filtered by a name prefix.
pub(crate) async fn authors(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<Params>, QueryRejection>,
) -> Result<Html<String>, AppError> {
    let params = Params::from(query);
    let prefix = params.q.trim().to_owned();
    let mode = SidebarMode::from_headers(&headers);
    let s = state.clone();
    let (paging, authors, sidebar) = {
        let prefix = prefix.clone();
        let page = params.page();
        state
            .db(move |conn| {
                let paging = Paging::new(page, count_authors(conn, &prefix)?, s.page_size());
                let authors = list_authors(conn, &prefix, paging.limit, paging.offset())?;
                Ok((
                    paging,
                    authors,
                    sidebar::build(conn, &s, mode, &Focus::None)?,
                ))
            })
            .await?
    };
    let urls = state.urls();
    render(&AuthorsPage {
        pager: paging.pager(|n| PageLink {
            href: urls.authors(&prefix, n),
            src: String::new(),
        }),
        layout: Layout::new(&state, "Authors", sidebar),
        prefix,
        authors: authors
            .into_iter()
            .map(|a: AuthorSummary| AuthorLink {
                href: urls.author(a.id),
                name: a.display_name,
                count: a.book_count,
            })
            .collect(),
    })
}

#[derive(Template)]
#[template(path = "author.html")]
struct AuthorPage {
    layout: Layout,
    name: String,
    count: u64,
    rows: Vec<Row>,
    empty: &'static str,
}

/// `GET /author/{id}`: an author's books, grouped by series.
pub(crate) async fn author(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Html<String>, AppError> {
    let id = parse_id(&id)?;
    let mode = SidebarMode::from_headers(&headers);
    let s = state.clone();
    let (author, books, sidebar) = state
        .db(move |conn| {
            let author = author_by_id(conn, id)?.ok_or(AppError::NotFound)?;
            Ok((
                author,
                books_by_author(conn, id)?,
                sidebar::build(conn, &s, mode, &Focus::Author(id))?,
            ))
        })
        .await?;
    let urls = state.urls();
    let mut rows = Vec::new();
    let mut series: Option<String> = None;
    for book in books {
        let name = book.series.as_ref().map(|s| s.name.clone());
        if name != series {
            rows.push(Row::Heading(
                name.clone().unwrap_or_else(|| "Other books".to_owned()),
            ));
            series = name;
        }
        rows.push(Row::Book(BookRow::new(urls, book)));
    }
    render(&AuthorPage {
        layout: Layout::new(&state, author.display_name.clone(), sidebar),
        name: author.display_name,
        count: author.book_count,
        rows,
        empty: "No books.",
    })
}

/// A sub-folder in the main area.
struct FolderLink {
    name: String,
    href: String,
    count: u64,
    kind: &'static str,
}

#[derive(Template)]
#[template(path = "folder.html")]
struct FolderPage {
    layout: Layout,
    name: String,
    crumbs: Vec<(String, String)>,
    folders: Vec<FolderLink>,
    rows: Vec<Row>,
    empty: &'static str,
    pager: Option<Pager>,
}

/// `GET /folder?path=` or `?zip=&in=`: a folder's sub-folders and books.
pub(crate) async fn folder(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<Params>, QueryRejection>,
) -> Result<Html<String>, AppError> {
    let params = Params::from(query);
    let folder = params.folder();
    let page = params.page();
    let mode = SidebarMode::from_headers(&headers);
    let s = state.clone();
    let target = folder.clone();
    let (paging, listing, sidebar) = state
        .db(move |conn| {
            let limit = s.page_size();
            let first = folder_children(conn, &target, limit, 0)?;
            let is_root = target == FolderPath::Dir(String::new());
            if !is_root && first.folders.is_empty() && first.book_total == 0 {
                return Err(AppError::NotFound);
            }
            let paging = Paging::new(page, first.book_total, limit);
            let listing = if paging.page == 1 {
                first
            } else {
                folder_children(conn, &target, limit, paging.offset())?
            };
            let sidebar = sidebar::build(conn, &s, mode, &Focus::Folder(target))?;
            Ok((paging, listing, sidebar))
        })
        .await?;

    let urls = state.urls();
    let crumbs: Vec<(String, String)> = sidebar::ancestors(&folder)
        .into_iter()
        .map(|path| (folder_name(&path), urls.folder(&path, 1)))
        .collect();
    let name = folder_name(&folder);
    render(&FolderPage {
        layout: Layout::new(&state, name.clone(), sidebar),
        name,
        crumbs,
        folders: listing
            .folders
            .into_iter()
            .map(|entry| FolderLink {
                href: urls.folder(&entry.path, 1),
                name: entry.name,
                count: entry.book_count,
                kind: match entry.kind {
                    FolderKind::Archive => "archive",
                    FolderKind::Directory | FolderKind::ArchiveDirectory => "folder",
                },
            })
            .collect(),
        rows: listing
            .books
            .into_iter()
            .map(|book| Row::Book(BookRow::new(urls, book)))
            .collect(),
        empty: "No books here.",
        pager: paging.pager(|n| PageLink {
            href: urls.folder(&folder, n),
            src: String::new(),
        }),
    })
}

/// The last segment of a folder path; "Library" for the root.
fn folder_name(folder: &FolderPath) -> String {
    let path = match folder {
        FolderPath::Dir(dir) => dir.as_str(),
        FolderPath::Archive { archive, inner } if inner.is_empty() => archive.as_str(),
        FolderPath::Archive { inner, .. } => inner.as_str(),
    };
    match path.rsplit('/').next() {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => "Library".to_owned(),
    }
}
