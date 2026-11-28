//! The working set across a compaction (#118), through the real binary on
//! a local store: what `working-set` prints on `compact`, in which order
//! and form, from the store as it is now; that its two compact handlers
//! open one epoch in either order; that no other start restores; and what
//! `pre-compact` asks the summary to keep.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
#[path = "support/time.rs"]
mod time;
use time::HOOK_ROOM;

use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};
use tempfile::{TempDir, tempdir};

/// Opens the working set.
const HEADER: &str = "What this context worked with before it was compacted, from user-memory as it is now (a memory changed since shows its current text):";

/// A sandboxed home with its own store and a checkout.
struct Home {
    dir: TempDir,
}

impl Home {
    fn new() -> Self {
        let home = Self {
            dir: tempdir().unwrap(),
        };
        std::fs::create_dir(home.checkout()).unwrap();
        home
    }

    fn checkout(&self) -> PathBuf {
        self.dir.path().join("app")
    }

    fn db(&self) -> PathBuf {
        self.dir.path().join("memory.sqlite3")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .args(args)
            .current_dir(self.checkout())
            .sandboxed(self.dir.path())
            .env("USER_MEMORY_DB", self.db())
            .env(HOOK_ROOM.0, HOOK_ROOM.1);
        command
    }

    /// Runs the CLI with `args`; its JSON reply.
    fn cli(&self, args: &[&str]) -> Value {
        let out = self.command(args).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// Records a global fact the user stated, with `extra` arguments; its
    /// full id.
    fn record_with(&self, title: &str, content: &str, extra: &[&str]) -> String {
        let mut args = vec![
            "record",
            "--type",
            "fact",
            "--category",
            "build",
            "--scope",
            "global",
            "--title",
            title,
            "--content",
            content,
        ];
        args.extend(extra);
        let out = self.cli(&args);
        assert_eq!(out["stored"], true, "{out}");
        out["id"].as_str().unwrap().to_owned()
    }

    fn record(&self, title: &str, content: &str) -> String {
        self.record_with(title, content, &[])
    }

    /// Runs `subcommand` for Claude Code with `payload`; its stdout.
    fn hook(&self, subcommand: &str, payload: &Value) -> String {
        let mut child = self
            .command(&[subcommand, "--client", "claude"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = time::finish(child, subcommand);
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn started(&self, session: &str, source: &str) -> Value {
        json!({"session_id": session, "cwd": self.checkout(), "hook_event_name": "SessionStart", "source": source})
    }

    /// Session start for `session` as `source` says.
    fn start(&self, session: &str, source: &str) -> String {
        self.hook("session-start", &self.started(session, source))
    }

    /// The working-set hook for `session` started as `source` says.
    fn working_set(&self, session: &str, source: &str) -> String {
        self.hook("working-set", &self.started(session, source))
    }

    /// Both compact handlers, the working set first when `set_first`; what
    /// the working set printed.
    fn compact(&self, session: &str, set_first: bool) -> String {
        if set_first {
            let set = self.working_set(session, "compact");
            self.start(session, "compact");
            set
        } else {
            self.start(session, "compact");
            self.working_set(session, "compact")
        }
    }

    /// What PreCompact prints for `session`.
    fn pre_compact(&self, session: &str) -> String {
        self.hook(
            "pre-compact",
            &json!({"session_id": session, "cwd": self.checkout(), "hook_event_name": "PreCompact", "trigger": "auto"}),
        )
    }

    /// The post-tool hook after `tool` gave `reply` in `session`.
    fn call(&self, session: &str, tool: &str, reply: &Value) -> String {
        self.hook(
            "post-tool-use",
            &json!({"session_id": session, "cwd": self.checkout(), "hook_event_name": "PostToolUse",
                    "tool_name": tool, "tool_input": {}, "tool_response": [{"type": "text", "text": reply.to_string()}]}),
        )
    }

    /// `read` returning `ids` in `session`.
    fn read(&self, session: &str, ids: &[&str]) {
        let memories: Vec<Value> = ids.iter().map(|id| json!({"id": id})).collect();
        self.call(
            session,
            "mcp__user-memory__read",
            &json!({"memories": memories}),
        );
    }

    /// The push after `command` ran in `session`.
    fn run(&self, session: &str, command: &str) -> String {
        self.hook(
            "post-tool-use",
            &json!({"session_id": session, "cwd": self.checkout(), "hook_event_name": "PostToolUse",
                    "tool_name": "Bash", "tool_input": {"command": command}, "tool_response": {"stdout": ""}}),
        )
    }

    /// The main context's epoch in `session`'s state.
    fn epoch(&self, session: &str) -> i64 {
        let dir = sandbox::folder(self.dir.path(), "state").join("sessions");
        let file = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                name.starts_with(&format!("claude-{session}-")) && name.ends_with(".recall")
            })
            .unwrap();
        Connection::open(file)
            .unwrap()
            .query_row("SELECT epoch FROM contexts WHERE agent = 'main'", [], |r| {
                r.get(0)
            })
            .unwrap()
    }
}

fn short(id: &str) -> &str {
    &id[..8]
}

// After a compaction the working set prints what the context read, then
// what it recorded or changed, then what a push showed, newest first
// within each, short memories whole and a long one as a pointer, and what
// a search found last, as pointers; what it printed whole is not pushed
// again in the new epoch.
#[test]
fn a_compaction_restores_what_the_context_read_recorded_and_was_shown() {
    let home = Home::new();
    let first = home.record("Parser tables", "Keep `src/tables.rs` sorted.");
    let second = home.record("Docs site", "The docs build with mdBook.");
    let long = home.record(
        "Release steps",
        &"Tag, build and publish the crate. ".repeat(30),
    );
    let recorded = home.record("Bench machine", "The bench runs on a quiet box.");
    let pushed = home.record(
        "Dry run before publishing",
        "Run `cargo publish --dry-run` before `cargo publish`.",
    );
    let found = home.record("Lint config", "Clippy denies warnings here.");
    home.start("s1", "startup");
    home.read("s1", &[&first]);
    home.read("s1", &[&long]);
    home.read("s1", &[&second]);
    home.call(
        "s1",
        "mcp__user-memory__record",
        &json!({"stored": true, "id": recorded}),
    );
    assert!(home.run("s1", "cargo publish").contains(short(&pushed)));
    home.call(
        "s1",
        "mcp__user-memory__search",
        &json!({"memories": [{"id": found}]}),
    );
    let set = home.compact("s1", false);
    assert_eq!(
        set,
        format!(
            "{HEADER}\nRead:\n- {} The docs build with mdBook.\n- {} Release steps; if it applies, read it again first\n- {} Keep `src/tables.rs` sorted.\nRecorded or changed:\n- {} The bench runs on a quiet box.\nShown after a step:\n- {} Run `cargo publish --dry-run` before `cargo publish`.\nFound by a search:\n- {} Lint config; if it applies, read it again first\n",
            short(&second),
            short(&long),
            short(&first),
            short(&recorded),
            short(&pushed),
            short(&found),
        )
    );
    assert_eq!(
        home.run("s1", "cargo publish"),
        "",
        "printed whole, so not pushed again"
    );
    // PreCompact names the same set by id and title, from the new epoch.
    let keep = home.pre_compact("s1");
    assert!(
        keep.contains(&format!("{} Docs site", short(&second))),
        "{keep}"
    );
    assert!(
        keep.contains(&format!("{} Lint config", short(&found))),
        "{keep}"
    );
}

// Both compact handlers open one epoch however they run: the working set
// before session start prints what it prints after it.
#[test]
fn the_two_compact_handlers_open_one_epoch_in_either_order() {
    let home = Home::new();
    let note = home.record("Parser tables", "Keep `src/tables.rs` sorted.");
    let mut sets = Vec::new();
    for (session, set_first) in [("s1", true), ("s2", false)] {
        home.start(session, "startup");
        home.read(session, &[&note]);
        let before = home.epoch(session);
        sets.push(home.compact(session, set_first));
        assert_eq!(home.epoch(session), before + 1, "{session}");
    }
    assert!(sets[0].contains(short(&note)), "{}", sets[0]);
    assert_eq!(sets[0], sets[1]);
    // A second compaction with no action since restores the same set.
    assert_eq!(home.compact("s1", false), sets[0]);
}

// The restore reads the store as it is now: a memory changed since shows
// its new text, a superseded one the memory that replaced it, and a
// forgotten or stale one, or one whose condition ended, nothing.
#[test]
fn a_compaction_restores_the_store_as_it_is_now() {
    let home = Home::new();
    let changed = home.record("Docs site", "The docs build with mdBook.");
    let old = home.record("Bench machine", "The bench runs on a quiet box.");
    let forgotten = home.record("Release train", "Releases leave on Tuesdays.");
    let stale = home.record("Lint config", "Clippy denies warnings here.");
    let ended = home.record("Merge freeze", "No merges during the freeze.");
    home.start("s1", "startup");
    home.read("s1", &[&changed, &old, &forgotten, &stale, &ended]);
    home.cli(&[
        "update",
        &changed,
        "--content",
        "The docs build with Zola now.",
    ]);
    let new = home.record_with(
        "Bench machine",
        "The bench runs on the arm64 box.",
        &["--supersedes", &old],
    );
    home.cli(&["forget", &forgotten]);
    let conn = Connection::open(home.db()).unwrap();
    conn.execute(
        "UPDATE memories SET status = 'stale' WHERE id = ?1",
        [&stale],
    )
    .unwrap();
    // Its date passed: out of force, though still confirmed.
    conn.execute(
        "UPDATE memories SET until_kind = 'date', until_value = '2001-01-01T00:00:00Z' WHERE id = ?1",
        [&ended],
    )
    .unwrap();
    drop(conn);
    let set = home.compact("s1", false);
    assert!(set.contains("The docs build with Zola now."), "{set}");
    assert!(!set.contains("mdBook"), "{set}");
    assert!(
        set.contains(&format!(
            "- {} The bench runs on the arm64 box.",
            short(&new)
        )),
        "{set}"
    );
    for gone in [&old, &forgotten, &stale, &ended] {
        assert!(!set.contains(short(gone)), "{gone}: {set}");
    }
}

// Only a compaction restores: a clear asks for an empty context, a resumed
// transcript still holds the set, and a new session inherits none. A
// compaction right after a clear, with no action between, restores
// nothing either.
#[test]
fn only_a_compaction_restores() {
    let home = Home::new();
    let note = home.record("Parser tables", "Keep `src/tables.rs` sorted.");
    home.start("s1", "startup");
    home.read("s1", &[&note]);
    for source in ["startup", "resume", "clear"] {
        assert_eq!(home.working_set("s1", source), "", "{source}");
    }
    home.start("s1", "clear");
    assert_eq!(
        home.compact("s1", false),
        "",
        "a compaction just after a clear"
    );
    home.start("s2", "startup");
    assert_eq!(home.compact("s2", false), "", "a new session's");
    assert_eq!(home.pre_compact("s2"), "");
}
