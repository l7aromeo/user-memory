#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

//! Each memory's history, through the real binary: every operation writes
//! its event in its own transaction, whichever path makes the change, with
//! the client and the values it replaced; confirmations merge; forget and
//! `drop_history` leave tombstones; old events go as a backup is made; and
//! exports carry the history.

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
#[path = "support/session.rs"]
mod session;
#[path = "support/time.rs"]
mod time;
use session::Serve;

use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::{TempDir, tempdir};

/// A store in a sandbox of its own.
struct Store {
    root: TempDir,
    db: PathBuf,
}

impl Store {
    fn new() -> Self {
        let root = tempdir().unwrap();
        let db = root.path().join("memory.sqlite3");
        Self { root, db }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .args(args)
            .sandboxed(self.root.path())
            .env("USER_MEMORY_DB", &self.db)
            .env_remove("USER_MEMORY_URL")
            .env_remove("USER_MEMORY_TOKEN_FILE");
        command
    }

    fn output(&self, args: &[&str]) -> Output {
        let out = self.command(args).output().unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        out
    }

    fn value(&self, args: &[&str]) -> Value {
        serde_json::from_slice(&self.output(args).stdout).unwrap()
    }

    /// Records a global memory and returns its id.
    fn record(&self, category: &str, content: &str, extra: &[&str]) -> String {
        let mut args = vec![
            "record",
            "--category",
            category,
            "--scope",
            "global",
            "--title",
            content,
            "--content",
            content,
        ];
        args.extend_from_slice(extra);
        let out = self.value(&args);
        assert_eq!(out["stored"], true, "{out}");
        out["id"].as_str().unwrap().to_owned()
    }

    /// The events of memory `id` as `history` shows them.
    fn history(&self, id: &str) -> Vec<Value> {
        let shown = self.value(&["history", id]);
        let memory = shown["memories"]
            .get(0)
            .or_else(|| shown["forgotten"].get(0))
            .unwrap_or_else(|| panic!("{id}: {shown}"));
        memory["history"].as_array().unwrap().clone()
    }

    fn ops(&self, id: &str) -> Vec<String> {
        self.history(id)
            .iter()
            .map(|e| e["op"].as_str().unwrap().to_owned())
            .collect()
    }

    fn sql(&self) -> Connection {
        Connection::open(&self.db).unwrap()
    }
}

/// The operations the store's events table accepts, read from its schema,
/// so a new operation cannot go untested here.
fn operations(store: &Store) -> BTreeSet<String> {
    let sql: String = store
        .sql()
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='memory_events'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let list = sql.split("op IN (").nth(1).unwrap();
    let list = &list[..list.find(')').unwrap()];
    list.split(',')
        .map(|op| op.trim().trim_matches('\'').to_owned())
        .collect()
}

/// The newest event of memory `id`.
fn last(store: &Store, id: &str) -> Value {
    store.history(id).last().unwrap().clone()
}

/// Each operation's case: makes the change through the command line and
/// checks the event it wrote. Returns the operation it covered.
type Case = fn(&Store) -> &'static str;

fn record(s: &Store) -> &'static str {
    let id = s.record(
        "editors",
        "Use Helix for editing",
        &["--origin", "chat/one"],
    );
    assert_eq!(
        s.history(&id),
        [json!({"op":"record","at":s.history(&id)[0]["at"],"origin":"chat/one","client":"cli"})]
    );
    "record"
}

fn confirm(s: &Store) -> &'static str {
    let id = s.record("shells", "Use zsh as the login shell", &[]);
    s.value(&[
        "record",
        "--category",
        "shells",
        "--scope",
        "global",
        "--title",
        "Use zsh",
        "--content",
        "Use zsh",
        "--confirms",
        &id,
        "--origin",
        "chat/two",
    ]);
    let event = last(s, &id);
    assert_eq!(event["op"], "confirm", "{event}");
    assert_eq!(event["origin"], "chat/two", "{event}");
    assert!(event.get("before").is_none(), "{event}");
    "confirm"
}

fn update(s: &Store) -> &'static str {
    let id = s.record(
        "fonts",
        "Use a serif font in documents",
        &["--reason", "Easier to read"],
    );
    s.value(&[
        "update",
        &id,
        "--content",
        "Use a sans font in documents",
        "--reason",
        "Cleaner",
    ]);
    let event = last(s, &id);
    assert_eq!(event["op"], "update", "{event}");
    assert_eq!(
        event["before"],
        json!({"content":"Use a serif font in documents","reason":"Easier to read"})
    );
    "update"
}

fn supersede(s: &Store) -> &'static str {
    let old = s.record("browsers", "Browse with Firefox daily", &[]);
    s.record(
        "browsers",
        "Browse with Safari daily",
        &["--supersedes", &old],
    );
    let event = last(s, &old);
    assert_eq!(event["op"], "supersede", "{event}");
    assert_eq!(event["before"]["status"], "confirmed", "{event}");
    "supersede"
}

fn merge(s: &Store) -> &'static str {
    let a = s.record("tea", "Drinks green tea mornings", &[]);
    let b = s.record("tea", "Morning green tea drinker", &["--distinct-from", &a]);
    let merged = s.record(
        "tea",
        "Drinks green tea every morning",
        &["--supersedes", &a, "--supersedes", &b, "--merge"],
    );
    assert_eq!(s.ops(&merged), ["merge"]);
    "merge"
}

fn forget(s: &Store) -> &'static str {
    let id = s.record("pets", "Has a cat named Miso", &["--origin", "chat/cat"]);
    s.value(&["update", &id, "--content", "Has a cat named Tofu"]);
    s.value(&["forget", &id]);
    let events = s.history(&id[..8]);
    // Tombstones only: what happened and when, no values or conversation.
    for event in &events[..events.len() - 1] {
        let fields: BTreeSet<&str> = event
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(fields, BTreeSet::from(["op", "at"]), "{event}");
    }
    assert_eq!(events.last().unwrap()["op"], "forget");
    assert_eq!(s.ops(&id), ["record", "update", "forget"]);
    "forget"
}

fn import(s: &Store) -> &'static str {
    let id = s.record("music", "Listens to jazz while coding", &[]);
    let file = s.root.path().join("export.json");
    s.output(&["export", file.to_str().unwrap()]);
    let mut document: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    for memory in document["memories"].as_array_mut().unwrap() {
        if memory["id"] == id.as_str() {
            memory["content"] = json!("Listens to blues while coding");
        }
    }
    fs::write(&file, document.to_string()).unwrap();
    s.output(&[
        "import",
        file.to_str().unwrap(),
        "--apply",
        "--on-conflict",
        "use-file",
    ]);
    // The replaced values are recorded with the import.
    let event = s
        .history(&id)
        .into_iter()
        .find(|e| e["op"] == "import" && e.get("imported").is_none())
        .unwrap();
    assert_eq!(
        event["before"],
        json!({"content":"Listens to jazz while coding"})
    );
    assert_eq!(event["client"], "cli");
    "import"
}

fn revive(s: &Store) -> &'static str {
    let old = s.record("coffee", "Takes coffee black", &[]);
    let new = s.record(
        "coffee",
        "Takes coffee with oat milk",
        &["--supersedes", &old],
    );
    s.value(&["forget", &new]);
    s.value(&["update", &old]);
    let event = last(s, &old);
    assert_eq!(event["op"], "revive", "{event}");
    assert_eq!(event["before"]["status"], "superseded", "{event}");
    "revive"
}

fn distinct(s: &Store) -> &'static str {
    let a = s.record("trains", "Prefers window seats on trains", &[]);
    let b = s.record("planes", "Prefers aisle seats on planes", &[]);
    s.value(&["update", &a, "--distinct-from", &b]);
    assert_eq!(last(s, &a)["op"], "distinct");
    assert_eq!(last(s, &b)["op"], "distinct");
    "distinct"
}

fn stale(s: &Store) -> &'static str {
    let a = s.record(
        "habits",
        "Runs every morning before work",
        &["--origin", "chat/run"],
    );
    let b = s.record(
        "habits",
        "Swims on weekends at the lake",
        &["--origin", "chat/swim"],
    );
    let pattern = s.value(&[
        "record",
        "--type",
        "pattern",
        "--category",
        "habits",
        "--scope",
        "global",
        "--title",
        "Exercises regularly",
        "--content",
        "Exercises regularly",
        "--source",
        "inferred",
        "--evidence-id",
        &a,
        "--evidence-id",
        &b,
    ]);
    let pattern = pattern["id"].as_str().unwrap().to_owned();
    // Forgetting a support stales its dependent, which says so.
    s.value(&["forget", &a]);
    let event = last(s, &pattern);
    assert_eq!(event["op"], "stale", "{event}");
    assert_eq!(event["before"], json!({"status":"tentative"}));
    "stale"
}

fn moved(s: &Store) -> &'static str {
    let out = s.value(&[
        "record",
        "--category",
        "builds",
        "--scope",
        "project",
        "--project",
        "github.com/acme/old",
        "--title",
        "Build with make",
        "--content",
        "Build with make",
    ]);
    let id = out["id"].as_str().unwrap().to_owned();
    s.output(&[
        "project",
        "move",
        "github.com/acme/old",
        "github.com/acme/new",
    ]);
    let event = last(s, &id);
    assert_eq!(event["op"], "move", "{event}");
    assert_eq!(event["before"], json!({"project":"github.com/acme/old"}));
    assert_eq!(event["client"], "cli");
    "move"
}

fn origin_removed(s: &Store) -> &'static str {
    let id = s.record("diet", "Eats vegetarian meals", &["--origin", "chat/a"]);
    s.value(&["update", &id, "--origin", "chat/b"]);
    s.value(&["forget", "--origin", "chat/a"]);
    let events = s.history(&id);
    assert_eq!(events.last().unwrap()["op"], "origin_removed");
    // The forgotten conversation is named nowhere in the history.
    assert!(!Value::from(events).to_string().contains("chat/a"));
    "origin_removed"
}

const CASES: &[Case] = &[
    record,
    confirm,
    update,
    supersede,
    merge,
    forget,
    import,
    revive,
    distinct,
    stale,
    moved,
    origin_removed,
];

// Each operation writes its event, and every operation the store knows has
// a case here.
#[test]
fn every_operation_writes_its_event() {
    let store = Store::new();
    let covered: BTreeSet<String> = CASES
        .iter()
        .map(|case| {
            let op = case(&store).to_owned();
            // A write clears the row naming its client before committing.
            assert_eq!(writer_rows(&store), 0, "{op} left its writer row");
            op
        })
        .collect();
    assert_eq!(covered, operations(&store));
}

// A write that names no client, such as another program's, still writes
// its event, with the client empty.
#[test]
fn a_write_that_names_no_client_still_writes_an_event() {
    let store = Store::new();
    let id = store.record("cars", "Drives an electric car", &[]);
    store
        .sql()
        .execute(
            "UPDATE memories SET content='Drives a hybrid car' WHERE id=?1",
            [&id],
        )
        .unwrap();
    let event = last(&store, &id);
    assert_eq!(event["op"], "update");
    assert!(event.get("client").is_none(), "{event}");
    assert_eq!(event["before"], json!({"content":"Drives an electric car"}));
    // Nothing of the writer's row is left behind by the command line.
    assert_eq!(writer_rows(&store), 0);
}

/// Rows left in the table naming a write's client.
fn writer_rows(store: &Store) -> i64 {
    store
        .sql()
        .query_row("SELECT count(*) FROM memory_writer", [], |r| r.get(0))
        .unwrap()
}

/// Longest client name an event holds (`NAME_LIMIT` in history.rs).
const NAME_LIMIT: usize = 240;

// An agent's write names its client from the MCP handshake, cleaned to one
// line and cut to the longest name an event holds: a client sends what it
// likes, and history shows it to agents.
#[test]
fn a_server_names_the_client_of_each_write() {
    let long = "x".repeat(NAME_LIMIT + 60);
    for (n, (sent, recorded)) in [
        ("test", "test".to_owned()),
        ("Claude\nCode   app\t", "Claude Code app".to_owned()),
        (long.as_str(), "x".repeat(NAME_LIMIT)),
        ("  ", "unknown".to_owned()),
    ]
    .into_iter()
    .enumerate()
    {
        let store = Store::new();
        let mut serve = Serve::spawn_as(store.command(&["serve"]), sent);
        let reply = serve.call(
            "record",
            json!({"category":"maps","title":format!("Uses offline maps {n}"),"content":"Uses offline maps","scope":"global","user_confirmed":true}),
        );
        serve.close();
        let id = reply["id"].as_str().unwrap();
        assert_eq!(last(&store, id)["client"], recorded.as_str(), "{sent:?}");
    }
}

// A writer row left behind, as another program or SQL run by hand could
// leave one, would name other programs' writes: doctor and repair report
// it, and repair clears it.
#[test]
fn a_writer_row_left_behind_is_damage_repair_clears() {
    let store = Store::new();
    let id = store.record("cars", "Drives an electric car", &[]);
    store
        .sql()
        .execute(
            "INSERT INTO memory_writer(id,client) VALUES (1,'stray')",
            [],
        )
        .unwrap();
    let out = store.output(&["repair"]);
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .contains("history: a write's client is left in memory_writer"),
        "{out:?}"
    );
    assert_eq!(writer_rows(&store), 0);
    store
        .sql()
        .execute(
            "UPDATE memories SET content='Drives a hybrid car' WHERE id=?1",
            [&id],
        )
        .unwrap();
    assert!(last(&store, &id).get("client").is_none());
}

// An import that brings only a later confirmation records an import event
// with the time the file holds, rather than counting it as a confirmation
// made here.
#[test]
fn an_imported_confirmation_is_an_import_not_a_local_confirmation() {
    let store = Store::new();
    let id = store.record("tea", "Drinks green tea", &[]);
    store.value(&["update", &id]);
    let file = store.root.path().join("export.json");
    store.output(&["export", file.to_str().unwrap()]);
    let mut document: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    for memory in document["memories"].as_array_mut().unwrap() {
        if memory["id"] == id.as_str() {
            memory["last_confirmed_at"] = json!("2099-01-01 00:00:00");
        }
    }
    fs::write(&file, document.to_string()).unwrap();
    store.output(&["import", file.to_str().unwrap(), "--apply"]);
    let events = store.history(&id);
    let ops: Vec<&str> = events.iter().map(|e| e["op"].as_str().unwrap()).collect();
    assert_eq!(ops, ["record", "confirm", "import"], "{events:?}");
    assert!(events[1].get("count").is_none(), "{events:?}");
    assert_eq!(events[2]["client"], "cli");
}

// Ten confirmations in a row, by confirms or an update changing nothing,
// are one event; one after a change is a new event.
#[test]
fn confirmations_in_a_row_merge_into_one_event() {
    let store = Store::new();
    let id = store.record("tabs", "Indents with tabs", &[]);
    for n in 0..10 {
        if n % 2 == 0 {
            store.value(&["update", &id]);
        } else {
            store.value(&[
                "record",
                "--category",
                "tabs",
                "--scope",
                "global",
                "--title",
                "Tabs",
                "--content",
                "Tabs",
                "--confirms",
                &id,
            ]);
        }
    }
    let events = store.history(&id);
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[1]["op"], "confirm");
    assert_eq!(events[1]["count"], 10);
    store.value(&["update", &id, "--content", "Indents with two tabs"]);
    store.value(&["update", &id]);
    assert_eq!(store.ops(&id), ["record", "confirm", "update", "confirm"]);
}

// drop_history leaves the earlier events as tombstones and keeps no
// replaced values for its own change, on a correction of what the memory
// says and on one of its title alone, which update writes on its own path.
#[test]
fn drop_history_leaves_tombstones_without_values() {
    for correction in [
        ["--content", "Stores the key in a manager"],
        ["--title", "Key manager"],
    ] {
        let store = Store::new();
        let id = store.record("keys", "Stores the key in a file", &["--origin", "chat/k"]);
        store.value(&["update", &id, "--content", "Stores the key in a vault"]);
        let mut args = vec!["update", &id];
        args.extend(correction);
        args.push("--drop-history");
        store.value(&args);
        let events = store.history(&id);
        assert_eq!(
            store.ops(&id),
            ["record", "update", "update"],
            "{correction:?}"
        );
        for event in &events[..2] {
            let fields: BTreeSet<&str> = event
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                fields,
                BTreeSet::from(["op", "at"]),
                "{correction:?}: {event}"
            );
        }
        assert!(
            events[2].get("before").is_none(),
            "{correction:?}: {:?}",
            events[2]
        );
        assert_eq!(events[2]["client"], "cli");
    }
}

/// Moves memory `id`'s events back `months`.
fn age(store: &Store, id: &str, months: i64) {
    store
        .sql()
        .execute(
            "UPDATE memory_events SET at=datetime('now',?2),last_at=datetime('now',?2) WHERE memory_id=?1",
            rusqlite::params![id, format!("-{months} months")],
        )
        .unwrap();
}

/// The automatic backups beside the store.
fn backups(store: &Store) -> Vec<PathBuf> {
    fs::read_dir(store.db.with_file_name("backups"))
        .map(|dir| dir.map(|entry| entry.unwrap().path()).collect())
        .unwrap_or_default()
}

fn events_in(db: &Path) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row("SELECT count(*) FROM memory_events", [], |r| r.get(0))
        .unwrap()
}

// Events older than a year go when a backup is made, and only then: a
// write with no backup due prunes nothing, and the backup from before the
// pruning still holds the older event.
#[test]
fn events_past_a_year_go_as_a_backup_is_made() {
    let store = Store::new();
    let old = store.record("boats", "Sails a dinghy", &[]);
    let newer = store.record("hikes", "Hikes in the alps", &[]);
    age(&store, &old, 13);
    age(&store, &newer, 11);
    // Today's backup exists, so this write makes none and prunes nothing.
    assert_eq!(backups(&store).len(), 1);
    store.record("kites", "Flies stunt kites", &[]);
    assert_eq!(store.history(&old).len(), 1);
    // A day later, the next write makes a backup.
    let before = backups(&store).pop().unwrap();
    let yesterday = before.with_file_name("memory-2000-01-01.sqlite3");
    fs::rename(&before, &yesterday).unwrap();
    fs::File::options()
        .write(true)
        .open(&yesterday)
        .unwrap()
        .set_modified(
            std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 24 * 60 * 60),
        )
        .unwrap();
    store.record("bikes", "Rides a gravel bike", &[]);
    assert!(
        store.value(&["history", &old])["memories"][0]["history"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.history(&newer).len(), 1);
    assert_eq!(backups(&store).len(), 2);
    // Yesterday's backup still holds the dropped event.
    let kept: i64 = Connection::open(&yesterday)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM memory_events WHERE memory_id=?1",
            [&old],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, 1);
    assert!(events_in(&yesterday) > 0);
}

// Exports carry the history; an import brings it in marked as imported,
// and a file without it imports as before.
#[test]
fn exports_and_imports_carry_the_history() {
    let store = Store::new();
    let id = store.record("teas", "Brews oolong tea", &["--origin", "chat/tea"]);
    store.value(&["update", &id, "--content", "Brews jasmine tea"]);
    let file = store.root.path().join("export.json");
    store.output(&["export", file.to_str().unwrap()]);
    let document: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!(document["history"].as_array().unwrap().len(), 2);

    let other = Store::new();
    let summary = other.value(&["import", file.to_str().unwrap(), "--apply"]);
    assert_eq!(
        summary["history"],
        json!({"events":2,"skipped":0}),
        "{summary}"
    );
    let events = other.history(&id);
    let imported: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("imported").is_some())
        .collect();
    assert_eq!(imported.len(), 2, "{events:?}");
    assert_eq!(imported[1]["before"], json!({"content":"Brews oolong tea"}));
    assert_eq!(imported[0]["origin"], "chat/tea");
    // When the import ran, in the form of every other time in the store.
    for event in &imported {
        let at = event["imported"].as_str().unwrap();
        assert_eq!(at.len(), "2026-11-15 07:00:00".len(), "{event}");
        assert_eq!(&at[10..11], " ", "{event}");
        assert_eq!(at.len(), event["at"].as_str().unwrap().len(), "{event}");
    }
    // The import's own event is this store's, never imported.
    assert!(
        events
            .iter()
            .any(|e| e["op"] == "import" && e.get("imported").is_none())
    );

    // A hand-edited event that this store could not have written is skipped.
    let mut edited = document.clone();
    edited["history"][0]["op"] = json!("erase");
    edited.as_object_mut().unwrap().remove("exported_at");
    let third = Store::new();
    let path = third.root.path().join("edited.json");
    fs::write(&path, edited.to_string()).unwrap();
    let summary = third.value(&["import", path.to_str().unwrap(), "--apply"]);
    assert_eq!(
        summary["history"],
        json!({"events":1,"skipped":1}),
        "{summary}"
    );

    // A file without history, as 0.9 wrote, imports unchanged.
    let mut bare = document;
    bare.as_object_mut().unwrap().remove("history");
    let fourth = Store::new();
    let path = fourth.root.path().join("bare.json");
    fs::write(&path, bare.to_string()).unwrap();
    let summary = fourth.value(&["import", path.to_str().unwrap(), "--apply"]);
    assert_eq!(summary["applied"], true, "{summary}");
    assert!(summary.get("history").is_none(), "{summary}");
    assert_eq!(fourth.ops(&id), ["import"]);
}

// A reason a correction replaced comes back from the history.
#[test]
fn a_replaced_reason_is_restored_from_the_history() {
    let store = Store::new();
    let id = store.record(
        "reviews",
        "Reviews pull requests before lunch",
        &["--reason", "Mornings are for deep work"],
    );
    store.value(&[
        "update",
        &id,
        "--content",
        "Reviews pull requests after lunch",
        "--reason",
        "",
    ]);
    let replaced = store
        .history(&id)
        .into_iter()
        .rev()
        .find_map(|e| e["before"]["reason"].as_str().map(str::to_owned))
        .unwrap();
    store.value(&["update", &id, "--reason", &replaced]);
    let read = store.value(&["read", &id]);
    assert_eq!(read["memories"][0]["reason"], "Mornings are for deep work");
}
