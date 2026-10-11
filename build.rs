//! Build script: syncs the version from `Cargo.toml` into the documentation files and
//! exposes the displayed version to the crate as `SHELFGAUGE_VERSION`.

use std::fs;
use std::path::Path;

#[path = "build/version_sync.rs"]
mod version_sync;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=build/version_sync.rs");
    println!("cargo::rerun-if-changed=README.md");
    println!("cargo::rerun-if-changed=CHANGELOG.md");

    let version = env!("CARGO_PKG_VERSION");
    let display = version_sync::display_version(version);
    println!("cargo::rustc-env=SHELFGAUGE_VERSION={display}");

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    sync_file(&root.join("README.md"), |content| {
        version_sync::sync_readme(content, display)
    });
    let base = version_sync::base_version(version);
    sync_file(&root.join("CHANGELOG.md"), |content| {
        version_sync::sync_changelog(content, base, &version_sync::today_utc())
    });
}

/// Rewrites `path` with the result of `update`, if the file exists and `update` changed it.
fn sync_file(path: &Path, update: impl FnOnce(&str) -> Option<String>) {
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };
    if let Some(updated) = update(&content) {
        match fs::write(path, updated) {
            Ok(()) => println!("cargo::warning=Synced version in {}", path.display()),
            Err(e) => println!(
                "cargo::warning=Failed to sync version in {}: {e}",
                path.display()
            ),
        }
    }
}
