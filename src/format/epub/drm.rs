//! Recognizing DRM-protected EPUBs (never decrypting them).
//!
//! An EPUB is protected when `META-INF/encryption.xml` lists an encryption algorithm
//! other than font obfuscation, or when a Readium LCP license (`license.lcpl`) or an
//! Apple FairPlay `sinf.xml` is present. Font obfuscation (IDPF and Adobe algorithms)
//! is part of the EPUB standard, not DRM: such books are ordinary books, and the
//! obfuscated fonts are never used. `rights.xml` alone does not make a book protected;
//! it only names the scheme (Adobe ADEPT) when resources are encrypted.

use std::collections::HashSet;

use super::package::LocalName;
use super::paths::resolve;
use super::xml::with_document;
use crate::format::ParseError;
use crate::format::zipbook::ZipBook as Archive;

/// Font obfuscation algorithms (EPUB 3.3 §4.4.5 and Adobe's predecessor).
const FONT_OBFUSCATION: &[&str] = &[
    "http://www.idpf.org/2008/embedding",
    "http://ns.adobe.com/pdf/enc#RC",
];

/// What the container says about encryption.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Protection {
    /// The DRM scheme, if the book is protected (e.g. `Adobe ADEPT`).
    pub scheme: Option<String>,
    /// Entry paths of all encrypted or obfuscated resources.
    pub encrypted: HashSet<String>,
}

/// Inspects the `META-INF` files of the container.
pub(super) fn detect(archive: &mut Archive<'_>) -> Result<Protection, ParseError> {
    let mut protection = Protection::default();
    let mut drm_encryption = false;
    if let Some(data) = archive.read("META-INF/encryption.xml")? {
        let entries = with_document(&data, |doc| {
            doc.descendants()
                .filter(|n| n.has_tag_name_local("EncryptedData"))
                .map(|data| {
                    let algorithm = data
                        .descendants()
                        .find(|n| n.has_tag_name_local("EncryptionMethod"))
                        .and_then(|n| n.attribute("Algorithm"))
                        .map(|a| a.trim().to_owned());
                    let uri = data
                        .descendants()
                        .find(|n| n.has_tag_name_local("CipherReference"))
                        .and_then(|n| n.attribute("URI"))
                        .map(str::to_owned);
                    (algorithm, uri)
                })
                .collect::<Vec<_>>()
        })
        // Unreadable encryption data: assume the worst.
        .unwrap_or_else(|_| vec![(None, None)]);
        for (algorithm, uri) in entries {
            if !algorithm
                .as_deref()
                .is_some_and(|a| FONT_OBFUSCATION.contains(&a))
            {
                drm_encryption = true;
            }
            // CipherReference URIs are relative to the container root.
            if let Some((path, _)) = uri.as_deref().and_then(|u| resolve("", u)) {
                protection.encrypted.insert(path);
            }
        }
    }
    let lcp = archive.contains("META-INF/license.lcpl") || archive.contains("license.lcpl");
    let fairplay = archive.contains("META-INF/sinf.xml");
    protection.scheme = if lcp {
        Some("Readium LCP".to_owned())
    } else if fairplay {
        Some("Apple FairPlay".to_owned())
    } else if drm_encryption && archive.contains("META-INF/rights.xml") {
        Some("Adobe ADEPT".to_owned())
    } else if drm_encryption {
        Some("unknown DRM".to_owned())
    } else {
        None
    };
    Ok(protection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::zipbook::zip_bytes;

    fn encryption(algorithm: &str, uri: &str) -> Vec<u8> {
        format!(
            r#"<encryption xmlns="urn:oasis:names:tc:opendocument:xmlns:container" xmlns:enc="http://www.w3.org/2001/04/xmlenc#">
  <enc:EncryptedData><enc:EncryptionMethod Algorithm="{algorithm}"/>
    <enc:CipherData><enc:CipherReference URI="{uri}"/></enc:CipherData></enc:EncryptedData>
</encryption>"#
        )
        .into_bytes()
    }

    fn detect_in(entries: &[(&str, &[u8])]) -> Protection {
        let data = zip_bytes(entries);
        detect(&mut Archive::open(&data, "EPUB").unwrap()).unwrap()
    }

    #[test]
    fn font_obfuscation_is_not_drm() {
        let xml = encryption("http://www.idpf.org/2008/embedding", "OEBPS/Fonts/a.otf");
        let protection = detect_in(&[("META-INF/encryption.xml", &xml)]);
        assert_eq!(protection.scheme, None);
        assert!(protection.encrypted.contains("OEBPS/Fonts/a.otf"));
        let xml = encryption("http://ns.adobe.com/pdf/enc#RC", "b.otf");
        assert_eq!(detect_in(&[("META-INF/encryption.xml", &xml)]).scheme, None);
    }

    #[test]
    fn encrypted_content_is_drm() {
        let xml = encryption(
            "http://www.w3.org/2001/04/xmlenc#aes128-cbc",
            "OEBPS/ch1.xhtml",
        );
        let adept = detect_in(&[
            ("META-INF/encryption.xml", &xml),
            ("META-INF/rights.xml", b"<rights/>"),
        ]);
        assert_eq!(adept.scheme.as_deref(), Some("Adobe ADEPT"));
        assert!(adept.encrypted.contains("OEBPS/ch1.xhtml"));
        let unknown = detect_in(&[("META-INF/encryption.xml", &xml)]);
        assert_eq!(unknown.scheme.as_deref(), Some("unknown DRM"));
        let lcp = detect_in(&[("META-INF/license.lcpl", b"{}")]);
        assert_eq!(lcp.scheme.as_deref(), Some("Readium LCP"));
        let fairplay = detect_in(&[("META-INF/sinf.xml", b"<sinf/>")]);
        assert_eq!(fairplay.scheme.as_deref(), Some("Apple FairPlay"));
    }

    #[test]
    fn rights_alone_or_nothing_is_not_drm() {
        assert_eq!(detect_in(&[("META-INF/rights.xml", b"<r/>")]).scheme, None);
        assert_eq!(
            detect_in(&[("mimetype", b"application/epub+zip")]),
            Protection::default()
        );
    }

    #[test]
    fn unreadable_encryption_data_counts_as_drm() {
        let protection = detect_in(&[("META-INF/encryption.xml", b"<<<")]);
        assert!(protection.scheme.is_some());
    }
}
