//! Part blobs: the blocks of one part, cut out of the book and put together again.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::FormatError;
use crate::format::model::{
    Block, BlockId, BlockKind, Body, BookContent, Image, Inline, LinkTarget, Section,
};
use crate::render::html::{ReaderLinks, anchors, render_bodies, section_range};
use crate::render::split::Part;

/// The blocks of one part, in the book's section structure, with the targets of the
/// part's internal links.
///
/// Only the sections that have blocks in the part are kept, each with only those of its
/// title lines and blocks that are in the part; block ids are the book's. Rendering it
/// gives exactly what the server renders for the part ([`PartBlob::render`]), and the
/// blobs of all parts put together give the book again ([`assemble`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartBlob {
    /// The bodies with blocks in the part, in book order.
    pub bodies: Vec<PartBody>,
    /// The source anchors that links in the part point to, with their blocks (which
    /// may be in other parts), sorted by anchor.
    pub anchors: Vec<(String, BlockId)>,
}

/// A body cut down to one part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartBody {
    /// How many sections, starting with the root and following first children, began
    /// in an earlier part: they continue sections of the previous part (0 for a body
    /// that starts in this part).
    pub continued: u32,
    /// The body with its root section cut down to the part.
    pub body: Body,
}

fn overlaps(section: &Section, part: &Part) -> bool {
    section_range(section).is_some_and(|(first, last)| first <= part.end && last >= part.start)
}

/// `section` with only the blocks in `part` (`None` if it has none).
fn cut(section: &Section, part: &Part) -> Option<Section> {
    if !overlaps(section, part) {
        return None;
    }
    let keep = |blocks: &[Block]| {
        blocks
            .iter()
            .filter(|block| part.contains(block.id))
            .cloned()
            .collect()
    };
    Some(Section {
        anchor: section.anchor.clone(),
        title: keep(&section.title),
        blocks: keep(&section.blocks),
        children: section
            .children
            .iter()
            .filter_map(|c| cut(c, part))
            .collect(),
        nav_title: section.nav_title.clone(),
    })
}

/// [`PartBody::continued`] of `section` cut to `part`.
fn continued(section: &Section, part: &Part) -> u32 {
    if section
        .first_block_id()
        .is_none_or(|first| first >= part.start)
    {
        return 0;
    }
    let own = section
        .title
        .iter()
        .chain(&section.blocks)
        .any(|block| part.contains(block.id));
    if own {
        return 1;
    }
    let child = section.children.iter().find(|child| overlaps(child, part));
    1 + child.map_or(0, |child| continued(child, part))
}

/// The inline content of a block itself (not of nested blocks).
fn block_inlines(block: &Block) -> Vec<&[Inline]> {
    match &block.kind {
        BlockKind::Paragraph(inlines)
        | BlockKind::Subtitle(inlines)
        | BlockKind::Verse(inlines)
        | BlockKind::TextAuthor(inlines)
        | BlockKind::Preformatted(inlines) => vec![inlines],
        BlockKind::Table(table) => table
            .rows
            .iter()
            .flat_map(|row| row.cells.iter().map(|cell| cell.content.as_slice()))
            .collect(),
        _ => Vec::new(),
    }
}

fn link_anchors<'a>(inlines: &'a [Inline], out: &mut Vec<&'a str>) {
    for inline in inlines {
        match inline {
            Inline::Link {
                target, children, ..
            } => {
                if let LinkTarget::Anchor(anchor) = target {
                    out.push(anchor);
                }
                link_anchors(children, out);
            }
            Inline::Emphasis(children)
            | Inline::Strong(children)
            | Inline::Strikethrough(children)
            | Inline::Subscript(children)
            | Inline::Superscript(children)
            | Inline::Code(children) => link_anchors(children, out),
            Inline::Text(_) | Inline::Image(_) | Inline::LineBreak => {}
        }
    }
}

fn section_blocks<'a>(section: &'a Section, f: &mut impl FnMut(&'a Block)) {
    for block in section.title.iter().chain(&section.blocks) {
        block.walk(&mut *f);
    }
    for child in &section.children {
        section_blocks(child, f);
    }
}

impl PartBlob {
    /// Cuts `part` out of `content`.
    pub fn extract(content: &BookContent, part: &Part) -> Self {
        Self::extract_all(content, std::slice::from_ref(part))
            .pop()
            .expect("one part")
    }

    /// Cuts every part of `parts` out of `content` (resolving the anchors once).
    pub fn extract_all(content: &BookContent, parts: &[Part]) -> Vec<Self> {
        let resolved = anchors(content);
        parts
            .iter()
            .map(|part| {
                let bodies: Vec<PartBody> = content
                    .bodies()
                    .iter()
                    .filter_map(|body| {
                        Some(PartBody {
                            continued: continued(&body.section, part),
                            body: Body {
                                kind: body.kind,
                                section: cut(&body.section, part)?,
                            },
                        })
                    })
                    .collect();
                let mut targets = Vec::new();
                for body in &bodies {
                    section_blocks(&body.body.section, &mut |block| {
                        for inlines in block_inlines(block) {
                            link_anchors(inlines, &mut targets);
                        }
                    });
                }
                let anchors: BTreeMap<String, BlockId> = targets
                    .into_iter()
                    .filter_map(|anchor| Some((anchor.to_owned(), *resolved.get(anchor)?)))
                    .collect();
                Self {
                    bodies,
                    anchors: anchors.into_iter().collect(),
                }
            })
            .collect()
    }

    /// Renders the part as part `index` of `parts`: the same HTML as
    /// [`render_part`](crate::render::html::render_part) of the whole book.
    pub fn render(&self, parts: &[Part], index: usize, links: &dyn ReaderLinks) -> String {
        let anchor = |anchor: &str| {
            self.anchors
                .binary_search_by(|(a, _)| a.as_str().cmp(anchor))
                .ok()
                .map(|i| self.anchors[i].1)
        };
        render_bodies(
            self.bodies.iter().map(|body| &body.body),
            &anchor,
            parts,
            index,
            links,
        )
    }

    /// The postcard encoding.
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("serializing to a Vec cannot fail")
    }

    /// Decodes a part blob; fails for malformed data or anchors out of order.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let blob: Self = postcard::from_bytes(bytes)?;
        if !blob.anchors.is_sorted_by(|a, b| a.0 < b.0) {
            return Err(FormatError::Malformed("anchors out of order".into()));
        }
        Ok(blob)
    }
}

/// Merges `src`, which continues `target` for `levels` levels, into `target`.
fn merge(target: &mut Section, src: Section, levels: u32) -> Result<(), FormatError> {
    let broken = || FormatError::Malformed("a continued section has nothing to continue".into());
    target.title.extend(src.title);
    target.blocks.extend(src.blocks);
    let mut children = src.children.into_iter();
    if levels > 1 {
        let first = children.next().ok_or_else(broken)?;
        merge(
            target.children.last_mut().ok_or_else(broken)?,
            first,
            levels - 1,
        )?;
    }
    target.children.extend(children);
    Ok(())
}

/// Puts a book of `block_count` blocks ([`Manifest::block_count`]) together from the
/// blobs of all its parts, in order, and its images.
///
/// The result equals the book the parts were cut from, except that sections and
/// bodies without any blocks are gone (they render as nothing and hold no text).
/// Fails if the parts do not fit together, e.g. a part is missing.
///
/// [`Manifest::block_count`]: super::Manifest::block_count
pub fn assemble(
    parts: impl IntoIterator<Item = PartBlob>,
    images: BTreeMap<String, Image>,
    block_count: u32,
) -> Result<BookContent, FormatError> {
    let mut bodies: Vec<Body> = Vec::new();
    for part in parts {
        for PartBody { continued, body } in part.bodies {
            if continued == 0 {
                bodies.push(body);
                continue;
            }
            let target = bodies
                .last_mut()
                .filter(|target| target.kind == body.kind)
                .ok_or_else(|| FormatError::Malformed("a continued body has no start".into()))?;
            merge(&mut target.section, body.section, continued)?;
        }
    }
    let content = BookContent::from_numbered(bodies, images)
        .map_err(|e| FormatError::Malformed(e.to_string()))?;
    if content.block_count() != block_count as usize {
        return Err(FormatError::Malformed(format!(
            "{} blocks instead of {block_count}",
            content.block_count()
        )));
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ReaderConfig, SplitMode};
    use crate::render::html::render_part;
    use crate::render::split::{split, tests::book};

    struct Links;

    impl ReaderLinks for Links {
        fn block(&self, part: usize, id: BlockId) -> String {
            format!("?part={part}#{id}")
        }

        fn image(&self, id: &str) -> String {
            format!("/img/{id}")
        }
    }

    /// The test book with a note reference to note 2 and a link back, an anchor that
    /// leads nowhere, and an empty section.
    fn linked_book() -> BookContent {
        let mut bodies = book().bodies().to_vec();
        let link = |anchor: &str, text: &str| Inline::Link {
            target: LinkTarget::Anchor(anchor.into()),
            note: true,
            children: vec![Inline::Text(text.into())],
        };
        let chapter1 = &mut bodies[0].section.children[1].children[0];
        chapter1.blocks[0].kind = BlockKind::Paragraph(vec![
            Inline::Text("See ".into()),
            Inline::Emphasis(vec![link("n2", "2")]),
            link("nowhere", "?"),
        ]);
        chapter1.blocks[0].anchor = Some("ref2".into());
        chapter1.children.push(Section::default());
        let note2 = &mut bodies[1].section.children[1];
        note2.anchor = Some("n2".into());
        note2.blocks[0].kind = BlockKind::Paragraph(vec![link("ref2", "back")]);
        BookContent::new(bodies, BTreeMap::new())
    }

    fn configs() -> Vec<ReaderConfig> {
        let config = |split, section_depth, part_size| ReaderConfig {
            split,
            section_depth,
            part_size,
            max_part_size: 0,
            ..ReaderConfig::default()
        };
        vec![
            config(SplitMode::None, 1, 1),
            config(SplitMode::Section, 1, 1),
            config(SplitMode::Section, 2, 1),
            config(SplitMode::Size, 1, 1),
            config(SplitMode::Size, 1, 50),
        ]
    }

    /// `content` without sections that have no blocks.
    fn without_empty_sections(content: &BookContent) -> BookContent {
        fn clean(section: &Section) -> Section {
            Section {
                children: section
                    .children
                    .iter()
                    .filter(|child| child.first_block_id().is_some())
                    .map(clean)
                    .collect(),
                ..section.clone()
            }
        }
        let bodies = content
            .bodies()
            .iter()
            .map(|body| Body {
                kind: body.kind,
                section: clean(&body.section),
            })
            .collect();
        BookContent::from_numbered(bodies, content.images().clone()).unwrap()
    }

    #[test]
    fn parts_render_as_the_whole_book_does() {
        let content = linked_book();
        for config in configs() {
            let parts = split(&content, &config);
            let blobs = PartBlob::extract_all(&content, &parts);
            assert_eq!(blobs.len(), parts.len());
            for (index, blob) in blobs.iter().enumerate() {
                let blob = PartBlob::decode(&blob.encode()).unwrap();
                assert_eq!(
                    blob.render(&parts, index, &Links),
                    render_part(&content, &parts, index, &Links),
                    "{config:?}, part {index}"
                );
                assert_eq!(blob, PartBlob::extract(&content, &parts[index]));
            }
        }
    }

    #[test]
    fn parts_carry_the_targets_of_their_own_links_only() {
        let content = linked_book();
        let config = ReaderConfig {
            split: SplitMode::Section,
            ..ReaderConfig::default()
        };
        let parts = split(&content, &config);
        let blobs = PartBlob::extract_all(&content, &parts);
        let with = |anchor: &str| {
            blobs
                .iter()
                .filter(|blob| blob.anchors.iter().any(|(a, _)| a == anchor))
                .count()
        };
        assert_eq!(with("n2"), 1);
        assert_eq!(with("ref2"), 1);
        assert_eq!(with("nowhere"), 0);
        let html: String = (0..parts.len())
            .map(|i| blobs[i].render(&parts, i, &Links))
            .collect();
        assert!(html.contains("class=\"note-ref\""), "{html}");
    }

    #[test]
    fn all_parts_together_are_the_book_again() {
        let content = linked_book();
        let expected = without_empty_sections(&content);
        assert_ne!(expected, content, "the test book has an empty section");
        for config in configs() {
            let parts = split(&content, &config);
            let blobs = PartBlob::extract_all(&content, &parts);
            let book = assemble(blobs, BTreeMap::new(), content.block_count() as u32).unwrap();
            assert_eq!(book, expected, "{config:?}");
        }
    }

    #[test]
    fn missing_or_reordered_parts_are_errors() {
        let content = linked_book();
        let config = ReaderConfig {
            split: SplitMode::Size,
            part_size: 50,
            max_part_size: 0,
            ..ReaderConfig::default()
        };
        let parts = split(&content, &config);
        assert!(parts.len() > 3);
        let blobs = PartBlob::extract_all(&content, &parts);
        let count = content.block_count() as u32;
        for skip in 0..blobs.len() {
            let some = blobs.iter().enumerate().filter(|(i, _)| *i != skip);
            assert!(
                assemble(some.map(|(_, b)| b.clone()), BTreeMap::new(), count).is_err(),
                "{skip}"
            );
        }
        let mut swapped = blobs.clone();
        swapped.swap(1, 2);
        assert!(assemble(swapped, BTreeMap::new(), count).is_err());
    }

    #[test]
    fn garbage_and_truncated_parts_are_errors() {
        let content = linked_book();
        let parts = split(&content, &ReaderConfig::default());
        let bytes = PartBlob::extract(&content, &parts[0]).encode();
        for len in 0..bytes.len() {
            assert!(PartBlob::decode(&bytes[..len]).is_err(), "{len}");
        }
        assert!(PartBlob::decode(&[0xff; 64]).is_err());
        let unsorted = PartBlob {
            bodies: Vec::new(),
            anchors: vec![
                ("b".into(), BlockId::new(1).unwrap()),
                ("a".into(), BlockId::new(2).unwrap()),
            ],
        };
        assert!(PartBlob::decode(&unsorted.encode()).is_err());
    }
}
