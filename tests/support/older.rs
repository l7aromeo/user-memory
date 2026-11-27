//! Turning a store this binary made back to an earlier schema version, for
//! tests of a migration from it, and reading the committed snapshots.

#![allow(dead_code, reason = "each test file uses part of this module")]

use rusqlite::Connection;
use std::{
    fs,
    path::{Path, PathBuf},
};

/// The committed snapshots of every schema version.
pub fn snapshots() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/schema")
}

/// The statements of a snapshot file.
pub fn parse(text: &str) -> Vec<String> {
    let body: String = text
        .lines()
        .skip_while(|line| line.starts_with("--"))
        .map(|line| format!("{line}\n"))
        .collect();
    body.split("\n;\n")
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

/// The statement of schema version `n`'s snapshot that creates `name`.
pub fn snapshot_statement(n: i64, kind: &str, name: &str) -> String {
    let snapshot = fs::read_to_string(snapshots().join(format!("v{n}.sql"))).unwrap();
    let start = format!("CREATE {kind} {name} ");
    parse(&snapshot)
        .into_iter()
        .find(|statement| statement.starts_with(&start))
        .unwrap_or_else(|| panic!("v{n}.sql creates no {kind} {name}"))
}

/// Turns a store this binary made back to schema version 11: version 12's
/// use table, version 13's views, date index, end records, their triggers
/// and the condition columns, and version 14's kept pairs go, and the
/// history and the change counter get version 11's table and triggers.
pub fn back_to_v11(db: &Path) {
    let conn = Connection::open(db).unwrap();
    conn.execute_batch(
        "DROP TRIGGER memory_pairs_vector_inserted;
DROP TRIGGER memory_pairs_vector_changed;
DROP TRIGGER memory_pairs_vector_deleted;
DROP TABLE memory_pairs;
DROP TABLE memory_pairs_state;
DROP TABLE memory_pairs_due;
DROP TABLE memory_use;
DROP VIEW live_memories;
DROP VIEW out_of_force;
DROP INDEX memories_until_date;
DROP TRIGGER memory_support_ended;
DROP TRIGGER memory_event_ended;
DROP TRIGGER memory_event_reopened;
DROP TRIGGER memory_change_ended;
DROP TRIGGER memory_change_reopened;
DROP TABLE memory_ended;
DROP TRIGGER memory_change_updated;
DROP TRIGGER memory_event_recorded;
DROP TRIGGER memory_event_changed;
DROP TRIGGER memory_event_forgotten;
DROP TRIGGER memory_event_origin_removed;
DROP TRIGGER memory_event_distinct;
ALTER TABLE memories DROP COLUMN until_project;
ALTER TABLE memories DROP COLUMN until_value;
ALTER TABLE memories DROP COLUMN until_kind;
ALTER TABLE memory_events RENAME TO memory_events_v13;",
    )
    .unwrap();
    let mut old = vec![snapshot_statement(11, "TABLE", "memory_events")];
    old.push("INSERT INTO memory_events SELECT * FROM memory_events_v13".to_owned());
    old.push("DROP TABLE memory_events_v13".to_owned());
    old.push(snapshot_statement(11, "INDEX", "memory_events_by_memory"));
    for name in [
        "memory_event_recorded",
        "memory_event_changed",
        "memory_event_forgotten",
        "memory_event_origin_removed",
        "memory_event_distinct",
        "memory_change_updated",
    ] {
        old.push(snapshot_statement(11, "TRIGGER", name));
    }
    conn.execute_batch(&format!("{};", old.join(";\n")))
        .unwrap();
    conn.pragma_update(None, "user_version", 11).unwrap();
}

/// The current schema version: the newest committed snapshot's.
pub fn current_version() -> i64 {
    fs::read_dir(snapshots())
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().into_string().unwrap();
            name.strip_prefix('v')?.strip_suffix(".sql")?.parse().ok()
        })
        .max()
        .unwrap()
}
