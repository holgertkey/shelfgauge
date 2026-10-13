//! Annotation anchors against real rendering: the server's block text must equal what
//! a browser sees as the `textContent` of each `id="b<n>"` element.

use std::collections::HashMap;

use shelfgauge::annotations::anchor::{Anchor, find, quote};
use shelfgauge::annotations::text::BookText;
use shelfgauge::config::{ReaderConfig, SplitMode};
use shelfgauge::format::fb2::Fb2Parser;
use shelfgauge::format::model::{BlockId, BookContent};
use shelfgauge::format::{BookParser, run_with_parser_stack};
use shelfgauge::render::html::{ReaderLinks, render_part};
use shelfgauge::render::split::split;

const FULL: &[u8] = include_bytes!("fixtures/fb2/full.fb2");

/// A book with what `full.fb2` lacks: a captioned image, an inline image, astral
/// characters, entities and line breaks in a date.
const EXTRA: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<FictionBook xmlns:l="http://www.w3.org/1999/xlink"><description><title-info>
<book-title>Extra</book-title></title-info></description>
<body><section><title><p>Chapter 😀</p></title>
<image l:href="#pic" title="A &lt;caption&gt; &amp; more"/>
<image l:href="#pic" title="   "/>
<p>Smile 😀 and <image l:href="#pic"/> an inline image, <a l:href="#n1" type="note">[1]</a>.</p>
<poem><title><p>Poem</p></title><stanza><v>Line &quot;one&quot;</v><v>Line 'two'</v></stanza>
<date>1 Jan
1900</date></poem>
<table><tr><th>H1</th><th align="right">H2</th></tr><tr><td>c1</td><td>c<emphasis>2</emphasis></td></tr></table>
</section></body>
<body name="notes"><section id="n1"><title><p>1</p></title><p>The note 𝄞.</p></section></body>
<binary id="pic" content-type="image/png">iVBORw0KGgo=</binary>
</FictionBook>"##;

struct Links;

impl ReaderLinks for Links {
    fn block(&self, part: usize, id: BlockId) -> String {
        format!("?part={}#{id}", part + 1)
    }

    fn image(&self, id: &str) -> String {
        format!("/image/{id}")
    }
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// The text content of every element with an `id` in renderer output (which has no
/// comments, scripts or void elements other than `img`).
fn text_by_id(html: &str) -> HashMap<String, String> {
    let mut texts: HashMap<String, String> = HashMap::new();
    let mut open: Vec<Option<String>> = Vec::new();
    let mut rest = html;
    while !rest.is_empty() {
        if let Some(tag_rest) = rest.strip_prefix('<') {
            let end = tag_rest.find('>').expect("closed tag");
            let tag = &tag_rest[..end];
            rest = &tag_rest[end + 1..];
            if tag.starts_with('/') {
                open.pop();
            } else if !tag.starts_with("img") {
                let id = tag
                    .split_once("id=\"")
                    .map(|(_, r)| r[..r.find('"').unwrap()].to_owned());
                if let Some(id) = &id {
                    texts.entry(id.clone()).or_default();
                }
                open.push(id);
            }
        } else {
            let end = rest.find('<').unwrap_or(rest.len());
            let text = unescape(&rest[..end]);
            for id in open.iter().flatten() {
                texts.get_mut(id).unwrap().push_str(&text);
            }
            rest = &rest[end..];
        }
    }
    texts
}

fn check_book(content: &BookContent) {
    let book = BookText::new(content);
    for config in [
        ReaderConfig {
            split: SplitMode::None,
            ..ReaderConfig::default()
        },
        ReaderConfig::default(),
    ] {
        let parts = split(content, &config);
        let mut seen = 0;
        for index in 0..parts.len() {
            let html = render_part(content, &parts, index, &Links);
            for (id, text) in text_by_id(&html) {
                let block: BlockId = id.parse().expect("only block ids");
                assert_eq!(book.text(block), Some(text.as_str()), "block {id}");
                seen += 1;
            }
        }
        assert_eq!(seen, content.block_count(), "every block rendered once");
    }
}

#[test]
fn server_block_text_equals_rendered_text_content() {
    run_with_parser_stack(|| {
        check_book(&Fb2Parser.parse_content(FULL).unwrap());
        let extra = Fb2Parser.parse_content(EXTRA.as_bytes()).unwrap();
        check_book(&extra);
        let book = BookText::new(&extra);
        let texts: Vec<&str> = (1..=book.block_count() as u32)
            .map(|n| book.text(BlockId::new(n).unwrap()).unwrap())
            .collect();
        assert!(texts.contains(&"A <caption> & more"), "{texts:?}");
        assert!(
            texts.contains(&"1 Jan 1900") || texts.contains(&"1 Jan\n1900"),
            "{texts:?}"
        );
        assert!(texts.contains(&"H1H2c1c2"), "{texts:?}");
    });
}

#[test]
fn astral_characters_count_as_one_code_point() {
    run_with_parser_stack(|| {
        let content = Fb2Parser.parse_content(EXTRA.as_bytes()).unwrap();
        let book = BookText::new(&content);
        let smile = (1..=book.block_count() as u32)
            .map(|n| BlockId::new(n).unwrap())
            .find(|&id| book.text(id).is_some_and(|t| t.starts_with("Smile")))
            .unwrap();
        // "Smile 😀 and": the emoji is code point 6 (two UTF-16 units in a browser).
        let anchor = Anchor {
            start_block: smile,
            start_offset: 6,
            end_block: smile,
            end_offset: 11,
        };
        let q = quote(&book, &anchor).unwrap();
        assert_eq!(q.text, "😀 and");
        assert_eq!(find(&book, &q, smile), Some(anchor));
    });
}
