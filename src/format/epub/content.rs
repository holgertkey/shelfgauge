//! Spine documents → [`BookContent`].
//!
//! Every linear spine document is converted to items by the shared XHTML converter
//! ([`format::xhtml`](crate::format::xhtml)); non-linear documents go to the notes
//! body. The main body is cut into sections at the entries of the table of contents
//! (see [`build_sections`]); without a usable table of contents at the headings, and
//! without headings at each spine document.

use std::collections::{HashMap, HashSet};

use super::drm::Protection;
use super::nav::{self, Entry};
use super::package::Package;
use crate::format::ParseError;
use crate::format::model::BookContent;
use crate::format::sections::{Cut, build_sections, heading_cuts};
use crate::format::xhtml::document::with_document;
use crate::format::xhtml::{Converter, Resources, key};
use crate::format::zipbook::ZipBook;

/// The files of an EPUB; encrypted ones are not used.
struct EpubFiles<'z, 'a> {
    archive: &'z mut ZipBook<'a>,
    encrypted: &'z HashSet<String>,
}

impl Resources for EpubFiles<'_, '_> {
    fn read(&mut self, path: &str) -> Result<Option<Vec<u8>>, ParseError> {
        self.archive.read(path)
    }

    fn usable(&self, path: &str) -> bool {
        !self.encrypted.contains(path)
    }
}

/// Converts the spine of `package` (see the [module docs](self)).
pub(super) fn parse(
    package: &Package,
    archive: &mut ZipBook<'_>,
    protection: &Protection,
) -> Result<BookContent, ParseError> {
    let toc = nav::read(package, archive);
    let mut files = EpubFiles {
        archive,
        encrypted: &protection.encrypted,
    };
    let mut converter = Converter::new(&mut files);
    let mut items = Vec::new();
    let mut doc_starts = Vec::new();
    let mut linear_documents = 0;
    for item in &package.spine {
        let Some(data) = converter.read(&item.path)? else {
            tracing::debug!("skipping the missing spine document {:?}", item.path);
            continue;
        };
        converter.start_document(&item.path);
        let mut doc_items = Vec::new();
        with_document(&data, |doc| {
            let body = doc
                .root_element()
                .children()
                .find(|n| n.is_element() && n.tag_name().name().eq_ignore_ascii_case("body"))
                .unwrap_or(doc.root_element());
            if item.linear {
                converter.document(body, &mut doc_items);
            } else {
                converter.nonlinear(body);
            }
        })
        .map_err(|e| ParseError::new(format!("{}: {e}", item.path)))?;
        if item.linear {
            linear_documents += 1;
            if !doc_items.is_empty() {
                doc_starts.push(items.len());
            }
            items.extend(doc_items);
        }
    }
    if linear_documents == 0 {
        return Err(ParseError::new("the EPUB has no readable spine documents"));
    }

    // Table of contents entries → item indexes.
    let mut index_of: HashMap<&str, usize> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        item.block().walk(|b| {
            if let Some(anchor) = &b.anchor {
                index_of.entry(anchor.as_str()).or_insert(i);
            }
        });
    }
    let resolve_entry = |entry: &Entry| {
        let key = key(&entry.target.0, entry.target.1.as_deref());
        let canonical = converter.anchor_of(&key)?;
        index_of.get(canonical).copied()
    };
    let mut cuts: Vec<Cut> = Vec::new();
    for entry in &toc {
        let Some(index) = resolve_entry(entry) else {
            continue;
        };
        // Entries that go back in the text are dropped.
        if cuts.last().is_some_and(|last| index < last.index) {
            continue;
        }
        cuts.push(Cut {
            depth: entry.depth,
            label: Some(entry.label.clone()).filter(|l| !l.is_empty()),
            index,
        });
    }
    if cuts.is_empty() {
        cuts = heading_cuts(&items);
    }
    if cuts.is_empty() {
        cuts = doc_starts
            .iter()
            .map(|&index| Cut {
                depth: 1,
                label: None,
                index,
            })
            .collect();
    }
    let main = build_sections(items, &cuts);
    Ok(converter.finish_book(main))
}
