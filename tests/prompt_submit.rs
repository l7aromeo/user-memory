//! The prompt hook tells a running session what changed in memory since it
//! last looked, and never delays or breaks the prompt.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
#[path = "support/serve.rs"]
mod serve;
#[path = "support/time.rs"]
mod time;
use time::{HOOK_LIMITS, HOOK_ROOM};

use serde_json::{Value, json};
use std::{
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
use tempfile::{TempDir, tempdir};

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
    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .args(args)
            .current_dir(self.path())
            .sandboxed(self.path())
            .env("USER_MEMORY_DB", self.path().join("memory.sqlite3"))
            .env("GIT_CEILING_DIRECTORIES", self.path())
            .env_remove("USER_MEMORY_URL")
            .env_remove("USER_MEMORY_TOKEN_FILE")
            .env(HOOK_ROOM.0, HOOK_ROOM.1);
        command.envs(env.iter().copied());
        command
    }
    fn cli(&self, args: &[&str]) -> Value {
        let out = self.command(args, &[]).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn record(&self, title: &str) -> String {
        let out = self.cli(&[
            "record",
            "--type",
            "preference",
            "--category",
            title,
            "--title",
            title,
            "--content",
            title,
            "--scope",
            "global",
        ]);
        assert_eq!(out["stored"], true, "{out}");
        out["id"].as_str().unwrap().to_owned()
    }
    /// Runs a hook with the session's payload on stdin.
    fn hook(&self, hook: &str, session: &str, env: &[(&str, &str)]) -> Output {
        self.hook_in(hook, session, self.path(), env)
    }
    /// Runs a hook for a session whose working folder is now `cwd`.
    fn hook_in(&self, hook: &str, session: &str, cwd: &Path, env: &[(&str, &str)]) -> Output {
        let payload = json!({"session_id": session, "cwd": cwd, "hook_event_name": hook});
        let subcommand = if hook == "SessionStart" {
            "session-start"
        } else {
            "prompt-submit"
        };
        // A client runs its hooks in the session's folder.
        let mut child = self
            .command(&[subcommand, "--client", "claude"], env)
            .current_dir(cwd)
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
        let out = time::finish(child, "the hook");
        assert!(out.status.success(), "{out:?}");
        out
    }
    /// What the prompt hook adds to the session's context, or "" for nothing.
    fn notice(&self, session: &str) -> String {
        self.notice_in(session, self.path())
    }
    /// The prompt hook's notice for a session now working in `cwd`.
    fn notice_in(&self, session: &str, cwd: &Path) -> String {
        let out = self.hook_in("UserPromptSubmit", session, cwd, &[]);
        if out.stdout.is_empty() {
            return String::new();
        }
        let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(
            reply["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
        reply["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    /// Where the session's snapshot is kept: the sessions folder in the
    /// state folder, as user-memory resolves it in this home.
    fn snapshot(&self, session: &str) -> PathBuf {
        sandbox::folder(self.path(), "state")
            .join("sessions")
            .join(format!("claude-{session}.json"))
    }
}

fn id8(id: &str) -> &str {
    &id[..8]
}

#[test]
fn a_running_session_hears_what_changed_since_it_last_looked() {
    let home = Home::new();
    let kept = home.record("Prefer Rust for tools");
    let renamed = home.record("Answer in short paragraphs");
    let forgotten = home.record("Use tabs for indentation");
    home.hook("SessionStart", "s1", &[]);
    let snapshot = home.snapshot("s1");
    assert!(snapshot.is_file());
    // Mode bits are Unix's; on Windows the snapshot is made private by
    // private::create_file, which session/live.rs's tests check there.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&snapshot).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert_eq!(
        home.notice("s1"),
        "",
        "nothing changed since the session started"
    );
    let added = home.record("Ask before pushing");
    home.cli(&[
        "update",
        &renamed,
        "--title",
        "Answer in one short paragraph",
    ]);
    home.cli(&["forget", &forgotten]);
    let notice = home.notice("s1");
    assert_eq!(
        notice,
        [
            "Memory changed since this session last looked (+ new, ~ changed, - no longer active; read before applying):".to_owned(),
            format!("+ {} Ask before pushing", id8(&added)),
            format!("~ {} Answer in one short paragraph", id8(&renamed)),
            format!("- {}", id8(&forgotten)),
        ]
        .join("\n")
    );
    assert!(!notice.contains(id8(&kept)));
    assert_eq!(home.notice("s1"), "", "each change is told once");
    // Another session keeps its own snapshot.
    home.hook("SessionStart", "s2", &[]);
    home.record("Write release notes in plain English");
    assert!(home.notice("s2").contains("+ "));
    assert!(home.notice("s1").contains("Write release notes"));
}

#[test]
fn a_session_without_a_snapshot_gets_one_and_no_notice() {
    let home = Home::new();
    home.record("Prefer Rust for tools");
    assert_eq!(home.notice("late"), "");
    assert!(home.snapshot("late").is_file());
    let added = home.record("Ask before pushing");
    assert_eq!(
        home.notice("late").lines().nth(1),
        Some(format!("+ {} Ask before pushing", id8(&added)).as_str())
    );
}

#[test]
fn an_unreachable_or_silent_server_never_delays_or_breaks_the_prompt() {
    let home = Home::new();
    let token = home.path().join("token");
    std::fs::write(&token, "x".repeat(40)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let silent = format!("http://{}/mcp", listener.local_addr().unwrap());
    for url in ["http://127.0.0.1:9/mcp", silent.as_str()] {
        let env = [
            ("USER_MEMORY_URL", url),
            ("USER_MEMORY_TOKEN_FILE", token.to_str().unwrap()),
            HOOK_LIMITS,
        ];
        home.hook("SessionStart", "remote", &env);
        let out = home.hook("UserPromptSubmit", "remote", &env);
        assert!(out.stdout.is_empty(), "{url}: {out:?}");
        // The silent server never answers: the hook's own limit ends it.
        if url == silent {
            assert!(
                String::from_utf8_lossy(&out.stderr)
                    .contains("the prompt hook stopped at its time limit"),
                "{out:?}"
            );
        }
    }
    drop(listener);
    // A broken store, too.
    std::fs::write(home.path().join("memory.sqlite3"), "not a database").unwrap();
    let out = home.hook("UserPromptSubmit", "s1", &[]);
    assert!(out.stdout.is_empty(), "{out:?}");
}

// A project linked mid-conversation reaches that conversation at its next
// prompt, as new memories, and no other conversation.
#[test]
fn a_new_link_brings_its_projects_memories_to_the_next_prompt() {
    let home = Home::new();
    let fuzz = home.cli(&[
        "record",
        "--type",
        "preference",
        "--category",
        "testing",
        "--title",
        "Run the fuzzer before merging",
        "--content",
        "Run the fuzzer before merging",
        "--scope",
        "project",
        "--project",
        "github.com/acme/compiler",
    ])["id"]
        .as_str()
        .unwrap()
        .to_owned();
    home.hook("SessionStart", "s1", &[]);
    home.hook("SessionStart", "s2", &[]);
    let linked = serve::call(
        home.command(&["serve"], &[]),
        "link",
        json!({"origin":"claude:session/s1","projects":["github.com/acme/compiler"]}),
    );
    assert_eq!(
        linked["added"],
        json!(["github.com/acme/compiler"]),
        "{linked}"
    );
    let notice = home.notice("s1");
    let lines: Vec<&str> = notice.lines().collect();
    assert_eq!(
        lines[0],
        "Projects changed: + github.com/acme/compiler (linked)"
    );
    assert_eq!(
        lines[2],
        format!("+ {} Run the fuzzer before merging", id8(&fuzz))
    );
    assert_eq!(home.notice("s2"), "");
}

// An agent moves between a workspace and its repositories; that alone never
// drops a project the session has seen, so the prompt hook stays quiet.
// Taking a project off the workspace's list still drops it, and its
// memories are marked as leaving with it, not as no longer active.
#[test]
fn a_session_keeps_the_projects_it_has_seen_when_its_folder_changes() {
    let home = Home::new();
    let work = home.path().join("work");
    let mut rules = Vec::new();
    for name in ["a", "b", "c"] {
        let key = format!("github.com/acme/{name}");
        // A repository with its own remote, as a workspace's checkouts are.
        let repo = work.join(name);
        std::fs::create_dir_all(&repo).unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["remote", "add", "origin", &format!("https://{key}.git")],
        ] {
            let out = Command::new("git")
                .args(&args)
                .current_dir(&repo)
                .sandboxed(home.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        }
        let title = format!("Rule for {name}");
        let saved = home.cli(&[
            "record",
            "--type",
            "preference",
            "--category",
            "workflow",
            "--title",
            &title,
            "--content",
            &title,
            "--scope",
            "project",
            "--project",
            &key,
        ]);
        assert_eq!(saved["stored"], true, "{saved}");
        rules.push(saved["id"].as_str().unwrap().to_owned());
    }
    std::fs::write(work.join(".user-memory"), "@a\n@b\n@c\n").unwrap();
    home.hook_in("SessionStart", "s", &work, &[]);
    assert_eq!(home.notice_in("s", &work.join("a")), "");
    assert_eq!(home.notice_in("s", &work), "");
    assert_eq!(home.notice_in("s", &work.join("a")), "");
    // From the workspace, the list no longer gives c, and no other folder
    // the session saw it from does: it leaves.
    std::fs::write(work.join(".user-memory"), "@a\n@b\n").unwrap();
    let notice = home.notice_in("s", &work);
    let lines: Vec<&str> = notice.lines().collect();
    assert_eq!(
        lines[0], "Projects changed: - github.com/acme/c (no longer listed)",
        "{notice}"
    );
    assert!(
        lines.contains(
            &format!("< {} Rule for c (project no longer loaded)", id8(&rules[2])).as_str()
        ),
        "{notice}"
    );
    assert!(
        !notice.contains(&format!("- {}", id8(&rules[2]))),
        "{notice}"
    );
    // a, which the session also saw from its own folder, stays.
    std::fs::write(work.join(".user-memory"), "@b\n").unwrap();
    assert_eq!(home.notice_in("s", &work), "");
}
