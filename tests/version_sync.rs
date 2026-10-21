//! Runs the unit tests of the build script's version sync logic under `cargo test`, and
//! checks that both crates take their version from the workspace `Cargo.toml`.

#[path = "../build/version_sync.rs"]
mod version_sync;

/// The workspace's root manifest.
fn manifest() -> toml::Table {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("read Cargo.toml");
    text.parse().expect("parse Cargo.toml")
}

/// Looks up a dotted `path` of string keys in `table`.
fn get<'a>(table: &'a toml::Table, path: &[&str]) -> &'a toml::Value {
    let (last, parents) = path.split_last().unwrap();
    let mut table = table;
    for key in parents {
        table = table[*key]
            .as_table()
            .unwrap_or_else(|| panic!("`{key}` is a table"));
    }
    &table[*last]
}

// The exact requirement on `shelfgauge-core` needs no test: Cargo refuses to build
// when it does not match the workspace version.
#[test]
fn both_crates_inherit_the_workspace_version() {
    let core = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/crates/shelfgauge-core/Cargo.toml"
    );
    // A packaged crate (crates.io) has no workspace around it.
    if !std::path::Path::new(core).exists() {
        return;
    }
    assert_eq!(
        env!("CARGO_PKG_VERSION"),
        get(&manifest(), &["workspace", "package", "version"])
            .as_str()
            .unwrap()
    );
    let core: toml::Table = std::fs::read_to_string(core).unwrap().parse().unwrap();
    assert_eq!(
        get(&core, &["package", "version", "workspace"]).as_bool(),
        Some(true)
    );
}
