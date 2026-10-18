//! Logging setup shared by the binary and the tests.
//!
//! Logs go to one writer (stderr in the binary) as `text` lines or one JSON object per
//! line. [`dispatch`] builds the subscriber for a writer, [`install_panic_hook`] sends
//! panics to the same stream, and [`in_current_span`] carries the caller's span (and
//! subscriber) to blocking and parser threads, so their lines keep the request they
//! belong to.
//!
//! What is never logged (query strings, request bodies, credentials, absolute paths at
//! `info` and above) is described in `docs/ARCHITECTURE.md`, "Logging". Untrusted values
//! are logged with `?` (Debug escapes control characters), never with `{}`.

use std::cell::Cell;
use std::ffi::OsStr;
use std::panic::{AssertUnwindSafe, PanicHookInfo};
use std::str::FromStr;

use tracing::{Dispatch, Span};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::FmtSpan;

/// The format of log lines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable lines, the span chain as a prefix.
    #[default]
    Text,
    /// One JSON object per line, with the span chain as fields.
    Json,
}

impl FromStr for LogFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            _ => Err(format!("unknown log format {s:?} (expected text or json)")),
        }
    }
}

/// How to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogOptions {
    /// `tracing` filter directives, e.g. `warn,shelfgauge=info`.
    pub filter: String,
    /// The line format.
    pub format: LogFormat,
    /// Whether `text` lines get ANSI colours (see [`use_ansi`]); `json` never does.
    pub ansi: bool,
    /// Whether every closed span logs its duration (`FmtSpan::CLOSE`), for `--trace`.
    pub span_timings: bool,
}

/// Whether to colour `text` logs: only on a terminal, and not when `NO_COLOR` is set to
/// a non-empty value (<https://no-color.org>).
///
/// # Examples
///
/// ```
/// use shelfgauge::logging::use_ansi;
///
/// assert!(use_ansi(true, None));
/// assert!(use_ansi(true, Some("".as_ref())));
/// assert!(!use_ansi(true, Some("1".as_ref())));
/// assert!(!use_ansi(false, None));
/// ```
pub fn use_ansi(is_terminal: bool, no_color: Option<&OsStr>) -> bool {
    is_terminal && no_color.is_none_or(OsStr::is_empty)
}

/// Builds the subscriber for `options`, writing to `writer`.
///
/// Fails if the filter directives are invalid.
pub fn dispatch<W>(
    options: &LogOptions,
    writer: W,
) -> Result<Dispatch, tracing_subscriber::filter::ParseError>
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let filter = EnvFilter::try_new(&options.filter)?;
    let span_events = if options.span_timings {
        FmtSpan::CLOSE
    } else {
        FmtSpan::NONE
    };
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_span_events(span_events);
    Ok(match options.format {
        LogFormat::Text => Dispatch::new(builder.with_ansi(options.ansi).finish()),
        LogFormat::Json => Dispatch::new(
            builder
                .json()
                .with_current_span(true)
                .with_span_list(true)
                .with_ansi(false)
                .finish(),
        ),
    })
}

/// Sends panics to the log (see [`log_panic`]) instead of the default hook's stderr
/// output.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(log_panic));
}

thread_local! {
    /// Set while [`catch_expected_panic`] runs on this thread.
    static EXPECTED_PANIC: Cell<bool> = const { Cell::new(false) };
}

/// Logs a panic through `tracing`: at `error` with its message, thread and location,
/// and the backtrace when `RUST_BACKTRACE` asks for one; at `debug` inside
/// [`catch_expected_panic`].
///
/// The line is logged on the panicking thread, so it carries the spans entered there
/// (the request, the book).
pub fn log_panic(info: &PanicHookInfo<'_>) {
    let payload = info.payload();
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("Box<dyn Any>");
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_default();
    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("<unnamed>");
    if EXPECTED_PANIC.with(Cell::get) {
        tracing::debug!(thread, location, "caught a panic: {message:?}");
        return;
    }
    let backtrace = std::backtrace::Backtrace::capture();
    if backtrace.status() == std::backtrace::BacktraceStatus::Captured {
        tracing::error!(thread, location, %backtrace, "panicked: {message:?}");
    } else {
        tracing::error!(thread, location, "panicked: {message:?}");
    }
}

/// Runs `f`, catching a panic as an expected failure (a parser on broken input): the
/// panic is logged at `debug` only, the caller reports it.
pub fn catch_expected_panic<T>(f: impl FnOnce() -> T) -> std::thread::Result<T> {
    let outer = EXPECTED_PANIC.with(|expected| expected.replace(true));
    let result = std::panic::catch_unwind(AssertUnwindSafe(f));
    EXPECTED_PANIC.with(|expected| expected.set(outer));
    result
}

/// Wraps `f` to run in the current span with the current subscriber, wherever it is
/// called: on a blocking or parser thread, its log lines keep the caller's spans.
///
/// # Examples
///
/// ```
/// use shelfgauge::logging::in_current_span;
///
/// let span = tracing::info_span!("request", id = 1);
/// let job = span.in_scope(|| in_current_span(|| tracing::info!("inside the request")));
/// std::thread::spawn(job).join().unwrap();
/// ```
pub fn in_current_span<T>(f: impl FnOnce() -> T) -> impl FnOnce() -> T {
    let span = Span::current();
    let dispatch = tracing::dispatcher::get_default(Dispatch::clone);
    move || tracing::dispatcher::with_default(&dispatch, || span.in_scope(f))
}

/// A writer that keeps the log in memory, for tests.
#[cfg(test)]
pub(crate) mod capture {
    use std::sync::{Arc, Mutex};

    use super::{LogFormat, LogOptions};

    /// The captured log; clones share it.
    #[derive(Debug, Clone, Default)]
    pub(crate) struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Capture {
        /// Everything written so far.
        pub(crate) fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }

        /// A `text` subscriber with `filter` writing here.
        pub(crate) fn dispatch(&self, filter: &str) -> tracing::Dispatch {
            install_quiet();
            let options = LogOptions {
                filter: filter.to_owned(),
                format: LogFormat::Text,
                ansi: false,
                span_timings: false,
            };
            super::dispatch(&options, self.clone()).unwrap()
        }
    }

    /// The global fallback subscriber of the tests: it writes nothing but marks every
    /// callsite as "sometimes" enabled.
    ///
    /// Tests install their own subscriber per thread while other tests log in parallel;
    /// without this fallback, a callsite first hit while no test subscriber exists can be
    /// cached as disabled for good (a race in `tracing`'s interest cache), and its events
    /// vanish from a later test's capture.
    struct Quiet;

    impl tracing::Subscriber for Quiet {
        fn register_callsite(
            &self,
            _: &'static tracing::Metadata<'static>,
        ) -> tracing::subscriber::Interest {
            tracing::subscriber::Interest::sometimes()
        }

        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            false
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, _: &tracing::Event<'_>) {}

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// Installs [`Quiet`] as the global subscriber, once per test binary.
    fn install_quiet() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let _ = tracing::subscriber::set_global_default(Quiet);
        });
    }

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Self;

        fn make_writer(&'a self) -> Self {
            self.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_formats_parse() {
        assert_eq!("text".parse(), Ok(LogFormat::Text));
        assert_eq!("json".parse(), Ok(LogFormat::Json));
        assert!("JSON".parse::<LogFormat>().is_err());
    }

    #[test]
    fn ansi_only_on_a_terminal_without_no_color() {
        assert!(use_ansi(true, None));
        assert!(use_ansi(true, Some(OsStr::new(""))));
        assert!(!use_ansi(true, Some(OsStr::new("1"))));
        assert!(!use_ansi(false, None));
        assert!(!use_ansi(false, Some(OsStr::new(""))));
    }

    #[test]
    fn invalid_filters_are_rejected() {
        let options = LogOptions {
            filter: "shelfgauge=loud".to_owned(),
            format: LogFormat::Text,
            ansi: false,
            span_timings: false,
        };
        assert!(dispatch(&options, std::io::sink).is_err());
    }

    #[test]
    fn expected_panics_are_caught_and_the_flag_restored() {
        let result = catch_expected_panic(|| catch_expected_panic(|| panic!("inner")).is_err());
        assert_eq!(result.ok(), Some(true));
        assert!(!EXPECTED_PANIC.with(Cell::get));
        assert!(catch_expected_panic(|| panic!("outer")).is_err());
        assert!(!EXPECTED_PANIC.with(Cell::get));
    }
}
