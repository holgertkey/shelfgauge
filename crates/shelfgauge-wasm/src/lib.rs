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

use shelfgauge_core::annotations::anchor::Anchor;
use shelfgauge_core::format::model::BlockId;
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

    /// Whether every part has been added.
    pub fn complete(&self) -> bool {
        self.0.complete()
    }

    /// Builds the book's text from all parts (quotes, positions, lists, search).
    #[wasm_bindgen(js_name = loadText)]
    pub fn load_text(&mut self) -> Result<(), JsError> {
        self.0.load_text().map_err(error)
    }

    /// The 0-based part of block `block` (`"b12"`).
    #[wasm_bindgen(js_name = partOf)]
    pub fn part_of(&self, block: &str) -> Option<u32> {
        self.0
            .part_of(block_id(block).ok()?)
            .and_then(|part| u32::try_from(part).ok())
    }

    /// The quote of a selection `{start_block, start_offset, end_block, end_offset}`
    /// (blocks as `"b12"`), as JSON `{text, prefix, suffix, section_title}`.
    pub fn quote(&self, anchor: &str) -> Result<String, JsError> {
        #[derive(serde::Deserialize)]
        struct Selection {
            start_block: String,
            start_offset: u32,
            end_block: String,
            end_offset: u32,
        }
        let s: Selection =
            serde_json::from_str(anchor).map_err(|e| JsError::new(&e.to_string()))?;
        let anchor = Anchor {
            start_block: block_id(&s.start_block)?,
            start_offset: s.start_offset,
            end_block: block_id(&s.end_block)?,
            end_offset: s.end_offset,
        };
        json(&self.0.quote(&anchor).map_err(book_error)?)
    }

    /// The quote of a bookmark on `block`, as JSON (see [`Reader::quote`]).
    pub fn bookmark(&self, block: &str) -> Result<String, JsError> {
        json(&self.0.bookmark(block_id(block)?).map_err(book_error)?)
    }

    /// A reading position at `block` as JSON `{suffix, progress}`; `null` in the notes.
    pub fn position(&self, block: &str) -> Result<String, JsError> {
        json(&self.0.position(block_id(block)?).map_err(book_error)?)
    }

    /// The block (`"b12"`) to open a saved position (block ordinal, fingerprint) at.
    pub fn resume(&self, block: u32, suffix: &str) -> Option<String> {
        self.0
            .resume(BlockId::new(block)?, suffix)
            .map(|b| b.to_string())
    }

    /// The sidebar's lists for a JSON array of the book's live annotation records.
    pub fn lists(&self, annotations: &str) -> Result<String, JsError> {
        let records: Vec<book::AnnotationRecord> =
            serde_json::from_str(annotations).map_err(|e| JsError::new(&e.to_string()))?;
        self.0.lists(&records).map_err(book_error)
    }

    /// The Search tab's results for `query`; `current` is the followed result.
    pub fn find(&self, query: &str, current: Option<u32>) -> Result<String, JsError> {
        self.0
            .find(query, current.map(|c| c as usize))
            .map_err(book_error)
    }
}

fn error(e: shelfgauge_core::publication::FormatError) -> JsError {
    JsError::new(&e.to_string())
}

fn book_error(e: book::BookError) -> JsError {
    JsError::new(&e.0)
}

fn block_id(text: &str) -> Result<BlockId, JsError> {
    text.parse()
        .map_err(|_| JsError::new(&format!("not a block id: {text}")))
}

fn json(value: &impl serde::Serialize) -> Result<String, JsError> {
    serde_json::to_string(value).map_err(|e| JsError::new(&e.to_string()))
}
