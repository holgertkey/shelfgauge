//! WebAssembly bindings of `shelfgauge-core` for the hub's reader app.
//!
//! The reader app decrypts and inflates a publication's blobs in JavaScript (WebCrypto,
//! `DecompressionStream`) and hands the plaintext here: this crate decodes the
//! manifest and the parts and renders them with the same code as the instance, so
//! untrusted data becomes escaped HTML. It also derives the keys of a hub password
//! (Argon2id has no WebCrypto counterpart) and checks account addresses with the
//! hub's rules. The boundary carries only bytes, strings and JSON strings.
//!
//! Built with `scripts/build-wasm.sh` into `assets/hub/pkg/` (committed); natively it
//! builds as a plain library for the tests.

#![warn(missing_docs)]

pub mod book;

use shelfgauge_core::annotations::anchor::Anchor;
use shelfgauge_core::format::model::BlockId;
use shelfgauge_core::publication::container;
use shelfgauge_core::{address, password};
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

/// Checks the start of a book file (its first `HEAD_LEN` bytes, see
/// [`container::check_head`]); throws for any other file, an EPUB too.
#[wasm_bindgen(js_name = bookFileHead)]
pub fn book_file_head(head: &[u8]) -> Result<(), JsError> {
    container::check_head(head).map_err(container_error)
}

/// The number of bytes at the start of a book file that [`book_file_head`] checks.
#[wasm_bindgen(js_name = bookFileHeadLength)]
pub fn book_file_head_length() -> usize {
    container::HEAD_LEN
}

/// Where the central directory of a book file of `file_len` bytes is, from its last
/// 22 bytes: JSON `{offset, size}`.
#[wasm_bindgen(js_name = bookFileDirectory)]
pub fn book_file_directory(tail: &[u8], file_len: f64) -> Result<String, JsError> {
    let dir = container::end_of_directory(tail, file_len as u64).map_err(container_error)?;
    Ok(format!(
        r#"{{"offset":{},"size":{}}}"#,
        dir.offset, dir.size
    ))
}

/// The entries of a book file from its last 22 bytes and its central directory: JSON
/// `[{name, offset, size}]` in file order (`mimetype`, `sg.json`, then the blobs at
/// their hub paths); the data of each is the file's bytes `offset..offset + size`.
#[wasm_bindgen(js_name = bookFileEntries)]
pub fn book_file_entries(tail: &[u8], directory: &[u8], file_len: f64) -> Result<String, JsError> {
    let dir = container::end_of_directory(tail, file_len as u64).map_err(container_error)?;
    let entries = container::entries(directory, &dir).map_err(container_error)?;
    let list: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| serde_json::json!({ "name": e.name.to_string(), "offset": e.offset, "size": e.size }))
        .collect();
    json(&list)
}

/// The login key and the wrapping key of a hub password (see
/// [`shelfgauge_core::password::derive`]): 64 bytes, the login key first. Throws for
/// parameters below the built-in minimum, so a hub cannot weaken them.
#[wasm_bindgen(js_name = deriveKeys)]
pub fn derive_keys(
    password: &str,
    salt: &[u8],
    memory_kib: u32,
    iterations: u32,
) -> Result<Vec<u8>, JsError> {
    let keys = password::derive(password, salt, memory_kib, iterations)
        .map_err(|e| JsError::new(&e.to_string()))?;
    Ok([keys.login_key, keys.wrapping_key].concat())
}

/// The built-in password rules as JSON `{memory_kib, iterations, salt_len,
/// min_chars}`: the weakest parameters [`derive_keys`] accepts, the salt length and
/// the shortest new password.
#[wasm_bindgen(js_name = passwordRules)]
pub fn password_rules() -> String {
    serde_json::json!({
        "memory_kib": password::MIN_MEMORY_KIB,
        "iterations": password::MIN_ITERATIONS,
        "salt_len": password::SALT_LEN,
        "min_chars": password::MIN_PASSWORD_CHARS,
    })
    .to_string()
}

/// A name or handle in its stored form, or `undefined` if it breaks the hub's format
/// (see [`shelfgauge_core::address::normalize_label`]).
#[wasm_bindgen(js_name = normalizeLabel)]
pub fn normalize_label(text: &str) -> Option<String> {
    address::normalize_label(text)
}

/// An account address `name@handle` in its stored form, or `undefined` if it is not
/// one.
#[wasm_bindgen(js_name = normalizeAddress)]
pub fn normalize_address(text: &str) -> Option<String> {
    address::parse_address(text).map(|(name, handle)| format!("{name}@{handle}"))
}

fn container_error(e: container::ContainerError) -> JsError {
    JsError::new(&e.to_string())
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
