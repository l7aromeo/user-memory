#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

//! Text a forget or a correction removed leaves the store's files: the
//! store, its log and the search index, and so every backup made after. A
//! correction's replaced text stays only in the memory's history, until a
//! forget, `drop_history` or pruning takes it out of that too. Each case
//! looks at the bytes on disk, through the real binary.

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

    /// The files of the store that hold the marker as the keyword index
    /// stores it, which a correction's history never holds.
    fn indexing_the_marker(&self) -> Vec<PathBuf> {
        self.files()
            .into_iter()
            .filter(|path| holds(path, &indexed_tail()))
            .collect()
    }

    /// Whether the history of memory `id` holds the marker among the values
    /// its changes replaced.
    fn history_holds_the_marker(&self, id: &str) -> bool {
        let conn = Connection::open_with_flags(&self.db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM memory_events WHERE memory_id=?1 AND instr(before,?2))",
            [id, MARKER],
            |r| r.get(0),
        )
        .unwrap()
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
    holds(path, MARKER) || holds(path, &indexed_tail())
}

/// Whether the file at `path` holds the bytes of `form`.
fn holds(path: &Path, form: &str) -> bool {
    fs::read(path)
        .unwrap()
        .windows(form.len())
        .any(|window| window == form.as_bytes())
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

/// Corrects the marker out of memory `id` through `serve`, and checks that
/// the keyword index no longer holds it while the history does.
fn correct_the_marker(store: &Store, serve: &mut Serve, id: &str) {
    let reply: Value = serve.call(
        "update",
        json!({"memory_id":id,"content":"The deploy key is in the vault","user_confirmed":true}),
    );
    assert_eq!(reply["updated"], true, "{reply}");
    assert_eq!(store.indexing_the_marker(), Vec::<PathBuf>::new());
    assert!(store.history_holds_the_marker(id));
}

// The same for text a correction replaced: the memory stays, its old words
// leave the keyword index at once, and stay in its history only until the
// user drops it, which leaves the store, its log and later backups clean.
#[test]
fn text_a_correction_replaced_leaves_the_store_its_log_and_later_backups() {
    let store = Store::new();
    let mut serve = store.serve();
    let id = record_the_marker(&store, &mut serve);
    correct_the_marker(&store, &mut serve, &id);
    let reply: Value = serve.call(
        "update",
        json!({"memory_id":id,"drop_history":true,"user_confirmed":true}),
    );
    assert_eq!(reply["updated"], true, "{reply}");
    assert!(!store.history_holds_the_marker(&id));
    assert_eq!(store.holding_the_marker(), Vec::<PathBuf>::new());
    serve.close();
    assert!(!holds_marker(&store.back_up()));
}

// Forgetting a memory reduces its history to tombstones: the text its
// corrections replaced leaves the store and its log with it.
#[test]
fn a_forgotten_memorys_history_leaves_the_store_and_its_log() {
    let store = Store::new();
    let mut serve = store.serve();
    let id = record_the_marker(&store, &mut serve);
    correct_the_marker(&store, &mut serve, &id);
    let reply: Value = serve.call("forget", json!({"memory_id":id,"user_confirmed":true}));
    assert_eq!(reply["deleted"], true, "{reply}");
    assert_eq!(store.holding_the_marker(), Vec::<PathBuf>::new());
    serve.close();
    assert!(!holds_marker(&store.back_up()));
}

// History older than its year is dropped as the next automatic backup is
// made, before the copy: the store, its log and that backup lose the
// replaced text, and the backup made before still holds it.
#[test]
fn history_past_its_year_leaves_the_store_as_a_backup_is_made() {
    let store = Store::new();
    let mut serve = store.serve();
    let id = record_the_marker(&store, &mut serve);
    correct_the_marker(&store, &mut serve, &id);
    serve.close();
    // Yesterday's backup, made before the pruning.
    let earlier = store.back_up();
    let yesterday = earlier.with_file_name("memory-2000-01-01.sqlite3");
    fs::rename(&earlier, &yesterday).unwrap();
    let two_days = std::time::Duration::from_secs(2 * 24 * 60 * 60);
    fs::File::options()
        .write(true)
        .open(&yesterday)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - two_days)
        .unwrap();
    assert!(holds_marker(&yesterday));
    // A connection holds the store open from here, so the log keeps what is
    // written to it, as the events aged below are, until a write clears it.
    // A server would do, but its first backup check could make today's
    // backup before the events are aged.
    let held = Connection::open(&store.db).unwrap();
    held.query_row("SELECT count(*) FROM memories", [], |_| Ok(()))
        .unwrap();
    Connection::open(&store.db)
        .unwrap()
        .execute(
            "UPDATE memory_events SET at=datetime('now','-13 months'),last_at=datetime('now','-13 months') WHERE memory_id=?1",
            [&id],
        )
        .unwrap();
    // A write with a backup due makes it, pruning first.
    assert!(holds_marker(&store.log()));
    let out = store
        .command(&["record", "--category", "travel", "--scope", "global"])
        .args([
            "--title",
            "Window seat",
            "--content",
            "Books a window seat on flights",
        ])
        .output()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["stored"],
        true,
        "{out:?}"
    );
    assert!(!store.history_holds_the_marker(&id));
    assert_eq!(
        store.holding_the_marker(),
        Vec::<PathBuf>::new(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    drop(held);
    let backups: Vec<PathBuf> = fs::read_dir(store.db.with_file_name("backups"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(backups.len(), 2, "{backups:?}");
    for backup in backups {
        assert_eq!(holds_marker(&backup), backup == yesterday, "{backup:?}");
    }
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
    // The import keeps the replaced text in the memory's history, as a
    // correction does, and nowhere else.
    assert_eq!(store.indexing_the_marker(), Vec::<PathBuf>::new());
    assert!(store.history_holds_the_marker(&id));
    let out = store
        .command(&["update", &id, "--drop-history"])
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
        "INSERT INTO memories (id,memory_type,category,title,content) VALUES ('e0000001-0000-4000-8000-000000000001','fact','tooling','Deploy key',?1)",
        [format!("The deploy key {MARKER} is in the vault")],
    )
    .unwrap();
    conn.execute(
        "DELETE FROM memories WHERE id='e0000001-0000-4000-8000-000000000001'",
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

// Reindex and repair rebuild the keyword index without dropping its table:
// a word deleted from an index that lost its `secure-delete` (as one
// written by another program might) leaves the store with the rebuild,
// which sets `secure-delete` again, so a memory forgotten after it leaves
// no word either. Repair finds the lost setting as damage.
#[test]
fn a_rebuilt_index_holds_no_forgotten_word_and_drops_the_next_at_once() {
    for rebuild in ["reindex", "repair"] {
        let store = Store::new();
        let mut serve = store.serve();
        let id = record_the_marker(&store, &mut serve);
        Connection::open(&store.db)
            .unwrap()
            .execute(
                "INSERT INTO search_words(search_words, rank) VALUES('secure-delete', 0)",
                [],
            )
            .unwrap();
        let reply: Value = serve.call("forget", json!({"memory_id":id,"user_confirmed":true}));
        assert_eq!(reply["deleted"], true, "{reply}");
        serve.close();
        assert_eq!(
            store.indexing_the_marker(),
            vec![store.db.clone()],
            "{rebuild}: the index keeps the forgotten word"
        );
        let out = store.command(&[rebuild]).output().unwrap();
        assert!(out.status.success(), "{rebuild}: {out:?}");
        if rebuild == "repair" {
            assert!(
                String::from_utf8_lossy(&out.stdout)
                    .contains("search index: it keeps deleted words until its segments merge"),
                "{out:?}"
            );
        }
        assert_eq!(
            store.holding_the_marker(),
            Vec::<PathBuf>::new(),
            "{rebuild}"
        );
        let mut serve = store.serve();
        let id = record_the_marker(&store, &mut serve);
        let reply: Value = serve.call("forget", json!({"memory_id":id,"user_confirmed":true}));
        assert_eq!(reply["deleted"], true, "{reply}");
        assert_eq!(
            store.holding_the_marker(),
            Vec::<PathBuf>::new(),
            "{rebuild}: after the rebuild"
        );
        serve.close();
    }
}

/// Rows of the old keyword index beside the marker's in
/// [`migrate_leaves_no_word_deleted_under_the_old_keyword_index`].
const OLD_ROWS: u32 = 5_000;

// A store from before the keyword index stored its columns keeps a word
// deleted under it in the index's segments until they merge; the upgrade
// replaces that index, and neither the store nor its log keeps the word.
#[test]
fn migrate_leaves_no_word_deleted_under_the_old_keyword_index() {
    let store = Store::new();
    let found = search(&store);
    assert!(!found.is_empty());
    // The index as schema version 7 had it, stamped by an earlier version
    // so its rows are not damage, with the marker's word deleted from it.
    // Words of many other rows make it far larger than the new index, so
    // the upgrade cannot overwrite its pages by reusing them.
    let conn = Connection::open(&store.db).unwrap();
    conn.execute_batch(&format!(
            "DROP TABLE search_words;
CREATE VIRTUAL TABLE search_words USING fts5(category,title,type,body,content='',contentless_delete=1,detail=column,tokenize='ascii');
UPDATE search_state SET stemmed_by='an earlier version';
PRAGMA user_version=7;
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<{OLD_ROWS})
INSERT INTO search_words(rowid,body) SELECT 2000+i,printf('%x %x',i*7919,i*104729) FROM n;
INSERT INTO search_words(rowid,body) VALUES (1000,'{}');
DELETE FROM search_words WHERE rowid=1000;",
            indexed_marker()
        ))
        .unwrap();
    // Schema version 10's count of changes goes too: version 7 had none.
    let counting: Vec<String> = conn
        .prepare(
            "SELECT name FROM sqlite_schema WHERE type='trigger' AND sql LIKE '%store_changes%'",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(!counting.is_empty());
    for trigger in counting {
        conn.execute_batch(&format!("DROP TRIGGER {trigger}"))
            .unwrap();
    }
    conn.execute_batch("DROP TABLE store_changes; DROP TABLE memory_changes;")
        .unwrap();
    // And version 12's record of use.
    conn.execute_batch("DROP TABLE memory_use;").unwrap();
    // Closed, so its log is copied into the file the check reads.
    drop(conn);
    assert!(
        holds(&store.db, &indexed_tail()),
        "the old index holds the word"
    );
    let out = migrate(&store, &[]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("from schema version 7 to"),
        "{out:?}"
    );
    assert_eq!(store.holding_the_marker(), Vec::<PathBuf>::new());
    assert_eq!(search(&store), found, "search after the upgrade");
}

/// The ids search finds for the other memories' words.
fn search(store: &Store) -> Vec<Value> {
    let out = store
        .command(&["search", "editor theme", "--page-size", "50"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let found: Value = serde_json::from_slice(&out.stdout).unwrap();
    found["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].clone())
        .collect()
}

/// Records, through `serve`, a long memory with the marker in its third of
/// five passages, and returns its id and content.
fn record_the_marker_in_a_passage(serve: &mut Serve) -> (String, String) {
    let sentence = "and this sentence keeps the item apart from its neighbours, ".repeat(4);
    let content: Vec<String> = (1..=5)
        .map(|n| {
            let word = if n == 3 { MARKER } else { "harbour" };
            format!("- Item {n} names the {word} {}.", sentence.trim_end())
        })
        .collect();
    let content = content.join(" ");
    let reply = serve.call(
        "record",
        json!({"category":"travel","title":"Harbour list","content":content,"scope":"global","user_confirmed":true}),
    );
    assert_eq!(reply["stored"], true, "{reply}");
    let found = serve.call("search", json!({"topic":MARKER}));
    assert_eq!(found["memories"][0]["passages"], 5, "{found}");
    assert!(
        found["memories"][0]["passage"]
            .as_str()
            .unwrap()
            .contains(MARKER),
        "{found}"
    );
    (reply["id"].as_str().unwrap().to_owned(), content)
}

// A marker word of one passage of a long memory leaves the store and its
// log after a forget, and after a correction of that passage once its
// history is dropped; until then only the history holds it.
#[test]
fn a_word_of_one_passage_leaves_the_store_after_a_forget_or_a_correction() {
    let store = Store::new();
    let mut serve = store.serve();
    let (id, _) = record_the_marker_in_a_passage(&mut serve);
    record_others(&store, 13..=20);
    assert!(store.indexes_the_marker());
    let reply: Value = serve.call("forget", json!({"memory_id":id,"user_confirmed":true}));
    assert_eq!(reply["deleted"], true, "{reply}");
    assert_eq!(store.holding_the_marker(), Vec::<PathBuf>::new());
    let (id, content) = record_the_marker_in_a_passage(&mut serve);
    record_others(&store, 21..=28);
    let reply: Value = serve.call(
        "update",
        json!({"memory_id":id,"content":content.replace(MARKER, "lighthouse"),"user_confirmed":true}),
    );
    assert_eq!(reply["updated"], true, "{reply}");
    assert_eq!(store.indexing_the_marker(), Vec::<PathBuf>::new());
    // The history keeps the replaced text until the user drops it.
    assert!(store.history_holds_the_marker(&id));
    let reply: Value = serve.call(
        "update",
        json!({"memory_id":id,"drop_history":true,"user_confirmed":true}),
    );
    assert_eq!(reply["updated"], true, "{reply}");
    assert_eq!(store.holding_the_marker(), Vec::<PathBuf>::new());
    serve.close();
}
