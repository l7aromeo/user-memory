//! Several processes using one local store at once, as Claude Code's and
//! Codex's servers, session hooks and the command line do: the store must
//! stay intact.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
};
use tempfile::tempdir;

#[path = "support/time.rs"]
mod time;

/// Servers writing at once, as two clients plus a hook's would.
const SERVERS: usize = 3;
/// Records each server has in flight at once.
const RECORDS: usize = 160;

fn binary(db: &Path, home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
    command
        .sandboxed(home)
        .env("USER_MEMORY_DB", db)
        .env_remove("USER_MEMORY_URL")
        .env_remove("USER_MEMORY_TOKEN_FILE")
        .env_remove("USER_MEMORY_EMBED_URL");
    command
}

/// Sends `RECORDS` record calls to one server without waiting, then reads
/// every reply; a reply that never comes fails the test instead of hanging
/// it, with the server's stderr.
fn serve_records(db: &Path, home: &Path, server: usize) -> Vec<Value> {
    let mut child = Server::start(db, home, &format!("server {server}"), &[]);
    for i in 1..=RECORDS {
        // Every word unique, so no record is held back as similar.
        let title = format!("Alpha{server}x{i} bravo{server}y{i} charlie{server}z{i}");
        child.call(
            i as u64,
            "record",
            json!({"type":"preference","category":"workflow","title":title,"content":title,"scope":"global","user_confirmed":true}),
        );
    }
    let mut answers = Vec::new();
    while answers.len() < RECORDS {
        answers.push(child.reply(
            time::STUCK,
            &format!("after {} of {RECORDS}", answers.len()),
        ));
    }
    child.stop();
    answers
}

/// A `user-memory serve` child spoken to over MCP on its stdin and stdout,
/// its stderr kept in a file so a failure shows the server's own errors.
struct Server {
    child: std::process::Child,
    input: std::process::ChildStdin,
    replies: mpsc::Receiver<String>,
    stderr: std::path::PathBuf,
    name: String,
}

impl Server {
    /// Starts the server with `env` added, and opens the MCP session.
    fn start(db: &Path, home: &Path, name: &str, env: &[(&str, &str)]) -> Self {
        let stderr = home.join(format!("{name}.stderr"));
        let mut child = binary(db, home)
            .arg("serve")
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (lines, replies) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
        let mut server = Self {
            child,
            input,
            replies,
            stderr,
            name: name.to_owned(),
        };
        server.send(&json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}));
        server.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        server
    }
    fn send(&mut self, message: &Value) {
        writeln!(self.input, "{message}").unwrap();
        self.input.flush().unwrap();
    }
    /// Sends a `name` tool call with `arguments` as request `id`.
    fn call(&mut self, id: u64, name: &str, arguments: Value) {
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}}));
    }
    /// The next reply to a tool call, waiting at most `within`; `waiting`
    /// says what for in the failure, with the server's stderr.
    fn reply(&self, within: std::time::Duration, waiting: &str) -> Value {
        loop {
            let line = self.replies.recv_timeout(within).unwrap_or_else(|_| {
                panic!(
                    "{}: no reply {waiting} within {} s; its stderr:\n{}",
                    self.name,
                    within.as_secs(),
                    std::fs::read_to_string(&self.stderr).unwrap_or_default()
                )
            });
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"].as_u64().is_some_and(|id| id >= 1) {
                return message;
            }
        }
    }
    /// Closes the server's input and waits for it to end.
    fn stop(self) {
        drop(self.input);
        let mut child = self.child;
        child.wait().unwrap();
    }
}

/// How long a server may take to answer a read while its writes wait for
/// another process's write turn. A read needs no turn, so only a loaded
/// machine's scheduling delays it: far below the turn's 30-second wait,
/// which a server answering only between writes would reach.
const READ_WHILE_WRITES_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Reads sent while the writes wait.
const READS: u64 = 3;
/// Time between those reads: the writes start waiting within milliseconds,
/// so the later reads surely come while they wait.
const READ_SPACING: std::time::Duration = std::time::Duration::from_millis(300);

/// Writes a server is sent while another process holds the turn: more than
/// the async workers it is given, so none would be left to answer.
const WAITING_WRITES: u64 = 4;

// A server goes on answering while its writes wait for another process's
// write turn, however few async workers it has: a read needs no turn and
// is answered at once, and the writes go through once the turn is free.
#[test]
fn a_server_answers_a_read_while_its_writes_wait_for_the_turn() {
    let home = tempdir().unwrap();
    let db = home.path().join("store/memories.sqlite3");
    assert!(
        binary(&db, home.path())
            .arg("index")
            .output()
            .unwrap()
            .status
            .success()
    );
    // This test process stands in for another writer holding its turn.
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(home.path().join("store/memories.sqlite3.lock"))
        .unwrap();
    held.lock().unwrap();
    let mut server = Server::start(&db, home.path(), "server", &[("TOKIO_WORKER_THREADS", "2")]);
    for i in 1..=WAITING_WRITES {
        let title = format!("Delta{i} echo{i} foxtrot{i}");
        server.call(
            i,
            "record",
            json!({"type":"preference","category":"workflow","title":title,"content":title,"scope":"global","user_confirmed":true}),
        );
    }
    // Reads sent while the writes wait, spaced so that later ones come once
    // the writes are surely waiting: each is answered within the bound.
    for read in 1..=READS {
        std::thread::sleep(READ_SPACING);
        server.call(100 + read, "search", json!({"topic":"anything"}));
        let reply = server.reply(READ_WHILE_WRITES_WAIT, "to a read");
        assert_eq!(
            reply["id"],
            100 + read,
            "a write was answered during another's turn: {reply}"
        );
    }
    held.unlock().unwrap();
    for _ in 1..=WAITING_WRITES {
        let write = server.reply(time::STUCK, "to a write");
        let text = write["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(text.contains("\"stored\":true"), "{write}");
    }
    server.stop();
}

#[test]
fn servers_writing_one_store_at_once_keep_it_intact() {
    let home = tempdir().unwrap();
    let db = home.path().join("store/memories.sqlite3");
    // Create the store first, as setup does.
    assert!(
        binary(&db, home.path())
            .arg("index")
            .output()
            .unwrap()
            .status
            .success()
    );
    let reading = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let reader = {
        let (db, home, reading) = (db.clone(), home.path().to_owned(), reading.clone());
        std::thread::spawn(move || {
            while reading.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = binary(&db, &home).args(["search", "alpha"]).output();
            }
        })
    };
    let servers: Vec<_> = (0..SERVERS)
        .map(|server| {
            let (db, home) = (db.clone(), home.path().to_owned());
            std::thread::spawn(move || serve_records(&db, &home, server))
        })
        .collect();
    let answers: Vec<Value> = servers
        .into_iter()
        .flat_map(|server| server.join().unwrap())
        .collect();
    reading.store(false, std::sync::atomic::Ordering::Relaxed);
    reader.join().unwrap();
    for answer in &answers {
        let text = answer["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(!text.contains("malformed"), "{answer}");
    }
    // Writes went through: the store is not intact merely by staying empty.
    let stored = answers
        .iter()
        .filter(|a| {
            a["result"]["content"][0]["text"]
                .as_str()
                .is_some_and(|t| t.contains("\"stored\":true"))
        })
        .count();
    assert!(stored > 0, "no record was stored: {}", answers[0]);
    let conn = rusqlite::Connection::open(&db).unwrap();
    let check: Vec<String> = conn
        .prepare("PRAGMA integrity_check")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(check, ["ok"]);
    let dangling: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(dangling, 0);
}

// Writers in every process take turns on the lock file beside the store,
// created owner-only with it: while another process holds the file's lock, a
// write waits, and gives up once the wait for a turn passes (shortened here);
// then it goes ahead.
#[test]
fn a_write_waits_for_another_process_s_turn_on_the_lock_file() {
    let home = tempdir().unwrap();
    let db = home.path().join("store/memories.sqlite3");
    assert!(
        binary(&db, home.path())
            .arg("index")
            .output()
            .unwrap()
            .status
            .success()
    );
    let lock = home.path().join("store/memories.sqlite3.lock");
    // Mode bits are Unix's; on Windows the lock file is made private by the
    // same private::create_file, whose Windows tests cover it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(lock.is_file());
    let record = || {
        binary(&db, home.path())
            .env("USER_MEMORY_TURN_WAIT_MS", "1000")
            .args([
                "record",
                "--type",
                "preference",
                "--category",
                "workflow",
                "--title",
                "Ask before pushing",
                "--content",
                "Ask before pushing",
                "--scope",
                "global",
            ])
            .output()
            .unwrap()
    };
    // This test process stands in for another writer holding its turn.
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock)
        .unwrap();
    held.lock().unwrap();
    let waited = record();
    let said = String::from_utf8_lossy(&waited.stderr);
    assert!(
        !waited.status.success(),
        "wrote during another's turn: {said}"
    );
    assert!(
        said.contains("kept its write turn on the store for 1 s"),
        "{said}"
    );
    held.unlock().unwrap();
    let out = record();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("\"stored\":true"),
        "{out:?}"
    );
}

// A store another process creates while this one opens it is either still
// empty or already current to the opener, never half made: an opener paused
// between reading the version and looking at the tables, while the other
// process creates the schema, still opens it.
#[test]
fn a_store_created_meanwhile_is_never_judged_unversioned() {
    let home = tempdir().unwrap();
    let db = home.path().join("store/memories.sqlite3");
    let pause = home.path().join("pause");
    std::fs::write(&pause, "").unwrap();
    let paused = binary(&db, home.path())
        .env("USER_MEMORY_PAUSE_OPEN", &pause)
        .arg("index")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let reached = home.path().join("pause.reached");
    let deadline = std::time::Instant::now() + time::STUCK;
    while !reached.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the opener never reached the pause"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let creator = binary(&db, home.path()).arg("index").output().unwrap();
    assert!(creator.status.success(), "{creator:?}");
    std::fs::remove_file(&pause).unwrap();
    let out = time::finish(paused, "the paused opener");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
