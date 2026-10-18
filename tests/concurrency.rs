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
/// every reply; a reply that never comes fails the test instead of hanging it.
fn serve_records(db: &Path, home: &Path, server: usize) -> Vec<Value> {
    let mut child = binary(db, home)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
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
    writeln!(input, "{}", json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}})).unwrap();
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    for i in 1..=RECORDS {
        // Every word unique, so no record is held back as similar.
        let title = format!("Alpha{server}x{i} bravo{server}y{i} charlie{server}z{i}");
        let arguments = json!({"type":"preference","category":"workflow","title":title,"content":title,"scope":"global","user_confirmed":true});
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","id":i,"method":"tools/call","params":{"name":"record","arguments":arguments}})
        )
        .unwrap();
    }
    input.flush().unwrap();
    let mut answers = Vec::new();
    while answers.len() < RECORDS {
        let line = replies.recv_timeout(time::STUCK).unwrap_or_else(|_| {
            panic!(
                "server {server}: no reply after {} of {RECORDS}",
                answers.len()
            )
        });
        let message: Value = serde_json::from_str(&line).unwrap();
        if message["id"].as_u64().is_some_and(|id| id >= 1) {
            answers.push(message);
        }
    }
    drop(input);
    child.wait().unwrap();
    answers
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
