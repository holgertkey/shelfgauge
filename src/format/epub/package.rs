//! The container document and the package document (OPF): manifest, spine and the raw
//! metadata elements.

use std::collections::HashMap;

use roxmltree::Node;

use super::archive::Archive;
use super::paths::resolve;
use super::xml::with_document;
use crate::format::ParseError;
use crate::format::xml::normalize_space;

/// The media type of a package document in `container.xml`.
const PACKAGE_MEDIA_TYPE: &str = "application/oebps-package+xml";

/// A resource listed in the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Item {
    /// The entry path in the container.
    pub path: String,
    /// The declared media type, lowercase.
    pub media_type: String,
    /// The `properties` tokens (EPUB 3), e.g. `nav`, `cover-image`.
    pub properties: Vec<String>,
}

/// A spine entry: a content document in reading order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SpineItem {
    /// The entry path of the document.
    pub path: String,
    /// `false` for `linear="no"` (auxiliary content read out of order).
    pub linear: bool,
}

/// A `<meta>` element of the package metadata (EPUB 3 `property` form or EPUB 2
/// `name`/`content` form).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Meta {
    /// `id` attribute.
    pub id: Option<String>,
    /// `property` (EPUB 3).
    pub property: Option<String>,
    /// `refines`, without the leading `#`.
    pub refines: Option<String>,
    /// `name` (EPUB 2).
    pub name: Option<String>,
    /// `content` (EPUB 2).
    pub content: Option<String>,
    /// The element text, whitespace-normalized.
    pub text: String,
}

/// A Dublin Core element (`dc:title`, `dc:creator`, ...).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct DcElement {
    /// `id` attribute.
    pub id: Option<String>,
    /// The element text, whitespace-normalized.
    pub text: String,
    /// The text as written (descriptions may contain markup as text).
    pub raw: String,
    /// `opf:role` (EPUB 2 creators).
    pub role: Option<String>,
    /// `opf:file-as` (EPUB 2 creators).
    pub file_as: Option<String>,
}

/// The parsed package document.
#[derive(Debug, Clone, Default)]
pub(super) struct Package {
    /// Manifest items by id.
    pub manifest: HashMap<String, Item>,
    /// The spine.
    pub spine: Vec<SpineItem>,
    /// The manifest id named by the spine's `toc` attribute (the NCX, EPUB 2).
    pub spine_toc: Option<String>,
    /// Dublin Core elements by local name (`title`, `creator`, ...), in order.
    pub dc: HashMap<String, Vec<DcElement>>,
    /// `<meta>` elements in order.
    pub metas: Vec<Meta>,
    /// `<guide>` references: `(type, path)`.
    pub guide: Vec<(String, String)>,
}

impl Package {
    /// The Dublin Core elements with local name `name`.
    pub(super) fn dc(&self, name: &str) -> &[DcElement] {
        self.dc.get(name).map_or(&[], Vec::as_slice)
    }

    /// The navigation document (EPUB 3): the manifest item with the `nav` property.
    pub(super) fn nav_path(&self) -> Option<&str> {
        self.manifest
            .values()
            .filter(|item| item.properties.iter().any(|p| p == "nav"))
            .map(|item| item.path.as_str())
            .min()
    }

    /// The NCX (EPUB 2): named by the spine's `toc`, else the item of its media type.
    pub(super) fn ncx_path(&self) -> Option<&str> {
        self.spine_toc
            .as_ref()
            .and_then(|id| self.manifest.get(id))
            .or_else(|| {
                self.manifest
                    .values()
                    .filter(|item| item.media_type == "application/x-dtbncx+xml")
                    .min_by(|a, b| a.path.cmp(&b.path))
            })
            .map(|item| item.path.as_str())
    }

    /// The text of the first meta refining the element with id `id` with property
    /// `property`.
    pub(super) fn refinement(&self, id: Option<&str>, property: &str) -> Option<&str> {
        self.refinements(id, property).into_iter().next()
    }

    /// The non-empty texts of all metas refining the element with id `id` with
    /// property `property`.
    pub(super) fn refinements(&self, id: Option<&str>, property: &str) -> Vec<&str> {
        let Some(id) = id else {
            return Vec::new();
        };
        self.metas
            .iter()
            .filter(|meta| {
                meta.refines.as_deref() == Some(id) && meta.property.as_deref() == Some(property)
            })
            .map(|meta| meta.text.as_str())
            .filter(|text| !text.is_empty())
            .collect()
    }
}

/// Reads `META-INF/container.xml` and the package document it names.
pub(super) fn read(archive: &mut Archive<'_>) -> Result<Package, ParseError> {
    let container = archive
        .read("META-INF/container.xml")?
        .ok_or_else(|| ParseError::new("not an EPUB: META-INF/container.xml is missing"))?;
    let path = with_document(&container, |doc| {
        let rootfiles: Vec<Node<'_, '_>> = doc
            .descendants()
            .filter(|n| n.has_tag_name_local("rootfile"))
            .collect();
        rootfiles
            .iter()
            .find(|n| {
                n.attribute("media-type")
                    .is_some_and(|t| t.trim().eq_ignore_ascii_case(PACKAGE_MEDIA_TYPE))
            })
            .or(rootfiles.first())
            .and_then(|n| n.attribute("full-path"))
            .and_then(|p| resolve("", p))
            .map(|(path, _)| path)
    })
    .map_err(|e| ParseError::new(format!("META-INF/container.xml: {e}")))?
    .ok_or_else(|| ParseError::new("META-INF/container.xml names no package document"))?;
    let opf = archive
        .read(&path)?
        .ok_or_else(|| ParseError::new(format!("the package document {path} is missing")))?;
    with_document(&opf, |doc| parse(doc.root_element(), path.clone()))
        .map_err(|e| ParseError::new(format!("{path}: {e}")))
}

/// Converts the `<package>` element.
fn parse(root: Node<'_, '_>, path: String) -> Package {
    let base = path.as_str();
    let mut package = Package::default();
    let section = |name: &str| root.children().find(|n| n.has_tag_name_local(name));
    if let Some(manifest) = section("manifest") {
        for item in manifest.children().filter(|n| n.has_tag_name_local("item")) {
            let (Some(id), Some(href)) = (item.attribute("id"), item.attribute("href")) else {
                continue;
            };
            let Some((item_path, _)) = resolve(base, href) else {
                continue;
            };
            package
                .manifest
                .entry(id.trim().to_owned())
                .or_insert(Item {
                    path: item_path,
                    media_type: item
                        .attribute("media-type")
                        .unwrap_or_default()
                        .trim()
                        .to_ascii_lowercase(),
                    properties: item
                        .attribute("properties")
                        .unwrap_or_default()
                        .split_whitespace()
                        .map(str::to_owned)
                        .collect(),
                });
        }
    }
    if let Some(spine) = section("spine") {
        package.spine_toc = spine.attribute("toc").map(|t| t.trim().to_owned());
        for itemref in spine.children().filter(|n| n.has_tag_name_local("itemref")) {
            let Some(item) = itemref
                .attribute("idref")
                .and_then(|id| package.manifest.get(id.trim()))
            else {
                continue;
            };
            package.spine.push(SpineItem {
                path: item.path.clone(),
                linear: itemref.attribute("linear").map(str::trim) != Some("no"),
            });
        }
    }
    if let Some(metadata) = section("metadata") {
        // EPUB 2 allows the elements inside `<dc-metadata>` / `<x-metadata>`.
        for node in metadata.descendants().filter(Node::is_element) {
            let name = node.tag_name().name();
            if name == "meta" {
                package.metas.push(Meta {
                    id: node.attribute("id").map(|v| v.trim().to_owned()),
                    property: node.attribute("property").map(|v| v.trim().to_owned()),
                    refines: node
                        .attribute("refines")
                        .map(|v| v.trim().trim_start_matches('#').to_owned()),
                    name: node.attribute("name").map(|v| v.trim().to_owned()),
                    content: node.attribute("content").map(normalize_space),
                    text: text(node),
                });
            } else if node.tag_name().namespace() == Some(DC_NS) {
                let attr = |local: &str| {
                    node.attributes()
                        .find(|a| a.name() == local)
                        .map(|a| normalize_space(a.value()))
                        .filter(|v| !v.is_empty())
                };
                let raw: String = node
                    .descendants()
                    .filter(Node::is_text)
                    .filter_map(|n| n.text())
                    .collect();
                package
                    .dc
                    .entry(name.to_owned())
                    .or_default()
                    .push(DcElement {
                        id: node.attribute("id").map(|v| v.trim().to_owned()),
                        text: normalize_space(&raw),
                        raw,
                        role: attr("role"),
                        file_as: attr("file-as"),
                    });
            }
        }
    }
    if let Some(guide) = section("guide") {
        for reference in guide
            .children()
            .filter(|n| n.has_tag_name_local("reference"))
        {
            if let (Some(kind), Some((path, _))) = (
                reference.attribute("type"),
                reference.attribute("href").and_then(|h| resolve(base, h)),
            ) {
                package.guide.push((kind.trim().to_ascii_lowercase(), path));
            }
        }
    }
    package
}

/// The Dublin Core namespace.
const DC_NS: &str = "http://purl.org/dc/elements/1.1/";

/// The whitespace-normalized text of `node`.
fn text(node: Node<'_, '_>) -> String {
    let raw: String = node
        .descendants()
        .filter(Node::is_text)
        .filter_map(|n| n.text())
        .collect();
    normalize_space(&raw)
}

/// Matching elements by local name, whatever their namespace.
pub(super) trait LocalName {
    /// Whether this is an element with the local name `name`.
    fn has_tag_name_local(&self, name: &str) -> bool;
}

impl LocalName for Node<'_, '_> {
    fn has_tag_name_local(&self, name: &str) -> bool {
        self.is_element() && self.tag_name().name() == name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::epub::archive::zip_bytes;

    const CONTAINER: &[u8] = br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;

    const OPF: &[u8] = br#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf" version="2.0">
  <metadata>
    <dc:title>  The   Title </dc:title>
    <dc:creator opf:role="aut" opf:file-as="Doe, John">John Doe</dc:creator>
    <meta name="cover" content="cover-img"/>
  </metadata>
  <manifest>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="c1" href="Text/ch%201.xhtml" media-type="application/xhtml+xml"/>
    <item id="c2" href="Text/notes.xhtml" media-type="application/xhtml+xml"/>
    <item id="cover-img" href="Images/cover.jpg" media-type="image/jpeg"/>
  </manifest>
  <spine toc="ncx"><itemref idref="c1"/><itemref idref="c2" linear="no"/><itemref idref="nope"/></spine>
  <guide><reference type="cover" href="Text/ch%201.xhtml"/></guide>
</package>"#;

    #[test]
    fn container_and_package_are_read() {
        let data = zip_bytes(&[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", OPF),
        ]);
        let mut archive = Archive::open(&data).unwrap();
        let package = read(&mut archive).unwrap();
        assert_eq!(
            package.spine,
            [
                SpineItem {
                    path: "OEBPS/Text/ch 1.xhtml".into(),
                    linear: true
                },
                SpineItem {
                    path: "OEBPS/Text/notes.xhtml".into(),
                    linear: false
                },
            ]
        );
        assert_eq!(package.ncx_path(), Some("OEBPS/toc.ncx"));
        assert_eq!(package.nav_path(), None);
        assert_eq!(package.dc("title")[0].text, "The Title");
        let creator = &package.dc("creator")[0];
        assert_eq!(creator.role.as_deref(), Some("aut"));
        assert_eq!(creator.file_as.as_deref(), Some("Doe, John"));
        assert_eq!(package.metas[0].content.as_deref(), Some("cover-img"));
        assert_eq!(
            package.guide,
            [("cover".into(), "OEBPS/Text/ch 1.xhtml".into())]
        );
    }

    #[test]
    fn missing_container_or_package_is_an_error() {
        let data = zip_bytes(&[("mimetype", b"application/epub+zip")]);
        let err = read(&mut Archive::open(&data).unwrap()).unwrap_err();
        assert!(err.message.contains("container.xml"), "{err}");
        let data = zip_bytes(&[("META-INF/container.xml", CONTAINER)]);
        let err = read(&mut Archive::open(&data).unwrap()).unwrap_err();
        assert!(err.message.contains("OEBPS/content.opf"), "{err}");
    }
}
