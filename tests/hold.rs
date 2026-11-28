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
    process::{Child, Command, Stdio},
    time::Duration,
};
use tempfile::{TempDir, tempdir};

/// Opens every push after a call that did not fail.
const HEADER: &str =
    "From user-memory, matching this step (if one applies, read it for the rest first):";

/// Opens every hold.
const HELD: &str = "Held back once by user-memory to show ";

/// A sandboxed home with its own store, its sessions working in it.
struct Home {
    dir: TempDir,
    /// The tool calls made so far: each new call gets its own id, as a
    /// client gives it, and its post-tool hook the same one.
    calls: std::cell::Cell<u32>,
}

impl Home {
    fn new() -> Self {
        Self {
            dir: tempdir().unwrap(),
            calls: std::cell::Cell::new(0),
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
                                 "tool_input": {"command": command}, "tool_use_id": format!("t-{}", self.calls.get())});
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
        self.calls.set(self.calls.get() + 1);
        let mut payload = self.call(session, agent, command);
        payload["hook_event_name"] = json!("PreToolUse");
        reason(&self.run("pre-tool-use", "claude", &payload, env))
    }

    /// Starts Claude Code's pre-tool hook for `command` in `session`, with
    /// `env` set too, without waiting; [`held`] reads its reason.
    fn spawn_before(&self, session: &str, command: &str, env: &[(&str, &str)]) -> Child {
        let mut payload = self.call(session, None, command);
        payload["hook_event_name"] = json!("PreToolUse");
        let mut child = self
            .command(&["pre-tool-use", "--client", "claude"])
            .envs(env.iter().copied())
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
        child
    }

    /// The path of a file that pauses a hook's refresh of the delivery
    /// files while it exists, after the refresh built its overlay; the hook
    /// writes `<path>.reached` there, even when the file is missing.
    fn pause(&self) -> PathBuf {
        self.path().join("pause")
    }
    /// An edit of `file` in `session` by the agent context `agent`: a new
    /// tool call, and no shell command.
    fn edit(&self, session: &str, agent: Option<&str>, file: &str) {
        self.calls.set(self.calls.get() + 1);
        let mut payload = json!({"session_id": session, "cwd": self.path(), "tool_name": "Edit",
                                 "hook_event_name": "PostToolUse", "tool_use_id": format!("t-{}", self.calls.get()),
                                 "tool_input": {"file_path": self.path().join(file)}, "tool_response": {}});
        if let Some(agent) = agent {
            payload["agent_id"] = json!(agent);
            payload["agent_type"] = json!("general-purpose");
        }
        self.run("post-tool-use", "claude", &payload, &[]);
    }

    /// Makes the home a checkout of a project, so its sessions' use counts
    /// for that project in `memory_use`.
    fn with_project(&self) {
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["remote", "add", "origin", "git@github.com:example/app.git"],
        ] {
            let out = Command::new("git")
                .arg("-C")
                .arg(self.path())
                .args(args)
                .sandboxed(self.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        }
    }

    /// A prompt in `session`, which sends its use to the store.
    fn prompt(&self, session: &str) {
        self.run(
            "prompt-submit",
            "claude",
            &json!({"session_id": session, "cwd": self.path(), "hook_event_name": "UserPromptSubmit", "prompt": "Go on."}),
            &[],
        );
    }

    /// Puts every use row in the second of the store's latest change, as
    /// when the sessions and the change all fall in one second: only the
    /// order the store wrote them in tells them apart then (#240).
    fn one_second(&self) {
        rusqlite::Connection::open(self.db())
            .unwrap()
            .execute(
                "UPDATE memory_use SET at = (SELECT max(max(updated_at, coalesce(last_confirmed_at, updated_at))) FROM memories)",
                [],
            )
            .unwrap();
    }

    /// The store's hold outcomes of `memory`, one per session: held and
    /// overridden, by origin.
    fn outcomes(&self, memory: &str) -> Vec<(String, Option<bool>, Option<bool>)> {
        let conn = rusqlite::Connection::open(self.db()).unwrap();
        let mut statement = conn
            .prepare(
                "SELECT origin, held, overridden FROM memory_use WHERE memory = ?1 ORDER BY origin",
            )
            .unwrap();
        statement
            .query_map([memory], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// A session `session` that reads `other` and is held back on
    /// `command`, which the agent then runs again unchanged, overriding the
    /// hold; its use sent.
    fn overridden(&self, session: &str, other: &str, command: &str) {
        self.start(session);
        self.calls.set(self.calls.get() + 1);
        let reply = json!({"memories": [{"id": other}]}).to_string();
        self.run(
            "post-tool-use",
            "claude",
            &json!({"session_id": session, "cwd": self.path(), "hook_event_name": "PostToolUse",
                    "tool_name": "mcp__user-memory__read", "tool_use_id": format!("t-{}", self.calls.get()),
                    "tool_input": {}, "tool_response": [{"type": "text", "text": reply}]}),
            &[],
        );
        assert!(
            self.before(session, None, command).is_some(),
            "{session}: held"
        );
        assert!(
            self.before(session, None, command).is_none(),
            "{session}: re-run"
        );
        self.after(session, None, command);
        self.prompt(session);
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

/// The reason in a pre-tool hook's output `out`; none when it is empty.
fn reason(out: &str) -> Option<String> {
    if out.is_empty() {
        return None;
    }
    let out: Value = serde_json::from_str(out).unwrap();
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

/// The reason the pre-tool hook `child` held its command back with, once it
/// exits; none when it ran.
fn held(child: Child) -> Option<String> {
    let out = time::finish(child, "the pre-tool hook");
    assert!(out.status.success(), "{out:?}");
    reason(&String::from_utf8(out.stdout).unwrap())
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
    // The working set keeps it as held (#118).
    let held: Vec<(String, bool)> = home
        .recall_files()
        .iter()
        .flat_map(|file| {
            let conn = rusqlite::Connection::open(file).unwrap();
            let mut statement = conn
                .prepare("SELECT memory, held FROM worked WHERE agent = 'main'")
                .unwrap();
            statement
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(held, [(rule.clone(), true)]);
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

#[test]
fn a_rule_on_a_script_holds_however_the_script_is_run() {
    let home = Home::new();
    let rule = home.record(
        "preference",
        "Dry run before releasing",
        "Run `tools/release.sh --dry-run` before `tools/release.sh`.",
        false,
    );
    for (n, command) in [
        "tools/release.sh",
        "./tools/release.sh",
        "bash tools/release.sh",
        "sh -e ./tools/release.sh",
    ]
    .into_iter()
    .enumerate()
    {
        let session = format!("s{n}");
        home.start(&session);
        let reason = home.before(&session, None, command);
        assert!(
            reason.as_deref().is_some_and(|r| r.contains(short(&rule))),
            "{command}: {reason:?}"
        );
    }
}

/// Longer than the tool hook's 100 ms limit at [`HOOK_ROOM`]'s scale (1 s)
/// and well within the hold's 250 ms (2.5 s): a pause or wait that ends a
/// hook working on the tool hook's limit and leaves the hold time to finish.
const PAST_TOOL_LIMIT: Duration = Duration::from_millis(1500);

/// Longer than the hold's 250 ms limit at [`HOOK_ROOM`]'s scale (2.5 s): a
/// pause that leaves nothing of it to whatever comes after it.
const PAST_HOLD_LIMIT: Duration = Duration::from_millis(3000);

/// Records a change after the start, so the next tool hook's refresh folds
/// it into the delivery files.
fn change(home: &Home) {
    home.record(
        "fact",
        "Release notes",
        "The release notes live in `notes/releases.md`.",
        true,
    );
}

#[test]
fn a_hold_is_decided_before_the_stores_changes_are_folded_in() {
    let home = Home::new();
    let rule = dry_run(&home);
    home.start("s1");
    change(&home);
    let pause = home.pause();
    std::fs::write(&pause, "").unwrap();
    let hook = home.spawn_before(
        "s1",
        "cargo publish",
        &[("USER_MEMORY_HOLD_REFRESH", pause.to_str().unwrap())],
    );
    let reached = home.path().join("pause.reached");
    time::until("the hook's refresh pauses", || reached.exists());
    std::thread::sleep(PAST_HOLD_LIMIT);
    std::fs::remove_file(&pause).unwrap();
    let reason = held(hook);
    assert!(
        reason.as_deref().is_some_and(|r| r.contains(short(&rule))),
        "a slow fold kept the hold from coming: {reason:?}"
    );
}

#[test]
fn a_hold_still_folds_the_stores_changes_in_after_it() {
    let home = Home::new();
    dry_run(&home);
    home.start("s1");
    change(&home);
    assert!(home.before("s1", None, "cargo publish").is_some());
    // The pause file is missing, so the next hook's refresh doesn't stop,
    // yet it marks when it built an overlay: one change was left to fold.
    let pause = home.pause();
    let probe = home.spawn_before(
        "s1",
        "ls",
        &[("USER_MEMORY_HOLD_REFRESH", pause.to_str().unwrap())],
    );
    assert_eq!(held(probe), None);
    assert!(
        !home.path().join("pause.reached").exists(),
        "the hook that held left the change unfolded"
    );
}

#[test]
fn a_hold_waits_for_the_session_state_past_the_tool_hooks_limit() {
    let home = Home::new();
    let rule = dry_run(&home);
    home.start("s1");
    let files = home.recall_files();
    assert_eq!(files.len(), 1, "{files:?}");
    // Another of the session's hooks writing its state, as parallel tool
    // calls do.
    let other = rusqlite::Connection::open(&files[0]).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let hook = home.spawn_before("s1", "cargo publish", &[]);
    std::thread::sleep(PAST_TOOL_LIMIT);
    other.execute_batch("COMMIT").unwrap();
    let reason = held(hook);
    assert!(
        reason.as_deref().is_some_and(|r| r.contains(short(&rule))),
        "{reason:?}"
    );
}

#[test]
fn a_rule_recorded_through_the_cli_holds_the_very_next_command() {
    let home = Home::new();
    home.start("s1");
    let rule = dry_run(&home);
    let reason = home.before("s1", None, "cargo publish");
    assert!(
        reason.as_deref().is_some_and(|r| r.contains(short(&rule))),
        "{reason:?}"
    );
}

#[test]
fn a_slow_fold_still_holds_the_very_next_command() {
    let home = Home::new();
    home.start("s1");
    let rule = dry_run(&home);
    let pause = home.pause();
    std::fs::write(&pause, "").unwrap();
    let hook = home.spawn_before(
        "s1",
        "cargo publish",
        &[("USER_MEMORY_HOLD_REFRESH", pause.to_str().unwrap())],
    );
    time::until("the hook's refresh pauses", || {
        home.path().join("pause.reached").exists()
    });
    std::thread::sleep(PAST_TOOL_LIMIT);
    std::fs::remove_file(&pause).unwrap();
    let reason = held(hook);
    assert!(
        reason.as_deref().is_some_and(|r| r.contains(short(&rule))),
        "a fold past the tool hook's limit and within the hold's: {reason:?}"
    );
}

#[test]
fn a_rule_whose_fold_misses_the_hook_holds_the_command_after() {
    let home = Home::new();
    home.start("s1");
    let rule = dry_run(&home);
    let pause = home.pause();
    std::fs::write(&pause, "").unwrap();
    let hook = home.spawn_before(
        "s1",
        "cargo publish",
        &[("USER_MEMORY_HOLD_REFRESH", pause.to_str().unwrap())],
    );
    time::until("the hook's refresh pauses", || {
        home.path().join("pause.reached").exists()
    });
    std::thread::sleep(PAST_HOLD_LIMIT);
    std::fs::remove_file(&pause).unwrap();
    assert_eq!(held(hook), None, "the fold came too late for this hook");
    let reason = home.before("s1", None, "cargo publish");
    assert!(
        reason.as_deref().is_some_and(|r| r.contains(short(&rule))),
        "the next one holds it: {reason:?}"
    );
}

/// The agents' note about lock files, recorded in `home`; its id.
fn locks_note(home: &Home) -> String {
    home.record(
        "lesson",
        "Lock files in a chained rm",
        "Never delete another session's lock file in `rm -rf .locks/` chained after another command.",
        true,
    )
}

const CHAINED_RM: &str = "cargo build && rm -rf .locks/";

/// A memory each test session reads, so its use counts: the store's
/// most-used part then weighs what the holds sent beside it.
fn docs_fact(home: &Home) -> String {
    home.record("fact", "Docs site", "The docs build with mdBook.", false)
}

// An agent's note whose hold agents ran past, unchanged, in three
// sessions in a row holds no more in the fourth: it is pushed after the
// command instead. The user's rule, run past as often, keeps holding
// (#118).
#[test]
fn a_note_agents_run_past_in_three_sessions_stops_holding_and_comes_after() {
    let home = Home::new();
    home.with_project();
    let note = locks_note(&home);
    let rule = dry_run(&home);
    let other = docs_fact(&home);
    for session in ["s1", "s2", "s3"] {
        home.overridden(session, &other, CHAINED_RM);
        home.overridden(session, &other, "cargo publish");
    }
    let sessions = |id: &str| {
        home.outcomes(id)
            .into_iter()
            .map(|(_, held, overridden)| (held, overridden))
            .collect::<Vec<_>>()
    };
    assert_eq!(sessions(&note), [(Some(true), Some(true)); 3]);
    home.start("s4");
    assert_eq!(home.before("s4", None, CHAINED_RM), None, "stopped");
    assert!(
        home.after("s4", None, CHAINED_RM).contains(short(&note)),
        "pushed after instead"
    );
    assert!(
        home.before("s4", None, "cargo publish").is_some(),
        "the user's rule keeps holding"
    );
    let _ = rule;
    // A note an agent observes again holds again: the count starts at its
    // confirmation, though the sessions before it wrote in its second.
    home.record_with(
        "lesson",
        "Lock files in a chained rm",
        "Never delete another session's lock file in `rm -rf .locks/` chained after another command.",
        &[
            "--source",
            "observed",
            "--evidence",
            "Seen again in a third session",
            "--confirms",
            &note,
            "--origin",
            "claude:session/other",
        ],
    );
    home.one_second();
    home.start("s5");
    assert!(
        home.before("s5", None, CHAINED_RM).is_some(),
        "confirmed, so held"
    );
}

// A hold counts as overridden only when the context's very next tool
// call runs the held command again: an edit first is heeding it, even with
// the same command after. A session whose hold was overridden in one
// context and then heeded in another counts as heeded, its latest, so the
// count starts again and the note holds in the next session.
#[test]
fn a_hold_heeded_before_the_same_command_starts_the_count_again() {
    let home = Home::new();
    home.with_project();
    let note = locks_note(&home);
    let other = docs_fact(&home);
    home.overridden("s1", &other, CHAINED_RM);
    home.overridden("s2", &other, CHAINED_RM);
    home.overridden("s3", &other, CHAINED_RM);
    home.start("s4");
    assert_eq!(home.before("s4", None, CHAINED_RM), None, "stopped");
    // A new title is wording: the note stays stopped, though its sessions
    // wrote a second before it (#240).
    rusqlite::Connection::open(home.db())
        .unwrap()
        .execute("UPDATE memory_use SET at = datetime(at, '-1 second')", [])
        .unwrap();
    let out = home
        .command(&[
            "update",
            &note,
            "--title",
            "Lock files in chained rm commands",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    home.start("s4t");
    assert_eq!(
        home.before("s4t", None, CHAINED_RM),
        None,
        "retitled, still stopped"
    );
    // A changed note holds again: its count starts at its change, though
    // the sessions before it wrote in its second.
    let out = home
        .command(&[
            "update",
            &note,
            "--context",
            "Seen when two sessions share a checkout.",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    home.one_second();
    home.start("s5");
    assert!(
        home.before("s5", None, CHAINED_RM).is_some(),
        "changed, so held"
    );
    home.edit("s5", None, "notes.md");
    assert!(home.before("s5", None, CHAINED_RM).is_none(), "the re-run");
    home.after("s5", None, CHAINED_RM);
    home.prompt("s5");
    // Overridden in the main context, then heeded in a subagent's.
    home.start("s6");
    assert!(home.before("s6", None, CHAINED_RM).is_some());
    assert!(home.before("s6", None, CHAINED_RM).is_none());
    home.after("s6", None, CHAINED_RM);
    home.prompt("s6");
    assert!(home.before("s6", Some("a1"), CHAINED_RM).is_some());
    home.edit("s6", Some("a1"), "notes.md");
    home.after("s6", Some("a1"), CHAINED_RM);
    home.prompt("s6");
    let outcomes: Vec<(String, Option<bool>)> = home
        .outcomes(&note)
        .into_iter()
        .map(|(origin, _, overridden)| (origin, overridden))
        .collect();
    assert_eq!(
        outcomes[3..],
        [
            ("claude:session/s5".to_owned(), Some(false)),
            ("claude:session/s6".to_owned(), Some(false)),
        ]
    );
    home.one_second();
    home.start("s7");
    assert!(
        home.before("s7", None, CHAINED_RM).is_some(),
        "the count started again"
    );
}

// An export carries no use: a store it is imported into starts with
// none, and no reset. An import that brings a later confirmation of a
// stopped note confirms it here too, so it holds again (#240).
#[test]
fn an_import_carries_no_use_and_its_confirmation_restarts_the_count() {
    let home = Home::new();
    home.with_project();
    let note = locks_note(&home);
    let other = docs_fact(&home);
    for session in ["s1", "s2", "s3"] {
        home.overridden(session, &other, CHAINED_RM);
    }
    home.start("s4");
    assert_eq!(home.before("s4", None, CHAINED_RM), None, "stopped");
    let file = home.path().join("export.json");
    let out = home
        .command(&["export", file.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let fresh = Home::new();
    let out = fresh
        .command(&["import", file.to_str().unwrap(), "--apply"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let count = |home: &Home, table: &str| -> i64 {
        rusqlite::Connection::open(home.db())
            .unwrap()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(count(&fresh, "memory_use"), 0);
    assert_eq!(count(&fresh, "hold_reset"), 0);
    // The file says the note was confirmed later than this store knows.
    let mut exported: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    for memory in exported["memories"].as_array_mut().unwrap() {
        if memory["id"] == note.as_str() {
            memory["last_confirmed_at"] = json!("2099-01-01 00:00:00");
        }
    }
    std::fs::write(&file, exported.to_string()).unwrap();
    let out = home
        .command(&["import", file.to_str().unwrap(), "--apply"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    home.one_second();
    home.start("s5");
    assert!(
        home.before("s5", None, CHAINED_RM).is_some(),
        "confirmed by the import, so held"
    );
}

// `review` lists an agent's note that stopped holding, after `recent`,
// with how many sessions in a row overrode it; once the note is
// confirmed it holds again and the group is gone (#238).
#[test]
fn review_lists_a_note_that_stopped_holding_until_it_is_confirmed() {
    let home = Home::new();
    home.with_project();
    let note = locks_note(&home);
    let other = docs_fact(&home);
    let review = || -> Value {
        home.one_second();
        let out = home.command(&["review"]).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let stopped = |review: &Value| -> Option<Value> {
        review["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["kind"] == "stopped")
            .cloned()
    };
    // An override, then a heeded session: the run counts from there, though
    // every session writes in one second.
    home.overridden("s0", &other, CHAINED_RM);
    home.start("h");
    assert!(home.before("h", None, CHAINED_RM).is_some());
    home.edit("h", None, "notes.md");
    home.after("h", None, CHAINED_RM);
    home.prompt("h");
    home.overridden("s1", &other, CHAINED_RM);
    home.overridden("s2", &other, CHAINED_RM);
    assert_eq!(stopped(&review()), None, "two sessions are not enough");
    home.overridden("s3", &other, CHAINED_RM);
    let found = review();
    let group = stopped(&found).unwrap();
    assert_eq!(
        group["reason"],
        "Agents ran past these notes' holds, unchanged, in 3 sessions in a row, so they no longer hold a command back and are shown after it runs. Show them to the user: confirm one to make it hold again, change it, or forget it, only with their yes."
    );
    assert_eq!(group["members"][0]["id"], note.as_str());
    assert_eq!(group["members"][0]["overrides"], 3);
    let kinds: Vec<&str> = found["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["kind"].as_str().unwrap())
        .collect();
    let at = kinds.iter().position(|k| *k == "stopped").unwrap();
    assert_eq!(kinds[..at], ["recent"], "right after recent: {kinds:?}");
    home.record_with(
        "lesson",
        "Lock files in a chained rm",
        "Never delete another session's lock file in `rm -rf .locks/` chained after another command.",
        &[
            "--source",
            "observed",
            "--evidence",
            "Seen again in a third session",
            "--confirms",
            &note,
            "--origin",
            "claude:session/other",
        ],
    );
    assert_eq!(stopped(&review()), None, "confirmed, so it holds again");
    // The count starts after the confirmation: the session that wrote just
    // before it is not one of the run.
    home.overridden("s4", &other, CHAINED_RM);
    home.overridden("s5", &other, CHAINED_RM);
    assert_eq!(
        stopped(&review()),
        None,
        "two overrides since the confirmation"
    );
}
