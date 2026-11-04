#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

//! Text a forget or a correction removed leaves the store's files: the
//! store, its log and the search index, and so every backup made after.
//! Each case looks at the bytes on disk, through the real binary.

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
#[path = "support/session.rs"]
mod session;
#[path = "support/time.rs"]
mod time;
use session::Serve;

use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::{TempDir, tempdir};

/// A word no other text in a test holds, standing for a secret.
const MARKER: &str = "zq7marker";

/// The marker as the keyword index stores it: the lowercase hex of its
/// bytes (src/memory/index/stems.rs).
fn indexed_marker() -> String {
    hex(MARKER)
}

/// The index's form of the marker after its first letter. The index stores
/// each word after the part it shares with the word before it, so the whole
/// form may never appear, while this tail does.
fn indexed_tail() -> String {
    hex(&MARKER[1..])
}

fn hex(text: &str) -> String {
    text.bytes().map(|b| format!("{b:02x}")).collect()
}

/// A store in a sandbox of its own, holding memories without the marker.
struct Store {
    root: TempDir,
    db: PathBuf,
}

impl Store {
    fn new() -> Self {
        let root = tempdir().unwrap();
        let db = root.path().join("memory.sqlite3");
        let store = Self { root, db };
        record_others(&store, 1..=12);
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

    fn serve(&self) -> Serve {
        Serve::spawn(self.command(&["serve"]))
    }

    /// The store and its log, as files on disk.
    fn files(&self) -> Vec<PathBuf> {
        [self.db.clone(), self.log()]
            .into_iter()
            .filter(|path| path.is_file())
            .collect()
    }

    fn log(&self) -> PathBuf {
        let mut log = self.db.clone().into_os_string();
        log.push("-wal");
        PathBuf::from(log)
    }

    /// The files of the store that hold the marker's bytes.
    fn holding_the_marker(&self) -> Vec<PathBuf> {
        self.files()
            .into_iter()
            .filter(|path| holds_marker(path))
            .collect()
    }

    /// Whether the search index lists the marker as a word. A deleted word
    /// leaves this list before its bytes leave the index, so only the bytes
    /// show that it's gone.
    fn indexes_the_marker(&self) -> bool {
        let conn = Connection::open_with_flags(&self.db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE temp.words USING fts5vocab(main,'search_words','row');",
        )
        .unwrap();
        conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM temp.words WHERE term=?1)",
            [indexed_marker()],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// Makes a backup now and returns its file.
    fn back_up(&self) -> PathBuf {
        let out = self.command(&["backup", "now"]).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        let backups = self.db.with_file_name("backups");
        let mut files: Vec<PathBuf> = fs::read_dir(backups)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        files.sort();
        files.pop().unwrap()
    }
}

/// Whether the file at `path` holds the marker, as text or as the index
/// stores it.
fn holds_marker(path: &Path) -> bool {
    let bytes = fs::read(path).unwrap();
    [MARKER.to_owned(), indexed_tail()].iter().any(|form| {
        bytes
            .windows(form.len())
            .any(|window| window == form.as_bytes())
    })
}

/// Records memories `numbers` without the marker on the command line, as
/// the user's own statements, beside the running server.
fn record_others(store: &Store, numbers: std::ops::RangeInclusive<u32>) {
    for n in numbers {
        let content = format!("Prefers editor theme number{n} for writing");
        let out = store
            .command(&["record", "--category", "tooling", "--scope", "global"])
            .args(["--title", &format!("Theme {n}"), "--content", &content])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    }
}

/// Records a memory holding the marker through `serve`, between others
/// recorded before (by [`Store::new`]) and after it, and returns its id. The
/// others make the keyword index keep the marker's word in a merged
/// segment, where a delete only marks it. The server holds the store open,
/// so the log keeps the marker too: a later check that neither holds it can
/// fail.
fn record_the_marker(store: &Store, serve: &mut Serve) -> String {
    let content = format!("The deploy key {MARKER} is in the vault");
    let reply = serve.call(
        "record",
        json!({"category":"tooling","title":"Deploy key","content":content,"scope":"global","user_confirmed":true}),
    );
    assert_eq!(reply["stored"], true, "{reply}");
    record_others(store, 13..=20);
    assert!(holds_marker(&store.log()), "the log holds the marker");
    assert!(store.indexes_the_marker());
    reply["id"].as_str().unwrap().to_owned()
}

// After a forget, the store, its log while the server still runs, the
// search index and a backup made afterwards hold none of its text.
#[test]
fn a_forgotten_memory_leaves_the_store_its_log_and_later_backups() {
    let store = Store::new();
    let mut serve = store.serve();
    let id = record_the_marker(&store, &mut serve);
    let reply: Value = serve.call("forget", json!({"memory_id":id,"user_confirmed":true}));
    assert_eq!(reply["deleted"], true, "{reply}");
    assert_eq!(store.holding_the_marker(), Vec::<PathBuf>::new());
    serve.close();
    assert!(!holds_marker(&store.back_up()));
}

// The same for text a correction replaced: the memory stays, its old words
// don't.
#[test]
fn text_a_correction_replaced_leaves_the_store_its_log_and_later_backups() {
    let store = Store::new();
    let mut serve = store.serve();
    let id = record_the_marker(&store, &mut serve);
    let reply: Value = serve.call(
        "update",
        json!({"memory_id":id,"content":"The deploy key is in the vault","user_confirmed":true}),
    );
    assert_eq!(reply["updated"], true, "{reply}");
    assert_eq!(store.holding_the_marker(), Vec::<PathBuf>::new());
    serve.close();
    assert!(!holds_marker(&store.back_up()));
}

// An import that takes the file's side of a conflict replaces the store's
// text, as a correction does; on the command line the log is gone once the
// process ends, so the store and a later backup show it.
#[test]
fn text_an_import_replaced_leaves_the_store_and_later_backups() {
    let store = Store::new();
    let mut serve = store.serve();
    let id = record_the_marker(&store, &mut serve);
    serve.close();
    let export = store.root.path().join("export.json");
    let out = store.command(&["export"]).arg(&export).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let mut document: Value = serde_json::from_slice(&fs::read(&export).unwrap()).unwrap();
    for memory in document["memories"].as_array_mut().unwrap() {
        if memory["id"] == id.as_str() {
            memory["content"] = json!("The deploy key is in the vault");
        }
    }
    fs::write(&export, document.to_string()).unwrap();
    let out = store
        .command(&["import", "--apply", "--on-conflict", "use-file"])
        .arg(&export)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(store.holding_the_marker(), Vec::<PathBuf>::new());
    assert!(!holds_marker(&store.back_up()));
}

/// Stores the marker and deletes it as a version before this one did, with
/// SQLite leaving deleted content in place, and checks the file holds it.
fn forget_the_marker_as_before(db: &Path) {
    let conn = Connection::open(db).unwrap();
    conn.pragma_update(None, "secure_delete", false).unwrap();
    conn.execute(
        "INSERT INTO memories (id,memory_type,category,title,content) VALUES ('0bd1ba49-2b63-4aab-8d57-38d9be10a281','fact','tooling','Deploy key',?1)",
        [format!("The deploy key {MARKER} is in the vault")],
    )
    .unwrap();
    conn.execute(
        "DELETE FROM memories WHERE id='0bd1ba49-2b63-4aab-8d57-38d9be10a281'",
        [],
    )
    .unwrap();
    drop(conn);
    assert!(holds_marker(db), "the store holds the forgotten text");
}

fn migrate(store: &Store, env: &[(&str, &str)]) -> std::process::Output {
    let out = store
        .command(&["migrate"])
        .envs(env.iter().copied())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    out
}

// A store from before this version keeps text forgotten then in free space
// inside its pages, which no count shows; migrate, which upgrades the
// installer's Docker server's store, rewrites it without that space.
#[test]
fn migrate_rewrites_a_store_holding_text_forgotten_before() {
    let store = Store::new();
    forget_the_marker_as_before(&store.db);
    migrate(&store, &[]);
    assert_eq!(store.holding_the_marker(), Vec::<PathBuf>::new());
}

// A store without memories has nothing to rewrite, and its file stays as
// it is.
#[test]
fn migrate_leaves_a_store_without_memories_as_it_is() {
    let root = tempdir().unwrap();
    let store = Store {
        db: root.path().join("memory.sqlite3"),
        root,
    };
    let out = store
        .command(&["record", "--category", "tooling", "--scope", "global"])
        .args([
            "--title",
            "Uses zsh",
            "--content",
            "Uses zsh for every shell",
        ])
        .output()
        .unwrap();
    let id = serde_json::from_slice::<Value>(&out.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = store.command(&["forget", &id]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let before = fs::read(&store.db).unwrap();
    migrate(&store, &[]);
    assert_eq!(fs::read(&store.db).unwrap(), before);
}
