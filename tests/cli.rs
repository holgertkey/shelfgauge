//! End-to-end tests of the `shelfgauge` binary: library root, config file, CLI overrides.

use std::path::Path;
use std::process::{Command, Output};

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_shelfgauge"))
        .args(args)
        .current_dir(dir)
        .env_remove("RUST_LOG")
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run shelfgauge")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn first_run_creates_config_and_prints_effective_config() {
    let dir = tempfile::tempdir().unwrap();
    let output = run(dir.path(), &["."]);
    assert!(output.status.success(), "{}", stderr(&output));

    let config = dir.path().join(".shelfgauge").join("config.toml");
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        shelfgauge::config::DEFAULT_CONFIG_TOML
    );
    let out = stdout(&output);
    assert!(out.contains("port = 8742"), "{out}");
    assert!(out.contains("background scan: on"), "{out}");
    assert!(stderr(&output).contains("created default config"));

    // The second run reuses the file and does not announce it again.
    let output = run(dir.path(), &[]);
    assert!(output.status.success());
    assert!(!stderr(&output).contains("created default config"));
}

#[test]
fn library_path_argument_and_scan_subcommand() {
    let dir = tempfile::tempdir().unwrap();
    let books = dir.path().join("books");
    std::fs::create_dir(&books).unwrap();

    let output = run(dir.path(), &["scan", "books"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(books.join(".shelfgauge").join("config.toml").is_file());
    assert!(!dir.path().join(".shelfgauge").exists());
    let out = stdout(&output);
    assert!(out.contains("0 added"), "{out}");
    assert!(books.join(".shelfgauge").join("library.db").is_file());
}

#[test]
fn cli_flags_override_config_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".shelfgauge")).unwrap();
    std::fs::write(
        dir.path().join(".shelfgauge").join("config.toml"),
        "[server]\nport = 9000\nbind = \"127.0.0.1\"\n",
    )
    .unwrap();

    let output = run(dir.path(), &["--port", "9100", "--no-scan"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("port = 9100"), "{out}");
    assert!(out.contains("bind = \"127.0.0.1\""), "{out}");
    assert!(out.contains("background scan: off"), "{out}");
}

#[test]
fn unknown_config_keys_are_warnings() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".shelfgauge")).unwrap();
    std::fs::write(
        dir.path().join(".shelfgauge").join("config.toml"),
        "[server]\nprot = 9000\n",
    )
    .unwrap();

    let output = run(dir.path(), &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    let err = stderr(&output);
    assert!(
        err.contains("WARN") && err.contains("`server.prot`"),
        "{err}"
    );
}

#[test]
fn invalid_config_fails_with_readable_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".shelfgauge")).unwrap();
    std::fs::write(
        dir.path().join(".shelfgauge").join("config.toml"),
        "[reader]\npart_size = 0\n",
    )
    .unwrap();

    let output = run(dir.path(), &[]);
    assert!(!output.status.success());
    let err = stderr(&output);
    // Regression: the message used to be printed twice (again under "Caused by:").
    assert_eq!(
        err.matches("`reader.part_size` must be greater than 0")
            .count(),
        1,
        "{err}"
    );
    assert!(!err.contains("Caused by"), "{err}");
}

#[test]
fn missing_library_directory_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = run(dir.path(), &["missing"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("does not exist"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn scan_indexes_books_and_reports_a_summary() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("book.fb2"),
        include_bytes!("fixtures/fb2/minimal.fb2"),
    )
    .unwrap();
    let output = run(dir.path(), &["scan", "."]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("1 added"), "{out}");
    assert!(out.contains("(1 parsed)"), "{out}");

    let output = run(dir.path(), &["scan"]);
    let out = stdout(&output);
    assert!(
        out.contains("0 added") && out.contains("1 unchanged"),
        "{out}"
    );
    assert!(out.contains("(0 parsed)"), "{out}");
}

/// Regression: `shelfgauge -v` in an arbitrary directory used to create
/// `.shelfgauge/config.toml` there. Without an explicit PATH, nothing is created.
#[test]
fn no_library_is_created_without_an_explicit_path() {
    let dir = tempfile::tempdir().unwrap();
    for args in [&[][..], &["-v"], &["scan"], &["--no-scan"]] {
        let output = run(dir.path(), args);
        assert!(!output.status.success(), "{args:?}");
        let err = stderr(&output);
        assert!(
            err.contains("is not a shelfgauge library"),
            "{args:?}: {err}"
        );
        assert!(
            err.contains("shelfgauge ."),
            "{args:?}: hint expected: {err}"
        );
        assert!(!dir.path().join(".shelfgauge").exists(), "{args:?}");
    }
    // Explicit `.` creates the library; afterwards the bare command works.
    assert!(run(dir.path(), &["scan", "."]).status.success());
    assert!(run(dir.path(), &["scan"]).status.success());
    assert!(run(dir.path(), &["-v"]).status.success());
}
