//! The EPUB format (EPUB 2.0.1 and EPUB 3.x).
//!
//! An EPUB is a zip container (OCF): `META-INF/container.xml` names the package
//! document (OPF) with the metadata, the manifest of resources and the spine (the
//! reading order of the XHTML content documents). The parser reads the container
//! itself from the bytes it is given, with limits (see `archive`).
//!
//! Parsing is defensive like FB2: elements are matched by local name, unknown elements
//! keep their text, and no input makes the parser panic. CSS, scripts, embedded fonts
//! and remote resources are ignored; images are used only if they are of a known
//! raster type.
//!
//! DRM-protected books are recognized, never decrypted: [`EpubParser::parse_metadata`]
//! returns the readable metadata with [`BookMetadata::drm`] set, and
//! [`EpubParser::parse_content`] refuses them.

use super::model::{BookContent, BookMetadata};
use super::{BookParser, ParseError};

mod archive;
mod content;
mod drm;
mod entities;
mod metadata;
mod nav;
mod package;
mod paths;
mod xml;

/// The EPUB parser.
#[derive(Debug, Clone, Copy, Default)]
pub struct EpubParser;

impl BookParser for EpubParser {
    fn format(&self) -> &'static str {
        "epub"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["epub"]
    }

    /// Reads the package metadata and the cover; spine documents are not read.
    ///
    /// For a DRM-protected book whose package document cannot be read, the metadata
    /// is empty apart from [`BookMetadata::drm`].
    fn parse_metadata(&self, data: &[u8]) -> Result<BookMetadata, ParseError> {
        let mut archive = archive::Archive::open(data)?;
        let protection = drm::detect(&mut archive)?;
        match package::read(&mut archive) {
            Ok(package) => Ok(metadata::build(&package, &mut archive, &protection)),
            Err(_) if protection.scheme.is_some() => Ok(BookMetadata {
                drm: protection.scheme,
                ..BookMetadata::default()
            }),
            Err(e) => Err(e),
        }
    }

    /// Converts the spine documents; a DRM-protected book is an error.
    fn parse_content(&self, data: &[u8]) -> Result<BookContent, ParseError> {
        let mut archive = archive::Archive::open(data)?;
        let protection = drm::detect(&mut archive)?;
        if let Some(scheme) = &protection.scheme {
            return Err(ParseError::new(format!(
                "the book is protected by DRM ({scheme})"
            )));
        }
        let package = package::read(&mut archive)?;
        content::parse(&package, &mut archive, &protection)
    }
}
