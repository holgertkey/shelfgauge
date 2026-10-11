//! shelfgauge command-line entry point.

mod cli;

use clap::Parser;
use shelfgauge::config::Config;
use shelfgauge::library::Library;

use cli::{Cli, Mode};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    cli::init_logging(cli.verbose);
    let invocation = cli.invocation();

    let library = Library::open(&invocation.path)?;
    let config_path = library.config_path();
    let loaded = Config::load_or_create(&config_path)?;
    if loaded.created {
        tracing::info!("created default config {}", config_path.display());
    }
    for key in &loaded.unknown_keys {
        tracing::warn!(
            "unknown key `{key}` in {} is ignored",
            config_path.display()
        );
    }
    let mut config = loaded.config;
    config.apply_overrides(&invocation.overrides)?;

    // Temporary stub until the scanner (Phase 7) and the web server (Phase 8) exist.
    println!("Library: {}", library.root().display());
    println!("Config:  {}", config_path.display());
    println!();
    println!("Effective configuration:");
    println!();
    print!("{}", config.to_toml());
    println!();
    match invocation.mode {
        Mode::Serve { scan } => println!(
            "Serving is not implemented yet (background scan: {}).",
            if scan { "on" } else { "off" }
        ),
        Mode::Scan => println!("Scanning is not implemented yet."),
    }
    Ok(())
}
