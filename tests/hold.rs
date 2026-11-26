//! Holding a shell command back once (#118), through the real binary: the
//! pre-tool hook denies a command that names a passage's anchor strongly
//! enough, with the passage as the reason, once per agent context and
//! stretch, and runs every other command, and any it cannot judge in time.

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

use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tempfile::{TempDir, tempdir};

/// Opens every push after a call that did not fail.
const HEADER: &str =
    "From user-memory, matching this step (read one for the rest before applying it):";

/// Opens every hold.
const HELD: &str = "Held back once by user-memory to show ";

/// A sandboxed home with its own store, its sessions working in it.
struct Home {
    dir: TempDir,
}

impl Home {
    fn new() -> Self {
        Self {
            dir: tempdir().unwrap(),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn db(&self) -> PathBuf {
        self.path().join("memory.sqlite3")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .args(args)
            .current_dir(self.path())
            .sandboxed(self.path())
            .env("USER_MEMORY_DB", self.db())
            .env("GIT_CEILING_DIRECTORIES", self.path())
            .env(HOOK_ROOM.0, HOOK_ROOM.1);
        command
    }

    /// Records a global memory of `memory_type` stated by the user, or an
    /// agent's observation with `observed`; returns its id.
    fn record(&self, memory_type: &str, title: &str, content: &str, observed: bool) -> String {
        let evidence: &[&str] = if observed {
            &[
                "--source",
                "observed",
                "--evidence",
                "Seen in two sessions of synthetic work",
            ]
        } else {
            &[]
        };
        self.record_with(memory_type, title, content, evidence)
    }

    /// [`Home::record`] with `extra` arguments after the content.
    fn record_with(&self, memory_type: &str, title: &str, content: &str, extra: &[&str]) -> String {
        let mut args = vec![
            "record",
            "--type",
            memory_type,
            "--category",
            "build",
            "--title",
            title,
            "--content",
            content,
            "--scope",
            "global",
        ];
        args.extend(extra);
        let out = self.command(&args).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        let out: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(out["stored"], true, "{out}");
        out["id"].as_str().unwrap().to_owned()
    }

    /// Runs `subcommand` for `client` with `payload` on stdin, with `env`
    /// set too; its stdout.
    fn run(&self, subcommand: &str, client: &str, payload: &Value, env: &[(&str, &str)]) -> String {
        let mut command = self.command(&[subcommand, "--client", client]);
        command.envs(env.iter().copied());
        let mut child = command
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

    /// Starts Claude Code's `session` as a new one.
    fn start(&self, session: &str) {
        self.run(
            "session-start",
            "claude",
            &json!({"session_id": session, "cwd": self.path(), "hook_event_name": "SessionStart", "source": "startup"}),
            &[],
        );
    }

    /// Starts Codex's `session` as a new one.
    fn start_codex(&self, session: &str) {
        self.run(
            "session-start",
            "codex",
            &json!({"session_id": session, "cwd": self.path(), "hook_event_name": "SessionStart", "source": "startup"}),
            &[],
        );
    }

    /// The recall state file of every session started in this home.
    fn recall_files(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut folders = vec![self.path().to_owned()];
        while let Some(folder) = folders.pop() {
            for entry in std::fs::read_dir(&folder).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    folders.push(path);
                } else if path.extension().is_some_and(|e| e == "recall") {
                    found.push(path);
                }
            }
        }
        found
    }

    fn call(&self, session: &str, agent: Option<&str>, command: &str) -> Value {
        let mut payload = json!({"session_id": session, "cwd": self.path(), "tool_name": "Bash",
                                 "tool_input": {"command": command}, "tool_use_id": "t-1"});
        if let Some(agent) = agent {
            payload["agent_id"] = json!(agent);
            payload["agent_type"] = json!("general-purpose");
        }
        payload
    }

    /// The reason Claude Code's pre-tool hook holds `command` back with in
    /// `session` (by the agent context `agent` when given); none when it
    /// runs.
    fn before(&self, session: &str, agent: Option<&str>, command: &str) -> Option<String> {
        self.before_with(session, agent, command, &[])
    }

    fn before_with(
        &self,
        session: &str,
        agent: Option<&str>,
        command: &str,
        env: &[(&str, &str)],
    ) -> Option<String> {
        let mut payload = self.call(session, agent, command);
        payload["hook_event_name"] = json!("PreToolUse");
        let out = self.run("pre-tool-use", "claude", &payload, env);
        if out.is_empty() {
            return None;
        }
        let out: Value = serde_json::from_str(&out).unwrap();
        let decision = &out["hookSpecificOutput"];
        assert_eq!(decision["hookEventName"], "PreToolUse", "{out}");
        assert_eq!(decision["permissionDecision"], "deny", "{out}");
        Some(
            decision["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .to_owned(),
        )
    }

    /// The push after `command` ran in `session`; empty for none.
    fn after(&self, session: &str, agent: Option<&str>, command: &str) -> String {
        let mut payload = self.call(session, agent, command);
        payload["hook_event_name"] = json!("PostToolUse");
        payload["tool_response"] = json!({"stdout": ""});
        let out = self.run("post-tool-use", "claude", &payload, &[]);
        if out.is_empty() {
            return out;
        }
        let out: Value = serde_json::from_str(&out).unwrap();
        out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

/// The id as hooks print it.
fn short(id: &str) -> &str {
    &id[..8]
}

/// The user's rule to dry-run a publish, recorded in `home`; its id.
fn dry_run(home: &Home) -> String {
    home.record(
        "preference",
        "Dry run before publishing",
        "Run `cargo publish --dry-run` before `cargo publish`.",
        false,
    )
}

#[test]
fn a_command_naming_a_rule_is_held_once_with_the_rule_before_it_runs() {
    let home = Home::new();
    let rule = dry_run(&home);
    home.start("s1");
    assert_eq!(
        home.before("s1", None, "cargo publish").as_deref(),
        Some(
            format!(
                "Held back once by user-memory to show the user's rule before this runs (not an error, and not the user's refusal): Run `cargo publish --dry-run` before `cargo publish`. ({}) If the command keeps to it, run the same command again; it will not be held again.",
                short(&rule)
            )
            .as_str()
        )
    );
    assert_eq!(
        home.before("s1", None, "cargo publish"),
        None,
        "the re-run runs"
    );
    assert_eq!(
        home.after("s1", None, "cargo publish"),
        "",
        "nor pushed after it"
    );
    // Codex shows the reason followed by ". Command: <command>".
    home.start_codex("c1");
    let mut payload = home.call("c1", None, "cargo publish");
    payload["hook_event_name"] = json!("PreToolUse");
    let out: Value =
        serde_json::from_str(&home.run("pre-tool-use", "codex", &payload, &[])).unwrap();
    let reason = out["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert_eq!(
        out["hookSpecificOutput"]["permissionDecision"], "deny",
        "{out}"
    );
    assert!(
        reason.ends_with("it will not be held again") && reason.starts_with(HELD),
        "{reason}"
    );
}

#[test]
fn an_agents_note_is_held_as_a_note_to_weigh() {
    let home = Home::new();
    let note = home.record(
        "lesson",
        "Lock files in a chained rm",
        "Never delete another session's lock file in `rm -rf .locks/` chained after another command.",
        true,
    );
    home.start("s1");
    assert_eq!(
        home.before("s1", None, "cargo build && rm -rf .locks/").as_deref(),
        Some(
            format!(
                "Held back once by user-memory to show an agent's note before this runs (not an error, not the user's refusal, and not the user's rule: weigh it): Never delete another session's lock file in `rm -rf .locks/` chained after another command. ({}) If it does not apply here, run the same command again; it will not be held again.",
                short(&note)
            )
            .as_str()
        )
    );
}

#[test]
fn nothing_holds_on_words_a_near_miss_an_unnamed_command_or_a_shared_anchor_alone() {
    let home = Home::new();
    dry_run(&home);
    for n in 0..8 {
        home.record(
            "lesson",
            &format!("Commit check {n}"),
            &format!("Before `git commit`, check item {n} of the list in notes{n}.txt."),
            true,
        );
    }
    home.start("s1");
    for command in [
        "echo cargo publish",
        "publish the cargo crate",
        "cargo package",
        "cargo publish-docs",
        "cargo test",
        "git commit -m wip",
    ] {
        assert_eq!(home.before("s1", None, command), None, "{command}");
    }
}

#[test]
fn a_guess_or_an_observation_about_the_user_never_holds() {
    let home = Home::new();
    home.record(
        "preference",
        "Publishes with a dry run",
        "The user runs `cargo publish --dry-run` before `cargo publish`.",
        true,
    );
    let a = home.record(
        "fact",
        "Release one",
        "Release one went out on a Monday.",
        true,
    );
    let b = home.record(
        "fact",
        "Release two",
        "Release two went out on a Monday.",
        true,
    );
    home.record_with(
        "fact",
        "Releases on Mondays",
        "Releases go out with `cargo publish` on Mondays.",
        &[
            "--source",
            "inferred",
            "--evidence-id",
            &a,
            "--evidence-id",
            &b,
        ],
    );
    home.start("s1");
    assert_eq!(home.before("s1", None, "cargo publish"), None);
}

#[test]
fn seven_matches_hold_five_and_the_other_two_come_after_the_run() {
    let home = Home::new();
    let tools = [
        "quill", "ember", "fjord", "lumen", "basil", "cobalt", "tundra",
    ];
    let ids: Vec<String> = tools
        .iter()
        .map(|tool| {
            home.record(
                "fact",
                &format!("{tool} sync needs its token"),
                &format!("`{tool} sync` reads ~/.{tool}/token and stops when it is missing."),
                true,
            )
        })
        .collect();
    home.start("s1");
    let line = tools
        .iter()
        .map(|tool| format!("{tool} sync"))
        .collect::<Vec<_>>()
        .join(" && ");
    let reason = home.before("s1", None, &line).unwrap();
    let held: Vec<&String> = ids.iter().filter(|id| reason.contains(short(id))).collect();
    assert_eq!(held.len(), 5, "{reason}");
    assert!(
        reason.contains("An agent's note is not the user's rule: weigh it."),
        "{reason}"
    );
    assert_eq!(home.before("s1", None, &line), None, "the re-run runs");
    let after = home.after("s1", None, &line);
    assert!(after.starts_with(HEADER), "{after}");
    let pushed: Vec<&String> = ids.iter().filter(|id| after.contains(short(id))).collect();
    assert_eq!(pushed.len(), 2, "{after}");
    assert!(pushed.iter().all(|id| !held.contains(id)), "{after}");
}

#[test]
fn each_agent_context_is_held_once_per_stretch() {
    let home = Home::new();
    dry_run(&home);
    home.start("s1");
    assert!(home.before("s1", None, "cargo publish").is_some());
    assert_eq!(
        home.before("s1", None, "cargo publish --allow-dirty"),
        None,
        "another command matching only what was held runs"
    );
    assert!(
        home.before("s1", Some("a1"), "cargo publish").is_some(),
        "a subagent is a context of its own"
    );
    assert_eq!(home.before("s1", Some("a1"), "cargo publish"), None);
}

#[test]
fn an_error_missing_state_or_an_overrun_never_holds() {
    let home = Home::new();
    dry_run(&home);
    assert_eq!(
        home.before("s0", None, "cargo publish"),
        None,
        "a session without a start"
    );
    home.start("s1");
    assert_eq!(
        home.before_with(
            "s1",
            None,
            "cargo publish",
            &[("USER_MEMORY_HOOK_TIME_SCALE", "0")]
        ),
        None,
        "past its deadline"
    );
    assert!(
        home.before("s1", None, "cargo publish").is_some(),
        "the overrun marked nothing as held"
    );
    home.start("s2");
    for file in home.recall_files() {
        std::fs::write(file, b"not a database").unwrap();
    }
    assert_eq!(
        home.before("s2", None, "cargo publish"),
        None,
        "a damaged state file"
    );
}

#[test]
fn a_session_whose_state_the_release_before_holds_wrote_holds_and_pushes() {
    let home = Home::new();
    let rule = dry_run(&home);
    let [before, after] = ["quill", "ember"].map(|tool| {
        home.record(
            "fact",
            &format!("{tool} sync needs its token"),
            &format!("`{tool} sync` reads ~/.{tool}/token and stops when it is missing."),
            true,
        )
    });
    home.start("s1");
    assert!(
        home.after("s1", None, "quill sync")
            .contains(short(&before))
    );
    // The state file as the release before holds writes it: layout 1, with
    // no `held` table. The session's next hook runs on this release.
    let files = home.recall_files();
    assert_eq!(files.len(), 1, "{files:?}");
    rusqlite::Connection::open(&files[0])
        .unwrap()
        .execute_batch("DROP TABLE held; PRAGMA user_version = 1;")
        .unwrap();
    let reason = home.before("s1", None, "cargo publish");
    assert!(
        reason.as_deref().is_some_and(|r| r.contains(short(&rule))),
        "{reason:?}"
    );
    assert_eq!(
        home.after("s1", None, "cargo publish"),
        "",
        "held, so shown"
    );
    assert_eq!(
        home.after("s1", None, "quill sync"),
        "",
        "what the session was shown is kept"
    );
    assert!(home.after("s1", None, "ember sync").contains(short(&after)));
}
