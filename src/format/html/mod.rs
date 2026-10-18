//! Single-file HTML books (`.html`, `.htm`, `.xhtml`): lib.ru pages, Project Gutenberg
//! HTML, saved books. Not indexed unless `html` is listed in `[scan] formats`: most HTML
//! files in a folder are not books.
//!
//! HTML is not XML, so a page goes through four steps:
//!
//! 1. Bytes → text: a BOM, else the `charset` of a `<meta>` in the first
//!    [`PRESCAN_LIMIT`] bytes or the encoding of an XML declaration (checked against
//!    the bytes, see [`format::encoding`](crate::format::encoding)), else UTF-8 or the
//!    detector. In an XHTML file, self-closed elements whose content HTML reads as raw
//!    text (`<title/>`, `<script src="..."/>`) are written as start and end tags, or the
//!    HTML parser would take the rest of the file as their text.
//! 2. Text → tree with `html5ever` (the browsers' parsing algorithm: tag soup,
//!    unclosed `<p>`, `<BR>`, unquoted attributes) into a small arena DOM.
//! 3. Tree → well-formed XHTML of the body (see `serialize`): scripts, forms and site
//!    chrome left out, deep nesting flattened, a `<pre>` holding the whole book read
//!    like plain text.
//! 4. XHTML → the book with the converter shared with EPUB (one document, no images;
//!    `#fragment` links within the page work, links to other files are text, only
//!    `http`, `https` and `mailto` links are shown). Sections come from the headings:
//!    a heading's rank is its level, and among headings of one level part-like ones
//!    (`BOOK ONE`, `ЧАСТЬ ПЕРВАЯ`) rank above the others; a heading right after a
//!    higher-ranked one continues its title unless it looks like a heading line of its
//!    own (`CHAPTER I`). A single heading of the top rank that is not part-like (the
//!    book's title) starts no section.
//!
//! Metadata: Dublin Core metas (`dc.title`, `dc.creator`, `dc.language`,
//! `dc.description`), `<meta name="author">` and `"description"`, `<html lang>`, else
//! the `<title>` (`Lib.ru: Author. Title`, `Author. Title`, `The Project Gutenberg
//! eBook of Title, by Author`, `Title | Project Gutenberg`). No cover.
//!
//! A page with less than [`MIN_TEXT`] characters of text (whitespace not counted) or
//! whose text is mostly link text ([`MAX_LINK_SHARE`]) is
//! [not a book](crate::format::ParseErrorKind::NotABook).

use std::borrow::Cow;

use super::encoding::{Decoded, decode_bom, decode_declared};
use super::model::{Annotation, Block, BookContent, BookMetadata, Person};
use super::sections::{Cut, Item, build_sections};
use super::xhtml::document::with_document;
use super::xhtml::{Converter, NoResources};
use super::xml::normalize_space;
use super::{BookParser, ParseError};

mod dom;
mod serialize;

use dom::{Dom, Id};

/// How many leading bytes are searched for a `<meta charset>`.
pub const PRESCAN_LIMIT: usize = 1024;

/// Minimum characters of text (whitespace not counted) of a book.
pub const MIN_TEXT: usize = 5_000;

/// A page whose link text is more than this share of its text is not a book.
pub const MAX_LINK_SHARE: f64 = 0.5;

/// The name the page's own anchors are keyed by.
const DOCUMENT_PATH: &str = "book.html";

/// The HTML parser.
#[derive(Debug, Clone, Copy, Default)]
pub struct HtmlParser;

impl BookParser for HtmlParser {
    fn format(&self) -> &'static str {
        "html"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["html", "htm", "xhtml"]
    }

    /// Reads the metadata (see the [module docs](self)); pages that are not books are
    /// [`NotABook`](crate::format::ParseErrorKind::NotABook) errors.
    fn parse_metadata(&self, data: &[u8]) -> Result<BookMetadata, ParseError> {
        let dom = parse(data)?;
        let body = dom.find("body").unwrap_or(dom::DOCUMENT);
        let (text, links) = serialize::text_stats(&dom, body);
        if text < MIN_TEXT {
            return Err(ParseError::not_a_book(format!(
                "less than {MIN_TEXT} characters of text"
            )));
        }
        if links as f64 > text as f64 * MAX_LINK_SHARE {
            return Err(ParseError::not_a_book("the page is mostly links"));
        }
        Ok(metadata(&dom))
    }

    /// Converts the page (see the [module docs](self)).
    fn parse_content(&self, data: &[u8]) -> Result<BookContent, ParseError> {
        let dom = parse(data)?;
        let body = dom.find("body").unwrap_or(dom::DOCUMENT);
        let xhtml = serialize::to_xhtml(&dom, body);
        drop(dom);
        let mut none = NoResources;
        let mut converter = Converter::new(&mut none);
        converter.start_document(DOCUMENT_PATH);
        let mut items = Vec::new();
        with_document(xhtml.as_bytes(), |doc| {
            let root = doc.root_element();
            let body = root
                .children()
                .find(|n| n.has_tag_name("body"))
                .unwrap_or(root);
            converter.document(body, &mut items);
        })?;
        let cuts = cuts(&items);
        Ok(converter.finish_book(build_sections(items, &cuts)))
    }
}

/// Decodes `data` and builds the tree; a panic in `html5ever` is an error.
fn parse(data: &[u8]) -> Result<Dom, ParseError> {
    let decoded = decode(data);
    if decoded.had_errors {
        tracing::warn!(
            encoding = decoded.encoding.name(),
            "malformed byte sequences replaced while decoding"
        );
    }
    let text = expand_self_closed(&decoded.text);
    match crate::logging::catch_expected_panic(|| Dom::parse(&text)) {
        Ok(Ok(dom)) => Ok(dom),
        Ok(Err(dom::TooBig::Deep)) => Err(ParseError::new(format!(
            "elements are nested more than {} levels deep",
            dom::MAX_DEPTH
        ))),
        Ok(Err(dom::TooBig::Large)) => Err(ParseError::new(format!(
            "the page has more than {} elements and text nodes",
            dom::MAX_NODES
        ))),
        Err(_) => Err(ParseError::new("the HTML parser failed on this page")),
    }
}

/// Decodes the bytes of a page (see the [module docs](self)).
fn decode(data: &[u8]) -> Decoded<'_> {
    if let Some(decoded) = decode_bom(data) {
        return decoded;
    }
    let xml_label = || {
        super::fb2::encoding::declared_label(data).map(|l| String::from_utf8_lossy(l).into_owned())
    };
    decode_declared(data, prescan(data).or_else(xml_label))
}

/// Elements whose content HTML parses as raw text, so that `<name/>` would swallow
/// the rest of the file.
const RAW_TEXT: &[&str] = &[
    "title", "script", "style", "textarea", "iframe", "noscript", "noembed", "noframes", "xmp",
];

/// In an XHTML file (an XML declaration or the XHTML namespace near the start),
/// `<title/>` and the like become `<title></title>`; other text is returned as is.
fn expand_self_closed(text: &str) -> Cow<'_, str> {
    let head = &text[..text.floor_char_boundary(PRESCAN_LIMIT)];
    let xhtml =
        head.trim_start().starts_with("<?xml") || head.contains("http://www.w3.org/1999/xhtml");
    if !xhtml || !text.contains("/>") {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 64);
    let mut rest = text;
    while let Some(at) = rest.find('<') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let name_end = rest[1..]
            .find(|c: char| !c.is_ascii_alphanumeric())
            .map_or(rest.len(), |i| i + 1);
        let name = rest[1..name_end].to_ascii_lowercase();
        let tag_end = rest.find('>');
        match tag_end {
            Some(end)
                if RAW_TEXT.contains(&name.as_str())
                    && rest[..end].ends_with('/')
                    && rest[name_end..].starts_with(|c: char| c.is_whitespace() || c == '/') =>
            {
                out.push_str(rest[..end - 1].trim_end());
                out.push_str("></");
                out.push_str(&name);
                out.push('>');
                rest = &rest[end + 1..];
            }
            _ => {
                out.push('<');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// The `charset` of the first `<meta>` declaring one in the first [`PRESCAN_LIMIT`]
/// bytes (`<meta charset=...>` or `<meta http-equiv="Content-Type" content="...;
/// charset=...">`).
fn prescan(data: &[u8]) -> Option<String> {
    let head = &data[..data.len().min(PRESCAN_LIMIT)];
    let lower = head.to_ascii_lowercase();
    let mut from = 0;
    while let Some(start) = find(&lower[from..], b"<meta").map(|i| i + from) {
        let end = find(&lower[start..], b">").map_or(lower.len(), |i| i + start);
        let tag = &lower[start..end];
        from = end;
        let Some(at) = find(tag, b"charset") else {
            continue;
        };
        let rest = tag[at + b"charset".len()..].trim_ascii_start();
        let Some(rest) = rest.strip_prefix(b"=") else {
            continue;
        };
        let rest = rest.trim_ascii_start();
        let rest = rest
            .strip_prefix(b"\"")
            .or_else(|| rest.strip_prefix(b"'"))
            .unwrap_or(rest);
        let label: Vec<u8> = rest
            .iter()
            .copied()
            .take_while(|b| !matches!(b, b'"' | b'\'' | b';' | b'/' | b' ' | b'\t' | b'\n' | b'\r'))
            .collect();
        if !label.is_empty() {
            return Some(String::from_utf8_lossy(&label).into_owned());
        }
    }
    None
}

/// The position of the first occurrence of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Section cuts at the headings (see the [module docs](self)).
fn cuts(items: &[Item]) -> Vec<Cut> {
    let key = |item: &Item| match item {
        Item::Heading(level, block) => {
            let part = super::txt::heading_rank(&block.plain_text()) == Some(1);
            Some((*level, u8::from(!part)))
        }
        Item::Block(_) => None,
    };
    let mut keys: Vec<(u8, u8)> = items.iter().filter_map(key).collect();
    keys.sort_unstable();
    // A single heading of the top rank that is not part-like (the book's title in an
    // `<h1>`) is not a cut: it would put the whole book into one section.
    let single_top = match keys.as_slice() {
        [first, second, ..] if first != second && first.1 == 1 => Some(*first),
        _ => None,
    };
    keys.dedup();
    if let Some(top) = single_top {
        keys.retain(|k| *k != top);
    }
    let mut cuts = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let Some(this) = key(item) else {
            continue;
        };
        let continues = index
            .checked_sub(1)
            .and_then(|i| key(&items[i]))
            .is_some_and(|previous| previous < this)
            && super::txt::heading_rank(&item.block().plain_text()).is_none();
        if !continues && Some(this) != single_top {
            cuts.push(Cut {
                depth: keys.iter().position(|k| *k == this).unwrap_or(0) + 1,
                label: None,
                index,
            });
        }
    }
    cuts
}

/// The metadata of a page (see the [module docs](self)).
fn metadata(dom: &Dom) -> BookMetadata {
    let mut metas: Vec<(String, String)> = Vec::new();
    for id in dom.descendants(dom::DOCUMENT) {
        if dom.name(id) != Some("meta") {
            continue;
        }
        if let (Some(name), Some(content)) = (dom.attr(id, "name"), dom.attr(id, "content")) {
            let content = normalize_space(content);
            if !content.is_empty() {
                metas.push((name.trim().to_ascii_lowercase(), content));
            }
        }
    }
    let meta = |names: &[&str]| -> Vec<&String> {
        metas
            .iter()
            .filter(|(n, _)| names.contains(&n.as_str()))
            .map(|(_, c)| c)
            .collect()
    };
    let (title_author, title) = dom
        .find("title")
        .map(|id| split_title(&text_of(dom, id)))
        .unwrap_or_default();
    let title = meta(&["dc.title", "dcterms.title"])
        .first()
        .map(|t| t.to_string())
        .unwrap_or(title);
    let mut authors: Vec<Person> = meta(&["dc.creator", "dcterms.creator", "author"])
        .into_iter()
        .filter_map(|creator| super::epub::person(creator, Some(creator)))
        .collect();
    authors.dedup();
    if authors.is_empty() {
        authors.extend(title_author.and_then(|a| super::epub::person(&a, None)));
    }
    let language = dom
        .find("html")
        .and_then(|html| dom.attr(html, "lang"))
        .map(str::to_owned)
        .or_else(|| {
            meta(&["dc.language", "dcterms.language"])
                .first()
                .map(|l| l.to_string())
        })
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty() && l.len() <= 35 && !l.contains(char::is_whitespace));
    let annotation = meta(&["description", "dc.description", "dcterms.description"])
        .first()
        .map(|d| Annotation::new(vec![Block::paragraph(d.as_str())]))
        .unwrap_or_default();
    BookMetadata {
        title,
        authors,
        language,
        annotation,
        ..BookMetadata::default()
    }
}

/// All text below `id`, whitespace-normalized.
fn text_of(dom: &Dom, id: Id) -> String {
    let mut text = String::new();
    for node in dom.descendants(id) {
        if let dom::Data::Text(t) = &dom.nodes[node].data {
            text.push_str(t);
            text.push(' ');
        }
    }
    normalize_space(&text)
}

/// The author (if the title names one) and the title of a `<title>`.
fn split_title(raw: &str) -> (Option<String>, String) {
    let mut title = raw.trim();
    for prefix in ["lib.ru:", "lib.ru :"] {
        if title
            .get(..prefix.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
        {
            title = title[prefix.len()..].trim_start();
        }
    }
    title = title
        .strip_suffix(" | Project Gutenberg")
        .unwrap_or(title)
        .trim_end();
    let gutenberg = "The Project Gutenberg eBook of ";
    if title
        .get(..gutenberg.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(gutenberg))
    {
        title = &title[gutenberg.len()..];
        if let Some((name, author)) = title.rsplit_once(", by ") {
            return (Some(author.trim().to_owned()), name.trim().to_owned());
        }
    }
    if let Some((name, author)) = title.rsplit_once(" by ")
        && looks_like_name(author)
        && !name.trim().is_empty()
    {
        return (
            Some(author.trim().to_owned()),
            name.trim().trim_end_matches(',').to_owned(),
        );
    }
    // `Author. Title`: the first ". " not after an initial.
    let mut from = 0;
    while let Some(at) = title[from..].find(". ").map(|i| i + from) {
        let word = title[..at].rsplit(' ').next().unwrap_or_default();
        if word.chars().count() >= 2 {
            let (author, name) = (&title[..at], title[at + 2..].trim());
            if looks_like_name(author) && !name.is_empty() {
                return (Some(author.trim().to_owned()), name.to_owned());
            }
            break;
        }
        from = at + 2;
    }
    (None, title.to_owned())
}

/// One to four words (initials included), each starting with an uppercase letter.
fn looks_like_name(text: &str) -> bool {
    let words: Vec<&str> = text.split_whitespace().collect();
    (1..=4).contains(&words.len())
        && words.iter().all(|w| {
            w.chars().next().is_some_and(char::is_uppercase)
                && w.chars()
                    .all(|c| c.is_alphabetic() || matches!(c, '.' | '-' | '\''))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_charset_prescan() {
        for (head, label) in [
            ("<meta charset=\"windows-1251\">", Some("windows-1251")),
            ("<META CHARSET=koi8-r>", Some("koi8-r")),
            (
                "<meta http-equiv=\"Content-Type\" content=\"text/html; charset=Windows-1251\">",
                Some("windows-1251"),
            ),
            (
                "<meta name=x content=y><meta charset='utf-8'/>",
                Some("utf-8"),
            ),
            ("<meta name=description content=charset>", None),
            ("<p>charset=koi8-r</p>", None),
        ] {
            assert_eq!(prescan(head.as_bytes()).as_deref(), label, "{head}");
        }
        let far = format!("{}<meta charset=koi8-r>", " ".repeat(PRESCAN_LIMIT));
        assert_eq!(prescan(far.as_bytes()), None);
    }

    #[test]
    fn self_closed_raw_text_elements_in_xhtml() {
        let xhtml = "<?xml version=\"1.0\"?><html><head><title/><SCRIPT src=\"a.js\" />\
                     <style/></head><body><br/><p/>x</body></html>";
        assert_eq!(
            expand_self_closed(xhtml),
            "<?xml version=\"1.0\"?><html><head><title></title><SCRIPT src=\"a.js\"></script>\
             <style></style></head><body><br/><p/>x</body></html>"
        );
        let html = "<html><head><title/></head></html>";
        assert!(
            matches!(expand_self_closed(html), Cow::Borrowed(_)),
            "plain HTML is left alone"
        );
        assert_eq!(
            expand_self_closed("<?xml?><titles/><title"),
            "<?xml?><titles/><title"
        );
    }

    #[test]
    fn titles() {
        let cases: &[(&str, Option<&str>, &str)] = &[
            (
                "Александр Сергеевич Пушкин. Дубровский",
                Some("Александр Сергеевич Пушкин"),
                "Дубровский",
            ),
            (
                "Lib.ru: Михаил Булгаков. Мастер и Маргарита",
                Some("Михаил Булгаков"),
                "Мастер и Маргарита",
            ),
            (
                "А. С. Пушкин. Капитанская дочка",
                Some("А. С. Пушкин"),
                "Капитанская дочка",
            ),
            (
                "The Project Gutenberg eBook of War and Peace, by Leo Tolstoy",
                Some("Leo Tolstoy"),
                "War and Peace",
            ),
            (
                "Pride and prejudice | Project Gutenberg",
                None,
                "Pride and prejudice",
            ),
            (
                "Alice's Adventures in Wonderland by Lewis Carroll",
                Some("Lewis Carroll"),
                "Alice's Adventures in Wonderland",
            ),
            ("Stand by me", None, "Stand by me"),
            ("Leo Tolstoy - Wikipedia", None, "Leo Tolstoy - Wikipedia"),
            ("Глава 1. Начало пути", None, "Глава 1. Начало пути"),
        ];
        for (raw, author, title) in cases {
            let (found_author, found_title) = split_title(raw);
            assert_eq!(
                (found_author.as_deref(), found_title.as_str()),
                (*author, *title),
                "{raw}"
            );
        }
    }
}
