//! End-to-end tests of the `shelfgauge` binary: library root, config file, CLI overrides.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_shelfgauge"))
        .args(args)
        .current_dir(dir)
        .env_remove("RUST_LOG")
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run shelfgauge")
}

/// A port that was free a moment ago.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// A running `shelfgauge` server; killed when dropped.
struct Server {
    child: Option<Child>,
    /// The URL from the `Serving <url>` line.
    url: String,
}

impl Server {
    /// Starts the server and waits until it prints its URL.
    fn start(dir: &Path, args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_shelfgauge"))
            .args(args)
            .current_dir(dir)
            .env_remove("RUST_LOG")
            .env("NO_COLOR", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to run shelfgauge");
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Some(rest) = line.strip_prefix("Serving ") {
                    let _ = tx.send(rest.split_whitespace().next().unwrap_or("").to_owned());
                }
            }
        });
        let mut server = Self {
            child: Some(child),
            url: String::new(),
        };
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(url) => server.url = url,
            Err(_) => panic!("shelfgauge did not start serving: {}", server.stop()),
        }
        server
    }

    /// A raw HTTP GET of `path`; returns the whole response.
    fn get(&self, path: &str) -> String {
        let authority = self.url["http://".len()..].split('/').next().unwrap();
        let mut stream = std::net::TcpStream::connect(authority).unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    /// Kills the server and returns its stderr.
    fn stop(mut self) -> String {
        let mut child = self.child.take().unwrap();
        let _ = child.kill();
        stderr(&child.wait_with_output().unwrap())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn first_run_creates_config_and_serves() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let server = Server::start(dir.path(), &[".", "--port", &port.to_string()]);

    let config = dir.path().join(".shelfgauge").join("config.toml");
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        shelfgauge::config::DEFAULT_CONFIG_TOML
    );
    assert_eq!(server.url, format!("http://127.0.0.1:{port}/"));
    let response = server.get("/");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains("No books yet."), "{response}");
    let err = server.stop();
    assert!(err.contains("created the default config"), "{err}");

    // The second run reuses the file and does not announce it again.
    let server = Server::start(dir.path(), &["--port", &free_port().to_string()]);
    let err = server.stop();
    assert!(!err.contains("created the default config"), "{err}");
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
    let file_port = free_port();
    std::fs::create_dir(dir.path().join(".shelfgauge")).unwrap();
    std::fs::write(
        dir.path().join(".shelfgauge").join("config.toml"),
        format!("[server]\nport = {file_port}\nbind = \"127.0.0.1\"\n"),
    )
    .unwrap();

    let server = Server::start(dir.path(), &["--no-scan"]);
    assert_eq!(server.url, format!("http://127.0.0.1:{file_port}/"));
    // The index opened without a running writer serves pages.
    let response = server.get("/");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains("No books yet."), "{response}");
    let err = server.stop();
    assert!(!err.contains("scanning"), "--no-scan starts no scan: {err}");

    // Without --no-scan the book is indexed in the background.
    std::fs::write(
        dir.path().join("book.fb2"),
        include_bytes!("fixtures/fb2/minimal.fb2"),
    )
    .unwrap();
    let port = free_port();
    let server = Server::start(dir.path(), &["--port", &port.to_string(), "--bind", "::1"]);
    assert_eq!(server.url, format!("http://[::1]:{port}/"));
    let deadline = Instant::now() + Duration::from_secs(30);
    while !server.get("/").contains("1 book · ") {
        assert!(Instant::now() < deadline, "the background scan did not run");
        std::thread::sleep(Duration::from_millis(50));
    }
    server.stop();
}

#[test]
fn unknown_config_keys_are_warnings() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".shelfgauge")).unwrap();
    std::fs::write(
        dir.path().join(".shelfgauge").join("config.toml"),
        format!("[server]\nprot = 9000\nport = {}\n", free_port()),
    )
    .unwrap();

    let err = Server::start(dir.path(), &[]).stop();
    assert!(
        err.contains("WARN")
            && err.contains("unknown config key")
            && err.contains("key=\"server.prot\""),
        "{err}"
    );
}

#[test]
fn busy_port_fails_with_a_clear_error() {
    let dir = tempfile::tempdir().unwrap();
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port().to_string();
    let output = run(dir.path(), &[".", "--port", &port]);
    assert!(!output.status.success());
    let err = stderr(&output);
    assert!(err.contains(&format!("port {port} ")), "{err}");
    assert!(
        err.contains("already in use") && err.contains("--port"),
        "{err}"
    );
}

#[test]
fn non_loopback_bind_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let output = run(dir.path(), &[".", "--bind", "0.0.0.0", "--port", "1"]);
    assert!(!output.status.success());
    let err = stderr(&output);
    assert!(err.contains("refusing to listen on 0.0.0.0"), "{err}");
}

/// Ctrl+C stops the server (and a running scan) and exits successfully.
#[cfg(unix)]
#[test]
fn interrupt_shuts_down_gracefully() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("book.fb2"),
        include_bytes!("fixtures/fb2/minimal.fb2"),
    )
    .unwrap();
    let mut server = Server::start(dir.path(), &[".", "--port", &free_port().to_string()]);
    let mut child = server.child.take().unwrap();
    let status = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "shelfgauge did not stop");
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = child.wait_with_output().unwrap();
    assert!(status.success(), "{status}: {}", stderr(&output));
    assert!(
        stderr(&output).contains("Shutting down"),
        "{}",
        stderr(&output)
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

#[test]
fn errors_lists_the_books_that_could_not_be_read() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("book.fb2"),
        include_bytes!("fixtures/fb2/minimal.fb2"),
    )
    .unwrap();
    assert!(run(dir.path(), &["scan", "."]).status.success());
    let output = run(dir.path(), &["errors"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "No books with errors.");

    std::fs::write(dir.path().join("broken.epub"), "not a zip archive").unwrap();
    let out = stdout(&run(dir.path(), &["scan"]));
    assert!(out.contains("1 errors"), "{out}");
    let output = run(dir.path(), &["errors", "."]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2, "{out}");
    assert!(lines[0].starts_with("broken.epub: "), "{out}");
    assert_eq!(lines[1], "1 book could not be read.");
}

/// Regression: `shelfgauge -v` in an arbitrary directory used to create
/// `.shelfgauge/config.toml` there. Without an explicit PATH, nothing is created.
#[test]
fn no_library_is_created_without_an_explicit_path() {
    let dir = tempfile::tempdir().unwrap();
    for args in [&[][..], &["-v"], &["scan"], &["--no-scan"], &["errors"]] {
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
    Server::start(dir.path(), &["-v", "--port", &free_port().to_string()]).stop();
}

/// Runs shelfgauge with `input` on stdin.
fn run_with_input(dir: &Path, args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_shelfgauge"))
        .args(args)
        .current_dir(dir)
        .env_remove("RUST_LOG")
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run shelfgauge");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn profiles_list_add_remove_reset_pin_and_export() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("book.fb2"),
        include_bytes!("fixtures/fb2/minimal.fb2"),
    )
    .unwrap();
    assert!(run(dir.path(), &["scan", "."]).status.success());

    let out = stdout(&run(dir.path(), &["profiles", "list"]));
    assert_eq!(out.trim(), "1  Me  admin, owner  no PIN  no password");
    let output = run(dir.path(), &["profiles", "add", "Anna", "--admin"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("Added the profile Anna (admin)"));
    assert!(
        run(dir.path(), &["profiles", "add", "Ben"])
            .status
            .success()
    );
    let output = run(dir.path(), &["profiles", "add", "anna"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("already has this name"),
        "{}",
        stderr(&output)
    );
    let out = stdout(&run(dir.path(), &["profiles", "list", "."]));
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[1].trim(),
        "2  Anna  admin  no PIN  no password",
        "{out}"
    );
    assert_eq!(
        lines[2].trim(),
        "3  Ben  reader  no PIN  no password",
        "{out}"
    );

    // An unknown name lists the profiles.
    let output = run(dir.path(), &["profiles", "remove", "Carl", "--yes"]);
    assert!(!output.status.success());
    let err = stderr(&output);
    assert!(
        err.contains("no profile named \"Carl\"") && err.contains("Me, Anna, Ben"),
        "{err}"
    );
    // Removal asks first; anything but yes keeps the profile.
    let output = run_with_input(dir.path(), &["profiles", "remove", "ben"], "n\n");
    assert!(output.status.success());
    assert!(stdout(&output).contains("Nothing was deleted."));
    let output = run_with_input(dir.path(), &["profiles", "remove", "ben"], "y\n");
    assert!(
        stdout(&output).contains("Deleted the profile Ben."),
        "{}",
        stdout(&output)
    );
    let output = run(dir.path(), &["profiles", "remove", "Me", "--yes"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("cannot be removed"));

    // reset-pin removes a forgotten PIN.
    {
        let data = shelfgauge::annotations::store::UserData::open(
            &dir.path().join(".shelfgauge/userdata.sqlite"),
        )
        .unwrap();
        let hash = shelfgauge::profiles::pin::hash("2468").unwrap();
        shelfgauge::profiles::store::set_pin_hash(
            &data.lock(),
            shelfgauge::profiles::ProfileId::OWNER,
            Some(&hash),
        )
        .unwrap();
    }
    assert!(stdout(&run(dir.path(), &["profiles", "list"])).contains("Me  admin, owner  PIN"));
    let output = run(dir.path(), &["profiles", "reset-pin", "me"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&run(dir.path(), &["profiles", "list"])).contains("owner  no PIN"));

    // Export of one profile goes to its folder.
    let output = run(dir.path(), &["annotations", "export", "--profile", "anna"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("Anna (2)"), "{}", stdout(&output));
    let output = run(
        dir.path(),
        &["annotations", "export", "--profile", "nobody"],
    );
    assert!(!output.status.success());
}

/// `auth = "password"` needs an admin with a password; `set-password` reads it from
/// standard input in scripts; the running server tells this computer from a request
/// through an `allowed_hosts` name by the real TCP peer.
#[test]
fn password_login_setup() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    std::fs::create_dir_all(dir.path().join(".shelfgauge")).unwrap();
    std::fs::write(
        dir.path().join(".shelfgauge/config.toml"),
        format!(
            "[server]\nport = {port}\nauth = \"password\"\n\
             allowed_hosts = [\"books.example.org\"]\n"
        ),
    )
    .unwrap();
    let output = run(dir.path(), &[".", "--no-scan"]);
    assert!(!output.status.success());
    let err = stderr(&output);
    assert!(
        err.contains("needs an admin profile with a password") && err.contains("set-password"),
        "{err}"
    );
    // The server is not running, so the web way goes through `auth = "none"` first.
    assert!(
        err.contains("auth = \"none\"") && err.contains("My profile"),
        "{err}"
    );

    // Too short: refused, nothing stored. Spaces are part of the password.
    let output = run_with_input(dir.path(), &["profiles", "set-password", "me"], "short\n");
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("at least 10 characters"),
        "{}",
        stderr(&output)
    );
    let output = run_with_input(
        dir.path(),
        &["profiles", "set-password", "me"],
        " correct horse \r\n",
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("Set the password of Me"),
        "{}",
        stdout(&output)
    );
    assert_eq!(
        stdout(&run(dir.path(), &["profiles", "list"])).trim(),
        "1  Me  admin, owner  no PIN  password"
    );
    {
        let data = shelfgauge::annotations::store::UserData::open(
            &dir.path().join(".shelfgauge/userdata.sqlite"),
        )
        .unwrap();
        let hash = shelfgauge::profiles::store::password_hash(
            &data.lock(),
            shelfgauge::profiles::ProfileId::OWNER,
        )
        .unwrap()
        .unwrap();
        assert!(shelfgauge::profiles::password::verify(
            " correct horse ",
            &hash
        ));
        assert!(!shelfgauge::profiles::password::verify(
            "correct horse",
            &hash
        ));
    }

    let server = Server::start(dir.path(), &[".", "--no-scan"]);
    let local = server.get("/");
    assert!(local.starts_with("HTTP/1.1 200"), "{local}");
    // The same loopback peer, but through the proxy's name: another device.
    let authority = &server.url["http://".len()..].trim_end_matches('/');
    let mut stream = std::net::TcpStream::connect(authority).unwrap();
    write!(
        stream,
        "GET / HTTP/1.1\r\nHost: books.example.org\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut remote = String::new();
    stream.read_to_string(&mut remote).unwrap();
    assert!(remote.starts_with("HTTP/1.1 303"), "{remote}");
    assert!(remote.contains("location: /login?next=/\r\n"), "{remote}");
    server.stop();

    let output = run(dir.path(), &["profiles", "remove-password", "Me"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&run(dir.path(), &["profiles", "list"])).contains("no password"));
}
