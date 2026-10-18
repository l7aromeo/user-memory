#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Command};
use tempfile::{TempDir, tempdir};

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
    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_user-memory"))
            .args(titled(args))
            .sandboxed(self.root.path())
            .env("USER_MEMORY_DB", &self.db)
            .env_remove("USER_MEMORY_URL")
            .output()
            .unwrap()
    }
    fn value(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        if args.first() == Some(&"search") {
            return one_page(value, args.contains(&"--page-size"));
        }
        value
    }
    /// Runs a command; when a record is held back as similar to existing
    /// memories, records it again as distinct from them. Fixtures use this:
    /// their memories are different even when their words overlap.
    fn resolved(&self, args: &[&str]) -> Value {
        let mut full: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        // A concurrent writer can add a candidate between calls, so retry.
        for _ in 0..10 {
            let refs: Vec<&str> = full.iter().map(String::as_str).collect();
            let out = self.value(&refs);
            if out["reason"] != "similar memories exist" {
                return out;
            }
            for m in out["similar"].as_array().unwrap() {
                full.extend([
                    "--distinct-from".to_owned(),
                    m["id"].as_str().unwrap().to_owned(),
                ]);
            }
        }
        panic!("{args:?} kept meeting new similar memories");
    }
    fn add(&self, kind: &str, content: &str) -> String {
        let out = self.resolved(&[
            "record",
            "--type",
            kind,
            "--category",
            "programming",
            "--content",
            content,
        ]);
        assert_eq!(out["stored"], true);
        out["id"].as_str().unwrap().to_owned()
    }
}

#[test]
fn database_contains_only_the_generic_memory_schema() {
    let s = Store::new();
    s.value(&["search"]);
    let conn = rusqlite::Connection::open(&s.db).unwrap();
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    // Search index tables belong to memory::index and are pinned by tests/retrieval.rs.
    let stored: Vec<_> = tables
        .iter()
        .filter(|name| !name.starts_with("memory_search") && *name != "memory_vectors")
        .collect();
    assert_eq!(
        stored,
        vec![
            "conversation_projects",
            "embedding_cutoffs",
            "memories",
            "memory_distinct",
            "memory_origins"
        ]
    );
    let columns: Vec<String> = conn
        .prepare("PRAGMA table_info(memories)")
        .unwrap()
        .query_map([], |r| r.get(1))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        columns,
        vec![
            "id",
            "created_at",
            "updated_at",
            "memory_type",
            "category",
            "context",
            "title",
            "content",
            "reason",
            "scope",
            "project",
            "source",
            "status",
            "evidence",
            "evidence_ids",
            "superseded_by",
            "last_confirmed_at"
        ]
    );
    assert!(!s.run(&["context"]).status.success());
}

#[test]
fn memory_ids_are_uuid_v4_primary_keys_and_numeric_ids_are_rejected() {
    let s = Store::new();
    let saved = s.value(&[
        "record",
        "--type",
        "preference",
        "--category",
        "workflow",
        "--content",
        "Use concrete examples",
    ]);
    let id = saved["id"]
        .as_str()
        .expect("Memory IDs must be UUID strings");
    assert_eq!(id.len(), 36);
    assert_eq!(&id[14..15], "4");
    assert!("89ab".contains(&id[19..20]));
    assert_eq!(s.value(&["search", "examples"])[0]["id"], id);
    let conn = rusqlite::Connection::open(&s.db).unwrap();
    let stored: String = conn
        .query_row("SELECT id FROM memories", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, id);
    assert!(!s.run(&["forget", "2"]).status.success());
    assert_eq!(
        s.value(&["update", id, "--reason", "Easier to follow"])["id"],
        id
    );
    assert_eq!(s.value(&["forget", id])["deleted"], true);
    assert!(s.value(&["search"]).as_array().unwrap().is_empty());
}

#[test]
fn uuid_order_does_not_override_insertion_recency() {
    let s = Store::new();
    s.value(&["search"]);
    let conn = rusqlite::Connection::open(&s.db).unwrap();
    conn.execute_batch("INSERT INTO memories(id,category,context,content,memory_type,title) VALUES ('ffffffff-ffff-4fff-bfff-ffffffffffff','workflow','','Older preference','preference','Older preference'); INSERT INTO memories(id,category,context,content,memory_type,title) VALUES ('00000000-0000-4000-8000-000000000001','workflow','','Newer preference','preference','Newer preference');").unwrap();
    assert!(conn.execute("INSERT INTO memories(id,category,context,content,memory_type,title) VALUES ('12','workflow','','Invalid','fact','Invalid')",[]).is_err());
    drop(conn);
    assert_eq!(
        s.value(&["search", "workflow"])[0]["content"],
        "Newer preference"
    );
    let out = s.run(&["index"]);
    let profile = String::from_utf8_lossy(&out.stdout);
    assert!(profile.find("Newer preference").unwrap() < profile.find("Older preference").unwrap());
}

#[test]
fn preferences_and_custom_types_are_first_class_memories_without_invented_reasons() {
    let s = Store::new();
    s.add("preference", "Prefer Rust");
    s.add("working-style", "Review the approach before changing code");
    let memories = s.value(&["search", "programming"]);
    assert_eq!(memories.as_array().unwrap().len(), 2);
    let rust = memories
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["content"] == "Prefer Rust")
        .unwrap();
    assert_eq!(rust["type"], "preference");
    assert_eq!(rust["category"], "programming");
    assert_eq!(rust["source"], "explicit");
    assert_eq!(rust["status"], "confirmed");
    assert_eq!(rust.get("reason"), None);
    let profile = s.run(&["index"]);
    assert!(String::from_utf8_lossy(&profile.stdout).contains("Prefer Rust"));
}

#[test]
fn project_memories_do_not_leak_into_other_projects() {
    let s = Store::new();
    s.add("preference", "Prefer Rust generally");
    assert_eq!(
        s.value(&[
            "record",
            "--type",
            "constraint",
            "--category",
            "programming",
            "--content",
            "Use Python for project A",
            "--scope",
            "project",
            "--project",
            "project-a"
        ])["stored"],
        true
    );
    let global = s.value(&["search", "programming"]);
    assert_eq!(global.as_array().unwrap().len(), 1);
    let other = s.value(&["search", "programming", "--project", "project-b"]);
    assert_eq!(other.as_array().unwrap().len(), 1);
    let same = s.value(&["search", "programming", "--project", "project-a"]);
    assert_eq!(same.as_array().unwrap().len(), 2);
}

#[test]
fn observed_behavior_requires_concrete_evidence_and_stays_tentative() {
    let s = Store::new();
    let refused = s.value(&[
        "record",
        "--type",
        "behavior",
        "--category",
        "workflow",
        "--content",
        "Asks for examples",
        "--source",
        "observed",
    ]);
    assert_eq!(refused["stored"], false);
    let stored = s.value(&[
        "record",
        "--type",
        "behavior",
        "--category",
        "workflow",
        "--content",
        "Asked for a concrete example",
        "--source",
        "observed",
        "--evidence",
        "User asked for an example while reviewing the explanation",
    ]);
    assert_eq!(stored["stored"], true);
    let recalled = s.value(&["search", "workflow"]);
    assert_eq!(recalled[0]["status"], "tentative");
}

#[test]
fn inferred_patterns_require_distinct_grounded_evidence_and_invalidate_after_forgetting_support() {
    let s = Store::new();
    let a = s.add("preference", "Prefer concrete examples");
    let b = s.add("preference", "Prefer runnable demonstrations");
    let a = a.to_string();
    let b = b.to_string();
    let single = s.value(&[
        "record",
        "--type",
        "pattern",
        "--category",
        "learning",
        "--content",
        "Learns through examples",
        "--source",
        "inferred",
        "--evidence-id",
        &a,
    ]);
    assert_eq!(single["stored"], false);
    let duplicate = s.value(&[
        "record",
        "--type",
        "pattern",
        "--category",
        "learning",
        "--content",
        "Learns through examples",
        "--source",
        "inferred",
        "--evidence-id",
        &a,
        "--evidence-id",
        &a,
    ]);
    assert_eq!(duplicate["stored"], false);
    let inferred = s.value(&[
        "record",
        "--type",
        "pattern",
        "--category",
        "learning",
        "--content",
        "Learns through examples",
        "--source",
        "inferred",
        "--evidence-id",
        &a,
        "--evidence-id",
        &b,
    ]);
    assert_eq!(inferred["stored"], true);
    assert_eq!(inferred["status"], "tentative");
    let id = inferred["id"].as_str().unwrap().to_string();
    assert_eq!(s.value(&["forget", &a])["deleted"], true);
    assert!(
        s.value(&["search", "learning"])
            .as_array()
            .unwrap()
            .is_empty()
    );
    let stale = s.value(&["search", "learning", "--status", "all"]);
    assert_eq!(stale[0]["id"].as_str().unwrap().to_string(), id);
    assert_eq!(stale[0]["status"], "stale");
}

#[test]
fn global_patterns_cannot_generalize_project_evidence() {
    let s = Store::new();
    let a = s.add("preference", "Global evidence").to_string();
    let local = s.value(&[
        "record",
        "--type",
        "preference",
        "--category",
        "programming",
        "--content",
        "Project-only evidence",
        "--scope",
        "project",
        "--project",
        "a",
    ])["id"]
        .as_str()
        .unwrap()
        .to_string();
    let result = s.value(&[
        "record",
        "--type",
        "pattern",
        "--category",
        "programming",
        "--content",
        "Global inference",
        "--source",
        "inferred",
        "--evidence-id",
        &a,
        "--evidence-id",
        &local,
    ]);
    assert_eq!(result["stored"], false);
}

#[test]
fn corrections_replace_content_and_confirmation_without_leaving_old_preferences_active() {
    let s = Store::new();
    let id = s.add("preference", "Prefer Rust").to_string();
    assert_eq!(
        s.value(&[
            "update",
            &id,
            "--content",
            "Prefer Python for prototypes",
            "--reason",
            "Faster iteration"
        ])["updated"],
        true
    );
    assert!(s.value(&["search", "Rust"]).as_array().unwrap().is_empty());
    let found = s.value(&["search", "Python"]);
    assert_eq!(found[0]["reason"], "Faster iteration");
    assert_eq!(found[0]["status"], "confirmed");
}

#[test]
fn deleting_support_directly_invalidates_inferences() {
    let s = Store::new();
    let a = s.add("preference", "Prefer examples").to_string();
    let b = s.add("preference", "Prefer demos").to_string();
    assert_eq!(
        s.value(&[
            "record",
            "--type",
            "pattern",
            "--category",
            "learning",
            "--content",
            "Prefer practical learning",
            "--source",
            "inferred",
            "--evidence-id",
            &a,
            "--evidence-id",
            &b
        ])["stored"],
        true
    );
    let conn = rusqlite::Connection::open(&s.db).unwrap();
    conn.execute("DELETE FROM memories WHERE id=?1", [a])
        .unwrap();
    drop(conn);
    assert!(
        s.value(&["search", "learning"])
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn correcting_support_invalidates_dependent_inferences() {
    let s = Store::new();
    let a = s.add("preference", "Prefer examples");
    let b = s.add("preference", "Prefer demonstrations");
    let inferred = s.value(&[
        "record",
        "--type",
        "pattern",
        "--category",
        "learning",
        "--content",
        "Prefer practical learning",
        "--source",
        "inferred",
        "--evidence-id",
        &a,
        "--evidence-id",
        &b,
    ]);
    assert_eq!(inferred["stored"], true);
    assert_eq!(
        s.value(&[
            "update",
            &a,
            "--content",
            "Prefer concise reference material"
        ])["updated"],
        true
    );
    assert!(
        s.value(&["search", "learning"])
            .as_array()
            .unwrap()
            .is_empty()
    );
    let stale = s.value(&["search", "learning", "--status", "all"]);
    assert_eq!(stale[0]["id"], inferred["id"]);
    assert_eq!(stale[0]["status"], "stale");
}

#[test]
fn reconfirming_unchanged_support_keeps_dependent_inferences_active() {
    let s = Store::new();
    let a = s.add("preference", "Prefer examples");
    let b = s.add("preference", "Prefer demonstrations");
    let inferred = s.value(&[
        "record",
        "--type",
        "pattern",
        "--category",
        "learning",
        "--content",
        "Prefer practical learning",
        "--source",
        "inferred",
        "--evidence-id",
        &a,
        "--evidence-id",
        &b,
    ]);
    assert_eq!(inferred["stored"], true);
    assert_eq!(
        s.value(&["update", &a, "--content", "Prefer examples"])["updated"],
        true
    );
    let active = s.value(&["search", "learning"]);
    assert_eq!(active[0]["id"], inferred["id"]);
    assert_eq!(active[0]["status"], "tentative");
}

#[test]
fn duplicate_recording_does_not_manufacture_independent_evidence() {
    let s = Store::new();
    let first = s.add("preference", "Prefer examples");
    let again = s.add("preference", "Prefer examples");
    assert_eq!(first, again);
    let id = first.to_string();
    assert_eq!(
        s.value(&[
            "record",
            "--type",
            "pattern",
            "--category",
            "learning",
            "--content",
            "Repeated preference",
            "--source",
            "inferred",
            "--evidence-id",
            &id,
            "--evidence-id",
            &id
        ])["stored"],
        false
    );
}

// A correction that changes the choice gives the new choice's reason, or an
// empty one, since the stored reason is otherwise kept.
#[test]
fn a_changed_choice_gives_its_own_reason() {
    let s = Store::new();
    let saved = s.value(&[
        "record",
        "--type",
        "preference",
        "--category",
        "programming",
        "--content",
        "Prefer Rust",
        "--reason",
        "Performance and safety",
    ]);
    let id = saved["id"].as_str().unwrap().to_string();
    let out = s.value(&[
        "update",
        &id,
        "--content",
        "Prefer Python for prototypes",
        "--reason",
        "",
    ]);
    assert_eq!(out["updated"], true, "{out}");
    assert_eq!(s.value(&["search", "Python"])[0].get("reason"), None);
}

#[test]
fn stable_preferences_are_considered_before_recent_incidental_facts() {
    let s = Store::new();
    s.add("preference", "Prefer Rust");
    let conn = rusqlite::Connection::open(&s.db).unwrap();
    for i in 0..25 {
        conn.execute("INSERT INTO memories(id,category,context,content,memory_type,title) VALUES (?2,'context','','incidental fact '||?1,'fact','incidental fact '||?1)",rusqlite::params![i,uuid::Uuid::new_v4().to_string()]).unwrap();
    }
    drop(conn);
    let profile = s.run(&["index"]);
    assert!(profile.status.success());
    assert!(String::from_utf8_lossy(&profile.stdout).contains("Prefer Rust"));
}

#[test]
fn the_index_lines_a_preference_and_read_keeps_its_conditions() {
    let s = Store::new();
    assert_eq!(
        s.value(&[
            "record",
            "--type",
            "preference",
            "--category",
            "programming",
            "--content",
            "Prefer Python",
            "--context",
            "Only for throwaway prototypes"
        ])["stored"],
        true
    );
    let index = s.run(&["index"]);
    assert!(index.status.success());
    let index = String::from_utf8_lossy(&index.stdout);
    let line = index
        .lines()
        .find(|l| l.ends_with(" Prefer Python"))
        .unwrap();
    let id = line.split_whitespace().next().unwrap();
    assert_eq!(
        s.value(&["read", id])["memories"][0]["context"],
        "Only for throwaway prototypes"
    );
}

fn infer(s: &Store, a: &str, b: &str) -> String {
    let inferred = s.resolved(&[
        "record",
        "--type",
        "pattern",
        "--category",
        "learning",
        "--content",
        "Prefer practical learning",
        "--source",
        "inferred",
        "--evidence-id",
        a,
        "--evidence-id",
        b,
    ]);
    assert_eq!(inferred["stored"], true);
    inferred["id"].as_str().unwrap().to_owned()
}

/// The probe for stale patterns: A observed in conversation c:1 (or in none
/// when `a_origin` is empty), B stated in c:2 and c:4, and pattern P inferred
/// from both.
fn support_probe(a_origin: &str) -> (Store, String, String, String) {
    let s = Store::new();
    let mut observed = vec![
        "--type",
        "behavior",
        "--source",
        "observed",
        "--evidence",
        "Asked for a demo twice",
        "--content",
        "Asks for runnable demos",
    ];
    if !a_origin.is_empty() {
        observed.extend(["--origin", a_origin]);
    }
    let a = stored(&s, &observed);
    let b = stored(
        &s,
        &[
            "--type",
            "preference",
            "--content",
            "Prefer examples over theory",
            "--origin",
            "c:2",
        ],
    );
    rusqlite::Connection::open(&s.db)
        .unwrap()
        .execute(
            "INSERT INTO memory_origins (memory_id,origin) VALUES (?1,'c:4')",
            [&b],
        )
        .unwrap();
    let p = infer(&s, &a, &b);
    assert_eq!(status(&s, &p), "tentative");
    (s, a, b, p)
}

// A pattern goes stale when what supports it weakens or changes meaning, not
// when a supporting memory is confirmed, reworded in its reason or loses one
// of several conversations while two independent ones remain.
#[test]
fn a_pattern_goes_stale_only_when_its_support_weakens() {
    // Confirming an observation strengthens it.
    let (s, a, _, p) = support_probe("c:1");
    assert_eq!(s.value(&["update", &a])["updated"], true);
    assert_eq!(inactive(&s, &a)["source"], "explicit");
    assert_eq!(status(&s, &p), "tentative", "confirming");
    // B keeps c:4, so c:1 and c:4 still stand behind P.
    let (s, _, b, p) = support_probe("c:1");
    s.value(&["forget", "--origin", "c:2"]);
    assert_eq!(inactive(&s, &b)["origins"], json!(["c:4"]));
    assert_eq!(
        status(&s, &p),
        "tentative",
        "forgetting one of B's conversations"
    );
    // A support without a conversation counts as its own.
    let (s, _, _, p) = support_probe("");
    s.value(&["forget", "--origin", "c:2"]);
    assert_eq!(status(&s, &p), "tentative", "an origin-less support");
    // A reason is wording about the memory, not the memory.
    let (s, _, b, p) = support_probe("c:1");
    let reworded = s.value(&["update", &b, "--reason", "Examples stick better"]);
    assert_eq!(reworded["updated"], true, "{reworded}");
    assert_eq!(status(&s, &p), "tentative", "a new reason");

    // What still stales: new content,
    let (s, _, b, p) = support_probe("c:1");
    s.value(&["update", &b, "--content", "Prefer theory before examples"]);
    assert_eq!(status(&s, &p), "stale", "new content");
    // a superseded support,
    let (s, _, b, p) = support_probe("c:1");
    stored(
        &s,
        &[
            "--type",
            "preference",
            "--content",
            "Prefer theory first",
            "--supersedes",
            &b,
            "--origin",
            "c:9",
        ],
    );
    assert_eq!(status(&s, &p), "stale", "a superseded support");
    // fewer than two conversations left, though every support remains,
    let (s, a, b, p) = support_probe("c:1");
    rusqlite::Connection::open(&s.db)
        .unwrap()
        .execute(
            "INSERT INTO memory_origins (memory_id,origin) VALUES (?1,'c:2')",
            [&a],
        )
        .unwrap();
    s.value(&["forget", "--origin", "c:1"]);
    assert_eq!(status(&s, &p), "tentative", "c:2 and c:4 remain");
    s.value(&["forget", "--origin", "c:4"]);
    assert_eq!(inactive(&s, &a)["origins"], json!(["c:2"]));
    assert_eq!(inactive(&s, &b)["origins"], json!(["c:2"]));
    assert_eq!(status(&s, &p), "stale", "only c:2 remains");
    // and a support forgotten entirely.
    let (s, a, _, p) = support_probe("c:1");
    s.value(&["forget", &a]);
    assert_eq!(status(&s, &p), "stale", "a forgotten support");
}

fn status(s: &Store, id: &str) -> Value {
    s.value(&["search", "--status", "all", "--page-size", "20"])
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .unwrap_or_else(|| panic!("{id} was not recalled"))["status"]
        .clone()
}

#[test]
fn search_filters_by_type_and_category() {
    let s = Store::new();
    s.add("preference", "Prefer Rust");
    s.add("goal", "Ship the parser");
    s.value(&[
        "record",
        "--type",
        "preference",
        "--category",
        "writing",
        "--content",
        "Prefer short sentences",
    ]);
    let contents = |args: &[&str]| -> Vec<String> {
        let mut found: Vec<String> = s
            .value(args)
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["content"].as_str().unwrap().to_owned())
            .collect();
        found.sort();
        found
    };
    assert_eq!(
        contents(&["search", "--type", "Preference"]),
        ["Prefer Rust", "Prefer short sentences"]
    );
    assert_eq!(
        contents(&["search", "--category", "Writing"]),
        ["Prefer short sentences"]
    );
    assert_eq!(
        contents(&["search", "--type", "goal", "--category", "programming"]),
        ["Ship the parser"]
    );
}

#[test]
fn newest_memory_is_found_in_a_large_store() {
    let s = Store::new();
    s.value(&["search"]);
    let mut conn = rusqlite::Connection::open(&s.db).unwrap();
    let tx = conn.transaction().unwrap();
    for i in 0..500 {
        tx.execute(
            "INSERT INTO memories(id,memory_type,category,content,title) VALUES (?1,'fact','archive','filler '||?2,'filler '||?2)",
            rusqlite::params![uuid::Uuid::new_v4().to_string(), i],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    drop(conn);
    s.add("preference", "Newest preference");
    assert_eq!(
        s.value(&["search", "newest"])[0]["content"],
        "Newest preference"
    );
}

#[test]
fn only_inferred_patterns_cite_supporting_memories() {
    let s = Store::new();
    let a = s.add("preference", "Prefer examples");
    let b = s.add("preference", "Prefer demonstrations");
    let inferred = infer(&s, &a, &b);
    let only = "Only inferred patterns cite supporting memories";
    let citing = s.value(&[
        "record",
        "--type",
        "preference",
        "--category",
        "programming",
        "--content",
        "Prefer worked examples in reviews",
        "--evidence-id",
        &a,
        "--evidence-id",
        &b,
    ]);
    assert_eq!(citing["reason"], only);
    let observed = s.value(&[
        "record",
        "--type",
        "behavior",
        "--category",
        "programming",
        "--content",
        "Asked for a demonstration",
        "--source",
        "observed",
        "--evidence",
        "User asked for a demo during review",
        "--evidence-id",
        &b,
    ]);
    assert_eq!(observed["reason"], only);
    let conn = rusqlite::Connection::open(&s.db).unwrap();
    for source in ["explicit", "observed"] {
        let direct = conn.execute(
            "INSERT INTO memories(id,memory_type,category,content,source,evidence_ids,title) VALUES (?1,'fact','x','Direct',?2,?3,'Direct')",
            rusqlite::params![uuid::Uuid::new_v4().to_string(), source, format!("[\"{a}\"]")],
        );
        assert!(direct.is_err(), "store accepted {source} evidence ids");
    }
    drop(conn);
    s.value(&["update", &a, "--content", "Prefer reference material"]);
    assert_eq!(status(&s, &inferred), "stale");
    assert_eq!(status(&s, &b), "confirmed");
}

#[test]
fn changing_or_forgetting_unrelated_memories_keeps_inferences_active() {
    let s = Store::new();
    let a = s.add("preference", "Prefer examples");
    let b = s.add("preference", "Prefer demonstrations");
    let unrelated = s.add("preference", "Prefer tabs");
    let inferred = infer(&s, &a, &b);
    s.value(&["update", &unrelated, "--content", "Prefer spaces"]);
    assert_eq!(status(&s, &inferred), "tentative");
    s.value(&["forget", &unrelated]);
    assert_eq!(status(&s, &inferred), "tentative");
}

#[test]
fn editing_any_field_of_evidence_in_the_store_invalidates_inferences() {
    for (column, value) in [
        ("memory_type", "'goal'"),
        ("category", "'other'"),
        ("context", "'changed'"),
        ("content", "'changed'"),
        ("scope", "'project'"),
        ("project", "'changed'"),
        ("source", "'observed'"),
        ("status", "'tentative'"),
        ("evidence", "'changed'"),
    ] {
        let s = Store::new();
        let a = s.add("preference", "Prefer examples");
        let b = s.add("preference", "Prefer demonstrations");
        let inferred = infer(&s, &a, &b);
        let conn = rusqlite::Connection::open(&s.db).unwrap();
        conn.execute(
            &format!("UPDATE memories SET {column}={value} WHERE id=?1"),
            [&a],
        )
        .unwrap();
        drop(conn);
        assert_eq!(status(&s, &inferred), "stale", "editing {column}");
    }
    // Wording about a memory is not the memory.
    for column in ["reason", "title"] {
        let s = Store::new();
        let a = s.add("preference", "Prefer examples");
        let b = s.add("preference", "Prefer demonstrations");
        let inferred = infer(&s, &a, &b);
        rusqlite::Connection::open(&s.db)
            .unwrap()
            .execute(
                &format!("UPDATE memories SET {column}='changed' WHERE id=?1"),
                [&a],
            )
            .unwrap();
        assert_eq!(status(&s, &inferred), "tentative", "editing {column}");
    }
}

#[test]
fn confirming_an_observation_makes_it_explicit_and_keeps_inferences() {
    let s = Store::new();
    let observed = s.value(&[
        "record",
        "--type",
        "behavior",
        "--category",
        "learning",
        "--content",
        "Asked for a demonstration",
        "--source",
        "observed",
        "--evidence",
        "User asked for a demo during review",
        "--reason",
        "Seemed to learn faster",
    ])["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let b = s.add("preference", "Prefer demonstrations");
    let inferred = infer(&s, &observed, &b);
    assert_eq!(s.value(&["update", &observed])["updated"], true);
    let confirmed = s
        .value(&["search", "demonstration"])
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == observed.as_str())
        .unwrap()
        .clone();
    assert_eq!(confirmed["source"], "explicit");
    assert_eq!(confirmed["status"], "confirmed");
    // The observation stays as the evidence; the agent's guessed reason goes.
    assert_eq!(confirmed["evidence"], "User asked for a demo during review");
    assert_eq!(confirmed.get("reason"), None);
    assert_eq!(confirmed.get("evidence_ids"), None);
    // Confirming strengthens the support, so the inference stands.
    assert_eq!(status(&s, &inferred), "tentative");
}

#[test]
fn corrections_and_invalidation_refresh_modification_time() {
    let s = Store::new();
    let a = s.add("preference", "Prefer examples");
    let b = s.add("preference", "Prefer demonstrations");
    let inferred = infer(&s, &a, &b);
    let conn = rusqlite::Connection::open(&s.db).unwrap();
    conn.execute("UPDATE memories SET updated_at='2000-01-01 00:00:00'", [])
        .unwrap();
    drop(conn);
    s.value(&["update", &a, "--content", "Prefer reference material"]);
    let memories = s.value(&["search", "--status", "all", "--page-size", "20"]);
    for id in [&a, &inferred] {
        let memory = memories
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id.as_str())
            .unwrap();
        assert_ne!(memory["updated_at"], "2000-01-01 00:00:00", "{id}");
    }
    let untouched = memories
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == b.as_str())
        .unwrap();
    assert_eq!(untouched["updated_at"], "2000-01-01 00:00:00");
}

#[test]
fn corrections_and_forgetting_touch_only_the_target_memory() {
    let s = Store::new();
    let a = s.add("preference", "Prefer examples");
    let b = s.add("preference", "Prefer demonstrations");
    s.value(&["update", &a, "--content", "Prefer reference material"]);
    let contents = |s: &Store| -> Vec<String> {
        let mut found: Vec<String> = s
            .value(&["search"])
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["content"].as_str().unwrap().to_owned())
            .collect();
        found.sort();
        found
    };
    assert_eq!(
        contents(&s),
        ["Prefer demonstrations", "Prefer reference material"]
    );
    assert_eq!(s.value(&["forget", &a])["deleted"], true);
    assert_eq!(contents(&s), ["Prefer demonstrations"]);
    assert_eq!(s.value(&["search"])[0]["id"], b.as_str());
}

#[test]
fn store_rejects_malformed_memory_ids() {
    let s = Store::new();
    s.value(&["search"]);
    let conn = rusqlite::Connection::open(&s.db).unwrap();
    let insert = |id: &str| {
        conn.execute(
            "INSERT INTO memories(id,memory_type,category,content,title) VALUES (?1,'fact','test','x','x')",
            [id],
        )
    };
    insert("00000000-0000-4000-8000-000000000000").unwrap();
    for (id, defect) in [
        ("00000000-0000-4000-8000-000000000001-", "extra character"),
        ("000000000-000-4000-8000-000000000002", "misplaced hyphen"),
        ("00000000-0000-1000-8000-000000000003", "not version 4"),
        ("00000000-0000-4000-c000-000000000004", "wrong variant"),
        ("00000000-0000-4000-8000-00000000000A", "uppercase"),
        ("00000000-0000-4000-8000-00000000000g", "non-hex"),
    ] {
        assert!(insert(id).is_err(), "accepted ID with {defect}: {id}");
    }
}

#[test]
fn recording_an_invalidated_inference_again_creates_a_fresh_tentative_memory() {
    let s = Store::new();
    let a = s.add("preference", "Prefer examples");
    let b = s.add("preference", "Prefer demonstrations");
    let stale = infer(&s, &a, &b);
    s.value(&["update", &a, "--content", "Prefer reference material"]);
    let fresh = infer(&s, &a, &b);
    assert_ne!(fresh, stale);
    assert_eq!(s.value(&["search", "learning"])[0]["id"], fresh.as_str());
    assert_eq!(status(&s, &fresh), "tentative");
}

#[test]
fn memories_differing_in_any_recorded_field_are_not_duplicates() {
    let s = Store::new();
    let a = s.add("preference", "Prefer examples");
    let b = s.add("preference", "Prefer demonstrations");
    let c = s.add("preference", "Prefer exercises");
    let id = |extra: &[&str]| -> String {
        let mut args = vec!["record"];
        for (flag, default) in [
            ("--type", "preference"),
            ("--category", "workflow"),
            ("--content", "Same content"),
        ] {
            if !extra.contains(&flag) {
                args.extend([flag, default]);
            }
        }
        args.extend(extra);
        let saved = s.resolved(&args);
        assert_eq!(saved["stored"], true, "{extra:?}: {saved}");
        saved["id"].as_str().unwrap().to_owned()
    };
    let variants: Vec<(&str, Vec<&str>)> = vec![
        ("type", vec!["--type", "goal"]),
        ("category", vec!["--category", "writing"]),
        ("context", vec!["--context", "During reviews"]),
        ("reason", vec!["--reason", "Easier to compare"]),
        ("project a", vec!["--scope", "project", "--project", "a"]),
        ("project b", vec!["--scope", "project", "--project", "b"]),
        ("evidence", vec!["--evidence", "Said so in review"]),
        (
            "observed",
            vec!["--source", "observed", "--evidence", "Said so in review"],
        ),
        (
            "observed evidence",
            vec!["--source", "observed", "--evidence", "Said so in planning"],
        ),
        (
            "inferred from a and b",
            vec![
                "--source",
                "inferred",
                "--evidence-id",
                &a,
                "--evidence-id",
                &b,
            ],
        ),
        (
            "inferred from a and c",
            vec![
                "--source",
                "inferred",
                "--evidence-id",
                &a,
                "--evidence-id",
                &c,
            ],
        ),
        ("origin x", vec!["--origin", "claude:session/x"]),
        ("origin y", vec!["--origin", "claude:session/y"]),
    ];
    let mut seen = vec![("base", id(&[]))];
    for (label, extra) in &variants {
        let saved = id(extra);
        if let Some((other, _)) = seen.iter().find(|(_, existing)| *existing == saved) {
            panic!("{label} variant was merged into {other}");
        }
        seen.push((label, saved));
    }
    for (label, extra) in &variants {
        let again = id(extra);
        let (_, original) = seen.iter().find(|(l, _)| l == label).unwrap();
        assert_eq!(&again, original, "{label} variant was not deduplicated");
    }
}

#[test]
fn search_ranks_category_then_type_then_text_matches_above_recency() {
    let s = Store::new();
    let record = |memory_type: &str, category: &str, content: &str| {
        s.value(&[
            "record",
            "--type",
            memory_type,
            "--category",
            category,
            "--content",
            content,
        ]);
    };
    record("preference", "testing", "Category match");
    record("testing", "workflow", "Type match");
    record("preference", "workflow", "Text match about testing");
    record("preference", "workflow", "No match");
    let order: Vec<Value> = s
        .value(&["search", "testing"])
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].clone())
        .collect();
    assert_eq!(
        order,
        ["Category match", "Type match", "Text match about testing"]
    );
}

#[test]
fn stop_words_in_a_topic_do_not_match_memories() {
    let s = Store::new();
    s.add("preference", "Prefer Rust");
    s.value(&[
        "record",
        "--type",
        "preference",
        "--category",
        "editor",
        "--content",
        "Use the same editor everywhere",
    ]);
    let found = s.value(&["search", "the rust"]);
    assert_eq!(found.as_array().unwrap().len(), 1, "{found}");
    assert_eq!(found[0]["content"], "Prefer Rust");
}

#[test]
fn the_index_lists_rules_then_patterns_goals_and_the_other_types() {
    let s = Store::new();
    // Each later memory is newer, so recency alone would reverse this order.
    let goal = s.add("goal", "Ship the parser");
    let fact = s.add("fact", "Works on a parser");
    let pattern = infer(&s, &goal, &fact);
    s.value(&[
        "record",
        "--type",
        "behavior",
        "--category",
        "learning",
        "--content",
        "Asked for a demonstration",
        "--source",
        "observed",
        "--evidence",
        "User asked for a demo during review",
    ]);
    s.add("preference", "Prefer Rust");
    let out = s.run(&["index"]);
    assert!(out.status.success());
    let index = String::from_utf8_lossy(&out.stdout);
    let position = |needle: &str| {
        index
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} missing from {index}"))
    };
    // Rules, then patterns, goals, and the remaining types largest first,
    // then by name.
    let order = [
        position("\nRules\n  programming (1)\n"),
        position("Prefer Rust"),
        position("\nOther memories\n  pattern (1)\n"),
        position(&format!(
            "{} Prefer practical learning (tentative)",
            &pattern[..8]
        )),
        position("  goal (1)"),
        position("Ship the parser"),
        position("  behavior (1)"),
        position("Asked for a demonstration (tentative)"),
        position("  fact (1)"),
        position("Works on a parser"),
    ];
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{index}");
    let read = s.value(&["read", &pattern[..8]]);
    let mut cited: Vec<&str> = read["memories"][0]["evidence_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    cited.sort();
    let mut expected = vec![goal.as_str(), fact.as_str()];
    expected.sort();
    assert_eq!(cited, expected);
}

#[test]
fn the_index_marks_project_scoped_memories() {
    let s = Store::new();
    s.value(&[
        "record",
        "--type",
        "constraint",
        "--category",
        "programming",
        "--content",
        "Use Python here",
        "--scope",
        "project",
        "--project",
        "project-a",
    ]);
    s.add("preference", "Prefer Rust");
    let out = s.run(&["index", "--project", "project-a"]);
    let profile = String::from_utf8_lossy(&out.stdout);
    let line = |needle: &str| {
        profile
            .lines()
            .find(|l| l.contains(needle))
            .unwrap()
            .to_owned()
    };
    assert!(line("Use Python here").ends_with("Use Python here [project]"));
    assert!(!line("Prefer Rust").contains("[project]"));
}

#[test]
fn reconfirming_an_explicit_memory_keeps_its_stated_reason() {
    let s = Store::new();
    let id = s.value(&[
        "record",
        "--type",
        "preference",
        "--category",
        "programming",
        "--content",
        "Prefer Rust",
        "--reason",
        "Performance and safety",
    ])["id"]
        .as_str()
        .unwrap()
        .to_owned();
    s.value(&["update", &id, "--category", "languages"]);
    let found = s.value(&["search", "languages"]);
    assert_eq!(found[0]["reason"], "Performance and safety");
}

#[cfg(unix)]
#[test]
fn store_creates_a_private_directory_without_changing_existing_ones() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempdir().unwrap();
    let mode = |path: &std::path::Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let run = |db: &std::path::Path| {
        let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
            .arg("search")
            .sandboxed(root.path())
            .env("USER_MEMORY_DB", db)
            .env_remove("USER_MEMORY_URL")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let fresh = root.path().join("new/private");
    run(&fresh.join("memory.sqlite3"));
    assert_eq!(mode(&fresh), 0o700);
    assert_eq!(mode(&fresh.join("memory.sqlite3")), 0o600);
    // Only the store's own folder is private; those above it are created
    // as any other.
    let plain = root.path().join("plain");
    fs::create_dir(&plain).unwrap();
    assert_eq!(mode(&root.path().join("new")), mode(&plain));
    let shared = root.path().join("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
    run(&shared.join("memory.sqlite3"));
    assert_eq!(mode(&shared), 0o755);
    assert_eq!(mode(&shared.join("memory.sqlite3")), 0o600);
}

#[test]
fn blank_type_category_or_content_is_refused() {
    let s = Store::new();
    for (flag, blank) in [("--type", " "), ("--category", " "), ("--content", " ")] {
        let mut args = vec!["record"];
        for (other, value) in [
            ("--type", "preference"),
            ("--category", "workflow"),
            ("--content", "Use examples"),
        ] {
            args.extend([other, if other == flag { blank } else { value }]);
        }
        let refused = s.value(&args);
        assert_eq!(refused["stored"], false, "blank {flag}");
        assert_eq!(
            refused["reason"],
            "type, category, and content are required"
        );
    }
}

#[test]
fn evidence_is_limited_to_twenty_real_memory_ids() {
    let s = Store::new();
    let support: Vec<String> = (0..21)
        .map(|i| s.add("preference", &format!("Support {i}")))
        .collect();
    let infer_from = |ids: &[String]| {
        let mut args = vec![
            "record",
            "--type",
            "pattern",
            "--category",
            "learning",
            "--content",
            "Broad pattern",
            "--source",
            "inferred",
        ];
        for id in ids {
            args.extend(["--evidence-id", id]);
        }
        s.value(&args)
    };
    let limit = "Evidence must contain at most 20 non-nil UUID memory IDs";
    assert_eq!(infer_from(&support)["reason"], limit);
    assert_eq!(infer_from(&support[..20])["stored"], true);
    let nil = uuid::Uuid::nil().to_string();
    assert_eq!(infer_from(&[support[0].clone(), nil])["reason"], limit);
}

#[test]
fn evidence_must_be_active_and_inferences_cannot_cite_inferences() {
    let s = Store::new();
    let a = s.add("preference", "Prefer examples");
    let b = s.add("preference", "Prefer demonstrations");
    let c = s.add("preference", "Prefer exercises");
    let inferred = infer(&s, &a, &b);
    let refused = s.value(&[
        "record",
        "--type",
        "pattern",
        "--category",
        "learning",
        "--content",
        "Builds on another pattern",
        "--source",
        "inferred",
        "--evidence-id",
        &inferred,
        "--evidence-id",
        &c,
    ]);
    assert_eq!(refused["stored"], false);
    let cite = |x: &str, y: &str| {
        s.value(&[
            "record",
            "--type",
            "pattern",
            "--category",
            "learning",
            "--content",
            "Prefer guided practice",
            "--source",
            "inferred",
            "--evidence-id",
            x,
            "--evidence-id",
            y,
        ])
    };
    let inactive = "Evidence must be active explicit or observed memory, not another inference";
    s.value(&["update", &a, "--content", "Prefer reference material"]);
    assert_eq!(status(&s, &inferred), "stale");
    assert_eq!(cite(&inferred, &c)["reason"], inactive);
    let drills = s.add("preference", "Prefer drills");
    let replaced = s.value(&[
        "record",
        "--type",
        "preference",
        "--category",
        "programming",
        "--content",
        "Prefer spaced repetition",
        "--supersedes",
        &drills,
    ]);
    assert_eq!(replaced["stored"], true, "{replaced}");
    assert_eq!(cite(&drills, &c)["reason"], inactive);
    assert_eq!(cite(&b, &c)["stored"], true);
}

#[test]
fn project_inferences_may_use_only_their_own_project_evidence() {
    let s = Store::new();
    let local = |project: &str, content: &str| {
        s.resolved(&[
            "record",
            "--type",
            "preference",
            "--category",
            "programming",
            "--content",
            content,
            "--scope",
            "project",
            "--project",
            project,
        ])["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let a1 = local("a", "Use Python here");
    let a2 = local("a", "Use pytest here");
    let infer_in = |project: &str| {
        s.value(&[
            "record",
            "--type",
            "pattern",
            "--category",
            "programming",
            "--content",
            "Python toolchain",
            "--source",
            "inferred",
            "--scope",
            "project",
            "--project",
            project,
            "--evidence-id",
            &a1,
            "--evidence-id",
            &a2,
        ])
    };
    let crossed = infer_in("b");
    assert_eq!(crossed["stored"], false);
    assert_eq!(
        crossed["reason"],
        "Inferences cannot broaden or cross the scope of their evidence"
    );
    assert_eq!(infer_in("a")["stored"], true);
}

#[test]
fn old_preferences_survive_many_newer_incidental_facts() {
    let s = Store::new();
    s.add("preference", "Prefer Rust");
    let mut conn = rusqlite::Connection::open(&s.db).unwrap();
    let tx = conn.transaction().unwrap();
    for i in 0..500 {
        tx.execute(
            "INSERT INTO memories(id,memory_type,category,content,title) VALUES (?1,'fact','context','incidental fact '||?2,'incidental fact '||?2)",
            rusqlite::params![uuid::Uuid::new_v4().to_string(), i],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    drop(conn);
    let profile = s.run(&["index"]);
    assert!(String::from_utf8_lossy(&profile.stdout).contains("Prefer Rust"));
    assert_eq!(s.value(&["search", "rust"])[0]["content"], "Prefer Rust");
}

#[test]
fn project_identifiers_are_stored_as_canonical_keys() {
    let s = Store::new();
    let saved = s.value(&[
        "record",
        "--type",
        "constraint",
        "--category",
        "programming",
        "--content",
        "Keep the public API stable",
        "--scope",
        "project",
        "--project",
        "git@github.com:Owner/Repo.git",
    ]);
    let found = s.value(&["search", "--project", "github.com/owner/repo"]);
    assert_eq!(found[0]["id"], saved["id"]);
    assert_eq!(found[0]["project"], "github.com/owner/repo");
    let id = saved["id"].as_str().unwrap();
    assert_eq!(
        s.value(&["update", id, "--project", "  Side   Quest "])["updated"],
        true
    );
    assert_eq!(
        s.value(&["search", "--project", "side quest"])[0]["project"],
        "side quest"
    );
}

fn record(s: &Store, extra: &[&str]) -> Value {
    let mut args = vec!["record", "--category", "formatting"];
    args.extend_from_slice(extra);
    s.resolved(&args)
}

fn stored(s: &Store, extra: &[&str]) -> String {
    let saved = record(s, extra);
    assert_eq!(saved["stored"], true, "{saved}");
    saved["id"].as_str().unwrap().to_owned()
}

fn inactive(s: &Store, id: &str) -> Value {
    s.value(&["search", "--status", "all", "--page-size", "20"])
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .unwrap_or_else(|| panic!("{id} was not recalled"))
        .clone()
}

#[test]
fn origins_are_short_cleaned_references_reported_on_every_memory() {
    let s = Store::new();
    let id = stored(
        &s,
        &[
            "--content",
            "Indent with tabs",
            "--origin",
            "  claude:session/a  1 ",
        ],
    );
    assert_eq!(
        inactive(&s, &id)["origins"],
        serde_json::json!(["claude:session/a 1"])
    );
    assert_eq!(inactive(&s, &id)["confirmations"], 1);
    assert_eq!(inactive(&s, &id)["last_confirmed_at"], Value::Null);
    let plain = stored(&s, &["--content", "Wrap at 100 columns"]);
    assert_eq!(inactive(&s, &plain).get("origins"), None);
    assert_eq!(inactive(&s, &plain)["confirmations"], 0);
    // Exact repeats are duplicates only within the same conversation, or when
    // neither side names one.
    assert_eq!(stored(&s, &["--content", "Wrap at 100 columns"]), plain);
    let elsewhere = stored(
        &s,
        &["--content", "Wrap at 100 columns", "--origin", "chat/q"],
    );
    assert_ne!(elsewhere, plain);
    assert_ne!(stored(&s, &["--content", "Indent with tabs"]), id);
    assert_ne!(
        stored(
            &s,
            &[
                "--content",
                "Wrap at 100 columns",
                "--origin",
                "claude:session/a 1"
            ]
        ),
        elsewhere,
        "another memory's origin does not make this one a duplicate"
    );
    let long = "x".repeat(241);
    let refused = record(&s, &["--content", "Too long", "--origin", &long]);
    assert_eq!(refused["stored"], false);
    assert_eq!(
        refused["reason"],
        "Origin must be a short reference of at most 240 characters"
    );
    let fits = "x".repeat(240);
    assert_eq!(
        record(&s, &["--content", "Fits", "--origin", &fits])["stored"],
        true
    );
}

#[test]
fn memories_without_origin_and_from_distinct_origins_are_independent_evidence() {
    let s = Store::new();
    let infer_from = |a: &str, b: &str| {
        record(
            &s,
            &[
                "--type",
                "pattern",
                "--content",
                "Cares about formatting",
                "--source",
                "inferred",
                "--evidence-id",
                a,
                "--evidence-id",
                b,
            ],
        )
    };
    let tabs = stored(&s, &["--content", "Indent with tabs", "--origin", "chat/1"]);
    let wrap = stored(&s, &["--content", "Wrap at 100", "--origin", "chat/1"]);
    let plain = stored(&s, &["--content", "Sort imports"]);
    let other = stored(&s, &["--content", "Trailing commas"]);
    let refused = infer_from(&tabs, &wrap);
    assert_eq!(refused["stored"], false);
    assert_eq!(
        refused["reason"],
        "Inferred patterns require at least two independent observations; memories from one conversation count once"
    );
    assert_eq!(infer_from(&tabs, &plain)["stored"], true);
    assert_eq!(infer_from(&plain, &other)["stored"], true);
}

#[test]
fn superseding_marks_the_old_memory_and_returns_the_replacement() {
    let s = Store::new();
    let tabs = stored(&s, &["--content", "Indent with tabs"]);
    let conn = rusqlite::Connection::open(&s.db).unwrap();
    conn.execute(
        "UPDATE memories SET updated_at='2000-01-01 00:00:00' WHERE id=?1",
        [&tabs],
    )
    .unwrap();
    drop(conn);
    let saved = record(
        &s,
        &["--content", "Indent with spaces", "--supersedes", &tabs],
    );
    assert_eq!(saved["stored"], true);
    assert_eq!(saved["superseded"], serde_json::json!([tabs]));
    let spaces = saved["id"].as_str().unwrap();
    let old = inactive(&s, &tabs);
    assert_eq!(old["status"], "superseded");
    assert_eq!(old["superseded_by"], spaces);
    assert_ne!(old["updated_at"], "2000-01-01 00:00:00");
    assert_eq!(inactive(&s, spaces)["superseded_by"], Value::Null);
    let refused = s.value(&["update", &tabs, "--content", "Indent with tabs again"]);
    assert_eq!(refused["updated"], false);
    assert_eq!(
        refused["reason"],
        "Memory was superseded; correct the memory that replaced it"
    );
    assert_eq!(inactive(&s, &tabs)["status"], "superseded");
    let again = stored(&s, &["--content", "Indent with tabs"]);
    assert_ne!(
        again, tabs,
        "a superseded memory is not reused as a duplicate"
    );
    s.value(&["forget", spaces]);
    assert_eq!(inactive(&s, &tabs)["status"], "superseded");
}

#[test]
fn supersession_refuses_weaker_sources_other_projects_and_itself() {
    let s = Store::new();
    let tabs = stored(&s, &["--content", "Indent with tabs"]);
    let observed = record(
        &s,
        &[
            "--type",
            "behavior",
            "--content",
            "Used spaces in a patch",
            "--source",
            "observed",
            "--evidence",
            "User wrote a patch with spaces",
            "--supersedes",
            &tabs,
        ],
    );
    assert_eq!(
        observed["reason"],
        "Only the user's explicit statement can supersede an explicit memory"
    );
    let habit = stored(
        &s,
        &[
            "--type",
            "behavior",
            "--content",
            "Used tabs in a patch",
            "--source",
            "observed",
            "--evidence",
            "User wrote a patch with tabs",
        ],
    );
    assert_eq!(
        record(&s, &["--content", "Use spaces", "--supersedes", &habit])["stored"],
        true
    );
    let a = stored(
        &s,
        &[
            "--content",
            "Use rustfmt",
            "--scope",
            "project",
            "--project",
            "a",
        ],
    );
    let crossed = record(
        &s,
        &[
            "--content",
            "Use dprint",
            "--scope",
            "project",
            "--project",
            "b",
            "--supersedes",
            &a,
        ],
    );
    assert_eq!(
        crossed["reason"],
        "A memory can only supersede one with the same scope and project"
    );
    let itself = record(
        &s,
        &["--content", "Indent with tabs", "--supersedes", &tabs],
    );
    assert_eq!(itself["reason"], "A memory cannot supersede itself");
    assert_eq!(inactive(&s, &tabs)["status"], "confirmed");
    let wrap = stored(&s, &["--content", "Wrap at 100"]);
    let restated = record(
        &s,
        &["--content", "Indent with tabs", "--supersedes", &wrap],
    );
    assert_eq!(restated["id"], tabs.as_str());
    assert_eq!(restated["duplicate"], true);
    assert_eq!(inactive(&s, &wrap)["superseded_by"], tabs.as_str());
    let missing = record(
        &s,
        &[
            "--content",
            "Anything",
            "--supersedes",
            "3d9f829a-c594-45d2-84b6-1469b20d653b",
        ],
    );
    assert_eq!(missing["reason"], "Superseded memory does not exist");
    let again = record(&s, &["--content", "Wrap at 80", "--supersedes", &wrap]);
    assert_eq!(again["reason"], "Memory was already superseded");
}

#[test]
fn an_inference_cannot_cite_the_memory_it_supersedes() {
    let s = Store::new();
    let a = stored(&s, &["--content", "Indent with tabs"]);
    let b = stored(&s, &["--content", "Wrap at 100"]);
    let c = stored(&s, &["--content", "Sort imports"]);
    let pattern = |content: &str, cite: [&str; 2], replaces: &str| {
        record(
            &s,
            &[
                "--type",
                "pattern",
                "--content",
                content,
                "--source",
                "inferred",
                "--evidence-id",
                cite[0],
                "--evidence-id",
                cite[1],
                "--supersedes",
                replaces,
            ],
        )
    };
    let first = record(
        &s,
        &[
            "--type",
            "pattern",
            "--content",
            "Cares about formatting",
            "--source",
            "inferred",
            "--evidence-id",
            &a,
            "--evidence-id",
            &b,
        ],
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let refused = pattern("Cares about layout", [&first, &c], &first);
    assert_eq!(refused["stored"], false);
    let replaced = pattern("Cares about layout", [&a, &c], &first);
    assert_eq!(replaced["stored"], true, "{replaced}");
    assert_eq!(inactive(&s, &first)["status"], "superseded");
}

#[test]
fn forgetting_needs_exactly_one_target_on_the_command_line() {
    let s = Store::new();
    let id = stored(&s, &["--content", "Indent with tabs", "--origin", "chat/1"]);
    assert!(!s.run(&["forget"]).status.success());
    assert!(
        !s.run(&["forget", &id, "--origin", "chat/1"])
            .status
            .success()
    );
    let out = s.value(&["forget", "--origin", " chat/1 "]);
    assert_eq!(
        out,
        serde_json::json!({"deleted":true,"count":1,"ids":[id],"kept":[],"unlinked":0})
    );
}

#[test]
fn storage_accepts_only_known_statuses_with_consistent_supersession() {
    let s = Store::new();
    let a = s.add("preference", "Prefer examples");
    let b = s.add("preference", "Prefer demonstrations");
    let conn = rusqlite::Connection::open(&s.db).unwrap();
    for (sql, label) in [
        (
            "UPDATE memories SET status='archived' WHERE id=?1",
            "unknown status",
        ),
        (
            "UPDATE memories SET status='superseded' WHERE id=?1",
            "superseded without replacement",
        ),
        (
            "UPDATE memories SET superseded_by=id WHERE id=?1",
            "replacement on an active memory",
        ),
    ] {
        assert!(conn.execute(sql, [&a]).is_err(), "store accepted {label}");
    }
    for status in ["confirmed", "tentative", "stale"] {
        conn.execute("UPDATE memories SET status=?2 WHERE id=?1", [&a, status])
            .unwrap();
    }
    conn.execute(
        "UPDATE memories SET status='superseded',superseded_by=?2 WHERE id=?1",
        [&a, &b],
    )
    .unwrap();
}

#[test]
fn a_superseded_inference_stays_superseded_when_its_evidence_changes() {
    let s = Store::new();
    let a = s.add("preference", "Prefer examples");
    let b = s.add("preference", "Prefer demonstrations");
    let c = s.add("preference", "Prefer exercises");
    let old = infer(&s, &a, &b);
    let replaced = s.value(&[
        "record",
        "--type",
        "pattern",
        "--category",
        "learning",
        "--content",
        "Learns by doing",
        "--source",
        "inferred",
        "--evidence-id",
        &b,
        "--evidence-id",
        &c,
        "--supersedes",
        &old,
    ]);
    assert_eq!(replaced["stored"], true, "{replaced}");
    assert_eq!(
        s.value(&["update", &a, "--content", "Prefer reference material"])["updated"],
        true
    );
    assert_eq!(status(&s, &old), "superseded");
    assert_eq!(s.value(&["forget", &b])["deleted"], true);
    assert_eq!(status(&s, &old), "superseded");
    assert_eq!(status(&s, replaced["id"].as_str().unwrap()), "stale");
}

/// A store written by user-memory before schema versioning: no origin or
/// superseded_by columns and an older trigger, holding one memory.
fn pre_roadmap_store(path: &std::path::Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(r#"CREATE TABLE memories (
    id TEXT PRIMARY KEY NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    memory_type TEXT NOT NULL,
    category TEXT NOT NULL,
    context TEXT NOT NULL DEFAULT '',
    content TEXT NOT NULL,
    reason TEXT NOT NULL DEFAULT '',
    scope TEXT NOT NULL DEFAULT 'global',
    project TEXT NOT NULL DEFAULT '',
    source TEXT NOT NULL DEFAULT 'explicit',
    status TEXT NOT NULL DEFAULT 'confirmed',
    evidence TEXT NOT NULL DEFAULT '',
    evidence_ids TEXT NOT NULL DEFAULT '[]'
);
CREATE TRIGGER memory_support_deleted AFTER DELETE ON memories BEGIN
    UPDATE memories SET status='stale' WHERE source='inferred' AND EXISTS (SELECT 1 FROM json_each(evidence_ids) WHERE value=OLD.id);
END;
INSERT INTO memories (id,memory_type,category,content) VALUES ('0bd1ba49-2b63-4aab-8d57-38d9be10a281','preference','programming','Prefer Rust');"#).unwrap();
}

fn user_version(db: &std::path::Path) -> i64 {
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_new_store_is_stamped_with_the_schema_version() {
    let s = Store::new();
    s.value(&["search"]);
    assert_eq!(user_version(&s.db), 5);
    s.add("preference", "Prefer Rust");
    assert_eq!(user_version(&s.db), 5);
}

#[test]
fn a_store_from_before_versioning_is_refused_and_left_untouched() {
    let s = Store::new();
    pre_roadmap_store(&s.db);
    // A refused store keeps even its mode, which is Unix's to check; on
    // Windows its bytes below are what shows it untouched.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&s.db, fs::Permissions::from_mode(0o644)).unwrap();
    }
    let before = fs::read(&s.db).unwrap();
    for args in [
        &["search"][..],
        &["index"],
        &["record", "--category", "x", "--content", "y"],
    ] {
        let out = s.run(args);
        assert!(!out.status.success(), "{args:?}");
        let error = String::from_utf8_lossy(&out.stderr);
        for part in [
            s.db.to_str().unwrap(),
            "has an unversioned schema but this user-memory needs schema version 5",
            "it holds 1 memories and was left unchanged",
            "Move it aside",
            "existing memories are not copied automatically",
            "run `user-memory export FILE` with the version that created the store",
            "`user-memory import FILE --apply`",
        ] {
            assert!(error.contains(part), "{args:?} missing {part:?}: {error}");
        }
        assert!(!error.contains("no such column"), "{error}");
    }
    assert_eq!(
        fs::read(&s.db).unwrap(),
        before,
        "refused store was modified"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&s.db).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }
    assert_eq!(
        fs::read_dir(s.root.path()).unwrap().count(),
        1,
        "no journal or index files"
    );
}

#[test]
fn stores_from_another_version_or_program_are_refused() {
    let s = Store::new();
    s.add("preference", "Prefer Rust");
    let conn = rusqlite::Connection::open(&s.db).unwrap();
    conn.pragma_update(None, "user_version", 99).unwrap();
    let error = String::from_utf8_lossy(&s.run(&["search"]).stderr).into_owned();
    assert!(
        error.contains("has schema version 99, but this user-memory is older than the store"),
        "{error}"
    );
    // Before the migration baseline: refused, with the way to keep memories.
    conn.pragma_update(None, "user_version", 1).unwrap();
    drop(conn);
    let error = String::from_utf8_lossy(&s.run(&["search"]).stderr).into_owned();
    assert!(
        error.contains(
            "has schema version 1 but this user-memory needs schema version 5; it holds 1 memories"
        ),
        "{error}"
    );
    let other = Store::new();
    let conn = rusqlite::Connection::open(&other.db).unwrap();
    conn.execute_batch("CREATE TABLE notes (body TEXT)")
        .unwrap();
    drop(conn);
    let error = String::from_utf8_lossy(&other.run(&["search"]).stderr).into_owned();
    assert!(
        error.contains("has an unversioned schema but this user-memory needs schema version 5; it has no user-memory table"),
        "{error}"
    );
}

/// A raw connection with foreign keys on, as the binary uses.
fn store_sql(s: &Store) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(&s.db).unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    conn
}

/// Marks `id` as also stated in conversation `origin`.
fn also_stated(s: &Store, id: &str, origin: &str) {
    store_sql(s)
        .execute(
            "INSERT INTO memory_origins (memory_id,origin) VALUES (?1,?2)",
            [id, origin],
        )
        .unwrap();
}

fn origins(s: &Store, id: &str) -> Value {
    inactive(s, id)["origins"].clone()
}

#[test]
fn forgetting_a_conversation_keeps_memories_another_conversation_stated() {
    let s = Store::new();
    let both = stored(&s, &["--content", "Indent with tabs", "--origin", "chat/x"]);
    also_stated(&s, &both, "chat/y");
    let only = stored(&s, &["--content", "Wrap at 100", "--origin", "chat/x"]);
    let old = stored(&s, &["--content", "Sort imports", "--origin", "chat/x"]);
    let replacement = stored(
        &s,
        &["--content", "Sort imports by group", "--supersedes", &old],
    );
    let out = s.value(&["forget", "--origin", "chat/x"]);
    assert_eq!(out["deleted"], true);
    assert_eq!(out["count"], 2);
    let mut deleted = vec![only.clone(), old.clone()];
    deleted.sort();
    assert_eq!(out["ids"], serde_json::json!(deleted));
    assert_eq!(out["kept"], serde_json::json!([both]));
    assert_eq!(origins(&s, &both), serde_json::json!(["chat/y"]));
    assert_eq!(inactive(&s, &both)["confirmations"], 1);
    assert_eq!(inactive(&s, &replacement)["status"], "confirmed");
    let all = s.value(&["search", "--status", "all", "--page-size", "20"]);
    assert!(
        all.as_array()
            .unwrap()
            .iter()
            .all(|m| m["id"] != only.as_str() && m["id"] != old.as_str())
    );
    let rows: i64 = store_sql(&s)
        .query_row(
            "SELECT COUNT(*) FROM memory_origins WHERE origin='chat/x'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0);
}

#[test]
fn losing_a_conversation_stales_inferences_even_when_their_support_survives() {
    let s = Store::new();
    let a = stored(&s, &["--content", "Indent with tabs", "--origin", "chat/x"]);
    let b = stored(&s, &["--content", "Wrap at 100", "--origin", "chat/x"]);
    also_stated(&s, &a, "chat/y");
    also_stated(&s, &b, "chat/y");
    let pattern = infer(&s, &a, &b);
    assert_eq!(status(&s, &pattern), "tentative");
    let c = stored(&s, &["--content", "Sort imports"]);
    let d = stored(&s, &["--content", "Trailing commas"]);
    let unrelated = infer(&s, &c, &d);
    let out = s.value(&["forget", "--origin", "chat/x"]);
    let mut kept = vec![a.clone(), b.clone()];
    kept.sort();
    assert_eq!(out["kept"], serde_json::json!(kept));
    assert_eq!(out["count"], 0);
    assert_eq!(status(&s, &a), "confirmed");
    assert_eq!(status(&s, &pattern), "stale");
    assert_eq!(status(&s, &unrelated), "tentative");
}

#[test]
fn a_superseded_inference_stays_superseded_when_its_support_loses_a_conversation() {
    let s = Store::new();
    let a = stored(&s, &["--content", "Indent with tabs", "--origin", "chat/x"]);
    let b = stored(&s, &["--content", "Wrap at 100", "--origin", "chat/y"]);
    also_stated(&s, &a, "chat/z");
    let old = infer(&s, &a, &b);
    let newer = s.value(&[
        "record",
        "--type",
        "pattern",
        "--category",
        "learning",
        "--content",
        "Cares about layout",
        "--source",
        "inferred",
        "--evidence-id",
        &a,
        "--evidence-id",
        &b,
        "--supersedes",
        &old,
    ]);
    assert_eq!(newer["stored"], true, "{newer}");
    let out = s.run(&["forget", "--origin", "chat/x"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(status(&s, &old), "superseded");
    let out = s.value(&["update", &a, "--content", "Indent with spaces"]);
    assert_eq!(out["updated"], true, "{out}");
    assert_eq!(status(&s, &old), "superseded");
    assert_eq!(status(&s, newer["id"].as_str().unwrap()), "stale");
}

#[test]
fn independence_counts_conversations_across_all_supports() {
    let s = Store::new();
    let a = stored(&s, &["--content", "Indent with tabs", "--origin", "chat/x"]);
    let b = stored(&s, &["--content", "Wrap at 100", "--origin", "chat/x"]);
    let pattern = |a: &str, b: &str| {
        record(
            &s,
            &[
                "--type",
                "pattern",
                "--content",
                "Cares about layout",
                "--source",
                "inferred",
                "--evidence-id",
                a,
                "--evidence-id",
                b,
            ],
        )
    };
    assert_eq!(pattern(&a, &b)["stored"], false);
    also_stated(&s, &a, "chat/y");
    assert_eq!(pattern(&a, &b)["stored"], true);
    let c = stored(&s, &["--content", "Sort imports"]);
    let d = stored(&s, &["--content", "Trailing commas"]);
    assert_eq!(
        pattern(&c, &d)["stored"],
        true,
        "each origin-less support is its own conversation"
    );
}

// A correction keeps the conversations that stated the memory and adds its
// own, cleaned like any origin; other memories of those conversations stay.
#[test]
fn a_correction_adds_its_cleaned_origin_to_the_conversations_that_stated_it() {
    let s = Store::new();
    let id = stored(&s, &["--content", "Indent with tabs", "--origin", "chat/x"]);
    also_stated(&s, &id, "chat/y");
    let other = stored(&s, &["--content", "Wrap at 100", "--origin", "chat/x"]);
    assert_eq!(
        s.value(&["update", &id, "--reason", "Readability"])["updated"],
        true
    );
    assert_eq!(origins(&s, &id), serde_json::json!(["chat/x", "chat/y"]));
    let out = s.value(&[
        "update",
        &id,
        "--content",
        "Indent with spaces",
        "--origin",
        " chat/z ",
    ]);
    assert_eq!(out["updated"], true);
    assert_eq!(
        origins(&s, &id),
        serde_json::json!(["chat/x", "chat/y", "chat/z"])
    );
    assert_eq!(
        s.value(&["update", &id, "--content", "Indent with 2 spaces"])["updated"],
        true
    );
    assert_eq!(
        origins(&s, &id),
        serde_json::json!(["chat/x", "chat/y", "chat/z"])
    );
    assert_eq!(origins(&s, &other), serde_json::json!(["chat/x"]));
    let long = "x".repeat(241);
    let refused = s.value(&[
        "update",
        &id,
        "--content",
        "Indent with 4 spaces",
        "--origin",
        &long,
    ]);
    assert_eq!(refused["updated"], false);
    assert_eq!(
        refused["reason"],
        "Origin must be a short reference of at most 240 characters"
    );
}

#[test]
fn origin_and_distinct_rows_follow_their_memories_and_their_rules() {
    let s = Store::new();
    let a = stored(&s, &["--content", "Indent with tabs", "--origin", "chat/x"]);
    let mut ids = [
        a.clone(),
        stored(&s, &["--content", "Wrap at 100"]),
        stored(&s, &["--content", "Sort imports"]),
    ];
    ids.sort();
    let [low, mid, high] = ids;
    let conn = store_sql(&s);
    for (x, y) in [(&low, &mid), (&mid, &high)] {
        conn.execute("INSERT INTO memory_distinct (a,b) VALUES (?1,?2)", [x, y])
            .unwrap();
    }
    let long = "x".repeat(241);
    for (sql, params, label) in [
        (
            "INSERT INTO memory_distinct (a,b) VALUES (?1,?2)",
            [high.as_str(), low.as_str()],
            "unordered pair",
        ),
        (
            "INSERT INTO memory_distinct (a,b) VALUES (?1,?2)",
            [low.as_str(), low.as_str()],
            "self pair",
        ),
        (
            "INSERT INTO memory_origins (memory_id,origin) VALUES (?1,?2)",
            ["3d9f829a-c594-45d2-84b6-1469b20d653b", "chat/q"],
            "unknown memory",
        ),
        (
            "INSERT INTO memory_origins (memory_id,origin) VALUES (?1,?2)",
            [a.as_str(), ""],
            "empty origin",
        ),
        (
            "INSERT INTO memory_origins (memory_id,origin) VALUES (?1,?2)",
            [a.as_str(), long.as_str()],
            "long origin",
        ),
    ] {
        assert!(conn.execute(sql, params).is_err(), "store accepted {label}");
    }
    drop(conn);
    let count = |table: &str| -> i64 {
        store_sql(&s)
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    };
    // Forgetting a memory removes the pairs naming it in either column, and its origins.
    assert_eq!(s.value(&["forget", &high])["deleted"], true);
    assert_eq!(count("memory_distinct"), 1);
    assert_eq!(s.value(&["forget", &low])["deleted"], true);
    assert_eq!(count("memory_distinct"), 0);
    assert_eq!(s.value(&["forget", &mid])["deleted"], true);
    assert_eq!(count("memory_origins"), 0);
}

// A conversation's links hold one row per project, each a short reference,
// whatever writes them.
#[test]
fn conversation_links_follow_their_rules() {
    let s = Store::new();
    stored(&s, &["--content", "Indent with tabs"]);
    let conn = store_sql(&s);
    let insert = "INSERT INTO conversation_projects (origin,project) VALUES (?1,?2)";
    conn.execute(insert, ["chat/a", "github.com/acme/app"])
        .unwrap();
    let long = "x".repeat(241);
    for (params, label) in [
        (
            ["chat/a", "github.com/acme/app"],
            "a second row for one link",
        ),
        (["", "github.com/acme/app"], "empty origin"),
        ([long.as_str(), "github.com/acme/app"], "long origin"),
        (["chat/a", ""], "empty project"),
        (["chat/a", long.as_str()], "long project"),
    ] {
        assert!(
            conn.execute(insert, params).is_err(),
            "store accepted {label}"
        );
    }
}

/// `args` with `--title` set to the content and `--scope` to global when a
/// record gives none: most tests here are about other fields. A test about
/// the title or scope passes its own.
fn titled<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut args = args.to_vec();
    if args.first() == Some(&"record") && !args.contains(&"--scope") {
        args.extend(["--scope", "global"]);
    }
    if args.first() == Some(&"record") && !args.contains(&"--title") {
        let content = args
            .iter()
            .position(|a| *a == "--content")
            .and_then(|i| args.get(i + 1).copied());
        if let Some(content) = content {
            args.extend(["--title", content]);
        }
    }
    args
}

// Every memory carries a one-line title for the index; a record without one
// is refused with what to write, and one that would not fit a line is refused
// rather than cut.
#[test]
fn records_need_a_one_line_title_that_fits() {
    let s = Store::new();
    let base = ["record", "--category", "workflow", "--content", "Ask first"];
    let missing = s.value(&[&base[..], &["--title", "  "]].concat());
    assert_eq!(missing["stored"], false);
    assert_eq!(
        missing["reason"],
        "title is required: write one line that states the memory, such as the rule itself"
    );
    let long = "s".repeat(121);
    let refused = s.value(&[&base[..], &["--title", &long]].concat());
    assert_eq!(
        refused["reason"],
        "title has 121 characters; the limit is 120; shorten it"
    );
    assert!(s.value(&["search"]).as_array().unwrap().is_empty());
    let fits = "s".repeat(120);
    let saved = s.value(&[&base[..], &["--title", &format!("  {fits}\n")]].concat());
    assert_eq!(saved["stored"], true, "{saved}");
    assert_eq!(inactive(&s, saved["id"].as_str().unwrap())["title"], fits);
}

// A title is only wording: rewording it keeps the memory's
// conversations, source and status, and no pattern built on it goes stale.
#[test]
fn rewording_a_title_changes_nothing_else() {
    let s = Store::new();
    let a = stored(
        &s,
        &[
            "--type",
            "behavior",
            "--source",
            "observed",
            "--evidence",
            "Asked twice",
            "--content",
            "Prefers short replies",
            "--origin",
            "chat/x",
        ],
    );
    let b = stored(
        &s,
        &[
            "--type",
            "behavior",
            "--source",
            "observed",
            "--evidence",
            "Said so",
            "--content",
            "Skips long preambles",
            "--origin",
            "chat/y",
        ],
    );
    let pattern = infer(&s, &a, &b);
    // Back-date the memory, so the update's own date shows.
    rusqlite::Connection::open(&s.db)
        .unwrap()
        .execute(
            "UPDATE memories SET updated_at='2026-01-01 00:00:00' WHERE id=?1",
            [&a],
        )
        .unwrap();
    let before = inactive(&s, &a);
    let out = s.value(&["update", &a, "--title", "Keep replies short"]);
    assert_eq!(out["updated"], true, "{out}");
    let after = inactive(&s, &a);
    assert_eq!(after["title"], "Keep replies short");
    assert_eq!(inactive(&s, &b)["title"], "Skips long preambles");
    for field in ["source", "status", "origins", "content", "created_at"] {
        assert_eq!(after[field], before[field], "{field}");
    }
    // Like any change, a new title dates the memory.
    assert_ne!(after["updated_at"], before["updated_at"]);
    assert_eq!(inactive(&s, &pattern)["status"], "tentative");
    let refused = s.value(&["update", &a, "--title", ""]);
    assert_eq!(refused["updated"], false);
    assert_eq!(inactive(&s, &a)["title"], "Keep replies short");
    // With any other field, the title rides along with a full update.
    let out = s.value(&[
        "update",
        &a,
        "--title",
        "Short replies",
        "--category",
        "communication",
    ]);
    assert_eq!(out["updated"], true, "{out}");
    let after = inactive(&s, &a);
    assert_eq!(
        (&after["title"], &after["category"]),
        (&json!("Short replies"), &json!("communication"))
    );
    assert_eq!(inactive(&s, &pattern)["status"], "stale");
}

// The evidence of an explicit memory holds the user's own words, so a
// correction keeps it until the update gives new evidence.
#[test]
fn an_update_keeps_the_evidence_until_it_gives_new_evidence() {
    let s = Store::new();
    let said = "User: \"keep your replies short\"";
    let a = stored(
        &s,
        &[
            "--type",
            "preference",
            "--content",
            "Keep replys short",
            "--evidence",
            said,
        ],
    );
    let evidence = |s: &Store| inactive(s, &a)["evidence"].clone();
    let out = s.value(&["update", &a, "--content", "Keep replies short"]);
    assert_eq!(out["updated"], true, "{out}");
    assert_eq!(evidence(&s), said, "a content fix");
    s.value(&["update", &a, "--title", "Short replies"]);
    assert_eq!(evidence(&s), said, "a title");
    // New evidence replaces it, even text that starts with a hyphen, and is
    // more than wording when it comes with a title.
    let out = s.value(&[
        "update",
        &a,
        "--title",
        "Keep replies short",
        "--evidence",
        "- said again: short replies",
    ]);
    assert_eq!(out["updated"], true, "{out}");
    assert_eq!(evidence(&s), "- said again: short replies");
    let refused = s.value(&["update", &a, "--evidence", &"w".repeat(2001)]);
    assert_eq!(
        refused["reason"],
        "evidence has 2001 characters; the limit is 2000; shorten it or split the memory"
    );
    assert_eq!(evidence(&s), "- said again: short replies");
    // Marking memories distinct changes neither of them.
    let b = s.add("preference", "Prefer tabs");
    let refused = s.value(&["update", &a, "--distinct-from", &b, "--evidence", "x"]);
    assert_eq!(
        refused["reason"],
        "Mark memories distinct separately from changing one"
    );
    assert_eq!(evidence(&s), "- said again: short replies");
    // Empty evidence clears it, and read leaves an empty field out.
    s.value(&["update", &a, "--evidence", ""]);
    assert_eq!(evidence(&s), Value::Null);
}

// The reason of an explicit memory is the user's stated why, so a correction
// keeps it, as it keeps the evidence, until the update gives a new one.
#[test]
fn an_update_keeps_the_stated_reason_until_it_gives_a_new_one() {
    let s = Store::new();
    let a = stored(
        &s,
        &[
            "--type",
            "preference",
            "--content",
            "Ask before evry push",
            "--reason",
            "A push deploys production",
        ],
    );
    let reason = |s: &Store| inactive(s, &a)["reason"].clone();
    let out = s.value(&["update", &a, "--content", "Ask before every push"]);
    assert_eq!(out["updated"], true, "{out}");
    assert_eq!(reason(&s), "A push deploys production", "a content fix");
    s.value(&["update", &a, "--reason", "- pushes deploy production"]);
    assert_eq!(reason(&s), "- pushes deploy production");
    // Empty reason clears it, and read leaves an empty field out.
    s.value(&["update", &a, "--reason", ""]);
    assert_eq!(reason(&s), Value::Null);
}

// A confirmed inference is the user's own statement now: it no longer rests
// on its supports, and only inferences may cite them.
#[test]
fn confirming_an_inference_drops_its_supports_and_keeps_its_evidence() {
    let s = Store::new();
    let a = s.add("preference", "Prefer examples");
    let b = s.add("preference", "Prefer demonstrations");
    let p = stored(
        &s,
        &[
            "--type",
            "pattern",
            "--content",
            "Prefer practical learning",
            "--source",
            "inferred",
            "--evidence",
            "Asked for examples twice",
            "--evidence-id",
            &a,
            "--evidence-id",
            &b,
        ],
    );
    let out = s.value(&["update", &p]);
    assert_eq!(out["updated"], true, "{out}");
    let confirmed = inactive(&s, &p);
    assert_eq!(confirmed["source"], "explicit");
    assert_eq!(confirmed.get("evidence_ids"), None);
    assert_eq!(confirmed["evidence"], "Asked for examples twice");
}

// Labels and project keys over their limits are refused like text, never
// cut, and a title is told only to be shorter.
#[test]
fn labels_titles_and_project_keys_over_their_limits_are_refused() {
    let s = Store::new();
    let record = |field: &str, value: &str, content: &str| {
        let mut args = vec![
            "record",
            "--type",
            "preference",
            "--category",
            "workflow",
            "--title",
            "Ask first",
            "--content",
            content,
        ];
        let at = args.iter().position(|a| *a == field).unwrap_or(args.len());
        if at < args.len() {
            args.drain(at..at + 2);
        }
        args.extend([field, value]);
        if field == "--project" {
            args.extend(["--scope", "project"]);
        }
        s.value(&args)
    };
    // A project key usually comes from a git remote, which the user cannot
    // shorten; the override file can name the project instead.
    let rename =
        "use a shorter project key; a key that long from a git remote needs a shorter remote URL";
    for (field, limit, advice) in [
        ("--type", 40, "shorten it"),
        ("--category", 80, "shorten it"),
        ("--project", 240, rename),
        ("--title", 120, "shorten it"),
    ] {
        let over = "w".repeat(limit + 1);
        let refused = record(field, &over, "Ask first");
        assert_eq!(refused["stored"], false, "{field}: {refused}");
        assert_eq!(
            refused["reason"],
            format!(
                "{} has {} characters; the limit is {limit}; {advice}",
                &field[2..],
                limit + 1
            ),
            "{field}"
        );
    }
    assert!(
        s.value(&["search", "--scope", "all", "--status", "all"])
            .as_array()
            .unwrap()
            .is_empty()
    );
    for (field, limit, content) in [
        ("--type", 40, "Indent with tabs"),
        ("--category", 80, "Deploy on Fridays"),
        ("--project", 240, "Write release notes"),
    ] {
        let exact = "x".repeat(limit);
        let stored = record(field, &exact, content);
        assert_eq!(stored["stored"], true, "{field}: {stored}");
    }
}

// Text over the limit is refused with the field, its length and the limit,
// never cut: a memory that lost the end of its last sentence says something
// the user never said.
#[test]
fn over_long_text_is_refused_by_field_and_nothing_is_stored() {
    let s = Store::new();
    let refusal = |field: &str, limit: usize| {
        format!(
            "{field} has {} characters; the limit is {limit}; shorten it or split the memory",
            limit + 1
        )
    };
    for (field, limit) in [("--content", 4000), ("--context", 2000), ("--reason", 2000)] {
        let over = "w".repeat(limit + 1);
        let mut args = vec![
            "record",
            "--category",
            "workflow",
            "--title",
            "Ask first",
            "--content",
            "Ask first",
        ];
        if field == "--content" {
            args.truncate(5);
        }
        args.extend([field, over.as_str()]);
        let refused = s.value(&args);
        assert_eq!(refused["stored"], false, "{field}");
        assert_eq!(refused["reason"], refusal(&field[2..], limit), "{field}");
    }
    let observed = s.value(&[
        "record",
        "--type",
        "behavior",
        "--category",
        "workflow",
        "--source",
        "observed",
        "--title",
        "Asks first",
        "--content",
        "Asks first",
        "--evidence",
        &"w".repeat(2001),
    ]);
    assert_eq!(observed["reason"], refusal("evidence", 2000));
    assert!(
        s.value(&["search", "--status", "all"])
            .as_array()
            .unwrap()
            .is_empty()
    );
    // Each limit itself fits, and whitespace collapses before counting:
    // 2,500 characters as typed, 999 once runs of spaces collapse.
    let content = "w".repeat(4000);
    let note = "w".repeat(2000);
    let padded = "w    ".repeat(500);
    let fits = s.value(&[
        "record",
        "--category",
        "workflow",
        "--title",
        "Long",
        "--content",
        &content,
        "--context",
        &note,
        "--reason",
        &padded,
    ]);
    assert_eq!(fits["stored"], true, "{fits}");
    let id = fits["id"].as_str().unwrap();
    assert_eq!(inactive(&s, id)["content"], content);
    assert_eq!(inactive(&s, id)["context"], note);
    let observed = s.value(&[
        "record",
        "--type",
        "behavior",
        "--category",
        "habits",
        "--source",
        "observed",
        "--title",
        "Reads tests first",
        "--content",
        "Reads tests before the implementation",
        "--evidence",
        &note,
    ]);
    assert_eq!(observed["stored"], true, "{observed}");
    let update = s.value(&["update", id, "--content", &"w".repeat(4001)]);
    assert_eq!(update["updated"], false);
    assert_eq!(update["reason"], refusal("content", 4000));
    assert_eq!(inactive(&s, id)["content"], content);
}

// The command set is exactly this: commands were renamed with the tools,
// without aliases, so no earlier name still runs.
#[test]
fn the_cli_offers_exactly_these_commands() {
    let s = Store::new();
    let help = String::from_utf8(s.run(&["--help"]).stdout).unwrap();
    let commands: Vec<&str> = help
        .lines()
        .skip_while(|line| *line != "Commands:")
        .skip(1)
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert_eq!(
        commands,
        [
            "setup",
            "doctor",
            "uninstall",
            "endpoint",
            "serve",
            "host",
            "connect",
            "reindex",
            "migrate",
            "repair",
            "export",
            "import",
            "index",
            "record",
            "read",
            "search",
            "review",
            "update",
            "forget",
            "session-start",
            "prompt-submit",
            "project-id",
            "project",
            "embedding",
            "backup",
            "secure-file",
            "help"
        ]
    );
}

// read returns whole memories by full id or by the 8 characters the index
// shows; ids that match nothing, or only an inactive memory, are reported
// rather than skipped, and an ambiguous prefix is an error naming the matches.
#[test]
fn reading_by_id_resolves_prefixes_and_reports_every_miss() {
    let s = Store::new();
    let rust = stored(
        &s,
        &[
            "--type",
            "preference",
            "--content",
            "Prefer Rust",
            "--reason",
            "Speed",
        ],
    );
    let tabs = stored(&s, &["--content", "Indent with tabs"]);
    let spaces = stored(
        &s,
        &["--content", "Indent with spaces", "--supersedes", &tabs],
    );
    let read = s.value(&["read", &rust, &rust[..8].to_uppercase(), &spaces[..8]]);
    let ids: Vec<&str> = read["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [rust.as_str(), rust.as_str(), spaces.as_str()]);
    assert_eq!(read["memories"][0]["reason"], "Speed");
    assert_eq!(read["memories"][0]["title"], "Prefer Rust");
    assert!(
        read.get("unknown").is_none() && read.get("excluded").is_none(),
        "{read}"
    );

    let missing = "0000000000004000a000000000000000";
    let partial = s.value(&["read", &tabs, missing, "deadbeef"]);
    assert_eq!(partial["memories"], json!([]));
    assert_eq!(
        partial["excluded"],
        json!([{"id": tabs, "status": "superseded"}])
    );
    assert_eq!(partial["unknown"], json!([missing, "deadbeef"]));
    let history = s.value(&["read", &tabs, "--status", "all"]);
    assert_eq!(history["memories"][0]["status"], "superseded");

    // Two ids sharing the first 8 characters, each differing from `rust` in
    // the 10th, so 10 characters always single it out.
    let [twin_digit, other_digit] = ["1", "2"].map(|d| if &rust[9..10] == d { "e" } else { d });
    let twin = format!("{}-{twin_digit}111-4111-8111-111111111111", &rust[..8]);
    let other = format!("{}-{other_digit}222-4222-8222-222222222222", &rust[..8]);
    let conn = rusqlite::Connection::open(&s.db).unwrap();
    for id in [&twin, &other] {
        conn.execute(
            "INSERT INTO memories (id,memory_type,category,title,content) VALUES (?1,'fact','x','Twin','Twin')",
            [id],
        )
        .unwrap();
    }
    let ambiguous = s.run(&["read", &rust[..8]]);
    assert!(!ambiguous.status.success());
    let error = String::from_utf8_lossy(&ambiguous.stderr);
    assert!(error.contains("matches several memories"), "{error}");
    for id in [&rust, &twin, &other] {
        assert!(error.contains(id.as_str()), "{error}");
    }
    // One more character after the hyphen tells them apart.
    assert_eq!(
        s.value(&["read", &rust[..10]])["memories"][0]["id"],
        rust.as_str()
    );

    // Ids are not paged, and one read takes up to 20 of them.
    for paging in [["--page-size", "5"], ["--cursor", "p1.1.00"]] {
        let refused = s.run(&["read", &rust, paging[0], paging[1]]);
        let error = String::from_utf8_lossy(&refused.stderr);
        assert!(error.contains("ids need no paging"), "{error}");
    }
    let twenty = vec![rust.as_str(); 20];
    let read = s.value(&[&["read"][..], &twenty].concat());
    assert_eq!(read["memories"].as_array().unwrap().len(), 20);
    let refused = s.run(&[&["read", &rust][..], &twenty].concat());
    let error = String::from_utf8_lossy(&refused.stderr);
    assert!(error.contains("Read at most 20 ids"), "{error}");
}

#[test]
fn reading_a_category_lists_it_whole_and_rejects_bad_requests() {
    let s = Store::new();
    let global = s.add("preference", "Prefer Rust");
    let local = s.value(&[
        "record",
        "--category",
        "coding",
        "--content",
        "Use Python here",
        "--scope",
        "project",
        "--project",
        "github.com/acme/notebooks",
    ])["id"]
        .as_str()
        .unwrap()
        .to_owned();
    s.value(&["record", "--category", "tools", "--content", "Use Helix"]);
    let ids = |value: &Value| -> Vec<String> {
        value["memories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap().to_owned())
            .collect()
    };
    // coding and programming are synonyms; project memories come first.
    let read = s.value(&[
        "read",
        "--category",
        "coding",
        "--project",
        "git@github.com:acme/notebooks.git",
    ]);
    assert_eq!(ids(&read), [local.clone(), global.clone()]);
    assert_eq!(
        ids(&s.value(&["read", "--category", "Programming"])),
        std::slice::from_ref(&global)
    );

    for (args, message) in [
        (vec!["read"], "Give either ids or a category"),
        (
            vec!["read", global.as_str(), "--category", "coding"],
            "Give either ids or a category",
        ),
        (vec!["read", "abc"], "is not a memory id"),
        (vec!["read", "zzzzzzzzzz"], "is not a memory id"),
    ] {
        let out = s.run(&args);
        assert!(!out.status.success(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(message),
            "{args:?}"
        );
    }
    let many: Vec<String> = (0..21).map(|_| global.clone()).collect();
    let mut args = vec!["read"];
    args.extend(many.iter().map(String::as_str));
    let out = s.run(&args);
    assert!(String::from_utf8_lossy(&out.stderr).contains("Read at most 20 ids"));
}

/// The memories of a search result's page. Unless a test asks for a smaller
/// page, its results fit one, so a next_cursor would mean memories went
/// missing.
fn one_page(value: Value, page_size_given: bool) -> Value {
    assert!(
        page_size_given || value.get("next_cursor").is_none(),
        "{value}"
    );
    value["memories"].clone()
}

/// One page of ids and the cursor of the next, when more remain.
type Page = (Vec<String>, Option<String>);

/// Writes `count` workflow preferences straight into the store.
fn many_rules(s: &Store, count: usize) -> Vec<String> {
    s.value(&["search"]);
    let conn = rusqlite::Connection::open(&s.db).unwrap();
    (0..count)
        .map(|i| {
            let id = format!("{:08x}-0000-4000-8000-{:012x}", i + 1, i + 1);
            conn.execute(
                "INSERT INTO memories (id,memory_type,category,title,content) VALUES (?1,'preference','workflow',?2,?2)",
                rusqlite::params![id, format!("Workflow rule {i:03}")],
            )
            .unwrap();
            id
        })
        .collect()
}

// Results that grow with the store come in pages of 100 by default: every
// memory appears on exactly one page, the last page has no cursor, and a
// cursor that no longer fits the store is refused rather than skipping.
#[test]
fn index_category_reads_and_search_page_through_a_large_store() {
    let s = Store::new();
    let mut all = many_rules(&s, 250);
    all.sort();
    let pages = |ids: &dyn Fn(&str) -> Page| {
        let mut seen = Vec::new();
        let mut sizes = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let (found, next) = ids(cursor.as_deref().unwrap_or(""));
            sizes.push(found.len());
            seen.extend(found);
            match next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        seen.sort();
        (sizes, seen)
    };
    let json_page = |args: &[&str], cursor: &str| -> Page {
        let mut full = args.to_vec();
        if !cursor.is_empty() {
            full.extend(["--cursor", cursor]);
        }
        let out = s.run(&full);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        let ids = value["memories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap().to_owned())
            .collect();
        (ids, value["next_cursor"].as_str().map(str::to_owned))
    };
    // A search, which ranks, pages by 20; a category read, which lists, by 100.
    let mut by_twenty = vec![20; 12];
    by_twenty.push(10);
    for (args, expected) in [
        (&["search"][..], by_twenty),
        (&["read", "--category", "workflow"], vec![100, 100, 50]),
    ] {
        let (sizes, seen) = pages(&|cursor| json_page(args, cursor));
        assert_eq!(sizes, expected, "{args:?}");
        assert_eq!(seen, all, "{args:?}");
    }
    let index_page = |cursor: &str| -> Page {
        let mut args = vec!["index"];
        if !cursor.is_empty() {
            args.extend(["--cursor", cursor]);
        }
        let out = s.run(&args);
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.contains("\n  workflow (250)\n"), "{text}");
        let short: Vec<String> = text
            .lines()
            .filter(|l| l.starts_with("    "))
            .map(|l| l.split_whitespace().next().unwrap().to_owned())
            .collect();
        let next = text
            .lines()
            .find_map(|l| l.strip_prefix("next_cursor: "))
            .map(|rest| rest.split_whitespace().next().unwrap().to_owned());
        (short, next)
    };
    let (sizes, seen) = pages(&index_page);
    assert_eq!(sizes, [100, 100, 50]);
    let full: Vec<String> = all.iter().map(|id| id[..8].to_owned()).collect();
    assert_eq!(seen, full);

    // A cursor from before the store changed, or a mistyped one, is refused.
    let (_, Some(next)) = json_page(&["search"], "") else {
        panic!("a second page")
    };
    rusqlite::Connection::open(&s.db)
        .unwrap()
        .execute(
            "INSERT INTO memories (id,memory_type,category,title,content) VALUES ('ffffffff-0000-4000-8000-ffffffffffff','fact','x','New','New')",
            [],
        )
        .unwrap();
    for (args, cursor) in [(&["search"][..], next.as_str()), (&["index"], "nonsense")] {
        let mut full = args.to_vec();
        full.extend(["--cursor", cursor]);
        let out = s.run(&full);
        assert!(!out.status.success(), "{full:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("start again without a cursor"),
            "{full:?}"
        );
    }
    let out = s.run(&["search", "--page-size", "201"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("page_size must be between 1 and 200"));
}

/// Titles of the memories a search, read or index returns, sorted.
fn titles(s: &Store, args: &[&str]) -> Vec<String> {
    let out = s.run(args);
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut found: Vec<String> = if args[0] == "index" {
        let index = String::from_utf8(out.stdout).unwrap();
        index
            .lines()
            .filter(|l| l.starts_with("    "))
            .map(|l| {
                // The title without the line's markers.
                let mut title = l.trim().split_once(' ').unwrap().1;
                for marker in [" ×2", " ×3", " (tentative)", " (stale)", " [project]"] {
                    title = title.strip_suffix(marker).unwrap_or(title);
                }
                title.to_owned()
            })
            .collect()
    } else {
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        value["memories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["title"].as_str().unwrap().to_owned())
            .collect()
    };
    found.sort();
    found
}

const NOTEBOOKS: &str = "github.com/acme/notebooks";
const COMPILER: &str = "github.com/acme/compiler";

/// Global and project memories of every source, plus a superseded one.
fn mixed_scopes() -> Store {
    let s = Store::new();
    let record = |args: &[&str]| {
        let mut full = vec!["record"];
        full.extend_from_slice(args);
        let out = s.value(&full);
        assert_eq!(out["stored"], true, "{out}");
        out["id"].as_str().unwrap().to_owned()
    };
    record(&[
        "--type",
        "preference",
        "--category",
        "programming",
        "--content",
        "Prefer Rust",
        "--origin",
        "chat/a",
    ]);
    let tabs = record(&[
        "--type",
        "preference",
        "--category",
        "formatting",
        "--content",
        "Indent with tabs",
        "--origin",
        "chat/a",
    ]);
    record(&[
        "--type",
        "preference",
        "--category",
        "formatting",
        "--content",
        "Indent with spaces",
        "--supersedes",
        &tabs,
        "--origin",
        "chat/b",
    ]);
    let skims = record(&[
        "--type",
        "behavior",
        "--category",
        "communication",
        "--source",
        "observed",
        "--evidence",
        "Asked for a title",
        "--content",
        "Skims long answers",
        "--origin",
        "chat/b",
    ]);
    let brief = record(&[
        "--type",
        "behavior",
        "--category",
        "communication",
        "--source",
        "observed",
        "--evidence",
        "Said so",
        "--content",
        "Wants brief replies",
        "--origin",
        "chat/c",
    ]);
    record(&[
        "--type",
        "pattern",
        "--category",
        "communication",
        "--source",
        "inferred",
        "--content",
        "Values concise work",
        "--evidence-id",
        &skims,
        "--evidence-id",
        &brief,
    ]);
    record(&[
        "--type",
        "constraint",
        "--category",
        "coding",
        "--content",
        "Use Python here",
        "--scope",
        "project",
        "--project",
        NOTEBOOKS,
        "--origin",
        "chat/a",
    ]);
    record(&[
        "--type",
        "preference",
        "--category",
        "programming",
        "--content",
        "Use Zig here",
        "--scope",
        "project",
        "--project",
        COMPILER,
        "--origin",
        "chat/c",
    ]);
    s
}

// By default a request covers the active global memories plus the listed
// projects'; another project's memories appear only when asked for.
#[test]
fn scope_defaults_to_global_plus_the_listed_projects() {
    let s = mixed_scopes();
    let global = [
        "Prefer Rust",
        "Indent with spaces",
        "Skims long answers",
        "Values concise work",
        "Wants brief replies",
    ];
    let mut sorted_global = global.to_vec();
    sorted_global.sort();
    for verb in [
        &["index"][..],
        &["search"],
        &["read", "--category", "programming"],
    ] {
        let all = titles(&s, verb);
        assert!(!all.iter().any(|m| m.contains("here")), "{verb:?}: {all:?}");
    }
    assert_eq!(titles(&s, &["search"]), sorted_global);
    let notebooks = titles(&s, &["search", "--project", NOTEBOOKS]);
    assert!(notebooks.contains(&"Use Python here".to_owned()));
    assert!(!notebooks.contains(&"Use Zig here".to_owned()));
    let both = titles(
        &s,
        &[
            "index",
            "--project",
            "git@github.com:acme/notebooks.git",
            "--project",
            COMPILER,
        ],
    );
    assert!(
        both.contains(&"Use Python here".to_owned()) && both.contains(&"Use Zig here".to_owned()),
        "{both:?}"
    );
    // scope narrows or widens: global only, the listed projects only, or every project.
    assert_eq!(
        titles(&s, &["search", "--project", NOTEBOOKS, "--scope", "global"]),
        sorted_global
    );
    assert_eq!(
        titles(
            &s,
            &["search", "--project", NOTEBOOKS, "--scope", "project"]
        ),
        ["Use Python here"]
    );
    let every = titles(&s, &["index", "--scope", "all"]);
    assert_eq!(every.len(), global.len() + 2, "{every:?}");
    let out = s.run(&["search", "--scope", "project"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("scope project needs projects"));
}

// Filters narrow index, read and search alike, alone or together.
#[test]
fn filters_combine_on_index_read_and_search() {
    let s = mixed_scopes();
    let cases: [(&[&str], &[&str]); 8] = [
        (
            &["--type", "behavior"],
            &["Skims long answers", "Wants brief replies"],
        ),
        (&["--source", "inferred"], &["Values concise work"]),
        (
            &["--source", "observed", "--origin", "chat/c"],
            &["Wants brief replies"],
        ),
        (
            &["--origin", "chat/a", "--scope", "all"],
            &["Prefer Rust", "Use Python here"],
        ),
        (&["--status", "superseded"], &["Indent with tabs"]),
        (
            &["--status", "all", "--category", "formatting"],
            &["Indent with spaces", "Indent with tabs"],
        ),
        // coding and programming are synonyms.
        (
            &[
                "--category",
                "coding",
                "--scope",
                "all",
                "--type",
                "preference",
            ],
            &["Prefer Rust", "Use Zig here"],
        ),
        (
            &["--category", "communication", "--source", "explicit"],
            &[],
        ),
    ];
    for (filters, expected) in cases {
        for verb in ["index", "search"] {
            let mut args = vec![verb];
            args.extend_from_slice(filters);
            assert_eq!(titles(&s, &args), expected, "{args:?}");
        }
        if filters.contains(&"--category") {
            let mut args = vec!["read"];
            args.extend_from_slice(filters);
            assert_eq!(titles(&s, &args), expected, "{args:?}");
        }
    }
    // Pages walk the filtered result, not the store.
    let page = |extra: &[&str]| -> Value {
        let mut args = vec!["search", "--source", "observed", "--page-size", "1"];
        args.extend_from_slice(extra);
        serde_json::from_slice(&s.run(&args).stdout).unwrap()
    };
    let first = page(&[]);
    assert_eq!(first["memories"].as_array().unwrap().len(), 1);
    let next = first["next_cursor"].as_str().unwrap().to_owned();
    let second = page(&["--cursor", &next]);
    assert_eq!(second["memories"].as_array().unwrap().len(), 1);
    assert!(second.get("next_cursor").is_none(), "{second}");
    assert_ne!(first["memories"][0]["id"], second["memories"][0]["id"]);
    // Ids are read as they are: only status applies to them. The projects
    // and scope an agent carries over from index are ignored, since an id
    // names one memory in any scope; the other filters are refused.
    let id = first["memories"][0]["id"].as_str().unwrap();
    for filter in [
        ["--project", "github.com/acme/elsewhere"],
        ["--scope", "global"],
    ] {
        let out = s.run(&["read", id, filter[0], filter[1]]);
        assert!(
            out.status.success(),
            "{filter:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let read: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(read["memories"][0]["id"], id, "{filter:?}: {read}");
    }
    for filter in [
        ["--type", "preference"],
        ["--source", "observed"],
        ["--origin", "chat/a"],
    ] {
        let out = s.run(&["read", id, filter[0], filter[1]]);
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(error.contains("only status applies"), "{filter:?}: {error}");
    }
    let out = s.run(&["read", id, "--category", "coding"]);
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(
        error.contains(
            "Give either ids or a category; to list memories by project or scope, use index with those filters"
        ),
        "{error}"
    );
}

// The CLI resolves a checkout on this machine to its project key; the tools
// take keys or remote URLs only, since the server may be elsewhere.
#[test]
fn the_cli_resolves_a_checkout_path_to_its_project() {
    let s = mixed_scopes();
    let repo = s.root.path().join("notebooks");
    std::fs::create_dir(&repo).unwrap();
    for args in [
        &["init", "-q"][..],
        &[
            "remote",
            "add",
            "origin",
            "git@github.com:acme/notebooks.git",
        ],
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
    }
    let path = repo.to_str().unwrap();
    for args in [
        &["search", "--project", path][..],
        &["index", "--project", path],
        &["read", "--category", "coding", "--project", path],
    ] {
        let found = titles(&s, args);
        assert!(
            found.contains(&"Use Python here".to_owned()),
            "{args:?}: {found:?}"
        );
    } // Review resolves a path too; the tool would refuse one.
    assert!(s.value(&["review", "--project", path])["groups"].is_array());
    // Record and update store the checkout's key, never the path.
    let id = stored(
        &s,
        &[
            "--content",
            "Run the notebook tests",
            "--scope",
            "project",
            "--project",
            path,
        ],
    );
    let project = |s: &Store| {
        s.value(&["search", "--scope", "all", "--status", "all"])
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id.as_str())
            .unwrap()["project"]
            .clone()
    };
    assert_eq!(project(&s), "github.com/acme/notebooks");
    s.value(&["update", &id, "--project", "github.com/acme/other"]);
    s.value(&["update", &id, "--project", path]);
    assert_eq!(project(&s), "github.com/acme/notebooks");
}

// A store that cannot be created, as in a directory the user cannot write,
// says which file and why instead of a bare database error.
#[cfg(unix)]
#[test]
fn a_store_that_cannot_be_created_says_so() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    let db = locked.join("memories.sqlite3");
    let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .arg("index")
        .sandboxed(dir.path())
        .env("USER_MEMORY_DB", &db)
        .env_remove("USER_MEMORY_URL")
        .output()
        .unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!out.status.success());
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(
        error.contains(&format!("Cannot create {}", db.display())),
        "{error}"
    );
}

// Scope has no default on the command line either. A global memory typed
// there is the user's own, so it needs no confirmation whatever its source.
#[test]
fn the_cli_requires_scope_and_takes_a_typed_global_memory_as_the_users() {
    let s = Store::new();
    let unscoped = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .args(["record", "--category", "workflow"])
        .args(["--title", "Prefer Rust", "--content", "Prefer Rust"])
        .sandboxed(s.root.path())
        .env("USER_MEMORY_DB", &s.db)
        .env_remove("USER_MEMORY_URL")
        .output()
        .unwrap();
    assert!(!unscoped.status.success());
    assert!(
        String::from_utf8_lossy(&unscoped.stderr).contains("--scope <SCOPE>"),
        "{unscoped:?}"
    );
    let first = stored(
        &s,
        &[
            "--content",
            "Asks before force-pushing",
            "--origin",
            "chat/1",
        ],
    );
    let second = stored(
        &s,
        &[
            "--content",
            "Asks before deleting branches",
            "--origin",
            "chat/2",
        ],
    );
    let observed = stored(
        &s,
        &[
            "--content",
            "Writes commit bodies that explain the reason",
            "--source",
            "observed",
            "--evidence",
            "Did so for each commit on 2026-10-17",
            "--scope",
            "global",
        ],
    );
    let inferred = stored(
        &s,
        &[
            "--content",
            "Checks with the user before destructive git commands",
            "--source",
            "inferred",
            "--evidence-id",
            &first,
            "--evidence-id",
            &second,
            "--scope",
            "global",
        ],
    );
    for id in [observed, inferred] {
        assert_eq!(inactive(&s, &id)["scope"], "global");
    }
}

// A moved support stales its pattern only where the pattern could no longer
// cite it. Made global, moved into the pattern's own project, or moved with
// its pattern as a whole project moves, it still holds.
#[test]
fn moving_a_support_stales_a_pattern_only_outside_the_patterns_project() {
    let in_a = ["--scope", "project", "--project", "a"];
    let state = |s: &Store, id: &str| {
        s.value(&["search", "--scope", "all", "--status", "all"])
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap_or_else(|| panic!("{id} was not recalled"))["status"]
            .clone()
    };
    // Supports A and B from two conversations, scoped by `scope`, and a
    // pattern inferred from both in project a.
    let probe = |scope: &[&str]| -> (Store, String, String) {
        let s = Store::new();
        let mut supports = Vec::new();
        for (content, origin) in [("Prefer examples", "c:1"), ("Prefer demonstrations", "c:2")] {
            let mut args = vec![
                "--type",
                "preference",
                "--content",
                content,
                "--origin",
                origin,
            ];
            args.extend(scope);
            supports.push(stored(&s, &args));
        }
        let mut args = vec![
            "--type",
            "pattern",
            "--content",
            "Prefer practical learning",
            "--source",
            "inferred",
            "--evidence-id",
            &supports[0],
            "--evidence-id",
            &supports[1],
        ];
        args.extend(in_a);
        let pattern = stored(&s, &args);
        assert_eq!(state(&s, &pattern), "tentative");
        (s, supports[0].clone(), pattern)
    };
    let update = |s: &Store, id: &str, to: &[&str]| {
        let mut args = vec!["update", id];
        args.extend(to);
        assert_eq!(s.value(&args)["updated"], true, "{to:?}");
    };
    let (s, a, pattern) = probe(&in_a);
    update(&s, &a, &["--scope", "project", "--project", "b"]);
    assert_eq!(state(&s, &pattern), "stale", "moved alone");
    let (s, a, pattern) = probe(&in_a);
    update(&s, &a, &["--scope", "global", "--project", ""]);
    assert_eq!(state(&s, &pattern), "tentative", "made global");
    let (s, a, pattern) = probe(&["--scope", "global"]);
    update(&s, &a, &in_a);
    assert_eq!(state(&s, &pattern), "tentative", "moved into its project");
    // A project moving as a whole re-keys the patterns first, so each
    // support then lands in its pattern's project; supports first would
    // leave them outside it for a moment.
    for (order, kept) in [("pattern first", "tentative"), ("supports first", "stale")] {
        let (s, _, pattern) = probe(&in_a);
        let conn = store_sql(&s);
        let pattern_moves = "UPDATE memories SET project='b' WHERE id=?1";
        let rest_moves = "UPDATE memories SET project='b' WHERE project='a' AND id!=?1";
        let steps = if kept == "tentative" {
            [pattern_moves, rest_moves]
        } else {
            [rest_moves, pattern_moves]
        };
        for sql in steps {
            conn.execute(sql, [&pattern]).unwrap();
        }
        drop(conn);
        assert_eq!(state(&s, &pattern), kept, "{order}");
    }
}

// An index that selects nothing from a store holding memories says so,
// rather than that nothing was ever recorded, which only an empty store is.
#[test]
fn an_index_that_selects_nothing_does_not_call_the_store_empty() {
    let s = Store::new();
    let text = |args: &[&str]| String::from_utf8(s.run(args).stdout).unwrap();
    assert_eq!(text(&["index"]).trim(), "No user memories recorded yet.");
    stored(&s, &["--content", "Indent with tabs"]);
    for args in [
        &[
            "index",
            "--scope",
            "project",
            "--project",
            "github.com/acme/app",
        ][..],
        &["index", "--category", "nothing-here"],
    ] {
        assert_eq!(
            text(args).trim(),
            "No memories match these projects and filters.",
            "{args:?}"
        );
    }
}

// SQLite applies a -wal file it finds beside a store to that store, whoever
// wrote it. One left by a crash beside a restored backup brings back writes
// the restore was meant to undo, so the restore steps delete -wal and -shm
// before copying the backup in.
#[test]
fn a_restored_backup_holds_only_its_own_memories_once_the_old_log_is_gone() {
    let s = Store::new();
    let kept = s.value(&[
        "record",
        "--type",
        "preference",
        "--category",
        "workflow",
        "--content",
        "Kept from before the backup",
        "--scope",
        "project",
        "--project",
        "github.com/me/old",
    ]);
    assert_eq!(kept["stored"], true);
    let moved = s.run(&["project", "move", "github.com/me/old", "github.com/me/new"]);
    assert!(moved.status.success(), "{moved:?}");
    let backup = fs::read_dir(s.root.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.to_string_lossy().contains("memory.sqlite3.move-"))
        .unwrap();
    let log = |suffix: &str| PathBuf::from(format!("{}-{suffix}", s.db.display()));
    // A connection held open keeps later writes in the -wal, as a server
    // killed before its last close leaves them.
    let holder = rusqlite::Connection::open(&s.db).unwrap();
    let _: i64 = holder
        .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
        .unwrap();
    s.add("preference", "Recorded after the backup");
    let stale = fs::read(log("wal")).unwrap();
    drop(holder);
    let contents = || -> Result<Vec<String>, rusqlite::Error> {
        let conn = rusqlite::Connection::open(&s.db)?;
        let check: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        let mut rows: Vec<String> = conn
            .prepare("SELECT content FROM memories ORDER BY content")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        rows.insert(0, check);
        Ok(rows)
    };
    // Without the step: the crash's log stays beside the restored store.
    fs::write(log("wal"), &stale).unwrap();
    fs::copy(&backup, &s.db).unwrap();
    assert_eq!(
        contents().unwrap(),
        [
            "ok",
            "Kept from before the backup",
            "Recorded after the backup"
        ],
        "the old log was not applied"
    );
    // With it: delete -wal and -shm, then copy the backup in.
    for suffix in ["wal", "shm"] {
        let _ = fs::remove_file(log(suffix));
    }
    fs::copy(&backup, &s.db).unwrap();
    assert_eq!(contents().unwrap(), ["ok", "Kept from before the backup"]);
}

// Switching to WAL needs a moment with the store to itself. While another
// connection keeps reading, a command works on in the old mode, and a later
// open switches the store.
#[test]
fn a_store_busy_with_a_reader_still_opens_and_switches_to_wal_later() {
    let s = Store::new();
    s.add("preference", "Prefer small commits");
    let mode = || -> String {
        rusqlite::Connection::open(&s.db)
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap()
    };
    rusqlite::Connection::open(&s.db)
        .unwrap()
        .pragma_update(None, "journal_mode", "DELETE")
        .unwrap();
    let reader = rusqlite::Connection::open(&s.db).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
        .unwrap();
    let found = s.value(&["search", "commits"]);
    assert_eq!(found.as_array().unwrap().len(), 1, "{found}");
    reader.execute_batch("COMMIT").unwrap();
    drop(reader);
    assert_eq!(mode(), "delete");
    s.value(&["search", "commits"]);
    assert_eq!(mode(), "wal");
}

// Backups and exports taken while writes still sit in the -wal, not yet
// checkpointed into the store file, hold those writes too, and a backup is
// one whole file with no log of its own.
#[test]
fn backups_and_exports_hold_writes_still_in_the_write_ahead_log() {
    let s = Store::new();
    let project = |content: &str| {
        let out = s.value(&[
            "record",
            "--type",
            "preference",
            "--category",
            "workflow",
            "--content",
            content,
            "--scope",
            "project",
            "--project",
            "github.com/me/old",
        ]);
        assert_eq!(out["stored"], true, "{out}");
    };
    project("Checkpointed into the store file");
    // A connection held open keeps the next write in the -wal.
    let holder = rusqlite::Connection::open(&s.db).unwrap();
    let _: i64 = holder
        .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
        .unwrap();
    project("Still only in the write-ahead log");
    let wal = PathBuf::from(format!("{}-wal", s.db.display()));
    assert!(fs::metadata(&wal).unwrap().len() > 0, "nothing in the -wal");
    let export = s.run(&["export", "-"]);
    assert!(export.status.success(), "{export:?}");
    let export = String::from_utf8_lossy(&export.stdout);
    for content in [
        "Checkpointed into the store file",
        "Still only in the write-ahead log",
    ] {
        assert!(export.contains(content), "export lacks {content:?}");
    }
    let moved = s.run(&["project", "move", "github.com/me/old", "github.com/me/new"]);
    assert!(moved.status.success(), "{moved:?}");
    let backup = fs::read_dir(s.root.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.to_string_lossy().contains("memory.sqlite3.move-"))
        .unwrap();
    let mut beside: Vec<String> = fs::read_dir(s.root.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".move-"))
        .collect();
    beside.sort();
    assert_eq!(beside.len(), 1, "{beside:?}");
    let copy =
        rusqlite::Connection::open_with_flags(&backup, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mode: String = copy
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "delete");
    let held: Vec<String> = copy
        .prepare("SELECT content FROM memories WHERE project='github.com/me/old' ORDER BY content")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        held,
        [
            "Checkpointed into the store file",
            "Still only in the write-ahead log"
        ]
    );
    drop(holder);
}

/// The conversations a memory is recorded from, sorted.
fn sorted_origins(s: &Store, id: &str) -> Vec<String> {
    let mut origins: Vec<String> = inactive(s, id)["origins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o.as_str().unwrap().to_owned())
        .collect();
    origins.sort();
    origins
}

// An update keeps the conversations a memory came from, whatever it changes;
// new content still stales the patterns that cite it.
#[test]
fn an_update_keeps_the_memory_s_origins_by_default() {
    let (s, _, b, p) = support_probe("c:1");
    let updated = s.value(&["update", &b, "--content", "Prefer theory before examples"]);
    assert_eq!(updated["updated"], true, "{updated}");
    assert_eq!(sorted_origins(&s, &b), ["c:2", "c:4"]);
    assert_eq!(status(&s, &p), "stale", "new content");
    let updated = s.value(&["update", &b, "--category", "teaching"]);
    assert_eq!(updated["updated"], true, "{updated}");
    assert_eq!(sorted_origins(&s, &b), ["c:2", "c:4"]);
}

// An origin adds the conversation the update came from to the others.
#[test]
fn an_update_with_an_origin_adds_that_conversation() {
    let (s, _, b, p) = support_probe("c:1");
    let updated = s.value(&[
        "update",
        &b,
        "--content",
        "Prefer theory before examples",
        "--origin",
        "c:9",
    ]);
    assert_eq!(updated["updated"], true, "{updated}");
    assert_eq!(sorted_origins(&s, &b), ["c:2", "c:4", "c:9"]);
    assert_eq!(status(&s, &p), "stale", "new content");
    let updated = s.value(&["update", &b, "--origin", "c:10"]);
    assert_eq!(updated["updated"], true, "{updated}");
    assert_eq!(sorted_origins(&s, &b), ["c:10", "c:2", "c:4", "c:9"]);
}

// A full restatement in a new conversation replaces the origins with it;
// replacing needs that conversation.
#[test]
fn replace_origins_leaves_only_the_given_conversation() {
    let (s, _, b, p) = support_probe("c:1");
    let refused = s.value(&[
        "update",
        &b,
        "--content",
        "Prefer theory first",
        "--replace-origins",
    ]);
    assert_eq!(refused["updated"], false, "{refused}");
    assert_eq!(sorted_origins(&s, &b), ["c:2", "c:4"]);
    let updated = s.value(&[
        "update",
        &b,
        "--content",
        "Prefer theory first",
        "--origin",
        "c:9",
        "--replace-origins",
    ]);
    assert_eq!(updated["updated"], true, "{updated}");
    assert_eq!(sorted_origins(&s, &b), ["c:9"]);
    assert_eq!(status(&s, &p), "stale", "new content");
}

// Forgetting either conversation of an updated memory keeps the memory: the
// other still stated it.
#[test]
fn forgetting_one_origin_of_an_updated_memory_keeps_it() {
    for forgotten in ["c:2", "c:9"] {
        let s = Store::new();
        let id = stored(
            &s,
            &[
                "--type",
                "preference",
                "--content",
                "Prefer examples",
                "--origin",
                "c:2",
            ],
        );
        let updated = s.value(&[
            "update",
            &id,
            "--content",
            "Prefer short examples",
            "--origin",
            "c:9",
        ]);
        assert_eq!(updated["updated"], true, "{updated}");
        s.value(&["forget", "--origin", forgotten]);
        let kept = inactive(&s, &id);
        assert_eq!(
            kept["status"], "confirmed",
            "after forgetting {forgotten}: {kept}"
        );
        assert_eq!(kept["content"], "Prefer short examples");
        assert_eq!(
            sorted_origins(&s, &id).len(),
            1,
            "after forgetting {forgotten}"
        );
    }
}

// Text that starts with a hyphen, such as a Markdown list item or a value
// that looks like a flag, is that argument's value; flags after it still
// count as flags.
#[test]
fn text_values_may_start_with_a_hyphen() {
    let s = Store::new();
    let saved = s.value(&[
        "record",
        "--type",
        "behavior",
        "--source",
        "observed",
        "--category",
        "workflow",
        "--content",
        "- Keeps commits small",
        "--evidence",
        "- split a change in two",
        "--reason",
        "--easier review",
    ]);
    assert_eq!(saved["stored"], true, "{saved}");
    let id = saved["id"].as_str().unwrap().to_owned();
    let memory = inactive(&s, &id);
    assert_eq!(memory["content"], "- Keeps commits small");
    assert_eq!(memory["title"], "- Keeps commits small");
    assert_eq!(memory["evidence"], "- split a change in two");
    assert_eq!(memory["reason"], "--easier review");
    for (flag, field, value) in [
        ("--content", "content", "--x"),
        ("--context", "context", "- in reviews"),
        ("--reason", "reason", "-v"),
        ("--title", "title", "- Small commits"),
    ] {
        let out = s.value(&["update", &id, flag, value, "--category", "habits"]);
        assert_eq!(out["updated"], true, "{flag}: {out}");
        let memory = inactive(&s, &id);
        assert_eq!(memory[field], value, "{flag}");
        assert_eq!(memory["category"], "habits", "{flag}: the flag after it");
    }
}

// backup now names the copy and the backups it deleted past the newest
// seven; a missing store and a client of a memory server are refused.
#[test]
fn backup_now_reports_the_copy_and_the_backups_it_deleted() {
    let s = Store::new();
    let missing = s.run(&["backup", "now"]);
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr)
            .contains(&format!("No memory store at {}", s.db.display())),
        "{missing:?}"
    );
    let stored = s.value(&[
        "record",
        "--category",
        "tooling",
        "--content",
        "Uses zsh",
        "--scope",
        "global",
    ]);
    assert_eq!(stored["stored"], true, "{stored}");
    let folder = s.db.with_file_name("backups");
    let today: Vec<PathBuf> = fs::read_dir(&folder)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(today.len(), 1);
    for day in 1..=7 {
        fs::copy(
            &today[0],
            folder.join(format!("memory-2000-01-0{day}.sqlite3")),
        )
        .unwrap();
    }
    let now = s.run(&["backup", "now"]);
    assert!(now.status.success(), "{now:?}");
    let backed_up = format!(
        "Backed up {} to {} (1 memory)\n",
        s.db.display(),
        today[0].display()
    );
    assert_eq!(
        String::from_utf8_lossy(&now.stdout),
        format!("{backed_up}Deleted backups past the newest 7: memory-2000-01-01.sqlite3\n")
    );
    let again = s.run(&["backup", "now"]);
    assert_eq!(String::from_utf8_lossy(&again.stdout), backed_up);
    // A client names the command for the server that keeps the store: the
    // installer's Docker server on this machine, or another machine's.
    let docker = sandbox::folder(s.root.path(), "docker");
    fs::create_dir_all(&docker).unwrap();
    fs::write(
        docker.join("compose.yaml"),
        "    ports:\n      - \"127.0.0.1:18765:8765\"\n",
    )
    .unwrap();
    for command in ["list", "now"] {
        let client = |url: &str| {
            let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
                .args(["backup", command])
                .sandboxed(s.root.path())
                .env("USER_MEMORY_URL", url)
                .env("USER_MEMORY_TOKEN_FILE", s.root.path().join("token"))
                .output()
                .unwrap();
            assert!(!out.status.success());
            String::from_utf8(out.stderr).unwrap()
        };
        let refusal =
            format!("Error: Run backup {command} on the memory server that owns the database");
        assert_eq!(
            client("http://127.0.0.1:18765/mcp"),
            format!(
                "{refusal}. It runs in Docker on this machine:\n  docker compose -f '{}' exec -T memory user-memory backup {command}\n",
                docker.join("compose.yaml").display()
            )
        );
        assert_eq!(
            client("https://memory.example.com/mcp"),
            format!(
                "{refusal}: on the machine that serves https://memory.example.com/mcp, run: user-memory backup {command}\n"
            )
        );
    }
}

// A store named by a bare file name lives in the current folder, which
// exists: only a missing store folder is created, private.
#[test]
fn a_store_named_by_a_bare_file_name_lives_in_the_current_folder() {
    let dir = tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .args(["search", "anything"])
        .current_dir(dir.path())
        .sandboxed(dir.path())
        .env("USER_MEMORY_DB", "memories.sqlite3")
        .env_remove("USER_MEMORY_URL")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.path().join("memories.sqlite3").is_file());
}

// Without USER_MEMORY_DB the store is where the binary has always put it,
// under HOME: following XDG_DATA_HOME would leave existing stores behind.
#[cfg(unix)]
#[test]
fn the_default_store_is_under_home_whatever_xdg_data_home_says() {
    let dir = tempdir().unwrap();
    let data = dir.path().join("xdg-data");
    let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .args(["search", "anything"])
        .sandboxed(dir.path())
        .env("XDG_DATA_HOME", &data)
        .env_remove("USER_MEMORY_DB")
        .env_remove("USER_MEMORY_URL")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        dir.path()
            .join(".local/share/user-memory/memories.sqlite3")
            .is_file()
    );
    assert!(!data.exists());
}

/// Read and search leave a memory's empty fields out, which an agent would
/// otherwise load with every memory; an export keeps every field.
#[test]
fn read_and_search_leave_empty_fields_out_and_exports_keep_them() {
    let s = Store::new();
    let id = s.add("preference", "Prefer tabs");
    let empty = [
        "context",
        "reason",
        "project",
        "evidence",
        "evidence_ids",
        "origins",
        "last_confirmed_at",
        "superseded_by",
    ];
    let read = s.value(&["read", &id])["memories"][0].clone();
    let found = s.value(&["search", "tabs"])[0].clone();
    for memory in [&read, &found] {
        for field in empty {
            assert_eq!(memory.get(field), None, "{field}: {memory}");
        }
        assert_eq!(memory["id"], id, "{memory}");
        assert_eq!(memory["content"], "Prefer tabs", "{memory}");
        assert_eq!(memory["confirmations"], 0, "{memory}");
    }
    let export: Value = serde_json::from_slice(&s.run(&["export", "-"]).stdout).unwrap();
    let exported = &export["memories"][0];
    for field in empty {
        assert!(
            exported.get(field).is_some(),
            "export lost {field}: {exported}"
        );
    }
}

/// A search returns its 20 best matches unless asked for more; the index
/// and a category read, which list, keep pages of 100.
#[test]
fn a_search_page_is_twenty_unless_asked_for_more() {
    let s = Store::new();
    for n in 0..25 {
        s.add("preference", &format!("Use tabs in file kind {n}"));
    }
    let page = |args: &[&str]| -> Value {
        let out = s.run(args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let found = page(&["search", "tabs"]);
    assert_eq!(found["memories"].as_array().unwrap().len(), 20, "{found}");
    assert!(found.get("next_cursor").is_some(), "{found}");
    let more = page(&["search", "tabs", "--page-size", "25"]);
    assert_eq!(more["memories"].as_array().unwrap().len(), 25);
    let category = page(&["read", "--category", "programming"]);
    assert_eq!(category["memories"].as_array().unwrap().len(), 25);
    assert!(category.get("next_cursor").is_none(), "{category}");
}

/// The CLI takes `--projects`, the tools' own name, as `--project`: agents
/// copy a tool call into the shell.
#[test]
fn the_cli_takes_projects_as_the_tools_name_it() {
    let s = Store::new();
    let out = s.resolved(&[
        "record",
        "--type",
        "fact",
        "--category",
        "tooling",
        "--content",
        "The fuzzer needs nightly",
        "--scope",
        "project",
        "--project",
        "github.com/acme/compiler",
    ]);
    assert_eq!(out["stored"], true, "{out}");
    for command in ["index", "search"] {
        let out = s.run(&[command, "--projects", "github.com/acme/compiler"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stdout.contains("The fuzzer needs nightly"),
            "{command}: {stdout}"
        );
    }
    // The CLI's help says it takes a folder, which only the tools refuse.
    for command in ["index", "read", "search"] {
        let help = String::from_utf8(s.run(&[command, "--help"]).stdout).unwrap();
        assert!(
            help.contains("a folder such as . whose project is used"),
            "{help}"
        );
        assert!(!help.contains("never local paths"), "{help}");
    }
    let out = s.run(&[
        "read",
        "--category",
        "tooling",
        "--projects",
        "github.com/acme/compiler",
    ]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("The fuzzer needs nightly"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
