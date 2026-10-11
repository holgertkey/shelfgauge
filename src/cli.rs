//! Command-line interface definition and logging setup.

use std::net::IpAddr;
use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand};
use shelfgauge::config::ConfigOverrides;

/// Index a folder of ebooks and browse it in a local web library.
///
/// Without a subcommand, shelfgauge serves the library at PATH (default: the current
/// directory) and updates its index in the background.
#[derive(Debug, Parser)]
#[command(
    name = "shelfgauge",
    version = shelfgauge::VERSION,
    about,
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    /// The subcommand; serve when omitted.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Arguments of the default (serve) mode.
    #[command(flatten)]
    pub serve: ServeArgs,

    /// Increase log verbosity (-v: debug, -vv: trace); overrides RUST_LOG.
    #[arg(short, long, action = ArgAction::Count, global = true)]
    pub verbose: u8,
}

/// Arguments of the default mode: serve the library.
#[derive(Debug, Clone, Default, PartialEq, Eq, Args)]
pub struct ServeArgs {
    /// Library directory (default: the current directory).
    pub path: Option<PathBuf>,

    /// Port to listen on (overrides `server.port`).
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    pub port: Option<u16>,

    /// Address to listen on (overrides `server.bind`).
    #[arg(long)]
    pub bind: Option<IpAddr>,

    /// Do not update the index on startup.
    #[arg(long)]
    pub no_scan: bool,
}

/// Subcommands.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Update the library index and exit.
    Scan {
        /// Library directory (default: the current directory).
        path: Option<PathBuf>,
    },
}

/// What to run, resolved from the parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Serve the library (and scan in the background unless `scan` is false).
    Serve {
        /// Whether to update the index on startup.
        scan: bool,
    },
    /// Update the index and exit.
    Scan,
}

/// The resolved invocation: mode, library root and config overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// What to run.
    pub mode: Mode,
    /// The library root as given (default: the current directory).
    pub path: PathBuf,
    /// Whether PATH was given explicitly; only then may a new library be created.
    pub explicit_path: bool,
    /// Command-line config overrides.
    pub overrides: ConfigOverrides,
}

impl Cli {
    /// Resolves the parsed arguments into an [`Invocation`].
    pub fn invocation(self) -> Invocation {
        let current_dir = || PathBuf::from(".");
        match self.command {
            Some(Command::Scan { path }) => Invocation {
                mode: Mode::Scan,
                explicit_path: path.is_some(),
                path: path.unwrap_or_else(current_dir),
                overrides: ConfigOverrides::default(),
            },
            None => Invocation {
                mode: Mode::Serve {
                    scan: !self.serve.no_scan,
                },
                explicit_path: self.serve.path.is_some(),
                path: self.serve.path.unwrap_or_else(current_dir),
                overrides: ConfigOverrides {
                    port: self.serve.port,
                    bind: self.serve.bind,
                },
            },
        }
    }
}

/// The default log filter: warnings from dependencies, info from shelfgauge.
const DEFAULT_LOG_FILTER: &str = "warn,shelfgauge=info";

/// The `tracing` filter directives for `-v` count `verbose` and the `RUST_LOG` value.
///
/// `-v` flags take precedence over `RUST_LOG`; without either, [`DEFAULT_LOG_FILTER`].
pub fn log_filter(verbose: u8, rust_log: Option<&str>) -> String {
    match verbose {
        0 => match rust_log.map(str::trim) {
            Some(env) if !env.is_empty() => env.to_owned(),
            _ => DEFAULT_LOG_FILTER.to_owned(),
        },
        1 => "warn,shelfgauge=debug".to_owned(),
        _ => "info,shelfgauge=trace".to_owned(),
    }
}

/// Initializes logging to stderr.
pub fn init_logging(verbose: u8) {
    use tracing_subscriber::EnvFilter;

    let rust_log = std::env::var("RUST_LOG").ok();
    let directives = log_filter(verbose, rust_log.as_deref());
    let filter = EnvFilter::try_new(&directives).unwrap_or_else(|e| {
        eprintln!("warning: ignoring invalid RUST_LOG value {directives:?}: {e}");
        EnvFilter::new(DEFAULT_LOG_FILTER)
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Invocation, clap::Error> {
        let argv = std::iter::once("shelfgauge").chain(args.iter().copied());
        Cli::try_parse_from(argv).map(Cli::invocation)
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn version_flag_prints_program_version() {
        let err = Cli::try_parse_from(["shelfgauge", "--version"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert_eq!(
            err.to_string(),
            format!("shelfgauge {}\n", shelfgauge::VERSION)
        );
    }

    #[test]
    fn no_arguments_serves_current_directory_with_scan() {
        assert_eq!(
            parse(&[]).unwrap(),
            Invocation {
                mode: Mode::Serve { scan: true },
                path: PathBuf::from("."),
                explicit_path: false,
                overrides: ConfigOverrides::default(),
            }
        );
    }

    #[test]
    fn serve_flags_become_overrides() {
        let inv = parse(&["books", "--port", "9000", "--bind", "::1", "--no-scan"]).unwrap();
        assert_eq!(inv.mode, Mode::Serve { scan: false });
        assert_eq!(inv.path, PathBuf::from("books"));
        assert_eq!(inv.overrides.port, Some(9000));
        assert_eq!(inv.overrides.bind, Some("::1".parse().unwrap()));
    }

    #[test]
    fn scan_subcommand_takes_optional_path() {
        assert_eq!(parse(&["scan"]).unwrap().mode, Mode::Scan);
        assert_eq!(parse(&["scan"]).unwrap().path, PathBuf::from("."));
        assert!(!parse(&["scan"]).unwrap().explicit_path);
        assert!(parse(&["scan", "."]).unwrap().explicit_path);
        assert!(parse(&["."]).unwrap().explicit_path);
        assert!(!parse(&["-v"]).unwrap().explicit_path);
        assert_eq!(
            parse(&["scan", "books"]).unwrap().path,
            PathBuf::from("books")
        );
    }

    #[test]
    fn verbose_is_global_and_counted() {
        assert_eq!(
            Cli::try_parse_from(["shelfgauge", "-vv"]).unwrap().verbose,
            2
        );
        assert_eq!(
            Cli::try_parse_from(["shelfgauge", "scan", "-v"])
                .unwrap()
                .verbose,
            1
        );
    }

    #[test]
    fn serve_flags_are_rejected_with_scan() {
        assert!(parse(&["scan", "--port", "9000"]).is_err());
        assert!(parse(&["scan", "--no-scan"]).is_err());
    }

    #[test]
    fn scan_after_serve_flags_is_a_path() {
        // Once a serve flag is given, `scan` is no longer a subcommand but the PATH.
        let inv = parse(&["--port", "9000", "scan"]).unwrap();
        assert_eq!(inv.mode, Mode::Serve { scan: true });
        assert_eq!(inv.path, PathBuf::from("scan"));
    }

    #[test]
    fn invalid_ports_and_addresses_are_rejected() {
        for args in [
            &["--port", "0"][..],
            &["--port", "65536"],
            &["--port", "x"],
            &["--bind", "localhost"],
        ] {
            let err = parse(args).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ValueValidation,
                "{args:?}"
            );
        }
    }

    #[test]
    fn log_filter_precedence() {
        assert_eq!(log_filter(0, None), DEFAULT_LOG_FILTER);
        assert_eq!(log_filter(0, Some("  ")), DEFAULT_LOG_FILTER);
        assert_eq!(log_filter(0, Some("shelfgauge=trace")), "shelfgauge=trace");
        assert_eq!(log_filter(1, Some("error")), "warn,shelfgauge=debug");
        assert_eq!(log_filter(2, None), "info,shelfgauge=trace");
        assert_eq!(log_filter(5, None), "info,shelfgauge=trace");
    }
}
