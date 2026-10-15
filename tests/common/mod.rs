//! Synthetic EPUB books shared by the integration tests.
//!
//! Each test binary includes this module with `mod common;` and uses what it needs.
#![allow(dead_code)]

use std::io::Write;

/// The 8-byte PNG signature plus an IHDR chunk start: enough to be sniffed as PNG.
pub const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0";

/// Zips `(name, data)` entries into an EPUB container (`mimetype` stored first, the
/// rest deflated).
pub fn zip(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, data) in entries {
        let method = if *name == "mimetype" {
            zip::CompressionMethod::Stored
        } else {
            zip::CompressionMethod::Deflated
        };
        out.start_file(
            *name,
            zip::write::SimpleFileOptions::default().compression_method(method),
        )
        .unwrap();
        out.write_all(data).unwrap();
    }
    out.finish().unwrap().into_inner()
}

/// `META-INF/container.xml` naming `OEBPS/content.opf`.
pub const CONTAINER: &str = r##"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"##;

/// An XHTML content document with `body`.
pub fn xhtml(body: &str) -> Vec<u8> {
    format!(
        r##"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.1//EN" "http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd">
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>T</title><link rel="stylesheet" href="style.css"/><script>alert(1)</script></head>
<body>{body}</body></html>"##
    )
    .into_bytes()
}

/// An EPUB 3 book: nav document, a cover page (non-linear), a title page without a
/// heading, a part without a heading of its own, footnote asides, endnotes, lists,
/// preformatted text, a rule, a table, an SVG-wrapped image, links between documents
/// and a non-linear appendix.
pub fn epub3_entries() -> Vec<(&'static str, Vec<u8>)> {
    let opf = r##"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">urn:uuid:1</dc:identifier>
    <dc:title id="sub">A Subtitle</dc:title>
    <meta refines="#sub" property="title-type">subtitle</meta>
    <dc:title id="main">Война и мир</dc:title>
    <meta refines="#main" property="title-type">main</meta>
    <dc:creator id="a1">Лев Николаевич Толстой</dc:creator>
    <meta refines="#a1" property="file-as">Толстой, Лев Николаевич</meta>
    <meta refines="#a1" property="role" scheme="marc:relators">aut</meta>
    <dc:creator id="a2">Some Illustrator</dc:creator>
    <meta refines="#a2" property="role" scheme="marc:relators">ill</meta>
    <meta property="belongs-to-collection" id="c1">Эпопея</meta>
    <meta refines="#c1" property="collection-type">series</meta>
    <meta refines="#c1" property="group-position">2</meta>
    <meta property="belongs-to-collection" id="c2">100 Best Novels</meta>
    <meta refines="#c2" property="collection-type">set</meta>
    <dc:subject>Historical fiction</dc:subject>
    <dc:subject>War stories</dc:subject>
    <dc:description>&lt;p&gt;An &lt;em&gt;epic&lt;/em&gt; novel.&lt;/p&gt;&lt;p&gt;Second&amp;nbsp;paragraph.&lt;/p&gt;</dc:description>
    <dc:language>ru</dc:language>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="cover-img" href="img/cover.png" media-type="image/png" properties="cover-image"/>
    <item id="pic" href="img/pic.png" media-type="image/png"/>
    <item id="svgpic" href="img/drawing.svg" media-type="image/svg+xml"/>
    <item id="cover" href="text/cover.xhtml" media-type="application/xhtml+xml"/>
    <item id="title" href="text/title.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch1" href="text/ch1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch2" href="text/ch2.xhtml" media-type="application/xhtml+xml"/>
    <item id="notes" href="text/notes.xhtml" media-type="application/xhtml+xml"/>
    <item id="appendix" href="text/appendix.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="cover" linear="no"/>
    <itemref idref="title"/>
    <itemref idref="ch1"/>
    <itemref idref="ch2"/>
    <itemref idref="notes"/>
    <itemref idref="appendix" linear="no"/>
  </spine>
</package>"##;
    let nav = r##"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body>
<nav epub:type="toc"><h1>Contents</h1><ol>
  <li><a href="text/title.xhtml">Title page</a></li>
  <li><span>Part One</span><ol>
    <li><a href="text/ch1.xhtml">Chapter 1</a><ol>
      <li><a href="text/ch1.xhtml#s11">Chapter 1.1</a></li>
    </ol></li>
    <li><a href="text/ch2.xhtml#top">Chapter 2</a></li>
  </ol></li>
  <li><a href="text/notes.xhtml">Endnotes</a></li>
</ol></nav>
<nav epub:type="landmarks"><ol><li><a href="text/ch1.xhtml">Start</a></li></ol></nav>
</body></html>"##;
    vec![
        ("mimetype", b"application/epub+zip".to_vec()),
        ("META-INF/container.xml", CONTAINER.as_bytes().to_vec()),
        ("OEBPS/content.opf", opf.as_bytes().to_vec()),
        ("OEBPS/nav.xhtml", nav.as_bytes().to_vec()),
        ("OEBPS/img/cover.png", PNG.to_vec()),
        ("OEBPS/img/pic.png", PNG.to_vec()),
        (
            "OEBPS/img/drawing.svg",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".to_vec(),
        ),
        (
            "OEBPS/text/cover.xhtml",
            xhtml(
                r##"<div><svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><image xlink:href="../img/cover.png"/></svg></div>"##,
            ),
        ),
        (
            "OEBPS/text/title.xhtml",
            xhtml(r##"<p class="title">War&nbsp;and Peace</p>"##),
        ),
        (
            "OEBPS/text/ch1.xhtml",
            xhtml(
                r##"<section id="c1"><img src="../img/pic.png" alt="A picture"/>
<h1>Chapter 1</h1><h2>The Beginning</h2>
<p>Text with<br/>a break and a footnote<a epub:type="noteref" href="#fn1">1</a>.</p>
<aside epub:type="footnote" id="fn1"><p>Footnote one.</p></aside>
<ul><li>One<ul><li>Nested</li></ul></li><li><p>Two</p></li></ul>
<pre>  code
    indented</pre>
<hr/>
<div>Loose <em>inline</em> text<span epub:type="pagebreak" id="p5" title="5">5</span></div>
<p><img src="../img/drawing.svg" alt="svg"/></p>
<section id="s11"><h2>Chapter 1.1</h2><p>See <a href="ch2.xhtml#target">chapter 2</a>.</p></section>
</section>"##,
            ),
        ),
        (
            "OEBPS/text/ch2.xhtml",
            xhtml(
                r##"<h1 id="top">Chapter 2</h1>
<p>Here<span id="target"/> it is: <a href="https://example.org/">a site</a>, <a href="missing.xhtml">a lost link</a>, an endnote<sup><a href="notes.xhtml#en1" epub:type="noteref">2</a></sup>.</p>
<table><thead><tr><th>Head</th></tr></thead><tbody><tr><td><p>x</p><p>y</p></td></tr></tbody></table>
<div><svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><image xlink:href="../img/pic.png"/></svg></div>
<ol start="3"><li>Third</li></ol>"##,
            ),
        ),
        (
            "OEBPS/text/notes.xhtml",
            xhtml(
                r##"<section epub:type="endnotes" role="doc-endnotes"><h2>Endnotes</h2><ol>
<li id="en1" epub:type="endnote"><p>Endnote one. <a href="ch2.xhtml#target">↩</a></p></li>
</ol></section>"##,
            ),
        ),
        (
            "OEBPS/text/appendix.xhtml",
            xhtml("<h1>Appendix</h1><p>Extra text.</p>"),
        ),
    ]
}

/// The EPUB 3 book as bytes.
pub fn epub3() -> Vec<u8> {
    zip(&epub3_entries())
}

/// An EPUB 2 book: NCX, `opf:role` / `opf:file-as`, Calibre series, `meta
/// name="cover"`, HTML named entities, `.html` documents.
pub fn epub2_entries() -> Vec<(&'static str, Vec<u8>)> {
    let opf = r##"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
    <dc:title>Pride and Prejudice</dc:title>
    <dc:creator opf:role="aut" opf:file-as="Austen, Jane">Jane Austen</dc:creator>
    <dc:contributor opf:role="edt">An Editor</dc:contributor>
    <dc:description>Plain text description.</dc:description>
    <dc:language>en</dc:language>
    <meta name="calibre:series" content="Novels"/>
    <meta name="calibre:series_index" content="2.0"/>
    <meta name="cover" content="coverimage"/>
  </metadata>
  <manifest>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="coverimage" href="cover.png" media-type="image/png"/>
    <item id="c1" href="ch1.html" media-type="application/xhtml+xml"/>
    <item id="c2" href="ch2.html" media-type="application/xhtml+xml"/>
  </manifest>
  <spine toc="ncx"><itemref idref="c1"/><itemref idref="c2"/></spine>
</package>"##;
    let ncx = r##"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE ncx PUBLIC "-//NISO//DTD ncx 2005-1//EN" "http://www.daisy.org/z3986/2005/ncx-2005-1.dtd">
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1"><navMap>
<navPoint id="n1" playOrder="1"><navLabel><text>Chapter I.</text></navLabel><content src="ch1.html#c1"/></navPoint>
<navPoint id="n2" playOrder="2"><navLabel><text>Chapter II.</text></navLabel><content src="ch2.html"/></navPoint>
</navMap></ncx>"##;
    vec![
        ("mimetype", b"application/epub+zip".to_vec()),
        ("META-INF/container.xml", CONTAINER.as_bytes().to_vec()),
        ("OEBPS/content.opf", opf.as_bytes().to_vec()),
        ("OEBPS/toc.ncx", ncx.as_bytes().to_vec()),
        ("OEBPS/cover.png", PNG.to_vec()),
        (
            "OEBPS/ch1.html",
            xhtml(
                r##"<h2 id="c1">Chapter I.</h2><p>It is a truth&mdash;universally acknowledged&hellip; caf&eacute; &amp; more &unknown; Q&A</p>"##,
            ),
        ),
        (
            "OEBPS/ch2.html",
            xhtml(
                r##"<div class="chapter"><p class="chapter-title">CHAPTER II.</p><p>Mr. Bennet was among the earliest.</p></div>"##,
            ),
        ),
    ]
}

/// The EPUB 2 book as bytes.
pub fn epub2() -> Vec<u8> {
    zip(&epub2_entries())
}

/// `entries` with the entry `name` replaced (or added) or, with `None`, removed.
pub fn with_entry(
    mut entries: Vec<(&'static str, Vec<u8>)>,
    name: &'static str,
    data: Option<Vec<u8>>,
) -> Vec<(&'static str, Vec<u8>)> {
    entries.retain(|(n, _)| *n != name);
    if let Some(data) = data {
        entries.push((name, data));
    }
    entries
}
