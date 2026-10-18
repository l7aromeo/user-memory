//! `user-memory repair` on stores damaged the way two writers at once can
//! leave them: the backup holds every memory, and so does the repaired store.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
#[path = "support/damage.rs"]
#[allow(dead_code, reason = "each test file uses part of the shared damage")]
mod damage;

use rusqlite::Connection;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::{TempDir, tempdir};

fn user_memory(home: &Path, db: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .args(args)
        .sandboxed(home)
        .env("USER_MEMORY_DB", db)
        .env_remove("USER_MEMORY_URL")
        .env_remove("USER_MEMORY_TOKEN_FILE")
        .output()
        .unwrap()
}

/// A store holding three memories, in its own directory.
fn store() -> (TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    let db = dir.path().join("memories.sqlite3");
    for (category, title) in [
        ("tools", "Prefer ripgrep over grep for searching code"),
        ("writing", "Write release notes in plain English"),
        ("workflow", "Write commit messages with a why body"),
    ] {
        let out = user_memory(
            dir.path(),
            &db,
            &[
                "record",
                "--type",
                "preference",
                "--category",
                category,
                "--title",
                title,
                "--content",
                title,
                "--scope",
                "global",
            ],
        );
        assert!(out.status.success(), "{out:?}");
    }
    (dir, db)
}

fn ids(db: &Path) -> Vec<String> {
    Connection::open(db)
        .unwrap()
        .prepare("SELECT id FROM memories NOT INDEXED ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn backups(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with(".bak"))
        .collect()
}

#[test]
fn repair_backs_up_and_keeps_every_memory_of_a_damaged_store() {
    for (name, damage) in damage::ALL {
        let (dir, db) = store();
        let before = ids(&db);
        damage(&db);
        let out = user_memory(dir.path(), &db, &["repair"]);
        assert!(out.status.success(), "{name}: {out:?}");
        let [backup] = backups(dir.path()).try_into().unwrap();
        // The backup is the store as it was, damage and every memory included.
        assert_eq!(ids(&backup), before, "{name}: the backup lost memories");
        assert_ne!(damage::check(&backup, "integrity_check"), ["ok"], "{name}");
        assert_eq!(ids(&db), before, "{name}: the repair lost memories");
        assert_eq!(damage::check(&db, "integrity_check"), ["ok"], "{name}");
        // Each memory is found by its id again.
        let conn = Connection::open(&db).unwrap();
        for id in &before {
            let found: i64 = conn
                .query_row("SELECT count(*) FROM memories WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(found, 1, "{name}: {id}");
        }
        let again = user_memory(dir.path(), &db, &["repair"]);
        assert!(
            String::from_utf8_lossy(&again.stdout).contains("nothing to repair"),
            "{name}: {again:?}"
        );
    }
}
