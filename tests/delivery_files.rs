//! The delivery index's files on this machine (#175), through the real
//! binary: session start builds its scope's base, the prompt hook builds one
//! its scope lacks, the tool hooks keep its overlay current, and no file
//! keeps the words a removal took out.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
#[path = "support/serve.rs"]
mod serve;
use sandbox::Sandboxed;
#[path = "support/time.rs"]
mod time;
use time::HOOK_ROOM;

use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, SystemTime},
};
use tempfile::{TempDir, tempdir};

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

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .args(args)
            .current_dir(self.path())
            .sandboxed(self.path())
            .env("USER_MEMORY_DB", self.db())
            .env("GIT_CEILING_DIRECTORIES", self.path())
            .env(HOOK_ROOM.0, HOOK_ROOM.1);
        command.envs(env.iter().copied());
        command
    }

    fn cli(&self, args: &[&str]) -> Value {
        let out = self.command(args, &[]).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// Records a global memory whose content holds `anchor` as a code
    /// span, which the index keeps as written; returns its id.
    fn record(&self, title: &str, anchor: &str) -> String {
        self.record_in(None, title, anchor)
    }

    /// As [`Home::record`], for `project` when given.
    fn record_in(&self, project: Option<&str>, title: &str, anchor: &str) -> String {
        let content = format!("Run `{anchor}` before the {title} step.");
        let mut args = vec![
            "record",
            "--type",
            "procedure",
            "--category",
            "build",
            "--title",
            title,
            "--content",
            &content,
        ];
        match project {
            Some(project) => args.extend(["--scope", "project", "--project", project]),
            None => args.extend(["--scope", "global"]),
        }
        let out = self.cli(&args);
        assert_eq!(out["stored"], true, "{out}");
        out["id"].as_str().unwrap().to_owned()
    }

    /// Starts `hook` for `session`, its payload on stdin, without waiting.
    fn spawn(&self, hook: &str, session: &str, env: &[(&str, &str)]) -> Child {
        let (subcommand, payload) = match hook {
            "SessionStart" => (
                "session-start",
                json!({"session_id": session, "cwd": self.path(), "hook_event_name": hook, "source": "startup"}),
            ),
            "UserPromptSubmit" => (
                "prompt-submit",
                json!({"session_id": session, "cwd": self.path(), "hook_event_name": hook, "prompt": "go on"}),
            ),
            "PreToolUse" | "PostToolUse" => (
                if hook == "PreToolUse" {
                    "pre-tool-use"
                } else {
                    "post-tool-use"
                },
                json!({"session_id": session, "cwd": self.path(), "hook_event_name": hook,
                       "tool_name": "Bash", "tool_input": {"command": "ls"}, "tool_response": {"stdout": ""}}),
            ),
            other => panic!("no hook {other}"),
        };
        let mut child = self
            .command(&[subcommand, "--client", "claude"], env)
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

    /// Runs `hook` for `session` to its end.
    fn hook(&self, hook: &str, session: &str, env: &[(&str, &str)]) -> Output {
        let out = time::finish(self.spawn(hook, session, env), hook);
        assert!(out.status.success(), "{out:?}");
        out
    }

    /// Links (or with `unlink`, unlinks) `project` in `session`.
    fn link(&self, session: &str, project: &str, unlink: bool) {
        let linked = serve::call(
            self.command(&["serve"], &[]),
            "link",
            json!({"origin": format!("claude:session/{session}"), "projects": [project], "unlink": unlink}),
        );
        assert!(linked.is_object(), "{linked}");
    }

    fn state(&self) -> PathBuf {
        sandbox::folder(self.path(), "state")
    }

    fn delivery(&self) -> PathBuf {
        self.state().join("delivery")
    }

    /// The delivery folder's files whose names end in `ending`.
    fn files(&self, ending: &str) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(self.delivery()) else {
            return Vec::new();
        };
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with(ending))
            .collect();
        found.sort();
        found
    }

    fn base(&self) -> PathBuf {
        let bases = self.files(".index");
        assert_eq!(bases.len(), 1, "{bases:?}");
        bases[0].clone()
    }

    fn overlay(&self) -> PathBuf {
        self.base().with_extension("overlay")
    }

    /// The session's scope file, beside its snapshot.
    fn scope_file(&self, session: &str) -> PathBuf {
        let sessions = self.state().join("sessions");
        fs::read_dir(&sessions)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                let name = p.file_name().unwrap().to_string_lossy();
                name.starts_with(&format!("claude-{session}-")) && name.ends_with(".scope")
            })
            .unwrap_or_else(|| panic!("no scope file for {session} in {sessions:?}"))
    }
}

/// How the index keeps a term: the lowercase hex of its bytes.
fn hex(term: &str) -> String {
    term.bytes().map(|b| format!("{b:02x}")).collect()
}

/// Whether the file at `path` holds `needle`.
fn holds(path: &Path, needle: &str) -> bool {
    let bytes = fs::read(path).unwrap_or_default();
    bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// Every file under `dir`, recursively.
fn every_file(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(every_file(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// The files under the state folder holding `anchor`'s hex form or its text.
fn holding(home: &Home, anchor: &str) -> Vec<PathBuf> {
    every_file(&home.state())
        .into_iter()
        .filter(|path| holds(path, &hex(anchor)) || holds(path, anchor))
        .collect()
}

fn modified(path: &Path) -> SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

#[test]
fn session_start_builds_its_scope_s_base_and_a_second_session_shares_it() {
    let home = Home::new();
    home.record("deploy", "quokka-deploy");
    home.hook("SessionStart", "s1", &[]);
    let base = home.base();
    assert!(
        holds(&base, &hex("quokka-deploy")),
        "the base holds the anchor"
    );
    assert!(home.overlay().exists(), "an empty overlay naming the base");
    let built = modified(&base);
    std::thread::sleep(Duration::from_millis(20));
    home.hook("SessionStart", "s2", &[]);
    assert_eq!(home.base(), base, "one base for the scope");
    assert_eq!(modified(&base), built, "the second session builds nothing");
}

#[test]
fn a_base_not_written_for_a_day_is_built_again_and_leftovers_go() {
    let home = Home::new();
    home.record("deploy", "quokka-deploy");
    home.hook("SessionStart", "s1", &[]);
    let base = home.base();
    let day_ago = SystemTime::now() - Duration::from_secs(25 * 60 * 60);
    fs::File::options()
        .write(true)
        .open(&base)
        .unwrap()
        .set_modified(day_ago)
        .unwrap();
    // A writer killed while writing left its file behind.
    let leftover = base.with_file_name(format!(
        "{}.overlay.4242.partial",
        base.file_stem().unwrap().to_string_lossy()
    ));
    fs::write(&leftover, hex("quokka-deploy")).unwrap();
    home.hook("SessionStart", "s2", &[]);
    assert!(modified(&home.base()) > day_ago, "built again");
    assert!(!leftover.exists(), "the leftover is deleted");
}

#[test]
fn the_scope_file_is_written_at_every_save_and_read_by_the_tool_hooks() {
    let home = Home::new();
    home.hook("SessionStart", "s1", &[]);
    let scope = home.scope_file("s1");
    assert_eq!(fs::read_to_string(&scope).unwrap(), "", "no project here");
    let old = SystemTime::now() - Duration::from_secs(60 * 60);
    fs::File::options()
        .write(true)
        .open(&scope)
        .unwrap()
        .set_modified(old)
        .unwrap();
    home.hook("UserPromptSubmit", "s1", &[]);
    assert!(modified(&scope) > old, "a prompt's save writes it again");
}

#[test]
fn a_memory_recorded_elsewhere_is_in_the_overlay_after_the_next_tool_hook() {
    let home = Home::new();
    home.record("deploy", "quokka-deploy");
    home.hook("SessionStart", "s1", &[]);
    home.record("lint", "wombat-lint");
    assert!(!holds(&home.overlay(), &hex("wombat-lint")));
    let out = home.hook("PostToolUse", "s1", &[]);
    assert!(out.stdout.is_empty(), "a tool hook prints nothing");
    assert!(holds(&home.overlay(), &hex("wombat-lint")));
    assert!(
        !holds(&home.base(), &hex("wombat-lint")),
        "the base is kept"
    );
    // A second change: the overlay holds every change since the base.
    home.record("test", "numbat-test");
    home.hook("PreToolUse", "s1", &[]);
    assert!(holds(&home.overlay(), &hex("wombat-lint")));
    assert!(holds(&home.overlay(), &hex("numbat-test")));
}

#[test]
fn a_session_without_a_scope_file_does_nothing_in_its_tool_hooks() {
    let home = Home::new();
    home.record("deploy", "quokka-deploy");
    home.hook("SessionStart", "s1", &[]);
    // As a session started by an earlier version: no scope file.
    fs::remove_file(home.scope_file("s1")).unwrap();
    home.record("lint", "wombat-lint");
    let before = fs::read(home.overlay()).unwrap();
    let out = home.hook("PostToolUse", "s1", &[]);
    assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
    assert_eq!(fs::read(home.overlay()).unwrap(), before);
}

#[test]
fn past_the_bound_a_changed_memory_is_hidden_and_waits_for_session_start() {
    let home = Home::new();
    home.hook("SessionStart", "s1", &[]);
    for (title, anchor) in [
        ("one", "quokka-one"),
        ("two", "wombat-two"),
        ("three", "numbat-three"),
    ] {
        home.record(title, anchor);
    }
    home.hook(
        "PostToolUse",
        "s1",
        &[("USER_MEMORY_TEST_OVERLAY_MEMORIES", "2")],
    );
    let held = ["quokka-one", "wombat-two", "numbat-three"]
        .iter()
        .filter(|a| holds(&home.overlay(), &hex(a)))
        .count();
    assert_eq!(held, 2, "two memories' rows");
    home.hook("SessionStart", "s2", &[]);
    for anchor in ["quokka-one", "wombat-two", "numbat-three"] {
        assert!(
            holds(&home.base(), &hex(anchor)),
            "{anchor} in the new base"
        );
    }
}

#[test]
fn a_tool_hook_finding_the_lock_held_writes_nothing_and_waits_not() {
    let home = Home::new();
    home.record("deploy", "quokka-deploy");
    home.hook("SessionStart", "s1", &[]);
    home.record("lint", "wombat-lint");
    let hold = home.path().join("hold");
    fs::write(&hold, "").unwrap();
    let holder = home.spawn(
        "PostToolUse",
        "s1",
        &[("USER_MEMORY_HOLD_REFRESH", hold.to_str().unwrap())],
    );
    let reached = home.path().join("hold.reached");
    time::until("the first hook holds the lock", || reached.exists());
    let before = fs::read(home.overlay()).unwrap();
    // Its limit a hundred times longer: a hook that waited for the lock
    // would wait 10 s.
    let started = std::time::Instant::now();
    home.hook(
        "PostToolUse",
        "s1",
        &[("USER_MEMORY_HOOK_TIME_SCALE", "100")],
    );
    let waited = started.elapsed();
    assert_eq!(fs::read(home.overlay()).unwrap(), before, "nothing written");
    fs::remove_file(&hold).unwrap();
    time::finish(holder, "the first hook");
    assert!(
        holds(&home.overlay(), &hex("wombat-lint")),
        "the holder wrote it"
    );
    assert!(
        waited < Duration::from_secs(5),
        "it did not wait: {waited:?}"
    );
}

#[test]
fn a_tool_hook_past_its_deadline_renames_nothing() {
    let home = Home::new();
    home.record("deploy", "quokka-deploy");
    home.hook("SessionStart", "s1", &[]);
    home.record("lint", "wombat-lint");
    let hold = home.path().join("hold");
    fs::write(&hold, "").unwrap();
    let hook = home.spawn(
        "PostToolUse",
        "s1",
        &[
            ("USER_MEMORY_HOLD_REFRESH", hold.to_str().unwrap()),
            time::HOOK_LIMITS,
        ],
    );
    time::until("the hook holds the lock", || {
        home.path().join("hold.reached").exists()
    });
    std::thread::sleep(Duration::from_millis(200));
    fs::remove_file(&hold).unwrap();
    time::finish(hook, "the hook");
    assert!(!holds(&home.overlay(), &hex("wombat-lint")));
}

#[test]
fn an_overlay_whose_store_pruned_its_changes_writes_nothing() {
    let home = Home::new();
    home.record("deploy", "quokka-deploy");
    home.hook("SessionStart", "s1", &[]);
    home.record("lint", "wombat-lint");
    let conn = rusqlite::Connection::open(home.db()).unwrap();
    conn.execute(
        "UPDATE store_changes SET pruned_through=last_change WHERE id=1",
        [],
    )
    .unwrap();
    drop(conn);
    let before = fs::read(home.overlay()).unwrap();
    home.hook("PostToolUse", "s1", &[]);
    assert_eq!(fs::read(home.overlay()).unwrap(), before);
}

#[test]
fn a_tool_hook_deletes_a_base_holding_words_an_unaware_writer_removed() {
    // This binary's forget and update leave the delivery files as they are,
    // as one that knows nothing of them does.
    for remove in ["forget", "update"] {
        let home = Home::new();
        let id = home.record("deploy", "quokka-deploy");
        home.record("lint", "wombat-lint");
        home.hook("SessionStart", "s1", &[]);
        assert!(holds(&home.base(), &hex("quokka-deploy")));
        let out = if remove == "forget" {
            home.cli(&["forget", &id])
        } else {
            home.cli(&["update", &id, "--content", "Run the plain deploy step."])
        };
        assert!(out.is_object(), "{out}");
        home.hook("PostToolUse", "s1", &[]);
        assert_eq!(
            holding(&home, "quokka-deploy"),
            Vec::<PathBuf>::new(),
            "{remove}"
        );
        assert!(
            home.files(".index").is_empty(),
            "{remove}: the base is deleted"
        );
    }
}

#[test]
fn the_prompt_builds_a_base_its_scope_lacks_and_still_prints_its_notice() {
    let home = Home::new();
    home.record("deploy", "quokka-deploy");
    home.hook("SessionStart", "s1", &[]);
    // As after a project move or `migrate`: the store's files are gone.
    for file in every_file(&home.delivery()) {
        fs::remove_file(file).unwrap();
    }
    let lint = home.record("lint", "wombat-lint");
    let out = home.hook("UserPromptSubmit", "s1", &[]);
    let notice = String::from_utf8(out.stdout).unwrap();
    assert!(notice.contains(&lint[..8]), "the new memory told: {notice}");
    for anchor in ["quokka-deploy", "wombat-lint"] {
        assert!(holds(&home.base(), &hex(anchor)), "{anchor} in the base");
    }
    assert!(home.overlay().exists(), "an empty overlay naming the base");
}

#[test]
fn a_linked_scope_gets_its_base_at_the_next_prompt_and_an_unlinked_one_loses_its_memories() {
    const COMPILER: &str = "github.com/acme/compiler";
    let home = Home::new();
    home.record_in(Some(COMPILER), "fuzz", "quokka-fuzz");
    home.hook("SessionStart", "s1", &[]);
    assert!(
        !holds(&home.base(), &hex("quokka-fuzz")),
        "not this scope's"
    );
    home.link("s1", COMPILER, false);
    home.hook("UserPromptSubmit", "s1", &[]);
    let bases = home.files(".index");
    assert_eq!(bases.len(), 2, "the linked scope's base beside the first");
    assert_eq!(
        bases
            .iter()
            .filter(|base| holds(base, &hex("quokka-fuzz")))
            .count(),
        1,
        "the linked scope's base holds the project's memory"
    );
    home.link("s1", COMPILER, true);
    home.hook("UserPromptSubmit", "s1", &[]);
    home.record_in(Some(COMPILER), "bench", "wombat-bench");
    home.record("lint", "numbat-lint");
    home.hook("PostToolUse", "s1", &[]);
    let delivery = every_file(&home.delivery());
    assert!(
        delivery.iter().any(|file| holds(file, &hex("numbat-lint"))),
        "the hook kept the session's scope current: {delivery:?}"
    );
    assert!(
        !delivery
            .iter()
            .any(|file| holds(file, &hex("wombat-bench"))),
        "nothing of the unlinked project: {delivery:?}"
    );
}

#[test]
fn a_base_whose_changes_the_store_pruned_is_built_again_at_the_next_prompt() {
    let home = Home::new();
    home.record("deploy", "quokka-deploy");
    home.hook("SessionStart", "s1", &[]);
    home.record("lint", "wombat-lint");
    let conn = rusqlite::Connection::open(home.db()).unwrap();
    conn.execute(
        "UPDATE store_changes SET pruned_through=last_change WHERE id=1",
        [],
    )
    .unwrap();
    drop(conn);
    home.hook("UserPromptSubmit", "s1", &[]);
    assert!(holds(&home.base(), &hex("wombat-lint")), "built again");
}

#[test]
fn a_prompt_after_an_unaware_removal_builds_the_base_without_its_words() {
    let home = Home::new();
    let id = home.record("deploy", "quokka-deploy");
    home.record("lint", "wombat-lint");
    home.hook("SessionStart", "s1", &[]);
    home.cli(&["forget", &id]);
    home.hook("UserPromptSubmit", "s1", &[]);
    assert_eq!(holding(&home, "quokka-deploy"), Vec::<PathBuf>::new());
    assert!(holds(&home.base(), &hex("wombat-lint")), "built again");
}
