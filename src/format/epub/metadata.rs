//! Package metadata → [`BookMetadata`].
//!
//! - Title: the `dc:title` refined as `title-type` `main`, else the first one.
//! - Authors: `dc:creator` elements with the role `aut` or no role (EPUB 3 `role`
//!   refinement or EPUB 2 `opf:role`); the name parts come from `file-as`
//!   (`Last, First Middle`) when given, else from the display name (`First Middle
//!   Last`).
//! - Series: `belongs-to-collection` with `collection-type` `series` or none (`set` is a
//!   list such as "100 Greatest Novels", not a series), with `group-position`; else
//!   Calibre's `calibre:series` / `calibre:series_index`.
//! - Genres: `dc:subject` texts (free text, not FB2 codes).
//! - Annotation: `dc:description`, which may contain (escaped) HTML.
//! - Cover: the `cover-image` item, else the EPUB 2 `meta name="cover"`, else an image
//!   item whose id names a cover. Only known raster types; an encrypted cover is
//!   skipped.

use super::content;
use super::drm::Protection;
use super::package::Package;
use super::xml::with_document;
use crate::format::model::{Annotation, Block, BookMetadata, Image, Person, Series};
use crate::format::xml::{normalize_space, sniff_image_mime};
use crate::format::zipbook::ZipBook as Archive;

/// At most this many subjects are kept.
const MAX_SUBJECTS: usize = 20;

/// Longer subjects are dropped (they are descriptions, not genres).
const MAX_SUBJECT_CHARS: usize = 200;

/// Builds the metadata from the package; the cover is read from `archive`.
pub(super) fn build(
    package: &Package,
    archive: &mut Archive<'_>,
    protection: &Protection,
) -> BookMetadata {
    BookMetadata {
        title: title(package),
        authors: authors(package),
        series: series(package),
        genres: genres(package),
        annotation: annotation(package),
        language: package
            .dc("language")
            .iter()
            .map(|l| l.text.clone())
            .find(|l| !l.is_empty()),
        cover: cover(package, archive, protection),
        drm: protection.scheme.clone(),
    }
}

fn title(package: &Package) -> String {
    let titles = package.dc("title");
    titles
        .iter()
        .find(|t| package.refinement(t.id.as_deref(), "title-type") == Some("main"))
        .or_else(|| titles.iter().find(|t| !t.text.is_empty()))
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

fn authors(package: &Package) -> Vec<Person> {
    let mut authors = Vec::new();
    for creator in package.dc("creator") {
        // A creator may have several roles (SE: `ann`, `aut`, `dto`).
        let mut roles: Vec<&str> = package.refinements(creator.id.as_deref(), "role");
        if roles.is_empty() {
            roles.extend(creator.role.as_deref());
        }
        if !roles.is_empty() && !roles.iter().any(|r| r.trim().eq_ignore_ascii_case("aut")) {
            continue;
        }
        let file_as = package
            .refinement(creator.id.as_deref(), "file-as")
            .or(creator.file_as.as_deref());
        if let Some(person) = person(&creator.text, file_as)
            && !authors.contains(&person)
        {
            authors.push(person);
        }
    }
    authors
}

/// A person from a display name and an optional sort name (`Last, First Middle`).
///
/// Parenthesized parts (`A. I. (Aleksandr Ivanovich)`) and parts after a second comma
/// (`Tolstoy, Leo, graf`) are dropped from the sort name.
pub(super) fn person(name: &str, file_as: Option<&str>) -> Option<Person> {
    let some = |s: &str| (!s.is_empty()).then(|| s.to_owned());
    if let Some((last, rest)) = file_as.and_then(|f| f.split_once(',')) {
        let last = without_parentheses(last);
        let given = without_parentheses(rest.split(',').next().unwrap_or_default());
        let mut given = given.split_whitespace();
        let first = given.next().unwrap_or_default();
        let middle = given.collect::<Vec<_>>().join(" ");
        if !last.is_empty() {
            return Some(Person {
                first_name: some(first),
                middle_name: some(&middle),
                last_name: some(&last),
                nickname: None,
            });
        }
    }
    let words: Vec<&str> = name.split_whitespace().collect();
    match words.as_slice() {
        [] => None,
        [single] => Some(Person {
            last_name: some(single),
            ..Person::default()
        }),
        [first, middle @ .., last] => Some(Person {
            first_name: some(first),
            middle_name: some(&middle.join(" ")),
            last_name: some(last),
            nickname: None,
        }),
    }
}

/// `text` without parenthesized parts, whitespace-normalized.
fn without_parentheses(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0usize;
    for c in text.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    normalize_space(&out)
}

fn series(package: &Package) -> Vec<Series> {
    let mut series = Vec::new();
    for meta in &package.metas {
        if meta.property.as_deref() != Some("belongs-to-collection") || meta.text.is_empty() {
            continue;
        }
        let id = meta.id.as_deref();
        let kind = package.refinement(id, "collection-type");
        if kind.is_some_and(|k| !k.eq_ignore_ascii_case("series")) {
            continue;
        }
        series.push(Series {
            name: meta.text.clone(),
            index: package.refinement(id, "group-position").map(index),
        });
    }
    if series.is_empty() {
        let named = |name: &str| {
            package
                .metas
                .iter()
                .find(|m| m.name.as_deref() == Some(name))
                .and_then(|m| m.content.clone())
                .filter(|c| !c.is_empty())
        };
        if let Some(name) = named("calibre:series") {
            series.push(Series {
                name,
                index: named("calibre:series_index").map(|i| index(&i)),
            });
        }
    }
    series
}

/// A series number without a trailing `.0` (Calibre writes `2.0`).
fn index(value: &str) -> String {
    let value = value.trim();
    value.strip_suffix(".0").unwrap_or(value).to_owned()
}

fn genres(package: &Package) -> Vec<String> {
    let mut genres: Vec<String> = Vec::new();
    for subject in package.dc("subject") {
        let text = &subject.text;
        if text.is_empty() || text.chars().count() > MAX_SUBJECT_CHARS || genres.contains(text) {
            continue;
        }
        genres.push(text.clone());
        if genres.len() == MAX_SUBJECTS {
            break;
        }
    }
    genres
}

/// The first description: HTML (as markup or escaped as text) or plain text.
fn annotation(package: &Package) -> Annotation {
    let Some(description) = package
        .dc("description")
        .iter()
        .find(|d| !d.text.is_empty())
    else {
        return Annotation::default();
    };
    let raw = description.raw.trim();
    if raw.contains('<') {
        let fragment = format!("<div>{raw}</div>");
        if let Ok(blocks) = with_document(fragment.as_bytes(), |doc| {
            content::fragment_blocks(doc.root_element())
        }) && !blocks.is_empty()
        {
            return Annotation::new(blocks);
        }
    }
    let blocks = raw
        .split("\n\n")
        .map(normalize_space)
        .filter(|p| !p.is_empty())
        .map(Block::paragraph)
        .collect();
    Annotation::new(blocks)
}

fn cover(package: &Package, archive: &mut Archive<'_>, protection: &Protection) -> Option<Image> {
    let is_image = |media_type: &str| media_type.starts_with("image/");
    let mut candidates: Vec<&str> = Vec::new();
    let mut items: Vec<_> = package.manifest.iter().collect();
    items.sort_by(|a, b| a.0.cmp(b.0));
    candidates.extend(
        items
            .iter()
            .filter(|(_, item)| item.properties.iter().any(|p| p == "cover-image"))
            .map(|(_, item)| item.path.as_str()),
    );
    for meta in &package.metas {
        if meta.name.as_deref() != Some("cover") {
            continue;
        }
        let Some(content) = meta.content.as_deref() else {
            continue;
        };
        if let Some(item) = package.manifest.get(content) {
            candidates.push(&item.path);
        } else if let Some(item) = package
            .manifest
            .values()
            .find(|item| item.path.ends_with(content))
        {
            // Some books name the file instead of the item.
            candidates.push(&item.path);
        }
    }
    candidates.extend(
        items
            .iter()
            .filter(|(id, item)| is_image(&item.media_type) && id.to_lowercase().contains("cover"))
            .map(|(_, item)| item.path.as_str()),
    );
    for path in candidates {
        if protection.encrypted.contains(path) {
            continue;
        }
        let data = match archive.read(path) {
            Ok(Some(data)) => data,
            Ok(None) => continue,
            Err(e) => {
                tracing::debug!("skipping the cover: {e}");
                continue;
            }
        };
        // Raster images only: an SVG could carry scripts.
        if let Some(mime) = sniff_image_mime(&data) {
            return Some(Image {
                mime: mime.to_owned(),
                data,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(first: Option<&str>, middle: Option<&str>, last: Option<&str>) -> Person {
        Person {
            first_name: first.map(str::to_owned),
            middle_name: middle.map(str::to_owned),
            last_name: last.map(str::to_owned),
            nickname: None,
        }
    }

    #[test]
    fn names_from_file_as_or_display_name() {
        assert_eq!(
            person("graf Leo Tolstoy", Some("Tolstoy, Leo, graf")),
            Some(p(Some("Leo"), None, Some("Tolstoy")))
        );
        assert_eq!(
            person("A. I. Kuprin", Some("Kuprin, A. I. (Aleksandr Ivanovich)")),
            Some(p(Some("A."), Some("I."), Some("Kuprin")))
        );
        assert_eq!(
            person("Jane Austen", None),
            Some(p(Some("Jane"), None, Some("Austen")))
        );
        assert_eq!(
            person("Лев Николаевич Толстой", None),
            Some(p(Some("Лев"), Some("Николаевич"), Some("Толстой")))
        );
        assert_eq!(
            person("Homer", Some("Homer")),
            Some(p(None, None, Some("Homer")))
        );
        assert_eq!(person("  ", None), None);
    }

    #[test]
    fn series_numbers_drop_a_trailing_zero_fraction() {
        assert_eq!(index("2.0"), "2");
        assert_eq!(index("1.5"), "1.5");
        assert_eq!(index(" 3 "), "3");
    }
}
