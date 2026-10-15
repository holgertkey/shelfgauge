//! The format layer: parsers that turn the raw bytes of a book into the common model.
//!
//! Each ebook format ([`fb2`], [`epub`], [`txt`]) implements [`BookParser`]; a
//! [`Registry`] maps file extensions to parsers. Everything downstream depends only on [`model`], never on a specific format.

use std::path::Path;

pub mod encoding;
pub mod epub;
pub mod fb2;
pub mod model;
pub(crate) mod repair;
pub(crate) mod sections;
pub mod txt;
pub(crate) mod xml;
pub(crate) mod zipbook;

use model::{BookContent, BookMetadata};

/// Minimum stack size for threads that run a [`BookParser`].
///
/// XML parsing recurses per nesting level; parsers bound the depth they accept so that
/// parsing fits in this much stack even in debug builds. The default 2 MB of spawned
/// threads is not enough, so scanner and web worker threads must be configured with at
/// least this size (or use [`run_with_parser_stack`]).
pub const PARSER_STACK_SIZE: usize = 16 * 1024 * 1024;

/// Runs `f` on a new thread with [`PARSER_STACK_SIZE`] of stack and returns its result.
///
/// A panic in `f` is propagated to the caller.
///
/// # Examples
///
/// ```
/// use shelfgauge::format::{BookParser, run_with_parser_stack};
/// use shelfgauge::format::fb2::Fb2Parser;
///
/// let metadata = run_with_parser_stack(|| {
///     Fb2Parser.parse_metadata(b"<FictionBook/>")
/// });
/// assert!(metadata.is_ok());
/// ```
pub fn run_with_parser_stack<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .name("parser".to_owned())
            .stack_size(PARSER_STACK_SIZE)
            .spawn_scoped(scope, f)
            .expect("failed to spawn a parser thread");
        match handle.join() {
            Ok(value) => value,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

/// A parser for one ebook format.
///
/// Implementations must never panic on malformed input; they return [`ParseError`]
/// instead. Parsers are shared between scanner threads, hence `Send + Sync`. They must
/// run on threads with at least [`PARSER_STACK_SIZE`] of stack.
pub trait BookParser: Send + Sync {
    /// A short lowercase format name stored in the index, e.g. `fb2`.
    fn format(&self) -> &'static str;

    /// File extensions handled by this parser, lowercase and without the dot.
    fn extensions(&self) -> &'static [&'static str];

    /// Parses only the metadata; faster than [`BookParser::parse_content`] and used when
    /// scanning.
    fn parse_metadata(&self, data: &[u8]) -> Result<BookMetadata, ParseError>;

    /// Parses the full text of the book.
    fn parse_content(&self, data: &[u8]) -> Result<BookContent, ParseError>;

    /// Parses the metadata of a book stored under the file name `name` (the last
    /// component of its path, with the extension); used when scanning.
    ///
    /// Formats without in-book metadata (plain text) take the title and author from the
    /// name, and use it to tell books from other files ([`ParseErrorKind::NotABook`]).
    /// The default ignores the name and calls [`BookParser::parse_metadata`].
    fn parse_metadata_named(&self, data: &[u8], name: &str) -> Result<BookMetadata, ParseError> {
        let _ = name;
        self.parse_metadata(data)
    }
}

/// A book could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ParseError {
    /// What kind of failure this is.
    pub kind: ParseErrorKind,
    /// What went wrong, with as much context (e.g. line and column) as available.
    pub message: String,
}

/// The kind of a [`ParseError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// The file is a book of this format that cannot be read (broken, protected, over a
    /// limit). The scanner records it with an `error` status.
    Invalid,
    /// The file is not a book at all (a `readme.txt`, a short note, a web page that is
    /// not a book). The scanner does not record it.
    NotABook,
}

impl ParseError {
    /// Creates an [`Invalid`](ParseErrorKind::Invalid) error with the given message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            kind: ParseErrorKind::Invalid,
            message: message.into(),
        }
    }

    /// Creates a [`NotABook`](ParseErrorKind::NotABook) error saying why the file is
    /// not a book.
    pub fn not_a_book(message: impl Into<String>) -> Self {
        Self {
            kind: ParseErrorKind::NotABook,
            message: message.into(),
        }
    }

    /// Whether the file is not a book at all (see [`ParseErrorKind::NotABook`]).
    pub fn is_not_a_book(&self) -> bool {
        self.kind == ParseErrorKind::NotABook
    }
}

/// The names of the built-in formats.
pub const FORMATS: &[&str] = &["fb2", "epub", "txt"];

/// The formats a library indexes unless `[scan] formats` says otherwise.
pub const DEFAULT_FORMATS: &[&str] = &["fb2", "epub", "txt"];

/// A format name that is not one of [`FORMATS`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown format {name:?} (known formats: {})", FORMATS.join(", "))]
pub struct UnknownFormat {
    /// The name as given.
    pub name: String,
}

/// The parser of a built-in format by its name.
fn builtin(name: &str) -> Option<Box<dyn BookParser>> {
    match name {
        "fb2" => Some(Box::new(fb2::Fb2Parser)),
        "epub" => Some(Box::new(epub::EpubParser)),
        "txt" => Some(Box::new(txt::TxtParser)),
        _ => None,
    }
}

/// Maps file extensions to the parser for that format.
#[derive(Default)]
pub struct Registry {
    parsers: Vec<Box<dyn BookParser>>,
}

impl Registry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a registry with all built-in formats ([`FORMATS`]).
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::format::Registry;
    ///
    /// let registry = Registry::with_builtin_formats();
    /// assert_eq!(registry.for_extension("FB2").unwrap().format(), "fb2");
    /// assert_eq!(registry.for_extension("fbz").unwrap().format(), "fb2");
    /// assert_eq!(registry.for_extension("epub").unwrap().format(), "epub");
    /// assert_eq!(registry.for_extension("txt").unwrap().format(), "txt");
    /// ```
    pub fn with_builtin_formats() -> Self {
        Self::with_formats(FORMATS).expect("built-in formats are known")
    }

    /// Creates a registry with the named built-in formats (case-insensitive; repeated
    /// names count once).
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::format::Registry;
    ///
    /// let registry = Registry::with_formats(&["fb2", "EPUB"]).unwrap();
    /// assert!(registry.for_extension("txt").is_none());
    /// let error = Registry::with_formats(&["mobi"]).unwrap_err();
    /// assert_eq!(error.to_string(), r#"unknown format "mobi" (known formats: fb2, epub, txt)"#);
    /// ```
    pub fn with_formats<S: AsRef<str>>(names: &[S]) -> Result<Self, UnknownFormat> {
        let mut registry = Self::new();
        for name in names {
            let name = name.as_ref();
            let parser =
                builtin(&name.trim().to_ascii_lowercase()).ok_or_else(|| UnknownFormat {
                    name: name.to_owned(),
                })?;
            if !registry
                .parsers
                .iter()
                .any(|p| p.format() == parser.format())
            {
                registry.register(parser);
            }
        }
        Ok(registry)
    }

    /// The names of the registered formats, in order.
    pub fn formats(&self) -> Vec<&'static str> {
        self.parsers.iter().map(|parser| parser.format()).collect()
    }

    /// Adds a parser; for an extension claimed twice, the first registered parser wins.
    pub fn register(&mut self, parser: Box<dyn BookParser>) {
        self.parsers.push(parser);
    }

    /// The parser for a file extension (without the dot, case-insensitive).
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::format::Registry;
    ///
    /// let registry = Registry::new();
    /// assert!(registry.for_extension("fb2").is_none());
    /// ```
    pub fn for_extension(&self, extension: &str) -> Option<&dyn BookParser> {
        self.parsers
            .iter()
            .find(|parser| {
                parser
                    .extensions()
                    .iter()
                    .any(|ext| ext.eq_ignore_ascii_case(extension))
            })
            .map(Box::as_ref)
    }

    /// All file extensions handled by the registered parsers.
    pub fn extensions(&self) -> Vec<&'static str> {
        self.parsers
            .iter()
            .flat_map(|parser| parser.extensions().iter().copied())
            .collect()
    }

    /// The parser for a file name or path, by its extension (case-insensitive).
    pub fn for_path(&self, path: &Path) -> Option<&dyn BookParser> {
        self.for_extension(path.extension()?.to_str()?)
    }
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.parsers.iter().map(|parser| parser.format()))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dummy(&'static str, &'static [&'static str]);

    impl BookParser for Dummy {
        fn format(&self) -> &'static str {
            self.0
        }

        fn extensions(&self) -> &'static [&'static str] {
            self.1
        }

        fn parse_metadata(&self, _: &[u8]) -> Result<BookMetadata, ParseError> {
            Err(ParseError::new("dummy"))
        }

        fn parse_content(&self, _: &[u8]) -> Result<BookContent, ParseError> {
            Err(ParseError::new("dummy"))
        }
    }

    fn registry() -> Registry {
        let mut registry = Registry::new();
        registry.register(Box::new(Dummy("fb2", &["fb2"])));
        registry.register(Box::new(Dummy("txt", &["txt", "text"])));
        registry.register(Box::new(Dummy("other", &["fb2"])));
        registry
    }

    #[test]
    fn extensions_match_case_insensitively() {
        let registry = registry();
        assert_eq!(registry.for_extension("fb2").unwrap().format(), "fb2");
        assert_eq!(registry.for_extension("FB2").unwrap().format(), "fb2");
        assert_eq!(registry.for_extension("Text").unwrap().format(), "txt");
        assert!(registry.for_extension("epub").is_none());
        assert!(registry.for_extension("").is_none());
    }

    #[test]
    fn first_registered_parser_wins() {
        assert_eq!(registry().for_extension("fb2").unwrap().format(), "fb2");
    }

    #[test]
    fn paths_are_matched_by_extension() {
        let registry = registry();
        let parser = |p: &str| registry.for_path(Path::new(p)).map(|p| p.format());
        assert_eq!(parser("books/Война и мир.FB2"), Some("fb2"));
        assert_eq!(parser("notes.txt"), Some("txt"));
        assert_eq!(parser("archive.fb2.zip"), None);
        assert_eq!(parser("fb2"), None);
        assert_eq!(parser(".fb2"), None);
    }

    #[test]
    fn parse_errors_display_their_message() {
        assert_eq!(
            ParseError::new("line 3: bad tag").to_string(),
            "line 3: bad tag"
        );
        assert!(!ParseError::new("x").is_not_a_book());
        let error = ParseError::not_a_book("too short");
        assert!(error.is_not_a_book());
        assert_eq!(error.to_string(), "too short");
    }

    #[test]
    fn metadata_by_name_defaults_to_metadata() {
        let error = Dummy("fb2", &["fb2"])
            .parse_metadata_named(b"", "book.fb2")
            .unwrap_err();
        assert_eq!(error.message, "dummy");
    }

    #[test]
    fn registries_from_format_names() {
        let registry = Registry::with_formats(&["txt", "fb2", "FB2", " epub "]).unwrap();
        assert_eq!(registry.formats(), ["txt", "fb2", "epub"]);
        assert_eq!(registry.extensions(), ["txt", "fb2", "fbz", "epub"]);
        assert_eq!(Registry::with_builtin_formats().formats(), FORMATS);
        assert!(
            Registry::with_formats::<&str>(&[])
                .unwrap()
                .formats()
                .is_empty()
        );
        assert_eq!(
            Registry::with_formats(&["fb2", "html"]).unwrap_err().name,
            "html"
        );
    }

    #[test]
    fn registry_debug_lists_formats() {
        assert_eq!(format!("{:?}", registry()), r#"["fb2", "txt", "other"]"#);
    }
}
