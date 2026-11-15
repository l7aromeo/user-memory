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
    process::{Child, Command, Output, Stdio},
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
        self.cli_with(args, &[])
    }
    /// The command line's output with `env` added, such as another store.
    fn cli_with(&self, args: &[&str], env: &[(&str, &str)]) -> Value {
        let out = self.command(args, env).output().unwrap();
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
        let out = time::finish(self.spawn_hook(hook, session, cwd, env), "the hook");
        assert!(out.status.success(), "{out:?}");
        out
    }
    /// Starts a hook for a session working in `cwd`, without waiting for it.
    fn spawn_hook(&self, hook: &str, session: &str, cwd: &Path, env: &[(&str, &str)]) -> Child {
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
        child
    }
    /// What the prompt hook adds to the session's context, or "" for nothing.
    fn notice(&self, session: &str) -> String {
        self.notice_in(session, self.path())
    }
    /// The prompt hook's notice with `env` added, such as another store.
    fn notice_with(&self, session: &str, env: &[(&str, &str)]) -> String {
        let out = self.hook_in("UserPromptSubmit", session, self.path(), env);
        if out.stdout.is_empty() {
            return String::new();
        }
        let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
        reply["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .to_owned()
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
    /// The sessions folder in the state folder, as user-memory resolves it
    /// in this home.
    fn sessions(&self) -> PathBuf {
        sandbox::folder(self.path(), "state").join("sessions")
    }
    /// The session's one snapshot, of this home's store: its file name
    /// starts with the client and session id, and ends with the store's key.
    fn snapshot(&self, session: &str) -> PathBuf {
        let prefix = format!("claude-{session}-");
        let found: Vec<PathBuf> = std::fs::read_dir(self.sessions())
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| {
                        let name = path.file_name().unwrap().to_string_lossy();
                        name.starts_with(&prefix) && name.ends_with(".json")
                    })
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(found.len(), 1, "snapshots of {session}: {found:?}");
        found[0].clone()
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

// A snapshot save that does not finish in time never holds the prompt past
// the hook's limit, and never records a change the session was not told:
// the snapshot stays as it was, and the change comes with the next prompt.
#[test]
fn a_save_cut_off_by_the_limit_leaves_the_change_for_the_next_prompt() {
    let home = Home::new();
    home.record("Prefer Rust for tools");
    home.hook("SessionStart", "s", &[]);
    let added = home.record("Ask before pushing");
    let snapshot = home.snapshot("s");
    let told = std::fs::read(&snapshot).unwrap();
    let hold = home.path().join("hold");
    std::fs::write(&hold, "").unwrap();
    let out = home.hook(
        "UserPromptSubmit",
        "s",
        &[
            ("USER_MEMORY_HOLD_SNAPSHOT", hold.to_str().unwrap()),
            HOOK_LIMITS,
        ],
    );
    assert!(
        home.path().join("hold.reached").exists(),
        "the save was under way"
    );
    assert!(out.stdout.is_empty(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("the prompt hook stopped at its time limit"),
        "{out:?}"
    );
    assert_eq!(std::fs::read(&snapshot).unwrap(), told);
    std::fs::remove_file(&hold).unwrap();
    assert_eq!(
        home.notice("s").lines().nth(1),
        Some(format!("+ {} Ask before pushing", id8(&added)).as_str())
    );
}

// A save and its hook's limit race, and whichever comes first decides: a
// save that replaced the snapshot before the hook, past its limit, abandoned
// it is told, though the notice then ends past the limit by the rename's
// time. The order is set by holding each side, not by timing.
#[test]
fn a_save_that_lands_before_the_hook_abandons_it_is_told() {
    let home = Home::new();
    home.record("Prefer Rust for tools");
    home.hook("SessionStart", "s", &[]);
    let added = home.record("Ask before pushing");
    let snapshot = home.snapshot("s");
    let told = std::fs::read(&snapshot).unwrap();
    let save = home.path().join("hold-save");
    let abandon = home.path().join("hold-abandon");
    for hold in [&save, &abandon] {
        std::fs::write(hold, "").unwrap();
    }
    let hook = home.spawn_hook(
        "UserPromptSubmit",
        "s",
        home.path(),
        &[
            ("USER_MEMORY_HOLD_SNAPSHOT", save.to_str().unwrap()),
            ("USER_MEMORY_HOLD_ABANDON", abandon.to_str().unwrap()),
            HOOK_LIMITS,
        ],
    );
    // Past its limit, about to abandon the save, which has not landed.
    time::until("the hook reaching its limit", || {
        home.path().join("hold-abandon.reached").exists()
    });
    assert!(home.path().join("hold-save.reached").exists());
    assert_eq!(std::fs::read(&snapshot).unwrap(), told);
    std::fs::remove_file(&save).unwrap();
    time::until("the save landing", || {
        std::fs::read(&snapshot).unwrap() != told
    });
    std::fs::remove_file(&abandon).unwrap();
    let out = time::finish(hook, "the hook");
    assert!(out.status.success(), "{out:?}");
    assert!(!out.stdout.is_empty(), "the landed change went untold");
    let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        reply["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .lines()
            .nth(1),
        Some(format!("+ {} Ask before pushing", id8(&added)).as_str())
    );
    assert_eq!(home.notice("s"), "", "told once");
}

// Session start's first snapshot has its share of the limit: a save that
// does not finish in it never holds the index back, and the session then
// starts as one without a snapshot.
#[test]
fn a_first_snapshot_cut_off_by_its_share_still_shows_the_index() {
    let home = Home::new();
    home.record("Prefer Rust for tools");
    let hold = home.path().join("hold");
    std::fs::write(&hold, "").unwrap();
    let out = home.hook(
        "SessionStart",
        "s",
        &[
            ("USER_MEMORY_HOLD_SNAPSHOT", hold.to_str().unwrap()),
            HOOK_LIMITS,
        ],
    );
    assert!(
        home.path().join("hold.reached").exists(),
        "the save was under way"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Prefer Rust for tools"),
        "{out:?}"
    );
    std::fs::remove_file(&hold).unwrap();
    assert_eq!(home.notice("s"), "", "a session without a snapshot");
    assert!(home.snapshot("s").is_file());
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

// A session's snapshot belongs to the store it was taken from. A build or a
// setup using another store, under the same client and session id, keeps a
// snapshot of its own: neither store's prompts hear of the other's memories.
#[test]
fn a_snapshot_belongs_to_the_store_it_was_taken_from() {
    let home = Home::new();
    let other_db = home.path().join("other.sqlite3");
    let other = [("USER_MEMORY_DB", other_db.to_str().unwrap())];
    let record_in = |env: &[(&str, &str)], title: &str| {
        let out = home.cli_with(
            &[
                "record",
                "--type",
                "preference",
                "--category",
                "workflow",
                "--title",
                title,
                "--content",
                title,
                "--scope",
                "global",
            ],
            env,
        );
        assert_eq!(out["stored"], true, "{out}");
        out["id"].as_str().unwrap().to_owned()
    };
    record_in(&[], "Prefer tabs for indentation");
    home.hook("SessionStart", "s", &[]);
    record_in(&other, "Prefer spaces for indentation");
    home.hook("SessionStart", "s", &other);
    assert_eq!(
        home.notice("s"),
        "",
        "the other store's start changed nothing here"
    );
    assert_eq!(home.notice_with("s", &other), "", "nor this store's there");
    let added = record_in(&other, "Wrap lines at 100");
    assert_eq!(home.notice("s"), "");
    assert_eq!(
        home.notice_with("s", &other).lines().nth(1),
        Some(format!("+ {} Wrap lines at 100", id8(&added)).as_str())
    );
}

// A snapshot saved before snapshots named their store cannot tell which
// store it saw, so it is never compared: the first prompt after the upgrade
// is quiet, as a session's first prompt without a snapshot is, and changes
// after it are told as before.
#[test]
fn a_snapshot_from_before_the_upgrade_gives_no_notice() {
    let home = Home::new();
    home.record("Prefer Rust for tools");
    std::fs::create_dir_all(home.sessions()).unwrap();
    // As 0.9.0 wrote it: one memory this store never held.
    std::fs::write(
        home.sessions().join("claude-old.json"),
        json!({"projects":[],"stamps":{"0b9d3a11-0000-4000-8000-000000000000":{"updated_at":"2026-10-01 10:00:00","status":"confirmed","title":"Use tabs"}}}).to_string(),
    )
    .unwrap();
    assert_eq!(home.notice("old"), "");
    let added = home.record("Ask before pushing");
    assert_eq!(
        home.notice("old").lines().nth(1),
        Some(format!("+ {} Ask before pushing", id8(&added)).as_str())
    );
}

// An update that changes nothing an agent acts on, such as one that only
// adds this conversation, still counts as a confirmation but is no change
// to tell; a content change under the same title still is one.
#[test]
fn only_a_change_an_agent_acts_on_is_told() {
    let home = Home::new();
    let id = home.cli(&[
        "record",
        "--type",
        "preference",
        "--category",
        "style",
        "--title",
        "Keep pull requests small",
        "--content",
        "Keep pull requests under 400 changed lines",
        "--scope",
        "global",
        "--origin",
        "chat/one",
    ])["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Recorded a while ago, so a write now moves its update time, which
    // counts in seconds.
    rusqlite::Connection::open(home.path().join("memory.sqlite3"))
        .unwrap()
        .execute(
            "UPDATE memories SET updated_at='2026-01-01 00:00:00' WHERE id=?1",
            [&id],
        )
        .unwrap();
    home.hook("SessionStart", "s", &[]);
    home.cli(&["update", &id, "--origin", "chat/two"]);
    assert_eq!(home.notice("s"), "", "a confirmation is no change");
    let read = home.cli(&["read", &id]);
    assert_eq!(read["memories"][0]["confirmations"], 2, "{read}");
    home.cli(&[
        "update",
        &id,
        "--content",
        "Keep pull requests under 300 changed lines",
    ]);
    assert_eq!(
        home.notice("s"),
        [
            "Memory changed since this session last looked (+ new, ~ changed, - no longer active; read before applying):".to_owned(),
            format!("~ {} Keep pull requests small", id8(&id)),
        ]
        .join("\n")
    );
}

// A snapshot saved before stamps held digests, with each memory's last
// update in their place, is compared by status and title on the first
// prompt after the upgrade, so it does not list every memory as changed.
#[test]
fn a_snapshot_from_before_digests_lists_no_memory_as_changed() {
    let home = Home::new();
    for title in ["Prefer Rust for tools", "Ask before pushing", "Use tabs"] {
        home.record(title);
    }
    home.hook("SessionStart", "s", &[]);
    let path = home.snapshot("s");
    let mut snapshot: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let fields = snapshot.as_object_mut().unwrap();
    assert!(fields.remove("changes").is_some(), "{fields:?}");
    for stamp in fields["stamps"].as_object_mut().unwrap().values_mut() {
        let stamp = stamp.as_object_mut().unwrap();
        stamp.remove("digest").unwrap();
        stamp.insert("updated_at".into(), json!("2026-10-01 10:00:00"));
    }
    std::fs::write(&path, snapshot.to_string()).unwrap();
    assert_eq!(home.notice("s"), "");
    let added = home.record("Write release notes");
    assert_eq!(
        home.notice("s").lines().nth(1),
        Some(format!("+ {} Write release notes", id8(&added)).as_str())
    );
}

// Before each prompt the hook reads only the memories changed since the
// session last looked, however many the store holds; a session whose
// projects changed, or a store with a new id, is read whole.
#[test]
fn a_prompt_reads_only_what_changed_since_the_session_looked() {
    let home = Home::new();
    for i in 0..50 {
        home.record(&format!("Memory number {i}"));
    }
    home.hook("SessionStart", "s", &[]);
    let read = |home: &Home| {
        let out = home.hook("UserPromptSubmit", "s", &[]);
        String::from_utf8(out.stderr)
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("user-memory: read ").map(str::to_owned))
            .unwrap_or_default()
    };
    assert_eq!(read(&home), "0 stamps (changed)");
    home.record("Ask before pushing");
    assert_eq!(read(&home), "1 stamps (changed)");
    // A store that took a new id, as after a restore, is read whole.
    rusqlite::Connection::open(home.path().join("memory.sqlite3"))
        .unwrap()
        .execute(
            "UPDATE store_changes SET store_id='0123456789abcdef0123456789abcdef'",
            [],
        )
        .unwrap();
    assert_eq!(read(&home), "51 stamps (every)");
    assert_eq!(read(&home), "0 stamps (changed)");
}

// The hook's look at the session's links is a read: while another process
// holds the store's write lock, a prompt still hears what changed rather
// than wait behind it until its limit.
#[test]
fn a_prompt_during_another_write_still_hears_what_changed() {
    let home = Home::new();
    home.record("Prefer Rust for tools");
    home.hook("SessionStart", "s", &[]);
    let added = home.record("Ask before pushing");
    let writer = rusqlite::Connection::open(home.path().join("memory.sqlite3")).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let notice = home.notice("s");
    writer.execute_batch("ROLLBACK").unwrap();
    assert!(
        notice.contains(&format!("+ {} Ask before pushing", id8(&added))),
        "{notice:?}"
    );
}
