//! WebAssembly bindings of `shelfgauge-core` for the hub's reader app.
//!
//! The reader app decrypts and inflates a publication's blobs in JavaScript (WebCrypto,
//! `DecompressionStream`) and hands the plaintext here: this crate decodes the
//! manifest and the parts and renders them with the same code as the instance, so
//! untrusted data becomes escaped HTML. The boundary carries only bytes, strings and
//! JSON strings.
//!
//! Built with `scripts/build-wasm.sh` into `assets/hub/pkg/` (committed); natively it
//! builds as a plain library for the tests.

#![warn(missing_docs)]

pub mod book;

use wasm_bindgen::prelude::*;

/// An opened publication (see [`book::Book`]).
#[wasm_bindgen]
pub struct Reader(book::Book);

#[wasm_bindgen]
impl Reader {
    /// Opens a publication from its decoded manifest bytes.
    #[wasm_bindgen(constructor)]
    pub fn new(manifest: &[u8]) -> Result<Reader, JsError> {
        book::Book::open(manifest).map(Reader).map_err(error)
    }

    /// The manifest's figures, as JSON.
    pub fn summary(&self) -> String {
        self.0.summary()
    }

    /// The number of parts.
    #[wasm_bindgen(js_name = partCount)]
    pub fn part_count(&self) -> usize {
        self.0.part_count()
    }

    /// Adds the decoded blob of part `index` (0-based).
    #[wasm_bindgen(js_name = addPart)]
    pub fn add_part(&mut self, index: usize, bytes: &[u8]) -> Result<(), JsError> {
        self.0.add_part(index, bytes).map_err(error)
    }

    /// The HTML of part `index`.
    #[wasm_bindgen(js_name = renderPart)]
    pub fn render_part(&self, index: usize) -> Result<String, JsError> {
        self.0.render_part(index).map_err(error)
    }

    /// The table of contents with part `current` marked.
    #[wasm_bindgen(js_name = renderToc)]
    pub fn render_toc(&self, current: usize) -> String {
        self.0.render_toc(current)
    }
}

fn error(e: shelfgauge_core::publication::FormatError) -> JsError {
    JsError::new(&e.to_string())
}
