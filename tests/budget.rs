//! A tool call's waits share one budget (#227): an embedding endpoint that
//! never answers and a write turn another process holds each give up when
//! the call's budget runs out, and so do the two together, rather than
//! after each one's own bound in turn.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
#[path = "support/serve.rs"]
mod serve;
use serde_json::{Value, json};
use std::{
    io::Read,
    net::TcpListener,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
use tempfile::tempdir;

/// The budget these tests give a tool call, in milliseconds: long enough
/// for the call's own work on a small store, far under the waits it stands
/// in for (a 60 s embedding, a 30 s write turn).
const BUDGET_MS: u64 = 2000;

/// How long past the budget a call may take to answer: the server's start
/// and the call's own work on a small store, about 1.2 s on a loaded
/// machine. Under the shortest wait the budget cuts (a record's 6 s draft
/// embedding), so a wait left at its own bound fails the test.
const SLACK: Duration = Duration::from_millis(3500);

/// The settings that point the server at an embedding endpoint, cleared so
/// only the test's own apply.
const EMBED_ENV: [&str; 6] = [
    "USER_MEMORY_EMBED_URL",
    "USER_MEMORY_EMBED_MODEL",
    "USER_MEMORY_EMBED_DIMENSIONS",
    "USER_MEMORY_EMBED_QUERY_INSTRUCTION",
    "USER_MEMORY_EMBED_API_KEY_FILE",
    "USER_MEMORY_EMBED_ALLOW_HTTP",
];

/// A command of the binary on the store at `db` in the sandbox `home`, with
/// no embedding endpoint unless `embed` names one.
fn binary(db: &Path, home: &Path, subcommand: &str, embed: Option<&str>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
    command
        .arg(subcommand)
        .current_dir(home)
        .sandboxed(home)
        .env("USER_MEMORY_DB", db)
        .env("GIT_CEILING_DIRECTORIES", home)
        .env_remove("USER_MEMORY_URL")
        .env_remove("USER_MEMORY_TOKEN_FILE");
    for name in EMBED_ENV {
        command.env_remove(name);
    }
    if let Some(url) = embed {
        command
            .env("USER_MEMORY_EMBED_URL", url)
            .env("USER_MEMORY_EMBED_MODEL", "fake-model");
    }
    command
}

/// A store at `home` holding one global memory about the quartz lantern.
fn store(home: &Path) -> PathBuf {
    let db = home.join("store/memories.sqlite3");
    let out = binary(&db, home, "record", None)
        .args([
            "--type",
            "fact",
            "--category",
            "build",
            "--title",
            "The quartz lantern build",
            "--content",
            "The quartz lantern build needs the cobalt flag.",
            "--scope",
            "global",
        ])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("\"stored\":true"),
        "{out:?}"
    );
    db
}

/// An embedding endpoint that takes each request and never answers; its
/// URL. The connections it accepts stay open for the test's life.
fn stalled_endpoint() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1/embeddings", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut chunk = [0; 1024];
            let _ = stream.read(&mut chunk);
            held.push(stream);
        }
    });
    url
}

/// The store's lock file, locked by this process, which stands in for
/// another process holding its write turn.
fn held_turn(db: &Path) -> std::fs::File {
    let mut lock = db.as_os_str().to_owned();
    lock.push(".lock");
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(PathBuf::from(lock))
        .unwrap();
    held.lock().unwrap();
    held
}

/// Calls tool `name` with `arguments` through a `serve` on the store at
/// `db`, with the test's budget; its result and how long the server took
/// to answer it.
fn call(
    db: &Path,
    home: &Path,
    embed: Option<&str>,
    name: &str,
    arguments: Value,
) -> (Value, Duration) {
    let mut serve = binary(db, home, "serve", embed);
    serve.env("USER_MEMORY_TOOL_CALL_BUDGET_MS", BUDGET_MS.to_string());
    serve::timed_result(serve, name, arguments)
}

/// How long a stopping server waits for its last backup check, which a
/// locked store keeps waiting (`SHUTDOWN_WAIT` in src/memory/backups.rs).
const BACKUP_SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

/// What a refusal adds when the test's budget, not the wait's own bound,
/// ended the wait.
const RAN_OUT: &str = ", the rest of this tool call's 2 s";

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap()
}

fn within_budget(took: Duration) -> bool {
    took < Duration::from_millis(BUDGET_MS) + SLACK
}

// The query's embedding would wait 60 s; the search gives up on it at the
// budget and answers with its keyword ranking.
#[test]
fn a_search_whose_endpoint_never_answers_keeps_its_keyword_results_within_the_budget() {
    let tmp = tempdir().unwrap();
    let home = tmp.path();
    let db = store(home);
    let url = stalled_endpoint();
    let (result, took) = call(
        &db,
        home,
        Some(&url),
        "search",
        json!({"topic": "quartz lantern"}),
    );
    assert!(within_budget(took), "answered after {took:?}");
    assert_ne!(result["isError"], true, "{result}");
    assert!(text(&result).contains("quartz lantern"), "{result}");
}

// The write turn would wait 30 s for the other process; the record is
// refused at the budget with the stalled turn's message.
#[test]
fn a_record_behind_a_held_write_turn_is_refused_within_the_budget() {
    let tmp = tempdir().unwrap();
    let home = tmp.path();
    let db = store(home);
    let _held = held_turn(&db);
    let (result, took) = call(&db, home, None, "record", record());
    assert!(within_budget(took), "answered after {took:?}");
    assert!(
        text(&result).contains("kept its write turn on the store for"),
        "{result}"
    );
    assert!(text(&result).contains(RAN_OUT), "{result}");
}

// A record meets both: its draft embedding (6 s) and then the held turn
// (30 s). Together they still end at the budget, not at their sum.
#[test]
fn a_record_meeting_a_stalled_endpoint_and_a_held_turn_ends_at_the_budget() {
    let tmp = tempdir().unwrap();
    let home = tmp.path();
    let db = store(home);
    let url = stalled_endpoint();
    let _held = held_turn(&db);
    let (result, took) = call(&db, home, Some(&url), "record", record());
    assert!(within_budget(took), "answered after {took:?}");
    assert!(
        text(&result).contains("kept its write turn on the store for"),
        "{result}"
    );
    assert!(text(&result).contains(RAN_OUT), "{result}");
}

// SQLite's own lock, held by a process outside user-memory's turns (here
// this test), would keep the write waiting 30 s; it gives up at the budget.
#[test]
fn a_record_behind_a_held_sqlite_lock_is_refused_within_the_budget() {
    let tmp = tempdir().unwrap();
    let home = tmp.path();
    let db = store(home);
    let holder = rusqlite::Connection::open(&db).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (result, took) = call(&db, home, None, "record", record());
    holder.execute_batch("ROLLBACK").unwrap();
    assert!(within_budget(took), "answered after {took:?}");
    assert!(text(&result).contains("database is locked"), "{result}");
}

// A store still on SQLite's rollback journal, which another program holds
// in an exclusive transaction, would keep even a search's first read
// waiting 30 s; the search gives up at the budget. The server's own
// connection, still waiting for the lock, does not keep it from exiting:
// only its last backup check does, for its own 5 s.
#[test]
fn a_search_of_a_store_another_program_locks_gives_up_at_the_budget() {
    let tmp = tempdir().unwrap();
    let home = tmp.path();
    let db = store(home);
    let holder = rusqlite::Connection::open(&db).unwrap();
    holder
        .pragma_update(None, "journal_mode", "DELETE")
        .unwrap();
    holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let started = Instant::now();
    let (result, took) = call(
        &db,
        home,
        None,
        "search",
        json!({"topic": "quartz lantern"}),
    );
    let exited = started.elapsed();
    holder.execute_batch("ROLLBACK").unwrap();
    assert!(within_budget(took), "answered after {took:?}");
    assert!(text(&result).contains("database is locked"), "{result}");
    assert!(
        exited < Duration::from_millis(BUDGET_MS) + SLACK + BACKUP_SHUTDOWN_WAIT,
        "exited after {exited:?}"
    );
}

/// A record of a memory unlike the store's one.
fn record() -> Value {
    json!({
        "type": "fact", "category": "build", "title": "The walnut gate",
        "content": "The walnut gate opens with the ember key.", "scope": "global",
        "user_confirmed": true,
    })
}
