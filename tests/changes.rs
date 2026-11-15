#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

//! The store's count of its changes, through the real binary: every write
//! path in src/ is listed here and run; a change an agent acts on moves the
//! counter and every other write leaves it; after each write the marker
//! beside the store says what the store says; and a marker that does not
//! match the store gives the store a new id at the next write.

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
    time::{Duration, SystemTime},
};
use tempfile::{TempDir, tempdir};

/// A store in a sandbox of its own.
struct Store {
    root: TempDir,
    db: PathBuf,
}

/// The store's id and the number of its latest change.
type State = (String, i64);

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
    fn record(&self, title: &str, extra: &[&str]) -> String {
        let mut args = vec![
            "record",
            "--category",
            "tooling",
            "--scope",
            "global",
            "--title",
            title,
            "--content",
            title,
        ];
        args.extend_from_slice(extra);
        let out = self.value(&args);
        assert_eq!(out["stored"], true, "{out}");
        out["id"].as_str().unwrap().to_owned()
    }

    fn state(&self) -> State {
        Connection::open(&self.db)
            .unwrap()
            .query_row(
                "SELECT store_id,last_change FROM store_changes WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    fn marker_path(&self) -> PathBuf {
        let mut path = self.db.clone().into_os_string();
        path.push(".changed");
        path.into()
    }

    fn marker(&self) -> Option<String> {
        fs::read_to_string(self.marker_path()).ok()
    }

    /// Runs `write` and checks the counter moved or stayed as `moves`
    /// says, under the same id, and that the marker repeats the store.
    fn expect(&self, what: &str, moves: bool, write: impl FnOnce()) {
        let before = self.state();
        write();
        let after = self.state();
        assert_eq!(after.0, before.0, "{what}: the store kept its id");
        if moves {
            assert!(after.1 > before.1, "{what} moves the counter");
        } else {
            assert_eq!(after.1, before.1, "{what} leaves the counter");
        }
        assert_eq!(
            self.marker(),
            Some(format!("{} {}\n", after.0, after.1)),
            "{what}: the marker repeats the store"
        );
    }

    /// An export file of the store, with `edit` applied to each memory.
    fn export(&self, edit: impl Fn(&mut Value)) -> PathBuf {
        let file = self.root.path().join("export.json");
        self.output(&["export", file.to_str().unwrap()]);
        let mut document: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        for memory in document["memories"].as_array_mut().unwrap() {
            edit(memory);
        }
        fs::write(&file, document.to_string()).unwrap();
        file
    }
}

// Each write path's cases, named by the function in src/ that opens the
// write: what it changes and whether a session must hear of it.

fn connect(s: &Store) {
    // A new store starts at no change, with its marker.
    s.output(&["index"]);
    let (id, last) = s.state();
    assert_eq!(last, 0);
    assert_eq!(
        s.marker(),
        Some(format!("{id} 0\n")),
        "a new store's marker"
    );
}

fn record(s: &Store) {
    s.output(&["index"]);
    let mut id = String::new();
    s.expect("a new memory", true, || id = s.record("Use Helix", &[]));
    s.expect("a confirmation", false, || {
        s.value(&[
            "record",
            "--category",
            "tooling",
            "--scope",
            "global",
            "--title",
            "Use Helix",
            "--content",
            "Use Helix",
            "--confirms",
            &id,
            "--origin",
            "chat/two",
        ]);
    });
}

fn update(s: &Store) {
    let id = s.record("Use a serif font", &["--origin", "chat/one"]);
    s.expect("a correction", true, || {
        s.value(&["update", &id, "--content", "Use a sans font"]);
    });
    s.expect("a new conversation", false, || {
        s.value(&["update", &id, "--origin", "chat/two"]);
    });
    s.expect("a dropped history", false, || {
        s.value(&[
            "update",
            &id,
            "--title",
            "Use a serif font",
            "--drop-history",
        ]);
    });
}

fn mark_distinct(s: &Store) {
    let a = s.record("Prefers window seats on trains", &[]);
    let b = s.record("Prefers aisle seats on planes", &[]);
    s.expect("a distinct pair", false, || {
        s.value(&["update", &a, "--distinct-from", &b]);
    });
}

fn forget(s: &Store) {
    let id = s.record("Eats vegetarian", &["--origin", "chat/a"]);
    s.value(&["update", &id, "--origin", "chat/b"]);
    s.expect("one of two conversations forgotten", false, || {
        s.value(&["forget", "--origin", "chat/a"]);
    });
    s.expect("a memory forgotten", true, || {
        s.value(&["forget", &id]);
    });
}

fn keep(s: &Store) {
    let a = s.record("Runs in the morning", &[]);
    let b = s.record("Runs in the evening", &[]);
    s.expect("a group kept", false, || {
        s.value(&["review", "--keep", &a, "--keep", &b]);
    });
}

fn link(s: &Store) {
    s.record("Builds with make", &[]);
    let mut serve = Serve::spawn(s.command(&["serve"]));
    let link = |serve: &mut Serve| {
        serve.call(
            "link",
            json!({"origin":"claude:session/a","projects":["github.com/acme/api"]}),
        )
    };
    s.expect("a project linked", true, || {
        link(&mut serve);
    });
    s.expect("the same link again", false, || {
        link(&mut serve);
    });
    serve.close();
}

fn drop_vectors(s: &Store) {
    s.record("Writes tests first", &[]);
    s.expect("vectors dropped with semantic search off", false, || {
        s.output(&["embedding", "sync"]);
    });
}

fn reindex_with(s: &Store) {
    s.record("Writes tests first", &[]);
    s.expect("a reindex", false, || {
        s.output(&["reindex"]);
    });
}

fn prune(s: &Store) {
    s.record("Writes tests first", &[]);
    let id = s.record("Prefers dark themes", &[]);
    s.value(&["forget", &id]);
    let (_, forgotten) = s.state();
    // Forgotten longer ago than an unused session file lasts.
    let sql = Connection::open(&s.db).unwrap();
    sql.execute(
        "UPDATE memory_changes SET removed_at=datetime('now','-31 days') WHERE memory_id=?1",
        [&id],
    )
    .unwrap();
    let removals = || -> (i64, i64) {
        sql.query_row(
            "SELECT (SELECT COUNT(*) FROM memory_changes WHERE removed_at IS NOT NULL),pruned_through FROM store_changes",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    // Today's backup exists, so this write makes none and drops nothing.
    s.expect("a write with no backup due", true, || {
        s.record("Signs commits", &[]);
    });
    assert_eq!(removals(), (1, 0));
    // A day later, the next write makes a backup, and the old removal goes
    // with it; the counter moves for the write alone.
    let backups = s.db.with_file_name("backups");
    let today = fs::read_dir(&backups)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let yesterday = backups.join("memory-2000-01-01.sqlite3");
    fs::rename(&today, &yesterday).unwrap();
    fs::File::options()
        .write(true)
        .open(&yesterday)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60))
        .unwrap();
    s.expect("a write that makes the day's backup", true, || {
        s.record("Pins toolchains", &[]);
    });
    assert_eq!(
        removals(),
        (0, forgotten),
        "the old removal went with the backup"
    );
}

fn import(s: &Store) {
    s.record("Listens to jazz", &[]);
    let file = s.export(|memory| memory["content"] = json!("Listens to blues"));
    s.expect("an import previewed", false, || {
        s.output(&["import", file.to_str().unwrap()]);
    });
    s.expect("an import applied", true, || {
        s.output(&[
            "import",
            file.to_str().unwrap(),
            "--apply",
            "--on-conflict",
            "use-file",
        ]);
    });
}

fn move_project(s: &Store) {
    s.value(&[
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
    s.expect("a project moved", true, || {
        s.output(&[
            "project",
            "move",
            "github.com/acme/old",
            "github.com/acme/new",
        ]);
    });
}

/// The schema snapshots tests/schema.rs migrates from.
fn snapshot(name: &str) -> String {
    fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/schema")
            .join(name),
    )
    .unwrap()
}

fn run(s: &Store) {
    // A store from before the counter, migrated: it starts counting under
    // a new id, and its marker says so.
    let conn = Connection::open(&s.db).unwrap();
    conn.execute_batch(&snapshot("v9.sql")).unwrap();
    conn.pragma_update(None, "user_version", 9).unwrap();
    conn.execute_batch(&snapshot("v9.seed.sql")).unwrap();
    drop(conn);
    s.output(&["migrate"]);
    let (id, last) = s.state();
    assert_eq!(s.marker(), Some(format!("{id} {last}\n")), "after migrate");
    // Migrating a current store changes nothing.
    s.expect("migrate on a current store", false, || {
        s.output(&["migrate"]);
    });
}

fn reconcile(s: &Store) {
    s.record("Use Helix", &[]);
    // A matching marker: session start reads and writes nothing.
    s.expect("a session start", false, || {
        let out = s
            .command(&["session-start"])
            .current_dir(s.root.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    });
}

type Case = fn(&Store);

/// Every function in src/ that opens a write on the store, with its case.
const PATHS: &[(&str, Case)] = &[
    ("schema::connect", connect),
    ("write::record", record),
    ("write::update", update),
    ("write::mark_distinct", mark_distinct),
    ("write::forget", forget),
    ("review::keep", keep),
    ("links::link", link),
    ("embedding::drop_vectors", drop_vectors),
    ("index::reindex_with", reindex_with),
    ("history::prune", prune),
    ("transfer::import", import),
    ("rekey::move_project", move_project),
    ("migrate::run", run),
    ("changes::reconcile", reconcile),
];

/// The calls that open a write on the store: a write transaction on one
/// connection, or the store's turn for a writer that makes its own.
const OPENERS: [&str; 2] = ["write(&", "store_turn("];

/// Whether `rest`, what follows an opener, is one argument: `write(&conn)`
/// opens a write, where `write(&key, &value)` in setup writes a variable.
fn one_argument(rest: &str) -> bool {
    let name = rest.trim_start_matches(|c: char| c.is_alphanumeric() || c == '_');
    name.starts_with(')')
}

/// Every `file::function` in src/ outside its tests that calls one of
/// [`OPENERS`].
fn write_paths() -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut files = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
    while let Some(next) = files.pop() {
        if next.is_dir() {
            files.extend(fs::read_dir(next).unwrap().map(|e| e.unwrap().path()));
            continue;
        }
        if next.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let text = fs::read_to_string(&next).unwrap();
        let code = text.split("#[cfg(test)]\nmod tests").next().unwrap();
        let stem = next.file_stem().unwrap().to_str().unwrap().to_owned();
        let mut function = None;
        for line in code.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            if let Some(at) = trimmed.find("fn ")
                && (at == 0 || trimmed[..at].ends_with(' '))
            {
                let name: String = trimmed[at + 3..]
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                // The definitions of the openers are not callers.
                if name == "write" || name == "store_turn" {
                    function = None;
                    continue;
                }
                function = Some(name);
            }
            let opens = OPENERS.iter().any(|opener| {
                line.match_indices(opener).any(|(at, _)| {
                    // `fs::write(&…)` and `file.write(&…)` write files.
                    let before = &line[..at];
                    !before.ends_with(['.', '_'])
                        && !before.ends_with("fs::")
                        && !before.ends_with("fn ")
                        && (*opener != "write(&" || one_argument(&line[at + opener.len()..]))
                })
            });
            if opens {
                let function = function
                    .as_deref()
                    .unwrap_or_else(|| panic!("{}: a write outside a function", next.display()));
                found.insert(format!("{stem}::{function}"));
            }
        }
    }
    found
}

// Every write path is listed and run, and each case says which of its
// writes a session must hear of.
#[test]
fn every_write_moves_the_counter_only_for_a_change_an_agent_acts_on() {
    let listed: BTreeSet<String> = PATHS.iter().map(|(name, _)| (*name).to_owned()).collect();
    assert_eq!(listed, write_paths(), "every write path in src/ has a case");
    for (name, case) in PATHS {
        let store = Store::new();
        eprintln!("{name}");
        case(&store);
    }
}

// The marker a write finds must match the store; anything else means the
// store changed without it, so the store takes a new id and every session
// reads it whole once.
#[test]
fn a_marker_that_does_not_match_the_store_gives_it_a_new_id() {
    let store = Store::new();
    store.record("Use Helix", &[]);
    let (id, last) = store.state();
    let cases: [(&str, Option<String>, &str); 5] = [
        ("missing", None, "Prefers dark themes"),
        (
            "unreadable",
            Some("not a marker".to_owned()),
            "Keeps a changelog",
        ),
        (
            "behind",
            Some(format!("{id} {}\n", last - 1)),
            "Signs commits",
        ),
        (
            "ahead",
            Some(format!("{id} {}\n", last + 1)),
            "Reviews on Fridays",
        ),
        (
            "of another store",
            Some(format!("00000000000000000000000000000000 {last}\n")),
            "Pins toolchains",
        ),
    ];
    for (stale, marker, title) in cases {
        let before = store.state();
        match &marker {
            Some(text) => fs::write(store.marker_path(), text).unwrap(),
            None => fs::remove_file(store.marker_path()).unwrap(),
        }
        store.record(title, &[]);
        let after = store.state();
        assert_ne!(after.0, before.0, "a marker {stale} renews the id");
        assert_eq!(
            store.marker(),
            Some(format!("{} {}\n", after.0, after.1)),
            "{stale}"
        );
    }
    // A matching marker keeps the id.
    store.expect("a matching marker", true, || {
        store.record("Writes release notes", &[]);
    });
}

// A backup restored as `backup list` prints it, then written past, or
// first migrated as setup does on an upgrade: the restored store's count is
// behind what sessions saw, so whichever comes first gives it a new id.
#[cfg(unix)]
#[test]
fn a_restored_store_is_read_whole_after_its_next_write() {
    for first in [
        &[
            "record",
            "--category",
            "tooling",
            "--scope",
            "global",
            "--title",
            "Use fish",
            "--content",
            "Use fish",
        ][..],
        &["migrate"],
    ] {
        let store = Store::new();
        store.record("Use Helix", &[]);
        store.output(&["backup", "now"]);
        let backup = store.root.path().join("backups");
        let backup = fs::read_dir(backup)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        store.record("Use zsh", &[]);
        store.record("Use tmux", &[]);
        let seen = store.state();
        // What `backup list` prints, run as printed.
        let listed = String::from_utf8(store.output(&["backup", "list"]).stdout).unwrap();
        let commands: Vec<&str> = listed
            .lines()
            .skip_while(|line| !line.starts_with("To restore one"))
            .skip(1)
            .collect();
        assert!(
            commands[0].contains(".changed"),
            "the restore removes the marker: {listed}"
        );
        let out = Command::new("sh")
            .args(["-c", &commands.join("\n")])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(fs::read(&store.db).unwrap(), fs::read(&backup).unwrap());
        assert_eq!(store.marker(), None);
        store.output(first);
        let after = store.state();
        assert_ne!(
            after.0, seen.0,
            "{first:?}: the restored store has a new id"
        );
        assert_eq!(
            store.marker(),
            Some(format!("{} {}\n", after.0, after.1)),
            "{first:?}"
        );
    }
}

// A marker that missed a change, as after a crash between a commit and its
// marker, is set right at the next session start: the store takes a new id,
// so the sessions running on it read it whole at their next prompt. A
// matching marker is left as it is, and the store unwritten.
#[test]
fn session_start_sets_right_a_marker_that_missed_a_change() {
    let store = Store::new();
    store.record("Use Helix", &[]);
    let (id, last) = store.state();
    let start = |store: &Store| {
        let out = store
            .command(&["session-start"])
            .current_dir(store.root.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    };
    start(&store);
    assert_eq!(store.state(), (id.clone(), last), "a matching marker");
    fs::write(store.marker_path(), format!("{id} {}\n", last - 1)).unwrap();
    start(&store);
    let (renewed, now) = store.state();
    assert_ne!(renewed, id, "a new id");
    assert_eq!(now, last);
    assert_eq!(store.marker(), Some(format!("{renewed} {now}\n")));
}

// Setting a stale marker right waits for the write turn; a long writer then
// still leaves session start its index.
#[test]
fn session_start_prints_the_index_while_a_stale_marker_waits_for_a_writer() {
    let store = Store::new();
    store.record("Use Helix", &[]);
    let (id, last) = store.state();
    fs::write(store.marker_path(), format!("{id} {}\n", last - 1)).unwrap();
    let writer = Connection::open(&store.db).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let out = store
        .command(&["session-start"])
        .current_dir(store.root.path())
        .output()
        .unwrap();
    writer.execute_batch("ROLLBACK").unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Use Helix"), "{text}");
}

// Another program's statement with its own conflict clause replaces the
// clause of the statements its triggers run: the row a change leaves still
// names the latest change and whether the memory is live.
#[test]
fn a_statement_with_its_own_conflict_clause_still_counts_its_change() {
    let s = Store::new();
    let a = s.record("Prefer Rust for tools", &[]);
    let b = s.record("Ask before pushing", &[]);
    let conn = Connection::open(&s.db).unwrap();
    let row = |id: &str| -> (i64, Option<String>) {
        conn.query_row(
            "SELECT change,removed_at FROM memory_changes WHERE memory_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    let last = || s.state().1;
    conn.execute_batch(&format!(
        "CREATE TEMP TABLE kept AS SELECT * FROM memories WHERE id='{a}';
         DELETE FROM memories WHERE id='{a}';"
    ))
    .unwrap();
    assert!(row(&a).1.is_some(), "forgotten");
    for (what, id, sql) in [
        (
            "INSERT OR IGNORE over a forgotten memory",
            &a,
            "INSERT OR IGNORE INTO memories SELECT * FROM kept".to_owned(),
        ),
        (
            "UPDATE OR IGNORE",
            &b,
            format!("UPDATE OR IGNORE memories SET title='Ask before every push' WHERE id='{b}'"),
        ),
        (
            "INSERT OR REPLACE over a live memory",
            &a,
            "INSERT OR REPLACE INTO memories SELECT * FROM kept".to_owned(),
        ),
    ] {
        conn.execute_batch(&sql)
            .unwrap_or_else(|err| panic!("{what}: {err}"));
        assert_eq!(row(id), (last(), None), "{what}");
    }
}
