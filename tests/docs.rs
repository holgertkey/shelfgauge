//! Checks that the documentation stays in sync with the code.

use shelfgauge::config::DEFAULT_CONFIG_TOML;

/// Every key of the default config file is described in the README's configuration
/// reference.
#[test]
fn readme_describes_every_config_key() {
    let readme = include_str!("../README.md");
    let config: toml::Table = DEFAULT_CONFIG_TOML.parse().unwrap();
    let mut keys = 0;
    for (section, table) in &config {
        for key in table
            .as_table()
            .expect("every top-level entry is a section")
            .keys()
        {
            let name = format!("`{section}.{key}`");
            assert!(readme.contains(&name), "README.md does not describe {name}");
            keys += 1;
        }
    }
    assert!(keys >= 9, "only {keys} keys found");
}
