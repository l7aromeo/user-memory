#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

//! A server's own backup checks, through the real binary: at start, while
//! it runs once the store is quiet, and as it stops. The checks after each
//! write are tested beside the backup code (src/memory/backups.rs).

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
#[path = "support/time.rs"]
mod time;
use time::STUCK;
#[path = "support/session.rs"]
mod session;
use session::Serve;

use rusqlite::{Connection, OpenFlags};
use serde_json::json;
use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime},
};
use tempfile::{TempDir, tempdir};

const HOUR: Duration = Duration::from_secs(60 * 60);
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// A store in a sandbox of its own.
struct Store {
    root: TempDir,
    db: PathBuf,
}

impl Store {
    /// A store holding one memory, recorded on the command line, which made
    /// today's backup.
    fn new() -> Self {
        let root = tempdir().unwrap();
        let db = root.path().join("memory.sqlite3");
        let store = Self { root, db };
        store.record_on_the_command_line("Uses zsh for every shell session");
        assert_eq!(store.backups(), [store.backup(0)]);
        store
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .args(args)
            .sandboxed(self.root.path())
            .env("USER_MEMORY_DB", &self.db);
        command
    }

    fn record_on_the_command_line(&self, content: &str) {
        let out = self
            .command(&["record", "--category", "tooling", "--scope", "global"])
            .args(["--title", content, "--content", content])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    }

    /// The backup made `days` ago, as backups are named.
    fn backup(&self, days: u32) -> PathBuf {
        let date: String = Connection::open_in_memory()
            .unwrap()
            .query_row("SELECT date('now',?1)", [format!("-{days} days")], |r| {
                r.get(0)
            })
            .unwrap();
        self.db
            .with_file_name("backups")
            .join(format!("memory-{date}.sqlite3"))
    }

    /// The automatic backups, newest first.
    fn backups(&self) -> Vec<PathBuf> {
        let mut backups: Vec<PathBuf> = fs::read_dir(self.db.with_file_name("backups"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_file() && path.extension().is_some_and(|e| e == "sqlite3"))
            .collect();
        backups.sort();
        backups.reverse();
        backups
    }

    /// Makes today's backup `days` old, renamed for its day, and its
    /// file `age` old.
    fn age_todays_backup(&self, days: u32, age: Duration) -> PathBuf {
        let older = self.backup(days);
        fs::rename(self.backup(0), &older).unwrap();
        set_modified(&older, age);
        older
    }
}

fn set_modified(path: &Path, ago: Duration) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() - ago)
        .unwrap();
}

fn modified(path: &Path) -> SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

/// The memories a backup holds.
fn memories(backup: &Path) -> i64 {
    Connection::open_with_flags(backup, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
        .unwrap()
}

/// Waits until `done` holds while `server` still runs; fails past
/// [`STUCK`], or if the server ends first.
fn wait_while_running(server: &mut Child, what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + STUCK;
    while !done() {
        assert!(
            server.try_wait().unwrap().is_none(),
            "the server ended before {what}"
        );
        assert!(Instant::now() < deadline, "no {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

impl Serve {
    /// A `user-memory serve` of `store`, with the extra environment `env`.
    fn start(store: &Store, env: &[(&str, &str)]) -> Self {
        let mut command = store.command(&["serve"]);
        command.envs(env.iter().copied());
        Self::spawn(command)
    }

    fn record(&mut self, content: &str) {
        let reply = self.call(
            "record",
            json!({"category":"tooling","title":content,"content":content,"scope":"global","user_confirmed":true}),
        );
        assert_eq!(reply["stored"], true, "{reply}");
    }
}

/// A `user-memory host` of `store`, the installer's Docker server's command.
fn host(store: &Store) -> Child {
    let mut child = store
        .command(&["host", "--listen", "127.0.0.1:0", "--db"])
        .arg(&store.db)
        .arg("--token-file")
        .arg(store.root.path().join("token"))
        .env_remove("USER_MEMORY_DB")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(line.starts_with("Listening: "), "{line}");
    child
}

// A store changed after a day-old backup gets one when a server starts,
// before any write: `serve` for an agent session, and `host` for the
// Docker server, which may run for weeks without one.
#[test]
fn starting_a_server_backs_up_changes_made_after_a_day_old_backup() {
    for server in ["serve", "host"] {
        let store = Store::new();
        let yesterday = store.age_todays_backup(1, DAY + HOUR);
        let mut child = match server {
            "serve" => Serve::start(&store, &[]).child,
            _ => host(&store),
        };
        wait_while_running(&mut child, &format!("backup when {server} started"), || {
            store.backup(0).is_file()
        });
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(store.backups(), [store.backup(0), yesterday], "{server}");
        assert_eq!(memories(&store.backup(0)), 1, "{server}");
    }
}

// A store unchanged since its newest backup gets none, however old that
// backup is: copies of the same memories would rotate distinct older
// backups out. Starting and stopping a server must not count as a change.
#[test]
fn a_server_leaves_a_store_unchanged_since_its_newest_backup_alone() {
    let store = Store::new();
    let yesterday = store.age_todays_backup(1, DAY + HOUR);
    set_modified(&store.db, DAY + 2 * HOUR);
    let before = modified(&store.db);
    let mut serve = Serve::start(&store, &[("USER_MEMORY_BACKUP_QUIET_MS", "0")]);
    serve.ask("tools/call", json!({"name":"index","arguments":{}}));
    serve.close();
    assert_eq!(store.backups(), [yesterday]);
    assert_eq!(modified(&store.db), before);
}

// A running server backs up a change once the store has been quiet,
// replacing today's backup rather than adding one, so a week's backups
// stay seven.
#[test]
fn a_running_server_replaces_todays_backup_once_the_store_is_quiet() {
    let store = Store::new();
    for days in 1..=6 {
        fs::copy(store.backup(0), store.backup(days)).unwrap();
        set_modified(&store.backup(days), DAY * days);
    }
    let mut serve = Serve::start(
        &store,
        &[
            ("USER_MEMORY_BACKUP_QUIET_MS", "300"),
            ("USER_MEMORY_BACKUP_CHECK_MS", "50"),
        ],
    );
    serve.record("Prefers rebase merges for pull requests");
    wait_while_running(&mut serve.child, "backup of the quiet store", || {
        memories(&store.backup(0)) == 2
    });
    serve.close();
    assert_eq!(store.backups().len(), 7);
    assert_eq!(memories(&store.backup(0)), 2);
}

// A session that wrote and then ends leaves its write in a backup, with no
// quiet wait: a native store has no server again until the next session.
// The server's last connection copies the log into the store as it closes;
// that comes before the last check, so the next server finds nothing new.
#[test]
fn a_server_that_wrote_backs_up_the_write_as_its_session_ends() {
    let store = Store::new();
    let mut serve = Serve::start(&store, &[]);
    serve.record("Prefers rebase merges for pull requests");
    assert_eq!(memories(&store.backup(0)), 1, "no backup before the end");
    serve.close();
    assert_eq!(store.backups(), [store.backup(0)]);
    assert_eq!(memories(&store.backup(0)), 2);
    let backed_up = modified(&store.backup(0));
    Serve::start(&store, &[("USER_MEMORY_BACKUP_QUIET_MS", "0")]).close();
    assert_eq!(modified(&store.backup(0)), backed_up, "a second copy");
}

// A quiet backup taken while the log still holds the write includes it.
// When the session ends, closing the last connection copies that log into
// the store, which changes the file but not its memories: the last check
// must not copy them again, which past midnight would take a rotation slot.
#[test]
fn a_session_ending_after_a_quiet_backup_makes_no_second_copy() {
    let store = Store::new();
    let mut serve = Serve::start(
        &store,
        &[
            ("USER_MEMORY_BACKUP_QUIET_MS", "300"),
            ("USER_MEMORY_BACKUP_CHECK_MS", "50"),
        ],
    );
    serve.record("Prefers rebase merges for pull requests");
    wait_while_running(&mut serve.child, "backup of the quiet store", || {
        memories(&store.backup(0)) == 2
    });
    let backed_up = modified(&store.backup(0));
    // Past the next check, so a second copy would show in the time.
    std::thread::sleep(Duration::from_millis(200));
    serve.close();
    assert_eq!(modified(&store.backup(0)), backed_up, "a second copy");
}

// A client may end the session with SIGTERM rather than by closing the
// server's input; the last check still runs.
#[cfg(unix)]
#[test]
fn a_server_stopped_by_sigterm_backs_up_its_write() {
    let store = Store::new();
    let mut serve = Serve::start(&store, &[]);
    serve.record("Prefers rebase merges for pull requests");
    let killed = Command::new("kill")
        .args(["-TERM", &serve.child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let out = time::finish(serve.child, "serve after SIGTERM");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(memories(&store.backup(0)), 2);
}

// A server started before its store exists, as the installer's Docker
// server is on a new volume, has nothing to back up and no failure to note.
#[test]
fn a_server_without_a_store_yet_notes_no_failure() {
    let root = tempdir().unwrap();
    let store = Store {
        db: root.path().join("memory.sqlite3"),
        root,
    };
    let mut serve = Serve::start(&store, &[("USER_MEMORY_BACKUP_CHECK_MS", "20")]);
    serve.ask("tools/call", json!({"name":"index","arguments":{}}));
    let stderr = serve.close();
    assert!(!stderr.contains("automatic backup"), "{stderr}");
    assert!(!store.db.with_file_name("backups").exists());
}

// A server whose backups fail reports it once, on stderr and in the note
// doctor reads, and doesn't try again on every check before RETRY.
#[test]
fn a_server_reports_a_failed_backup_once_until_it_may_retry() {
    let root = tempdir().unwrap();
    let store = Store {
        db: root.path().join("memory.sqlite3"),
        root,
    };
    fs::create_dir_all(store.backup(0)).unwrap();
    store.record_on_the_command_line("Uses zsh for every shell session");
    let serve = Serve::start(
        &store,
        &[
            ("USER_MEMORY_BACKUP_QUIET_MS", "0"),
            ("USER_MEMORY_BACKUP_CHECK_MS", "20"),
        ],
    );
    std::thread::sleep(Duration::from_millis(400));
    let stderr = serve.close();
    assert_eq!(
        stderr.matches("the automatic backup of").count(),
        1,
        "{stderr}"
    );
    assert!(
        store
            .db
            .with_file_name("backups")
            .join("last-failure")
            .is_file()
    );
}
