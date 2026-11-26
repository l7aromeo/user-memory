//! A session's use of memories (#123) as its hooks keep it, through the
//! real binary on a local store: what the post-tool hook notes from
//! user-memory's own `read` and `record` and from a push, what the next
//! prompt or session start writes to `memory_use`, what a startup sends
//! for other sessions, and what the most-used part's whole lines spare
//! from a push.

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

/// The checkout's project, from its `origin` remote.
const PROJECT: &str = "github.com/example/app";

/// Opens the most-used part of session start's text.
const MOST_USED: &str =
    "Most used in this project's recent sessions (read one for the rest before applying it):";

/// A sandboxed home with its own store and a checkout of [`PROJECT`].
struct Home {
    dir: TempDir,
}

/// One `memory_use` row: memory, project, origin, read, recorded, shown,
/// passage.
type Row = (String, String, String, bool, bool, bool, Option<i64>);

impl Home {
    fn new() -> Self {
        let home = Self {
            dir: tempdir().unwrap(),
        };
        std::fs::create_dir(home.checkout()).unwrap();
        home.git(&["init", "-q", "-b", "main"]);
        home.git(&["remote", "add", "origin", "git@github.com:example/app.git"]);
        home
    }

    fn checkout(&self) -> PathBuf {
        self.dir.path().join("app")
    }

    fn db(&self) -> PathBuf {
        self.dir.path().join("memory.sqlite3")
    }

    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(self.checkout())
            .args(args)
            .sandboxed(self.dir.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
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

    /// Records a global memory of `memory_type`; its full id.
    fn record(&self, title: &str, content: &str, memory_type: &str) -> String {
        let out = self
            .command(&[
                "record",
                "--type",
                memory_type,
                "--category",
                "build",
                "--scope",
                "global",
                "--title",
                title,
                "--content",
                content,
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let out: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(out["stored"], true, "{out}");
        out["id"].as_str().unwrap().to_owned()
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

    /// What session start prints for `session` started as `source` says.
    fn start(&self, session: &str, source: &str) -> String {
        self.hook(
            "session-start",
            &json!({"session_id": session, "cwd": self.checkout(), "hook_event_name": "SessionStart", "source": source}),
        )
    }

    /// A prompt in `session`.
    fn prompt(&self, session: &str) {
        self.hook(
            "prompt-submit",
            &json!({"session_id": session, "cwd": self.checkout(), "hook_event_name": "UserPromptSubmit", "prompt": "Go on."}),
        );
    }

    /// The post-tool hook after `tool` gave `response` in `session`; the
    /// push it printed, or empty.
    fn call(&self, session: &str, tool: &str, input: Value, response: Value) -> String {
        self.hook(
            "post-tool-use",
            &json!({"session_id": session, "cwd": self.checkout(), "hook_event_name": "PostToolUse",
                    "tool_name": tool, "tool_input": input, "tool_response": response}),
        )
    }

    /// The push after `command` ran in `session`.
    fn run(&self, session: &str, command: &str) -> String {
        self.call(
            session,
            "Bash",
            json!({"command": command}),
            json!({"stdout": ""}),
        )
    }

    /// Every `memory_use` row, in order.
    fn rows(&self) -> Vec<Row> {
        let conn = Connection::open(self.db()).unwrap();
        let mut statement = conn
            .prepare("SELECT memory,project,origin,read,recorded,shown,passage FROM memory_use ORDER BY 1,2,3")
            .unwrap();
        statement
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// When the store last wrote each `memory_use` row.
    fn written(&self) -> Vec<String> {
        let conn = Connection::open(self.db()).unwrap();
        let mut statement = conn
            .prepare("SELECT at FROM memory_use ORDER BY memory,project,origin")
            .unwrap();
        statement
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// The recall state file of `session`.
    fn state(&self, session: &str) -> PathBuf {
        let dir = sandbox::folder(self.dir.path(), "state").join("sessions");
        std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                name.starts_with(&format!("claude-{session}-")) && name.ends_with(".recall")
            })
            .unwrap_or_else(|| panic!("no state of {session} in {}", dir.display()))
    }
}

/// A `read` reply as the client hands its hook the tool's result: the
/// result's text parts.
fn read_reply(ids: &[&str]) -> Value {
    let memories: Vec<Value> = ids.iter().map(|id| json!({"id": id})).collect();
    json!([{"type": "text", "text": json!({"memories": memories}).to_string()}])
}

fn row(memory: &str, read: bool, recorded: bool, shown: bool, passage: Option<i64>) -> Row {
    (
        memory.to_owned(),
        PROJECT.to_owned(),
        "claude:session/s1".to_owned(),
        read,
        recorded,
        shown,
        passage,
    )
}

// What `read` returned, what `record` stored and what a push showed reach
// `memory_use` at the next prompt, for the session's project; a refused
// record, a call's input alone and another tool note nothing. A prompt
// with nothing new writes nothing, and neither does a row whose epoch
// moved without a use.
#[test]
fn a_session_s_use_reaches_the_store_at_its_next_prompt() {
    let home = Home::new();
    let note = home.record(
        "Dry run before publishing",
        "Run `cargo publish --dry-run` before `cargo publish`.",
        "procedure",
    );
    let read = home.record("Parser tables", "Keep `src/tables.rs` sorted.", "procedure");
    let recorded = home.record("Bench machine", "The bench runs on a quiet box.", "fact");
    let codex_read = home.record("Docs site", "The docs build with mdBook.", "fact");
    home.start("s1", "startup");
    assert!(home.run("s1", "cargo publish").contains(&note[..8]));
    home.call(
        "s1",
        "mcp__user-memory__read",
        json!({"ids": [&read[..8]]}),
        read_reply(&[&read]),
    );
    // A reply as the result's JSON text, and the result with its content.
    home.call(
        "s1",
        "mcp__user_memory__read",
        json!({}),
        json!({"content": read_reply(&[&codex_read])}),
    );
    home.call(
        "s1",
        "mcp__user-memory__record",
        json!({}),
        json!(json!({"stored": true, "id": recorded}).to_string()),
    );
    home.call("s1", "mcp__user-memory__record", json!({}), read_reply(&[]));
    home.call(
        "s1",
        "mcp__user-memory__record",
        json!({"title": "Refused"}),
        json!([{"type": "text", "text": json!({"stored": false, "reason": "similar memories exist", "id": note}).to_string()}]),
    );
    home.call(
        "s1",
        "mcp__user-memory__search",
        json!({"topic": "parser"}),
        read_reply(&[&note]),
    );
    assert_eq!(home.rows(), [], "written only at the next prompt");
    home.prompt("s1");
    let mut expected = vec![
        row(&note, false, false, true, Some(1)),
        row(&read, true, false, false, None),
        row(&recorded, false, true, false, None),
        row(&codex_read, true, false, false, None),
    ];
    expected.sort();
    assert_eq!(home.rows(), expected);
    let written = home.written();
    // The store's times have whole seconds.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    home.prompt("s1");
    assert_eq!(home.written(), written, "nothing new, nothing sent");
    Connection::open(home.state("s1"))
        .unwrap()
        .execute("UPDATE worked SET epoch = epoch + 1, at = at + 5", [])
        .unwrap();
    home.prompt("s1");
    assert_eq!(home.written(), written, "an epoch moved is not a use");
}

// A startup sends what other sessions of the store used after their last
// prompt, under their own origins.
#[test]
fn a_startup_sends_what_other_sessions_left_unsent() {
    let home = Home::new();
    let read = home.record("Parser tables", "Keep `src/tables.rs` sorted.", "procedure");
    home.start("s1", "startup");
    home.call(
        "s1",
        "mcp__user-memory__read",
        json!({}),
        read_reply(&[&read]),
    );
    assert_eq!(home.rows(), []);
    home.start("s2", "startup");
    assert_eq!(home.rows(), [row(&read, true, false, false, None)]);
}

// A session that runs across an upgrade has the state file of the release
// before use (layout 2: no use tables, no origin kept). The new binary's
// tool hook upgrades it in place and notes the read with the session's
// origin, so a later startup's sweep sends it.
#[test]
fn a_startup_sends_the_use_of_a_session_that_ran_across_an_upgrade() {
    let home = Home::new();
    let read = home.record("Parser tables", "Keep `src/tables.rs` sorted.", "procedure");
    home.start("s1", "startup");
    let older = Connection::open(home.state("s1")).unwrap();
    older
        .execute_batch(
            "DROP TABLE worked; DROP TABLE pushed; DROP TABLE meta; PRAGMA user_version = 2;",
        )
        .unwrap();
    drop(older);
    home.call(
        "s1",
        "mcp__user-memory__read",
        json!({}),
        read_reply(&[&read]),
    );
    home.start("s2", "startup");
    assert_eq!(home.rows(), [row(&read, true, false, false, None)]);
}

// A memory the most-used part prints whole counts as shown: the next call
// naming it pushes nothing to that context.
#[test]
fn what_the_most_used_part_prints_whole_is_not_pushed_again() {
    let home = Home::new();
    let note = home.record(
        "Dry run before publishing",
        "Run `cargo publish --dry-run` before `cargo publish`.",
        "procedure",
    );
    home.start("s1", "startup");
    assert!(home.run("s1", "cargo publish").contains(&note[..8]));
    home.prompt("s1");
    // Shown in the project's one kept session: most used.
    let start = home.start("s2", "startup");
    assert!(
        start.contains(MOST_USED) && start.contains(&format!("- {}", &note[..8])),
        "{start}"
    );
    assert_eq!(home.run("s2", "cargo publish"), "");
    // A subagent the part reaches holds it whole too.
    let sub = home.hook(
        "subagent-start",
        &json!({"session_id": "s2", "agent_id": "a1", "agent_type": "general-purpose", "cwd": home.checkout(), "hook_event_name": "SubagentStart"}),
    );
    assert!(sub.contains(&note[..8]), "{sub}");
    let pushed = home.hook(
        "post-tool-use",
        &json!({"session_id": "s2", "agent_id": "a1", "agent_type": "general-purpose", "cwd": home.checkout(), "hook_event_name": "PostToolUse",
                "tool_name": "Bash", "tool_input": {"command": "cargo publish"}, "tool_response": {"stdout": ""}}),
    );
    assert_eq!(pushed, "");
}
