//! Command-line interface definition and logging setup.

use std::net::IpAddr;
use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use shelfgauge::config::ConfigOverrides;
use shelfgauge::logging::{self, LogFormat, LogOptions};

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

    /// Log at trace level, the same as -vv; overrides RUST_LOG.
    #[arg(long, global = true)]
    pub trace: bool,

    /// Log line format: text, or json (one object per line, for log collectors).
    #[arg(
        long,
        value_enum,
        default_value_t,
        global = true,
        value_name = "FORMAT"
    )]
    pub log_format: LogFormatArg,
}

/// The `--log-format` values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum LogFormatArg {
    /// Human-readable lines.
    #[default]
    Text,
    /// One JSON object per line.
    Json,
}

impl From<LogFormatArg> for LogFormat {
    fn from(arg: LogFormatArg) -> Self {
        match arg {
            LogFormatArg::Text => Self::Text,
            LogFormatArg::Json => Self::Json,
        }
    }
}

impl Cli {
    /// The log verbosity for [`log_filter`]: the `-v` count, at least 2 with `--trace`.
    pub fn log_level(&self) -> u8 {
        if self.trace {
            self.verbose.max(2)
        } else {
            self.verbose
        }
    }
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
    /// List the books that could not be read at the last scan, with the reason.
    ///
    /// Files that could not be opened at all (I/O errors) are not recorded: they are
    /// logged by `scan` and tried again at the next scan.
    Errors {
        /// Library directory (default: the current directory).
        path: Option<PathBuf>,
    },
    /// Work with bookmarks and quotes (`annotations export`: rewrite their Markdown export).
    #[command(subcommand)]
    Annotations(AnnotationsCommand),
    /// Manage the profiles of the people who read here (`profiles list`, `add`,
    /// `remove`, `reset-pin`, `set-password`, `remove-password`).
    #[command(subcommand)]
    Profiles(ProfilesCommand),
    /// Run a hub: encrypted books and reading data for the phones and other devices of
    /// shelfgauge libraries (`hub takedown`, `accounts`, `invite` manage it).
    Hub(HubArgs),
}

/// `shelfgauge hub` arguments.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(args_conflicts_with_subcommands = false)]
pub struct HubArgs {
    /// What to do; serve the hub when omitted.
    #[command(subcommand)]
    pub command: Option<HubCommand>,

    /// The hub's data directory (default: the current directory). A new hub is created
    /// only in a directory given here.
    #[arg(long, global = true, value_name = "DIR")]
    pub data: Option<PathBuf>,
}

/// `shelfgauge hub` subcommands.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum HubCommand {
    /// Delete a publication, whichever account it belongs to, and refuse its id from
    /// now on.
    Takedown {
        /// The publication id (32 hex digits, from its /r/<id> link).
        id: String,
    },
    /// List the accounts: internal id, age, usage (no secrets).
    Accounts,
    /// Create a one-time invite code for registering an account.
    Invite,
}

/// `shelfgauge profiles` subcommands.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum ProfilesCommand {
    /// List the profiles: id, name, role and whether they have a PIN.
    List {
        /// Library directory (default: the current directory).
        path: Option<PathBuf>,
    },
    /// Add a profile (a reader unless --admin), without a PIN.
    Add {
        /// The name (1 to 32 characters).
        name: String,
        /// Library directory (default: the current directory).
        path: Option<PathBuf>,
        /// Give it the admin role (settings, rescans, profiles).
        #[arg(long)]
        admin: bool,
    },
    /// Delete a profile with its bookmarks, quotes and reading places (its Markdown
    /// export is kept). The owner profile cannot be deleted.
    Remove {
        /// The profile's name.
        name: String,
        /// Library directory (default: the current directory).
        path: Option<PathBuf>,
        /// Do not ask for confirmation.
        #[arg(long, short)]
        yes: bool,
    },
    /// Remove a profile's PIN (a forgotten PIN of the owner profile) and sign out its
    /// devices.
    ResetPin {
        /// The profile's name.
        name: String,
        /// Library directory (default: the current directory).
        path: Option<PathBuf>,
    },
    /// Set a profile's password, the login from other devices (asked on the terminal
    /// without echo, or read as one line from standard input).
    SetPassword {
        /// The profile's name.
        name: String,
        /// Library directory (default: the current directory).
        path: Option<PathBuf>,
    },
    /// Remove a profile's password (a forgotten one) and sign out the devices that
    /// logged in with it.
    RemovePassword {
        /// The profile's name.
        name: String,
        /// Library directory (default: the current directory).
        path: Option<PathBuf>,
    },
}

/// `shelfgauge annotations` subcommands.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum AnnotationsCommand {
    /// Rewrite the Markdown export (.shelfgauge/annotations/) from the database and exit.
    Export {
        /// Library directory (default: the current directory).
        path: Option<PathBuf>,
        /// Only this profile's export (default: every profile).
        #[arg(long)]
        profile: Option<String>,
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
    /// Rewrite the annotation export (of one profile, or of all) and exit.
    ExportAnnotations {
        /// The profile's name; `None` for every profile.
        profile: Option<String>,
    },
    /// Manage the profiles and exit.
    Profiles(ProfilesAction),
    /// List the books that could not be parsed and exit.
    Errors,
    /// Run or manage a hub (the path is its data directory).
    Hub(HubAction),
}

/// What `shelfgauge hub` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubAction {
    /// Serve the hub.
    Serve,
    /// Take a publication down.
    Takedown {
        /// The publication id as given.
        id: String,
    },
    /// List the accounts.
    Accounts,
    /// Create an invite code.
    Invite,
}

/// What `shelfgauge profiles` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfilesAction {
    /// List the profiles.
    List,
    /// Add a profile.
    Add {
        /// The name.
        name: String,
        /// Whether it is an admin.
        admin: bool,
    },
    /// Delete a profile.
    Remove {
        /// The name.
        name: String,
        /// Whether not to ask for confirmation.
        yes: bool,
    },
    /// Remove a profile's PIN.
    ResetPin {
        /// The name.
        name: String,
    },
    /// Set a profile's password.
    SetPassword {
        /// The name.
        name: String,
    },
    /// Remove a profile's password.
    RemovePassword {
        /// The name.
        name: String,
    },
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
            Some(Command::Errors { path }) => Invocation {
                mode: Mode::Errors,
                explicit_path: false,
                path: path.unwrap_or_else(current_dir),
                overrides: ConfigOverrides::default(),
            },
            Some(Command::Annotations(AnnotationsCommand::Export { path, profile })) => {
                Invocation {
                    mode: Mode::ExportAnnotations { profile },
                    explicit_path: false,
                    path: path.unwrap_or_else(current_dir),
                    overrides: ConfigOverrides::default(),
                }
            }
            Some(Command::Profiles(command)) => {
                let (action, path) = match command {
                    ProfilesCommand::List { path } => (ProfilesAction::List, path),
                    ProfilesCommand::Add { name, path, admin } => {
                        (ProfilesAction::Add { name, admin }, path)
                    }
                    ProfilesCommand::Remove { name, path, yes } => {
                        (ProfilesAction::Remove { name, yes }, path)
                    }
                    ProfilesCommand::ResetPin { name, path } => {
                        (ProfilesAction::ResetPin { name }, path)
                    }
                    ProfilesCommand::SetPassword { name, path } => {
                        (ProfilesAction::SetPassword { name }, path)
                    }
                    ProfilesCommand::RemovePassword { name, path } => {
                        (ProfilesAction::RemovePassword { name }, path)
                    }
                };
                Invocation {
                    mode: Mode::Profiles(action),
                    explicit_path: false,
                    path: path.unwrap_or_else(current_dir),
                    overrides: ConfigOverrides::default(),
                }
            }
            Some(Command::Hub(args)) => {
                let action = match args.command {
                    None => HubAction::Serve,
                    Some(HubCommand::Takedown { id }) => HubAction::Takedown { id },
                    Some(HubCommand::Accounts) => HubAction::Accounts,
                    Some(HubCommand::Invite) => HubAction::Invite,
                };
                Invocation {
                    mode: Mode::Hub(action),
                    explicit_path: args.data.is_some(),
                    path: args.data.unwrap_or_else(current_dir),
                    overrides: ConfigOverrides::default(),
                }
            }
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

/// The logging options for verbosity `verbose` (see [`Cli::log_level`]), `format`, the
/// `RUST_LOG` value and whether stderr gets colours.
///
/// At trace level every closed span also logs its duration.
pub fn log_options(
    verbose: u8,
    format: LogFormat,
    rust_log: Option<&str>,
    ansi: bool,
) -> LogOptions {
    LogOptions {
        filter: log_filter(verbose, rust_log),
        format,
        ansi: ansi && format == LogFormat::Text,
        span_timings: verbose >= 2,
    }
}

/// Initializes logging to stderr and sends panics to the log.
pub fn init_logging(verbose: u8, format: LogFormat) {
    use std::io::IsTerminal;
    use tracing_subscriber::util::SubscriberInitExt;

    let rust_log = std::env::var("RUST_LOG").ok();
    let ansi = logging::use_ansi(
        std::io::stderr().is_terminal(),
        std::env::var_os("NO_COLOR").as_deref(),
    );
    let mut options = log_options(verbose, format, rust_log.as_deref(), ansi);
    let dispatch = logging::dispatch(&options, std::io::stderr).unwrap_or_else(|e| {
        eprintln!(
            "warning: ignoring invalid RUST_LOG value {:?}: {e}",
            options.filter
        );
        options.filter = DEFAULT_LOG_FILTER.to_owned();
        logging::dispatch(&options, std::io::stderr).expect("the default filter is valid")
    });
    dispatch.init();
    logging::install_panic_hook();
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
    fn help_lists_annotations_export() {
        let help = Cli::command().render_help().to_string();
        assert!(help.contains("annotations export"), "{help}");
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
    fn annotations_export_never_creates_a_library() {
        let inv = parse(&["annotations", "export", "books"]).unwrap();
        assert_eq!(inv.mode, Mode::ExportAnnotations { profile: None });
        assert_eq!(inv.path, PathBuf::from("books"));
        assert!(!inv.explicit_path, "only an existing library is exported");
        assert_eq!(
            parse(&["annotations", "export"]).unwrap().path,
            PathBuf::from(".")
        );
        assert!(parse(&["annotations"]).is_err());
        assert_eq!(
            parse(&["annotations", "export", "--profile", "Anna"])
                .unwrap()
                .mode,
            Mode::ExportAnnotations {
                profile: Some("Anna".into())
            }
        );
    }

    #[test]
    fn profiles_subcommands() {
        let inv = parse(&["profiles", "list", "books"]).unwrap();
        assert_eq!(inv.mode, Mode::Profiles(ProfilesAction::List));
        assert_eq!(inv.path, PathBuf::from("books"));
        assert!(!inv.explicit_path, "never creates a library");
        assert_eq!(
            parse(&["profiles", "add", "Anna", "--admin"]).unwrap().mode,
            Mode::Profiles(ProfilesAction::Add {
                name: "Anna".into(),
                admin: true
            })
        );
        let inv = parse(&["profiles", "remove", "Anna", "books", "--yes"]).unwrap();
        assert_eq!(
            inv.mode,
            Mode::Profiles(ProfilesAction::Remove {
                name: "Anna".into(),
                yes: true
            })
        );
        assert_eq!(inv.path, PathBuf::from("books"));
        assert_eq!(
            parse(&["profiles", "reset-pin", "Me"]).unwrap().mode,
            Mode::Profiles(ProfilesAction::ResetPin { name: "Me".into() })
        );
        assert_eq!(
            parse(&["profiles", "set-password", "Me"]).unwrap().mode,
            Mode::Profiles(ProfilesAction::SetPassword { name: "Me".into() })
        );
        assert_eq!(
            parse(&["profiles", "remove-password", "Me"]).unwrap().mode,
            Mode::Profiles(ProfilesAction::RemovePassword { name: "Me".into() })
        );
        assert!(parse(&["profiles", "add"]).is_err());
    }

    #[test]
    fn hub_subcommands_take_the_data_directory() {
        let inv = parse(&["hub", "--data", "/srv/hub"]).unwrap();
        assert_eq!(inv.mode, Mode::Hub(HubAction::Serve));
        assert_eq!(inv.path, PathBuf::from("/srv/hub"));
        assert!(inv.explicit_path);
        let inv = parse(&["hub"]).unwrap();
        assert_eq!(inv.path, PathBuf::from("."));
        assert!(!inv.explicit_path, "only an existing hub is served");
        let inv = parse(&["hub", "takedown", "ab12", "--data", "d"]).unwrap();
        assert_eq!(
            inv.mode,
            Mode::Hub(HubAction::Takedown { id: "ab12".into() })
        );
        assert_eq!(inv.path, PathBuf::from("d"));
        assert_eq!(
            parse(&["hub", "--data", "d", "accounts"]).unwrap().mode,
            Mode::Hub(HubAction::Accounts)
        );
        assert_eq!(
            parse(&["hub", "invite"]).unwrap().mode,
            Mode::Hub(HubAction::Invite)
        );
        assert!(parse(&["hub", "takedown"]).is_err());
        assert_eq!(
            Cli::try_parse_from(["shelfgauge", "hub", "--log-format", "json"])
                .unwrap()
                .log_format,
            LogFormatArg::Json
        );
    }

    #[test]
    fn errors_never_creates_a_library() {
        let inv = parse(&["errors", "books"]).unwrap();
        assert_eq!(inv.mode, Mode::Errors);
        assert_eq!(inv.path, PathBuf::from("books"));
        assert!(!inv.explicit_path, "only an existing library is reported");
        assert_eq!(parse(&["errors"]).unwrap().path, PathBuf::from("."));
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
    fn trace_flag_is_global_and_equals_two_verbose() {
        let level = |args: &[&str]| {
            let mut argv = vec!["shelfgauge"];
            argv.extend_from_slice(args);
            Cli::try_parse_from(argv).unwrap().log_level()
        };
        assert_eq!(level(&[]), 0);
        assert_eq!(level(&["-v"]), 1);
        assert_eq!(level(&["--verbose"]), 1);
        assert_eq!(level(&["-vv"]), 2);
        assert_eq!(level(&["--trace"]), 2);
        assert_eq!(level(&["-v", "--trace"]), 2);
        assert_eq!(level(&["scan", "--trace"]), 2);
        assert_eq!(
            log_filter(level(&["--trace"]), None),
            log_filter(level(&["-vv"]), None)
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
    fn log_format_is_global_and_defaults_to_text() {
        let format = |args: &[&str]| {
            let argv = std::iter::once("shelfgauge").chain(args.iter().copied());
            Cli::try_parse_from(argv).map(|cli| LogFormat::from(cli.log_format))
        };
        assert_eq!(format(&[]).unwrap(), LogFormat::Text);
        assert_eq!(format(&["--log-format", "json"]).unwrap(), LogFormat::Json);
        assert_eq!(
            format(&["scan", "--log-format", "text"]).unwrap(),
            LogFormat::Text
        );
        assert_eq!(
            format(&["errors", "--log-format=json", "-v"]).unwrap(),
            LogFormat::Json
        );
        assert_eq!(
            format(&["--log-format", "xml"]).unwrap_err().kind(),
            clap::error::ErrorKind::InvalidValue
        );
    }

    #[test]
    fn log_options_combine_format_level_and_colours() {
        let text = log_options(0, LogFormat::Text, Some("debug"), true);
        assert_eq!(text.filter, "debug", "RUST_LOG without -v");
        assert!(text.ansi);
        assert!(!text.span_timings);
        let json = log_options(1, LogFormat::Json, Some("debug"), true);
        assert_eq!(
            json.filter, "warn,shelfgauge=debug",
            "-v overrides RUST_LOG"
        );
        assert!(!json.ansi, "JSON never has colours");
        assert!(!json.span_timings);
        let trace = log_options(2, LogFormat::Text, None, false);
        assert!(!trace.ansi);
        assert!(trace.span_timings, "span timings at trace level");
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
