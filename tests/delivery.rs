#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

//! Delivery matching through the real binaries: a store written by
//! `user-memory`, its delivery index built and matched by the benchmark's
//! `user-memory-bench`. What an action did, never what a tool returned,
//! joins the window; and a term's holders are the memories of the scope
//! that hold it, so another project's memories never make it common here.

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;

use serde_json::{Value, json};
use std::{
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

    fn run(&self, program: &str, args: &[&str]) -> Output {
        let out = Command::new(program)
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

    /// Records a memory in `scope` (a project, or global) and returns its id.
    fn record(&self, project: Option<&str>, title: &str, content: &str) -> String {
        let mut args = vec![
            "record",
            "--category",
            "tooling",
            "--title",
            title,
            "--content",
            content,
        ];
        match project {
            Some(project) => args.extend(["--scope", "project", "--project", project]),
            None => args.extend(["--scope", "global"]),
        }
        let out: Value =
            serde_json::from_slice(&self.run(env!("CARGO_BIN_EXE_user-memory"), &args).stdout)
                .unwrap();
        assert_eq!(out["stored"], true, "{out}");
        out["id"].as_str().unwrap().to_owned()
    }

    /// Builds the delivery index of the global memories and `projects`'.
    fn index(&self, projects: &[&str]) -> PathBuf {
        let out = self.root.path().join("delivery.index");
        let db = self.db.to_str().unwrap();
        let mut args = vec!["index", "--db", db, "--out", out.to_str().unwrap()];
        for project in projects {
            args.extend(["--project", project]);
        }
        self.run(env!("CARGO_BIN_EXE_user-memory-bench"), &args);
        out
    }

    /// What one action matches, as one hook call would.
    fn lookup(&self, index: &Path, action: &Value) -> Value {
        let action = action.to_string();
        let out = self.run(
            env!("CARGO_BIN_EXE_user-memory-bench"),
            &[
                "lookup",
                "--index",
                index.to_str().unwrap(),
                "--action",
                &action,
            ],
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// Every action of `tasks`, matched in order, as JSON lines.
    fn matched(&self, index: &Path, tasks: &Value) -> String {
        let file = self.root.path().join("tasks.json");
        std::fs::write(&file, tasks.to_string()).unwrap();
        let out = self.run(
            env!("CARGO_BIN_EXE_user-memory-bench"),
            &[
                "match",
                "--index",
                index.to_str().unwrap(),
                "--tasks",
                file.to_str().unwrap(),
            ],
        );
        String::from_utf8(out.stdout).unwrap()
    }
}

#[test]
fn a_tool_result_joins_nothing() {
    let store = Store::new();
    store.record(
        None,
        "Publishing",
        "Run `cargo publish` only from a clean tree.",
    );
    let hidden = store.record(
        None,
        "Quokka migration",
        "The quokka table moves in the next release; keep its old column.",
    );
    let index = store.index(&[]);
    for (kind, text) in [
        ("command", "cargo publish"),
        ("message", "Published the crate with cargo publish"),
    ] {
        let action = json!({"kind": kind, "text": text, "failed": false});
        let with_result = json!({"kind": kind, "text": text, "failed": false,
                                 "result": "error: the quokka table is locked"});
        let plain = store.matched(&index, &json!([{"id": "t", "actions": [action]}]));
        let answered = store.matched(&index, &json!([{"id": "t", "actions": [with_result]}]));
        assert_eq!(plain, answered, "{kind}");
        assert!(!plain.contains(&hidden), "{kind}: {plain}");
        assert!(plain.contains("\"anchored\":true"), "{kind}: {plain}");
    }
}

#[test]
fn holders_are_the_memories_of_the_scope() {
    let store = Store::new();
    let mine = store.record(
        Some("example/mine"),
        "Publishing",
        "Run `cargo publish` only from a clean tree.",
    );
    for (title, content) in [
        (
            "Signing",
            "Load the signing key before `cargo publish` on the build host.",
        ),
        (
            "Registry",
            "Our mirror rejects `cargo publish` without the internal token.",
        ),
        (
            "Changelog",
            "Write the changelog entry, then `cargo publish` and tag.",
        ),
    ] {
        store.record(Some("example/other"), title, content);
    }
    let action = json!({"kind": "command", "text": "cargo publish", "failed": false});
    let here = store.lookup(&store.index(&["example/mine"]), &action);
    let passing = here["passing"].as_array().unwrap();
    assert_eq!(passing.len(), 1, "{here}");
    assert_eq!(passing[0]["id"], mine.as_str());
    assert_eq!(passing[0]["fewest"], 1, "{here}");
    // With the other project in scope, the anchor has four holders.
    let matched = store.matched(
        &store.index(&["example/mine", "example/other"]),
        &json!([{"id": "t", "actions": [action]}]),
    );
    assert!(matched.contains("\"fewest\":4"), "{matched}");
}
