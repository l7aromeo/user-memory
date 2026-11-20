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
        self.cli_with(args, &[])
    }

    fn cli_with(&self, args: &[&str], env: &[(&str, &str)]) -> Value {
        let out = self.command(args, env).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// Records a global memory whose content holds `anchor` as a code
    /// span, which the index keeps as written; returns its id.
    fn record(&self, title: &str, anchor: &str) -> String {
        self.record_with(&[], None, title, anchor)
    }

    /// As [`Home::record`], for `project` when given.
    fn record_in(&self, project: Option<&str>, title: &str, anchor: &str) -> String {
        self.record_with(&[], project, title, anchor)
    }

    /// As [`Home::record_in`], with `extra` arguments.
    fn record_with(
        &self,
        extra: &[&str],
        project: Option<&str>,
        title: &str,
        anchor: &str,
    ) -> String {
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
        args.extend(extra);
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
fn session_start_looks_at_its_projects_once_for_its_snapshot_and_indexes() {
    let home = Home::new();
    home.record("deploy", "quokka-deploy");
    for (session, what) in [("s1", "building its base"), ("s2", "finding it")] {
        let out = home.hook("SessionStart", session, &[]);
        let looks = String::from_utf8_lossy(&out.stderr)
            .matches("looked at the session's projects")
            .count();
        assert_eq!(looks, 1, "{what}: {out:?}");
    }
    assert!(holds(&home.base(), &hex("quokka-deploy")), "built");
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

/// Makes a removing write leave the delivery files as they are, as a binary
/// that knows nothing of them does.
const UNAWARE: (&str, &str) = ("USER_MEMORY_TEST_NO_DELIVERY", "1");

#[test]
fn a_tool_hook_deletes_a_base_holding_words_an_unaware_writer_removed() {
    for remove in ["forget", "update"] {
        let home = Home::new();
        let id = home.record("deploy", "quokka-deploy");
        home.record("lint", "wombat-lint");
        home.hook("SessionStart", "s1", &[]);
        assert!(holds(&home.base(), &hex("quokka-deploy")));
        let out = if remove == "forget" {
            home.cli_with(&["forget", &id], &[UNAWARE])
        } else {
            home.cli_with(
                &["update", &id, "--content", "Run the plain deploy step."],
                &[UNAWARE],
            )
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
fn the_prompt_builds_a_base_deleted_beside_its_overlay() {
    let home = Home::new();
    home.record("deploy", "quokka-deploy");
    home.hook("SessionStart", "s1", &[]);
    // The overlay holds the store's latest change; only the base is gone.
    fs::remove_file(home.base()).unwrap();
    home.hook("UserPromptSubmit", "s1", &[]);
    assert!(holds(&home.base(), &hex("quokka-deploy")), "built again");
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
    home.cli_with(&["forget", &id], &[UNAWARE]);
    home.hook("UserPromptSubmit", "s1", &[]);
    assert_eq!(holding(&home, "quokka-deploy"), Vec::<PathBuf>::new());
    assert!(holds(&home.base(), &hex("wombat-lint")), "built again");
}

/// A home whose session started on a base holding `quokka-deploy` (the
/// memory returned) and `wombat-lint`.
fn started() -> (Home, String) {
    let home = Home::new();
    let id = home.record("deploy", "quokka-deploy");
    home.record("lint", "wombat-lint");
    home.hook("SessionStart", "s1", &[]);
    assert!(holds(&home.base(), &hex("quokka-deploy")));
    (home, id)
}

/// After a removal of `gone`: no delivery file holds its words (a session's
/// snapshot keeps the titles it saw until its next prompt), and the scope's
/// base was built again with `kept` and an empty overlay made for it.
fn removed(home: &Home, gone: &str, kept: &str, what: &str) {
    let holding: Vec<PathBuf> = every_file(&home.delivery())
        .into_iter()
        .filter(|path| holds(path, &hex(gone)) || holds(path, gone))
        .collect();
    assert_eq!(holding, Vec::<PathBuf>::new(), "{what}");
    assert!(holds(&home.base(), &hex(kept)), "{what}: built again");
    assert!(home.overlay().exists(), "{what}: with its overlay");
    assert!(home.files(".void").is_empty(), "{what}: no void mark left");
}

#[test]
fn forget_builds_the_files_again_without_the_forgotten_words() {
    let (home, id) = started();
    let out = home.cli(&["forget", &id]);
    assert!(out.is_object(), "{out}");
    removed(&home, "quokka-deploy", "wombat-lint", "forget ID");

    let home = Home::new();
    home.record_with(&["--origin", "chat/one"], None, "deploy", "quokka-deploy");
    home.record("lint", "wombat-lint");
    home.hook("SessionStart", "s1", &[]);
    home.cli(&["forget", "--origin", "chat/one"]);
    removed(&home, "quokka-deploy", "wombat-lint", "forget --origin");
}

#[test]
fn forgetting_an_import_builds_the_files_again_without_its_words() {
    let other = Home::new();
    other.record("deploy", "quokka-deploy");
    let file = other.path().join("export.json");
    other.cli(&["export", file.to_str().unwrap()]);
    let home = Home::new();
    home.record("lint", "wombat-lint");
    let summary = home.cli(&["import", file.to_str().unwrap(), "--apply"]);
    let import = summary["import"].as_str().unwrap().to_owned();
    home.hook("SessionStart", "s1", &[]);
    assert!(holds(&home.base(), &hex("quokka-deploy")));
    home.cli(&["forget", "--import", &import]);
    removed(&home, "quokka-deploy", "wombat-lint", "forget --import");
}

#[test]
fn update_builds_the_files_again_without_the_old_words() {
    for (what, args) in [
        ("content", vec!["--content", "Run `numbat-deploy` first."]),
        (
            "content, dropping the history",
            vec!["--content", "Run `numbat-deploy` first.", "--drop-history"],
        ),
        ("title", vec!["--title", "numbat-deploy"]),
    ] {
        let home = Home::new();
        let recorded = home.cli(&[
            "record",
            "--type",
            "procedure",
            "--category",
            "build",
            "--title",
            "quokka-deploy",
            "--content",
            "Run `wombat-lint` first.",
            "--scope",
            "global",
        ]);
        let id = recorded["id"].as_str().unwrap().to_owned();
        home.hook("SessionStart", "s1", &[]);
        assert!(holds(&home.base(), &hex("quokka")), "{what}");
        let mut update = vec!["update", id.as_str()];
        update.extend(&args);
        home.cli(&update);
        let gone = if what == "title" {
            "quokka"
        } else {
            "wombat-lint"
        };
        removed(&home, gone, "numbat", what);
    }
}

#[test]
fn an_import_taking_the_file_s_side_builds_the_files_again_without_the_store_s_words() {
    let home = Home::new();
    let id = home.record("deploy", "wombat-lint");
    let file = home.path().join("export.json");
    home.cli(&["export", file.to_str().unwrap()]);
    home.cli(&["update", &id, "--content", "Run `quokka-deploy` first."]);
    home.hook("SessionStart", "s1", &[]);
    assert!(holds(&home.base(), &hex("quokka-deploy")));
    let summary = home.cli(&[
        "import",
        file.to_str().unwrap(),
        "--apply",
        "--on-conflict",
        "use-file",
    ]);
    assert_eq!(summary["applied"], true, "{summary}");
    removed(&home, "quokka-deploy", "wombat-lint", "import use-file");
}

#[test]
fn a_write_removing_no_held_word_leaves_the_files_as_they_are() {
    let (home, _) = started();
    let other = home.record_in(Some("github.com/acme/compiler"), "fuzz", "numbat-fuzz");
    let base = home.base();
    let written = modified(&base);
    std::thread::sleep(Duration::from_millis(20));
    // A memory no file holds, forgotten; one held, its scope changed only.
    home.cli(&["forget", &other]);
    let held = home.record_in(Some("github.com/acme/compiler"), "bench", "numbat-bench");
    home.hook("PostToolUse", "s1", &[]);
    let overlay = modified(&home.overlay());
    home.cli(&["update", &held, "--scope", "global"]);
    assert_eq!(modified(&base), written, "the base is kept");
    assert_eq!(modified(&home.overlay()), overlay, "the overlay is kept");
}

#[test]
fn the_daily_prune_leaves_the_files_as_they_are() {
    let (home, id) = started();
    home.cli(&["forget", &id]);
    let base = home.base();
    let written = modified(&base);
    let conn = rusqlite::Connection::open(home.db()).unwrap();
    conn.execute(
        "UPDATE memory_changes SET removed_at=datetime('now','-40 days') WHERE removed_at IS NOT NULL",
        [],
    )
    .unwrap();
    drop(conn);
    // No backup yet: the next write makes one, and prunes.
    fs::remove_dir_all(home.path().join("backups")).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    home.record("test", "numbat-test");
    let pruned: i64 = rusqlite::Connection::open(home.db())
        .unwrap()
        .query_row(
            "SELECT pruned_through FROM store_changes WHERE id=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(pruned > 0, "the prune ran");
    assert_eq!(modified(&base), written);
}

#[test]
fn forget_through_the_server_builds_the_files_again() {
    let (home, id) = started();
    let out = serve::call(
        home.command(&["serve"], &[]),
        "forget",
        json!({"memory_id": id, "user_confirmed": true}),
    );
    assert_eq!(out["deleted"], true, "{out}");
    removed(
        &home,
        "quokka-deploy",
        "wombat-lint",
        "forget through serve",
    );
}

#[test]
fn a_forget_waits_for_a_hook_holding_the_lock_and_leaves_none_of_its_words() {
    let home = Home::new();
    home.record("lint", "wombat-lint");
    home.hook("SessionStart", "s1", &[]);
    let id = home.record("deploy", "quokka-deploy");
    // The hook built its overlay, holding the new memory, and waits to
    // rename it.
    let hold = home.path().join("hold");
    fs::write(&hold, "").unwrap();
    let hook = home.spawn(
        "PostToolUse",
        "s1",
        &[("USER_MEMORY_HOLD_REFRESH", hold.to_str().unwrap())],
    );
    time::until("the hook holds the lock", || {
        home.path().join("hold.reached").exists()
    });
    let forget = home
        .command(&["forget", &id], &[])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    fs::remove_file(&hold).unwrap();
    time::finish(hook, "the hook");
    let out = time::finish(forget, "the forget");
    assert!(out.status.success(), "{out:?}");
    removed(
        &home,
        "quokka-deploy",
        "wombat-lint",
        "forget after the hook",
    );
}

#[test]
fn a_forget_refuses_while_a_hook_holds_the_lock_and_changes_nothing() {
    let (home, id) = started();
    home.record("test", "numbat-test");
    let hold = home.path().join("hold");
    fs::write(&hold, "").unwrap();
    let hook = home.spawn(
        "PostToolUse",
        "s1",
        &[("USER_MEMORY_HOLD_REFRESH", hold.to_str().unwrap())],
    );
    time::until("the hook holds the lock", || {
        home.path().join("hold.reached").exists()
    });
    let refused = home
        .command(
            &["forget", &id],
            &[("USER_MEMORY_TEST_DELIVERY_LOCK_WAIT", "1")],
        )
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&refused.stderr);
    fs::remove_file(&hold).unwrap();
    let pid = hook.id();
    time::finish(hook, "the hook");
    assert!(!refused.status.success(), "{refused:?}");
    #[cfg(not(windows))]
    let who = format!("user-memory process {pid}");
    // Windows lets no other process read a locked file.
    #[cfg(windows)]
    let who = {
        let _ = pid;
        "another user-memory process".to_owned()
    };
    assert!(
        stderr.contains(&format!(
            "Nothing was forgotten: {who} has held this machine's delivery index for 1 s. Wait a moment and forget again; if it still holds it, stop that process first."
        )),
        "{stderr}"
    );
    let still = home.cli(&["read", &id]);
    assert_eq!(
        still["memories"].as_array().map(Vec::len),
        Some(1),
        "{still}"
    );
    assert!(home.files(".void").is_empty(), "nothing voided");
}

#[test]
fn a_forget_killed_after_its_commit_leaves_its_words_to_the_next_tool_hook() {
    let (home, id) = started();
    let hold = home.path().join("hold");
    fs::write(&hold, "").unwrap();
    let mut forget = home
        .command(
            &["forget", &id],
            &[("USER_MEMORY_HOLD_REMOVAL", hold.to_str().unwrap())],
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    time::until("the forget committed", || {
        home.path().join("hold.reached").exists()
    });
    forget.kill().unwrap();
    forget.wait().unwrap();
    home.hook("PostToolUse", "s1", &[]);
    assert_eq!(holding(&home, "quokka-deploy"), Vec::<PathBuf>::new());
    home.hook("UserPromptSubmit", "s1", &[]);
    assert!(holds(&home.base(), &hex("wombat-lint")), "built again");
}

#[test]
fn a_forget_whose_commit_fails_leaves_the_files_in_use() {
    let (home, id) = started();
    let base = home.base();
    let written = modified(&base);
    let out = home
        .command(&["forget", &id], &[("USER_MEMORY_TEST_FAIL_REMOVAL", "1")])
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    assert!(
        home.files(".void").is_empty(),
        "the void mark is taken back"
    );
    assert_eq!(modified(&base), written, "the base is kept");
    let still = home.cli(&["read", &id]);
    assert_eq!(
        still["memories"].as_array().map(Vec::len),
        Some(1),
        "{still}"
    );
}

/// The store's delivery files other than its lock.
fn store_files(home: &Home) -> Vec<PathBuf> {
    every_file(&home.delivery())
        .into_iter()
        .filter(|path| path.extension().is_none_or(|e| e != "lock"))
        .collect()
}

#[test]
fn a_project_move_deletes_the_store_s_files_and_the_next_prompt_builds_them() {
    const OLD: &str = "github.com/acme/old";
    let home = Home::new();
    home.record_in(Some(OLD), "fuzz", "quokka-fuzz");
    home.hook("SessionStart", "s1", &[]);
    home.link("s1", OLD, false);
    home.hook("UserPromptSubmit", "s1", &[]);
    assert!(
        home.files(".index")
            .iter()
            .any(|base| holds(base, &hex("quokka-fuzz")))
    );
    let moved = home
        .command(&["project", "move", OLD, "github.com/acme/new"], &[])
        .output()
        .unwrap();
    assert!(moved.status.success(), "{moved:?}");
    assert_eq!(store_files(&home), Vec::<PathBuf>::new());
    home.hook("UserPromptSubmit", "s1", &[]);
    assert!(
        home.files(".index")
            .iter()
            .any(|base| holds(base, &hex("quokka-fuzz"))),
        "the moved link's scope built again"
    );
}

#[test]
fn a_migration_deletes_the_store_s_files() {
    let home = Home::new();
    home.record("deploy", "quokka-deploy");
    home.hook("SessionStart", "s1", &[]);
    assert!(!store_files(&home).is_empty());
    // The same path now holds a store from before the change counter.
    for ending in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{ending}", home.db().display()));
    }
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/schema");
    let conn = rusqlite::Connection::open(home.db()).unwrap();
    conn.execute_batch(&fs::read_to_string(fixtures.join("v9.sql")).unwrap())
        .unwrap();
    conn.pragma_update(None, "user_version", 9).unwrap();
    conn.execute_batch(&fs::read_to_string(fixtures.join("v9.seed.sql")).unwrap())
        .unwrap();
    drop(conn);
    let out = home.command(&["migrate"], &[]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(store_files(&home), Vec::<PathBuf>::new());
}

/// The reply's line when a removal left files another program holds open.
const LEFT: &str = "The files in delivery_left still hold the removed words because another program has them open. Readers skip them; user-memory deletes them at its next write, and `user-memory doctor` deletes them at once.";

/// Makes renames and deletes of the file named `name` fail, as another
/// program holding it open does on Windows.
fn held(name: &str) -> (&'static str, &str) {
    ("USER_MEMORY_TEST_HELD_FILE", name)
}

/// The delivery files holding `word`'s hex form or its text.
fn delivered(home: &Home, word: &str) -> Vec<PathBuf> {
    every_file(&home.delivery())
        .into_iter()
        .filter(|path| holds(path, &hex(word)) || holds(path, word))
        .collect()
}

/// After a removal of `gone` that left `base`: the reply names it, the void
/// mark names it, and the next tool hook deletes it and every word with it.
fn left_until_the_next_writer(home: &Home, reply: &Value, base: &Path, gone: &str) {
    assert_eq!(reply["delivery_left"], json!([base]), "{reply}");
    assert!(reply["next"].as_str().unwrap().ends_with(LEFT), "{reply}");
    let void = home.files(".void");
    assert_eq!(void.len(), 1);
    let name = base.file_name().unwrap().to_str().unwrap();
    assert!(fs::read_to_string(&void[0]).unwrap().contains(name));
    home.hook("PostToolUse", "s1", &[]);
    assert_eq!(delivered(home, gone), Vec::<PathBuf>::new());
    assert!(home.files(".void").is_empty(), "the mark goes with it");
}

#[test]
fn a_file_another_program_holds_is_named_in_the_reply_and_deleted_by_the_next_writer() {
    for what in ["forget", "update", "update --title", "import"] {
        let home = Home::new();
        let title = if what == "update --title" {
            "quokka-title"
        } else {
            "deploy"
        };
        let id = home.record(title, "quokka-deploy");
        home.record("lint", "wombat-lint");
        let file = home.path().join("export.json");
        home.cli(&["export", file.to_str().unwrap()]);
        if what == "import" {
            home.cli(&["update", &id, "--content", "Run `numbat-deploy` first."]);
        }
        home.hook("SessionStart", "s1", &[]);
        let base = home.base();
        let name = base.file_name().unwrap().to_str().unwrap().to_owned();
        let (args, gone): (Vec<&str>, &str) = match what {
            "forget" => (vec!["forget", &id], "quokka-deploy"),
            "update" => (
                vec!["update", &id, "--content", "Run `numbat-deploy` first."],
                "quokka-deploy",
            ),
            "update --title" => (vec!["update", &id, "--title", "numbat-title"], "quokka"),
            _ => (
                vec![
                    "import",
                    file.to_str().unwrap(),
                    "--apply",
                    "--on-conflict",
                    "use-file",
                ],
                "numbat-deploy",
            ),
        };
        assert!(holds(&base, &hex(gone)), "{what}");
        let reply = home.cli_with(&args, &[held(&name)]);
        assert!(holds(&base, &hex(gone)), "{what}: still on disk");
        left_until_the_next_writer(&home, &reply, &base, gone);
    }
}

#[cfg(windows)]
#[test]
fn a_base_open_without_shared_delete_is_named_in_the_reply_and_deleted_once_closed() {
    use std::os::windows::fs::OpenOptionsExt;
    let (home, id) = started();
    let base = home.base();
    // As a scanner or backup tool opens it: no sharing at all.
    let open = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&base)
        .unwrap();
    let reply = home.cli(&["forget", &id]);
    drop(open);
    left_until_the_next_writer(&home, &reply, &base, "quokka-deploy");
}

/// Doctor's output in `home`, with `env`.
fn doctor(home: &Home, env: &[(&str, &str)]) -> String {
    let out = home.command(&["doctor"], env).output().unwrap();
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn doctor_deletes_the_files_a_removal_left_and_names_one_still_held() {
    let (home, id) = started();
    let base = home.base();
    let name = base.file_name().unwrap().to_str().unwrap().to_owned();
    home.cli_with(&["forget", &id], &[held(&name)]);
    let still = doctor(&home, &[held(&name)]);
    assert!(
        still.contains(&format!(
            "\nAction needed: Delivery index: {} still holds removed words because another program has it open; close that program, then run `user-memory doctor` again\n",
            base.display()
        )),
        "{still}"
    );
    assert!(!still.contains("Delivery index: deleted"), "{still}");
    // A writer killed while writing left a file too.
    let partial = |n: u32| {
        fs::write(base.with_extension(format!("overlay.{n}.partial")), "x").unwrap();
    };
    partial(4242);
    let done = doctor(&home, &[]);
    assert!(
        done.contains("\nDelivery index: deleted 2 files that held removed words.\n"),
        "{done}"
    );
    assert_eq!(holding(&home, "quokka-deploy"), Vec::<PathBuf>::new());
    assert!(home.files(".void").is_empty());
    partial(4243);
    let one = doctor(&home, &[]);
    assert!(
        one.contains("\nDelivery index: deleted 1 file that held removed words.\n"),
        "{one}"
    );
    let again = doctor(&home, &[]);
    assert!(!again.contains("Delivery index"), "silent: {again}");
}

#[test]
fn doctor_fails_while_another_process_keeps_the_lock() {
    let (home, _) = started();
    home.record("test", "numbat-test");
    let hold = home.path().join("hold");
    fs::write(&hold, "").unwrap();
    let hook = home.spawn(
        "PostToolUse",
        "s1",
        &[("USER_MEMORY_HOLD_REFRESH", hold.to_str().unwrap())],
    );
    time::until("the hook holds the lock", || {
        home.path().join("hold.reached").exists()
    });
    let out = home
        .command(&["doctor"], &[("USER_MEMORY_TEST_DELIVERY_LOCK_WAIT", "1")])
        .output()
        .unwrap();
    fs::remove_file(&hold).unwrap();
    let pid = hook.id();
    time::finish(hook, "the hook");
    assert!(!out.status.success(), "{out:?}");
    #[cfg(not(windows))]
    let who = format!("user-memory process {pid}");
    #[cfg(windows)]
    let who = {
        let _ = pid;
        "another user-memory process".to_owned()
    };
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!(
            "Delivery index: {who} has held this machine's delivery index for 1 s"
        )),
        "{stderr}"
    );
}

#[test]
fn a_removal_whose_void_mark_cannot_be_written_refuses_and_changes_nothing() {
    let (home, id) = started();
    let base = home.base();
    let written = modified(&base);
    let key = base
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .split('-')
        .next()
        .unwrap();
    let void = format!("{key}.void");
    let out = home
        .command(&["forget", &id], &[held(&void)])
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mark = base.with_file_name(&void);
    assert!(
        stderr.contains(&format!(
            "Nothing was forgotten: the mark that keeps this machine's delivery index from using the removed words could not be written to {}: ",
            mark.display()
        )),
        "{stderr}"
    );
    assert!(
        stderr
            .contains(". Make that folder writable, or free space on its disk, then forget again."),
        "{stderr}"
    );
    let still = home.cli(&["read", &id]);
    assert_eq!(
        still["memories"].as_array().map(Vec::len),
        Some(1),
        "{still}"
    );
    assert_eq!(modified(&base), written, "the base is kept");
    assert!(home.files(".void").is_empty());
    assert!(home.files(".partial").is_empty());
}

#[test]
fn a_mark_that_cannot_be_rewritten_after_the_commit_still_names_the_files_left() {
    let (home, id) = started();
    let base = home.base();
    let overlay = home.overlay();
    let name = base.file_name().unwrap().to_str().unwrap().to_owned();
    let key = name.split('-').next().unwrap();
    // The base is held open; the mark's first write, before the commit,
    // succeeds, and its rewrite after the commit fails.
    let held = format!("{name},{key}.void#2");
    let reply = home.cli_with(&["forget", &id], &[("USER_MEMORY_TEST_HELD_FILE", &held)]);
    assert_eq!(reply["deleted"], true, "{reply}");
    assert_eq!(reply["delivery_left"], json!([base]), "{reply}");
    assert!(reply["next"].as_str().unwrap().ends_with(LEFT), "{reply}");
    let gone = home.cli(&["read", &id]);
    assert_eq!(gone["memories"].as_array().map(Vec::len), Some(0), "{gone}");
    // The first mark still names both files, so readers skip both.
    let void = home.files(".void");
    assert_eq!(void.len(), 1);
    let named = fs::read_to_string(&void[0]).unwrap();
    for file in [&base, &overlay] {
        let file = file.file_name().unwrap().to_str().unwrap();
        assert!(named.lines().any(|line| line == file), "{named}");
    }
    home.hook("PostToolUse", "s1", &[]);
    assert_eq!(delivered(&home, "quokka-deploy"), Vec::<PathBuf>::new());
    assert!(
        !base.exists() && !overlay.exists(),
        "the next writer deletes them"
    );
    assert!(home.files(".void").is_empty(), "and the mark");
}
