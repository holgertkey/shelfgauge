//! shelfgauge command-line entry point.

use clap::Parser;

/// Index a folder of ebooks and browse it in a local web library.
#[derive(Debug, Parser)]
#[command(name = "shelfgauge", version = shelfgauge::VERSION, about)]
struct Cli {}

fn main() -> anyhow::Result<()> {
    let _cli = Cli::parse();
    println!("shelfgauge v{}: not implemented yet", shelfgauge::VERSION);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn version_flag_prints_display_version() {
        let err = Cli::try_parse_from(["shelfgauge", "--version"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert_eq!(
            err.to_string(),
            format!("shelfgauge {}\n", shelfgauge::VERSION)
        );
    }
}
