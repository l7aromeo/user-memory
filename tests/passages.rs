#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

//! Search by passage, through the real binary: a memory is found by the
//! part that matches and returns that part, a literal anchor counts beyond
//! its stems, and a long memory is ranked by its best passage.

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;

use rusqlite::Connection;
use serde_json::Value;
use std::{
    path::PathBuf,
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

    fn output(&self, args: &[&str]) -> Output {
        let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
            .args(args)
            .sandboxed(self.root.path())
            .env("USER_MEMORY_DB", &self.db)
            .env_remove("USER_MEMORY_URL")
            .env_remove("USER_MEMORY_TOKEN_FILE")
            .env_remove("USER_MEMORY_EMBED_URL")
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        out
    }

    fn value(&self, args: &[&str]) -> Value {
        serde_json::from_slice(&self.output(args).stdout).unwrap()
    }

    /// Records a global memory and returns its id.
    fn record(&self, category: &str, title: &str, content: &str, extra: &[&str]) -> String {
        let mut args = vec![
            "record",
            "--category",
            category,
            "--scope",
            "global",
            "--title",
            title,
            "--content",
            content,
        ];
        args.extend_from_slice(extra);
        let out = self.value(&args);
        assert_eq!(out["stored"], true, "{out}");
        out["id"].as_str().unwrap().to_owned()
    }

    /// The hits of a search for `topic`, best first.
    fn search(&self, topic: &str) -> Vec<Value> {
        self.value(&["search", topic, "--status", "all", "--page-size", "50"])["memories"]
            .as_array()
            .unwrap()
            .clone()
    }

    /// Takes away the backups repairs made: a repair names its backup by
    /// the second, so a second one within it would find the name taken.
    fn drop_repair_backups(&self) {
        for entry in std::fs::read_dir(self.root.path()).unwrap() {
            let path = entry.unwrap().path();
            if path.to_string_lossy().contains(".repair-") {
                std::fs::remove_file(path).unwrap();
            }
        }
    }

    fn ids(&self, topic: &str) -> Vec<String> {
        self.search(topic)
            .iter()
            .map(|hit| hit["id"].as_str().unwrap().to_owned())
            .collect()
    }

    fn hit(&self, topic: &str, id: &str) -> Value {
        self.search(topic)
            .into_iter()
            .find(|hit| hit["id"] == id)
            .unwrap_or_else(|| panic!("{topic}: {id} not found"))
    }
}

/// Words no other point shares, one set per point, so each point is found
/// by its own.
const SUBJECTS: [&str; 10] = [
    "aardvark",
    "bassoon",
    "cathedral",
    "dolphin",
    "eucalyptus",
    "flamingo",
    "glacier",
    "harpsichord",
    "iguana",
    "jasmine",
];

/// A memory of ten points, each one sentence long enough to be a passage
/// of its own: recording joins lines, so the list markers and the sentence
/// ends divide it.
fn ten_points() -> Vec<String> {
    SUBJECTS
        .iter()
        .enumerate()
        .map(|(n, subject)| {
            format!(
                "- Point {}: the {subject} note says this item keeps its own detail, apart \
                 from every other item in the list, and runs on as a single sentence with \
                 no stop inside it, long enough that no other point can share its passage.",
                n + 1
            )
        })
        .collect()
}

// A word of the seventh of ten points finds the memory, and search returns
// that point; a one-line memory is one passage, its content.
#[test]
fn a_point_of_a_long_memory_is_found_and_returned_as_its_passage() {
    let store = Store::new();
    let points = ten_points();
    let id = store.record("notes", "Ten points", &points.join("\n"), &[]);
    let line = store.record("shell", "Uses zsh", "Uses zsh as the login shell", &[]);
    let hit = store.hit("glacier", &id);
    assert_eq!(hit["passage"], points[6].as_str(), "{hit}");
    assert_eq!(hit["passages"], 10, "{hit}");
    let hit = store.hit("login shell", &line);
    assert_eq!(hit["passage"], "Uses zsh as the login shell", "{hit}");
    assert_eq!(hit["passages"], 1, "{hit}");
}

// A hit carries every field it did before, as `read` returns them, beside
// its passage.
#[test]
fn a_search_hit_keeps_every_field_read_returns() {
    let store = Store::new();
    let id = store.record(
        "notes",
        "Ten points",
        &ten_points().join("\n"),
        &[
            "--context",
            "When writing notes",
            "--reason",
            "Keeps them apart",
        ],
    );
    let mut hit = store.hit("glacier", &id);
    let fields = hit.as_object_mut().unwrap();
    assert!(fields.remove("passage").is_some());
    assert!(fields.remove("passages").is_some());
    let read = store.value(&["read", &id]);
    assert_eq!(hit, read["memories"][0], "{read}");
}

// A non-empty context is a passage of its own: a word of it finds the
// memory and returns the context.
#[test]
fn a_context_is_a_passage_of_its_own() {
    let store = Store::new();
    let points = ten_points();
    let id = store.record(
        "notes",
        "Ten points",
        &points.join("\n"),
        &["--context", "When the weekly zeppelin review runs"],
    );
    let hit = store.hit("zeppelin", &id);
    assert_eq!(hit["passage"], "When the weekly zeppelin review runs");
    assert_eq!(hit["passages"], 11);
}

// A word written as an anchor (here a file name) counts beyond its stems:
// the memory holding it as written comes first, though the other holds the
// same stems and is newer, which would break a tie. A command's anchor
// keeps its subcommand.
#[test]
fn a_literal_anchor_outranks_the_same_word_stemmed() {
    let store = Store::new();
    let literal = store.record(
        "rust",
        "Build script",
        "Edit `build.rs` before the release",
        &[],
    );
    let stemmed = store.record(
        "crates",
        "Crate builds",
        "The build of the rs crate runs nightly",
        &[],
    );
    assert_eq!(store.ids("build.rs")[..2], [literal.clone(), stemmed]);
    let stash = store.record(
        "git",
        "Stash first",
        "Run `git stash` before switching branches",
        &[],
    );
    let log = store.record(
        "history",
        "Find stash entries",
        "Run `git log` to find which stash entries git kept",
        &[],
    );
    assert_eq!(store.ids("git stash")[..2], [stash, log]);
}

// A long memory matching in five passages does not outrank a short one
// matching as well in one. The long one's best passage holds the topic's
// two rare words, as the short one does; its other passages each hold one
// word most memories hold, which, summed over the whole memory as before,
// put it first. The short one is newer, which breaks the tie.
#[test]
fn a_long_memory_is_ranked_by_its_best_passage() {
    let store = Store::new();
    let common = ["ingress", "certificates", "replicas", "probes"];
    store.record("shell", "Uses zsh", "Uses zsh as the login shell", &[]);
    // Other programs' memories, indexed by the next write, so none is held
    // back as similar to the others.
    let conn = Connection::open(&store.db).unwrap();
    for n in 0..20 {
        conn.execute(
            "INSERT INTO memories (id,memory_type,category,title,content) VALUES (?1,'fact','ops',?2,?3)",
            rusqlite::params![
                format!("00000000-0000-4000-8000-{n:012}"),
                format!("Operations note {n}"),
                format!("Note {n} mentions {}", common.join(" ")),
            ],
        )
        .unwrap();
    }
    drop(conn);
    let filler = "and the rest of this item keeps it apart from the others, ".repeat(4);
    let long: Vec<String> = ["helm kubernetes"]
        .iter()
        .chain(&common)
        .map(|words| format!("- The cluster uses {words} {}.", filler.trim_end()))
        .collect();
    let long = store.record("cluster", "Cluster notes", &long.join(" "), &[]);
    let short = store.record(
        "deploys",
        "Deploys with charts",
        "Deploys helm charts to kubernetes",
        &[],
    );
    let topic = format!("helm kubernetes {}", common.join(" "));
    assert_eq!(store.ids(&topic)[..2], [short, long.clone()]);
    assert_eq!(store.hit(&topic, &long)["passages"], 5);
}

// Reindex and repair rebuild the passages: after either, the seventh point
// is found and returned again.
#[test]
fn reindex_and_repair_rebuild_the_passages() {
    let store = Store::new();
    let points = ten_points();
    let id = store.record("notes", "Ten points", &points.join("\n"), &[]);
    store.output(&["reindex"]);
    assert_eq!(store.hit("glacier", &id)["passage"], points[6].as_str());
    // The seventh point's row goes missing, as damage would lose it.
    Connection::open(&store.db)
        .unwrap()
        .execute("DELETE FROM search_words WHERE rowid % 256 = 7", [])
        .unwrap();
    let out = store.output(&["repair"]);
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .contains("search index: the stored words of 1 memories differ from their text"),
        "{out:?}"
    );
    assert_eq!(store.hit("glacier", &id)["passage"], points[6].as_str());
    store.drop_repair_backups();
    // A passage's stored place goes wrong, as damage would move it.
    Connection::open(&store.db)
        .unwrap()
        .execute(
            "UPDATE search_words SET start=start+1 WHERE rowid % 256 = 7",
            [],
        )
        .unwrap();
    let out = store.output(&["repair"]);
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .contains("search index: the stored words of 1 memories differ from their text"),
        "{out:?}"
    );
    store.drop_repair_backups();
    let again = store.output(&["repair"]);
    assert!(
        String::from_utf8_lossy(&again.stdout).contains("nothing to repair"),
        "{again:?}"
    );
}
