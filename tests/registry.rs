//! The server's token registry (`accounts.sqlite3`) and `host token`'s
//! commands, through the real binary in a throwaway home: what each command
//! leaves on disk and prints, and how the registry refuses a newer or
//! damaged file.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;

use rusqlite::Connection;
use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
use tempfile::{TempDir, tempdir};

/// A server's folder: its store's path and the registry beside it.
struct Server {
    home: TempDir,
}

impl Server {
    fn new() -> Self {
        let home = tempdir().unwrap();
        fs::create_dir_all(home.path().join("data")).unwrap();
        Self { home }
    }

    fn db(&self) -> PathBuf {
        self.home.path().join("data").join("memories.sqlite3")
    }

    fn registry(&self) -> PathBuf {
        self.home.path().join("data").join("accounts.sqlite3")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command.args(args).sandboxed(self.home.path());
        command
    }

    /// `host token ARGS --db DB`, with no terminal.
    fn token(&self, args: &[&str]) -> Output {
        let db = self.db();
        self.command(&[&["host", "token"], args, &["--db", db.to_str().unwrap()]].concat())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[track_caller]
fn ok(out: &Output) -> String {
    assert!(out.status.success(), "{}{}", stdout(out), stderr(out));
    stdout(out)
}

#[track_caller]
fn refused(out: &Output) -> String {
    assert_eq!(out.status.code(), Some(1), "{}{}", stdout(out), stderr(out));
    stderr(out)
}

/// The rows `host token list` prints, each split into its columns.
fn listed(server: &Server) -> Vec<Vec<String>> {
    ok(&server.token(&["list"]))
        .lines()
        .skip(1)
        .map(|line| {
            line.split("  ")
                .map(str::trim)
                .filter(|cell| !cell.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .collect()
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn tokens_are_added_listed_and_revoked_by_name() {
    let server = Server::new();
    // With no registry, only the shared file is listed.
    assert_eq!(
        listed(&server),
        [["owner (shared file)", "write", "-", "-", "never"]]
    );
    let laptop = server.path("laptop.token");
    let out = server.token(&["add", "laptop", "--out", laptop.to_str().unwrap()]);
    ok(&out);
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));
    assert!(
        stderr(&out).contains("Wrote the write token for laptop to"),
        "{}",
        stderr(&out)
    );
    let token = fs::read_to_string(&laptop).unwrap();
    assert!(
        token.starts_with("umt_") && token.len() == 56,
        "{} bytes",
        token.len()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(mode(&laptop), 0o600);
        assert_eq!(mode(&server.registry()), 0o600);
        // A registry made readable by others, as a copy can, is owner-only
        // again once opened.
        fs::set_permissions(server.registry(), fs::Permissions::from_mode(0o644)).unwrap();
        listed(&server);
        assert_eq!(mode(&server.registry()), 0o600);
    }
    // No temporary file is left beside it.
    let beside: Vec<_> = fs::read_dir(server.home.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().contains("laptop"))
        .collect();
    assert_eq!(beside, ["laptop.token"]);

    let phone = server.path("phone.token");
    ok(&server.token(&[
        "add",
        "phone",
        "--read-only",
        "--expires",
        "30",
        "--out",
        phone.to_str().unwrap(),
    ]));
    let rows = listed(&server);
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[1][..2], ["laptop", "write"]);
    assert_eq!(rows[1][3], "never");
    assert_eq!(rows[2][..2], ["phone", "read"]);
    assert!(
        rows[2][4] < rows[1][4],
        "--expires comes before idleness: {rows:?}"
    );

    // A name in use is refused, and nothing is written.
    let again = server.path("again.token");
    let said = refused(&server.token(&["add", "laptop", "--out", again.to_str().unwrap()]));
    assert!(
        said.contains("A token named laptop exists; revoke it first with `user-memory host token revoke laptop`."),
        "{said}"
    );
    assert!(!again.exists());

    ok(&server.token(&["revoke", "laptop"]));
    let rows = listed(&server);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[1][0], "phone");
    // A revoked name can be issued again, as a new token.
    ok(&server.token(&["add", "laptop", "--out", again.to_str().unwrap()]));
    assert_ne!(fs::read_to_string(&again).unwrap(), token);

    let said = refused(&server.token(&["revoke", "tablet"]));
    assert!(said.contains("No token named tablet"), "{said}");
}

#[test]
fn a_bad_name_or_expiry_is_refused_before_anything_is_made() {
    let server = Server::new();
    let out = server.path("x.token");
    for args in [
        vec!["add", "with space"],
        vec!["add", ".hidden"],
        vec!["add", "laptop", "--expires", "0"],
    ] {
        let mut args = args.clone();
        args.extend(["--out", out.to_str().unwrap()]);
        let said = refused(&server.token(&args));
        assert!(!said.is_empty(), "{args:?}");
        assert!(!out.exists(), "{args:?}");
    }
    assert!(!server.registry().exists());
}

#[test]
fn add_shows_a_token_only_on_a_terminal() {
    let server = Server::new();
    let out = server.token(&["add", "laptop"]);
    let said = refused(&out);
    assert!(stdout(&out).is_empty());
    assert!(
        said.contains("Not showing the token: standard output is not a terminal. Give --out FILE to write it to a file only you can read."),
        "{said}"
    );
    assert!(
        !server.registry().exists(),
        "nothing is made before the refusal"
    );
    #[cfg(unix)]
    {
        let shown = on_terminal(&server, &["add", "laptop"]);
        assert!(shown.contains("umt_"), "{shown}");
        assert!(shown.contains("shown only this once"), "{shown}");
        assert_eq!(listed(&server)[1][0], "laptop");
    }
}

/// What `host token ARGS` prints with a terminal as its output, through
/// script(1), whose options differ between BSD and util-linux.
#[cfg(unix)]
fn on_terminal(server: &Server, args: &[&str]) -> String {
    let db = server.db();
    let mut words: Vec<String> = vec![
        env!("CARGO_BIN_EXE_user-memory").into(),
        "host".into(),
        "token".into(),
    ];
    words.extend(args.iter().map(|arg| (*arg).to_owned()));
    words.extend(["--db".into(), db.to_str().unwrap().to_owned()]);
    let mut command = Command::new("script");
    if cfg!(target_os = "macos") {
        command.args(["-q", "/dev/null"]).args(&words);
    } else {
        command.args(["-qec", &words.join(" "), "/dev/null"]);
    }
    let out = command
        .sandboxed(server.home.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    stdout(&out)
}

#[test]
fn out_never_replaces_a_file() {
    let server = Server::new();
    let taken = server.path("taken.token");
    fs::write(&taken, "the user's own file").unwrap();
    let said = refused(&server.token(&["add", "laptop", "--out", taken.to_str().unwrap()]));
    assert!(
        said.contains("exists; remove it or give another --out."),
        "{said}"
    );
    assert_eq!(fs::read_to_string(&taken).unwrap(), "the user's own file");
    assert_eq!(listed(&server).len(), 1, "no token was added");
}

#[test]
fn only_add_creates_the_registry() {
    let server = Server::new();
    ok(&server.token(&["list"]));
    refused(&server.token(&["revoke", "laptop"]));
    let _ = server
        .command(&["doctor"])
        .env("USER_MEMORY_DB", server.db())
        .output()
        .unwrap();
    // The server starting.
    let mut host = server
        .command(&[
            "host",
            "--listen",
            "127.0.0.1:0",
            "--db",
            server.db().to_str().unwrap(),
            "--token-file",
            server.path("access-token").to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(host.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(line.starts_with("Listening: http://"), "{line}");
    let _ = host.kill();
    let _ = host.wait();
    assert!(server.db().exists(), "the server ran on its store");
    assert!(!server.registry().exists());
}

/// The registry's tables in creation order, as sqlite_master holds them.
fn statements(path: &Path) -> Vec<String> {
    let conn = Connection::open(path).unwrap();
    conn.prepare("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY rowid")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn version(path: &Path) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

/// Version 1 is the registry `add` creates, statement for statement. A new
/// version keeps this snapshot and adds its own beside it, with a migration.
#[test]
fn a_new_registry_is_version_1_as_its_snapshot_holds() {
    let server = Server::new();
    ok(&server.token(&[
        "add",
        "laptop",
        "--out",
        server.path("l.token").to_str().unwrap(),
    ]));
    assert_eq!(version(&server.registry()), 1);
    let snapshot = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/registry/v1.sql"),
    )
    .unwrap();
    let expected: Vec<String> = snapshot
        .split(";\n")
        .map(|statement| {
            statement
                .lines()
                .filter(|line| !line.starts_with("--"))
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_owned()
        })
        .filter(|statement| !statement.is_empty())
        .collect();
    assert_eq!(statements(&server.registry()), expected);
}

/// A first `add` stopped after creating the file leaves it empty, at version
/// 0: the next command upgrades it like any older registry.
#[test]
fn an_empty_registry_is_upgraded_to_the_current_version() {
    let server = Server::new();
    fs::write(server.registry(), "").unwrap();
    ok(&server.token(&["list"]));
    assert_eq!(version(&server.registry()), 1);
    ok(&server.token(&[
        "add",
        "laptop",
        "--out",
        server.path("l.token").to_str().unwrap(),
    ]));
}

#[test]
fn a_newer_registry_is_refused_and_left_unchanged() {
    let server = Server::new();
    ok(&server.token(&[
        "add",
        "laptop",
        "--out",
        server.path("l.token").to_str().unwrap(),
    ]));
    Connection::open(server.registry())
        .unwrap()
        .pragma_update(None, "user_version", 2)
        .unwrap();
    for args in [vec!["list"], vec!["revoke", "laptop"]] {
        let said = refused(&server.token(&args));
        assert!(
            said.contains(&format!(
                "The server's token registry {} has schema version 2, but this user-memory is older than it (it uses version 1); update user-memory. The registry was left unchanged.",
                server.registry().display()
            )),
            "{args:?}: {said}"
        );
    }
    assert_eq!(version(&server.registry()), 2);
}

#[test]
fn a_damaged_registry_says_how_to_recover() {
    let server = Server::new();
    fs::write(
        server.registry(),
        "not a database, though long enough to be read as one by SQLite's header check",
    )
    .unwrap();
    let said = refused(&server.token(&["list"]));
    assert!(
        said.contains(&format!(
            "The server's token registry {} cannot be read (file is not a database). Move it aside and issue each client a new token with `user-memory host token add`.",
            server.registry().display()
        )),
        "{said}"
    );
    // As the message says: moved aside, a new token can be issued.
    fs::rename(server.registry(), server.path("accounts.sqlite3.damaged")).unwrap();
    ok(&server.token(&[
        "add",
        "laptop",
        "--out",
        server.path("l.token").to_str().unwrap(),
    ]));
}
