//! `<description>/<title-info>` → [`BookMetadata`].

use roxmltree::Node;

use super::{attr, attr_text, binaries, child, children, content, decode_binary, text_content};
use crate::format::model::{Annotation, BookMetadata, Image, Person, Series};

/// Extracts the metadata; missing parts are left empty.
pub(super) fn parse(root: Node<'_, '_>) -> BookMetadata {
    let Some(info) = child(root, "description").and_then(|d| child(d, "title-info")) else {
        return BookMetadata::default();
    };
    let mut genres: Vec<String> = Vec::new();
    for genre in children(info, "genre").map(text_content) {
        if !genre.is_empty() && !genres.contains(&genre) {
            genres.push(genre);
        }
    }
    let mut series = Vec::new();
    for sequence in children(info, "sequence") {
        collect_series(sequence, 0, &mut series);
    }
    BookMetadata {
        title: child(info, "book-title")
            .map(text_content)
            .unwrap_or_default(),
        authors: children(info, "author").filter_map(person).collect(),
        series,
        genres,
        annotation: child(info, "annotation")
            .map(|a| Annotation::new(content::annotation_blocks(a)))
            .unwrap_or_default(),
        language: child(info, "lang")
            .map(text_content)
            .filter(|lang| !lang.is_empty()),
        cover: cover(root, info),
    }
}

/// An `<author>`; `None` if all name parts are empty.
fn person(node: Node<'_, '_>) -> Option<Person> {
    let part = |name| {
        child(node, name)
            .map(text_content)
            .filter(|s| !s.is_empty())
    };
    let person = Person {
        first_name: part("first-name"),
        middle_name: part("middle-name"),
        last_name: part("last-name"),
        nickname: part("nickname"),
    };
    (person != Person::default()).then_some(person)
}

/// A `<sequence>` and its nested sequences (sub-series), in document order.
fn collect_series(node: Node<'_, '_>, depth: usize, out: &mut Vec<Series>) {
    if depth > super::MAX_DEPTH {
        return;
    }
    if let Some(name) = attr_text(node, "name") {
        out.push(Series {
            name,
            index: attr_text(node, "number"),
        });
    }
    for nested in children(node, "sequence") {
        collect_series(nested, depth + 1, out);
    }
}

/// The image referenced by `<coverpage>`, decoded from its `<binary>`.
fn cover(root: Node<'_, '_>, info: Node<'_, '_>) -> Option<Image> {
    let coverpage = child(info, "coverpage")?;
    children(coverpage, "image")
        .filter_map(|image| {
            let id = attr(image, "href")?.trim().strip_prefix('#')?.trim();
            binaries(root)
                .find(|binary| attr(*binary, "id").map(str::trim) == Some(id))
                .and_then(decode_binary)
        })
        .next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::BookParser;
    use crate::format::fb2::Fb2Parser;

    fn metadata(title_info: &str, rest: &str) -> BookMetadata {
        let xml = format!(
            r#"<FictionBook xmlns:l="http://www.w3.org/1999/xlink"><description><title-info>{title_info}</title-info></description><body/>{rest}</FictionBook>"#
        );
        Fb2Parser.parse_metadata(xml.as_bytes()).unwrap()
    }

    #[test]
    fn full_title_info() {
        let png = super::super::tests::PNG_1X1;
        let m = metadata(
            "<genre>sf</genre><genre> sf </genre><genre>adventure</genre><genre/>\
             <author><first-name>Лев</first-name><middle-name>Николаевич</middle-name>\
               <last-name>Толстой</last-name></author>\
             <author><nickname> Anon </nickname></author><author><first-name/></author>\
             <book-title>\n  Война  и мир \n</book-title>\
             <annotation><p>First.</p><p>Second.</p></annotation>\
             <coverpage><image l:href='#cover.png'/></coverpage>\
             <lang> ru </lang>\
             <sequence name='Epic' number='1.5'><sequence name='Part' number=''/></sequence>\
             <sequence name='  '/>",
            &format!("<binary id='cover.png' content-type='image/png'>{png}</binary>"),
        );
        assert_eq!(m.title, "Война и мир");
        assert_eq!(m.genres, ["sf", "adventure"]);
        assert_eq!(m.authors.len(), 2);
        assert_eq!(m.authors[0].last_name.as_deref(), Some("Толстой"));
        assert_eq!(m.authors[0].middle_name.as_deref(), Some("Николаевич"));
        assert_eq!(m.authors[1].nickname.as_deref(), Some("Anon"));
        assert_eq!(m.annotation.plain_text(), "First.\nSecond.");
        assert_eq!(m.annotation.blocks()[1].id.to_string(), "b2");
        assert_eq!(m.language.as_deref(), Some("ru"));
        assert_eq!(
            m.series,
            [
                Series {
                    name: "Epic".to_owned(),
                    index: Some("1.5".to_owned())
                },
                Series {
                    name: "Part".to_owned(),
                    index: None
                },
            ]
        );
        let cover = m.cover.unwrap();
        assert_eq!(cover.mime, "image/png");
    }

    #[test]
    fn missing_parts_are_empty() {
        assert_eq!(metadata("", ""), BookMetadata::default());
        let xml = b"<FictionBook><body><p>x</p></body></FictionBook>";
        assert_eq!(
            Fb2Parser.parse_metadata(xml).unwrap(),
            BookMetadata::default()
        );
    }

    #[test]
    fn broken_cover_references_are_ignored() {
        let m = metadata(
            "<coverpage><image l:href='missing'/><image l:href='#nope'/></coverpage>",
            "<binary id='bad'>%%%</binary>",
        );
        assert_eq!(m.cover, None);
        let m = metadata(
            "<coverpage><image l:href='#bad'/></coverpage>",
            "<binary id='bad'>%%%</binary>",
        );
        assert_eq!(m.cover, None);
    }

    #[test]
    fn plain_href_attribute_is_accepted() {
        let png = super::super::tests::PNG_1X1;
        let m = metadata(
            "<coverpage><image href='#c'/></coverpage>",
            &format!("<binary id='c'>{png}</binary>"),
        );
        assert!(m.cover.is_some());
    }
}
