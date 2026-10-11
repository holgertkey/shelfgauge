//! The format layer: parsers that turn the raw bytes of a book into the common model.
//!
//! Each ebook format implements [`BookParser`]; a [`Registry`] maps file extensions to
//! parsers. Everything downstream depends only on [`model`], never on a specific format.

use std::path::Path;

pub mod fb2;
pub mod model;

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
}

/// A book could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ParseError {
    /// What went wrong, with as much context (e.g. line and column) as available.
    pub message: String,
}

impl ParseError {
    /// Creates an error with the given message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
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

    /// Creates a registry with all built-in formats (FB2).
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::format::Registry;
    ///
    /// let registry = Registry::with_builtin_formats();
    /// assert_eq!(registry.for_extension("FB2").unwrap().format(), "fb2");
    /// ```
    pub fn with_builtin_formats() -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(fb2::Fb2Parser));
        registry
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
    }

    #[test]
    fn registry_debug_lists_formats() {
        assert_eq!(format!("{:?}", registry()), r#"["fb2", "txt", "other"]"#);
    }
}
