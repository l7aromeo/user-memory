//! Session start lists the standing rules added or changed since this
//! machine's last session of the store, once, and never an old rule as new.

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

/// Opens the block, as session start says it.
const NEW_RULES: &str = "New rules since your last session:";
/// Closes the block.
const REMOVE: &str = "If one isn't the user's, remove it with forget on their yes, or forget with its origin to remove everything that conversation recorded.";

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
    fn command(&self, args: &[&str], cwd: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .args(args)
            .current_dir(cwd)
            .sandboxed(self.path())
            .env("USER_MEMORY_DB", self.path().join("memory.sqlite3"))
            .env("GIT_CEILING_DIRECTORIES", self.path())
            .env_remove("USER_MEMORY_URL")
            .env_remove("USER_MEMORY_TOKEN_FILE")
            .env(HOOK_ROOM.0, HOOK_ROOM.1);
        command
    }
    /// The command line's JSON reply to `args`.
    fn cli(&self, args: &[&str]) -> Value {
        let out = self.command(args, self.path()).output().unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }
    /// Records a memory with these extra arguments; its id.
    fn record(&self, title: &str, extra: &[&str]) -> String {
        let mut args = vec![
            "record",
            "--category",
            "workflow",
            "--title",
            title,
            "--content",
            title,
        ];
        args.extend_from_slice(extra);
        let out = self.cli(&args);
        assert_eq!(out["stored"], true, "{out}");
        out["id"].as_str().unwrap().to_owned()
    }
    /// Records a global standing rule; its id.
    fn rule(&self, title: &str) -> String {
        self.record(title, &["--type", "preference", "--scope", "global"])
    }
    /// Records a standing rule of `project`; its id.
    fn project_rule(&self, title: &str, project: &str) -> String {
        self.record(
            title,
            &[
                "--type",
                "preference",
                "--scope",
                "project",
                "--project",
                project,
            ],
        )
    }
    /// A folder whose project key is `key`.
    fn project(&self, key: &str) -> PathBuf {
        let folder = self.path().join(key.replace('/', "_"));
        std::fs::create_dir_all(&folder).unwrap();
        let out = self
            .command(&["project-id", "--set", key], &folder)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        folder
    }
    /// What session start prints for a new session `session` in `cwd`,
    /// started as `source` names it.
    fn start(&self, session: &str, cwd: &Path, source: &str) -> String {
        self.told(session, cwd, source).0
    }
    /// What session start gives the agent and, when it has one, its
    /// message for the user.
    fn told(&self, session: &str, cwd: &Path, source: &str) -> (String, Option<String>) {
        let raw = self.raw(session, cwd, source);
        if !raw.starts_with('{') {
            return (raw, None);
        }
        let out: Value = serde_json::from_str(&raw).unwrap();
        (
            out["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .to_owned(),
            Some(out["systemMessage"].as_str().unwrap().to_owned()),
        )
    }
    /// What session start prints.
    fn raw(&self, session: &str, cwd: &Path, source: &str) -> String {
        let payload = json!({"session_id": session, "cwd": cwd, "hook_event_name": "SessionStart", "source": source});
        let mut child = self
            .command(&["session-start", "--client", "claude"], cwd)
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
        let out = time::finish(child, "session start");
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    }
    /// The ids (8 characters) session start lists as new rules for a
    /// startup in `cwd`.
    fn listed(&self, session: &str, cwd: &Path) -> Vec<String> {
        listed(&self.start(session, cwd, "startup"))
    }
    /// This home's file of the rules last shown.
    fn seen_file(&self) -> PathBuf {
        let sessions = sandbox::folder(self.path(), "state").join("sessions");
        let found: Vec<PathBuf> = std::fs::read_dir(&sessions)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                let name = p.file_name().unwrap().to_string_lossy();
                name.starts_with("seen-") && name.ends_with(".json")
            })
            .collect();
        assert_eq!(found.len(), 1, "{found:?}");
        found[0].clone()
    }
}

/// The block in session start's text, from its opening to its closing
/// line, or `None` when there is none.
fn block(text: &str) -> Option<String> {
    let start = text.find(NEW_RULES)?;
    let end = text[start..].find(REMOVE).unwrap() + start + REMOVE.len();
    Some(text[start..end].to_owned())
}

/// The ids the block lists, by their first 8 characters.
fn listed(text: &str) -> Vec<String> {
    block(text)
        .map(|block| {
            block
                .lines()
                .filter(|line| line.starts_with("  ") && !line.starts_with("  ..."))
                .map(|line| line.trim_start()[..8].to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// A title of about 70 characters that shares no word with another `n`'s,
/// so recording it is never refused as similar.
fn unlike(n: usize) -> String {
    [
        "alpha", "bravo", "carol", "delta", "early", "foxes", "grace", "hotel",
    ]
    .iter()
    .map(|word| format!("{word}{n}q"))
    .collect::<Vec<_>>()
    .join(" ")
}

/// Records an observation, as an agent does on its own; its id.
fn observe(home: &Home, title: &str) -> String {
    home.record(
        title,
        &[
            "--type",
            "fact",
            "--scope",
            "global",
            "--source",
            "observed",
            "--evidence",
            "Seen once",
        ],
    )
}

#[test]
fn the_user_is_told_once_at_startup_when_a_rule_is_new() {
    let home = Home::new();
    let here = home.path();
    // No file: nothing said, and the plain text as before.
    observe(&home, "Alpha observation before any file");
    assert_eq!(home.told("s1", here, "startup").1, None);
    assert!(!home.raw("s1", here, "startup").starts_with('{'));
    // Agents' memories alone: nothing said.
    observe(&home, "Bravo observation without a rule");
    assert_eq!(home.told("s2", here, "startup").1, None);
    // A rule: told, counting both agents' memories since.
    home.rule("Charlie rule another session recorded");
    observe(&home, "Delta observation beside the rule");
    let (text, message) = home.told("s3", here, "startup");
    assert_eq!(
        message.as_deref(),
        Some(
            "1 new rule and 2 new memories recorded by agents since your last session: `user-memory review` lists them."
        )
    );
    assert!(block(&text).is_some(), "{text}");
    // The agent's text is what a plain start prints: origin line first.
    assert!(
        text.starts_with("Conversation origin: claude:session/s3"),
        "{text}"
    );
    // Once.
    assert_eq!(home.told("s4", here, "startup").1, None);
}

#[test]
fn a_rule_first_shown_where_the_user_could_not_be_told_is_told_at_the_next_startup() {
    let home = Home::new();
    let here = home.path();
    home.listed("s1", here);
    let rule = home.rule("Echo rule recorded before a compaction");
    // A compacted session's start lists it to the agent, telling no one.
    let (text, message) = home.told("s1", here, "compact");
    assert_eq!((listed(&text), message), (vec![id8(&rule)], None));
    // The next new session tells the user, listing nothing again.
    let (text, message) = home.told("s2", here, "startup");
    assert_eq!(listed(&text), Vec::<String>::new());
    assert_eq!(
        message.as_deref(),
        Some("1 new rule since your last session: `user-memory review` lists them.")
    );
    assert_eq!(home.told("s3", here, "startup").1, None);
}

fn id8(id: &str) -> String {
    id[..8].to_owned()
}

#[test]
fn a_rule_recorded_between_sessions_is_listed_once_and_the_first_session_lists_none() {
    let home = Home::new();
    let here = home.path();
    home.rule("Rule from before the upgrade");
    // No file yet, as on the first session after the upgrade: nothing.
    assert_eq!(home.listed("s1", here), Vec::<String>::new());
    let added = home.record(
        "Rule another session recorded",
        &[
            "--type",
            "constraint",
            "--scope",
            "global",
            "--origin",
            "claude:session/other",
        ],
    );
    let text = home.start("s2", here, "startup");
    let block = block(&text).unwrap();
    assert!(
        block.contains(&format!(
            "  {} Rule another session recorded (from claude:session/other, cli)",
            id8(&added)
        )),
        "{block}"
    );
    // After the projects line and before the index.
    let at = |needle: &str| text.find(needle).unwrap();
    assert!(at("No project") < at(NEW_RULES) && at(REMOVE) < at("USER MEMORY INDEX"));
    // Shown once.
    assert_eq!(home.listed("s3", here), Vec::<String>::new());
    // A file removed by hand starts afresh, saying nothing.
    std::fs::remove_file(home.seen_file()).unwrap();
    home.rule("Rule recorded while the file was gone");
    assert_eq!(home.listed("s4", here), Vec::<String>::new());
}

#[test]
fn a_changed_or_widened_rule_is_listed_and_an_unchanged_update_is_not() {
    let home = Home::new();
    let here = home.path();
    let p = home.project("github.com/x/p");
    let rule = home.rule("Indent with tabs");
    let narrow = home.project_rule("Run the linter", "github.com/x/p");
    home.listed("s1", &p);
    // An update that changes nothing, a confirmation and a new category:
    // nothing an agent acts on.
    home.cli(&["update", &rule, "--content", "Indent with tabs"]);
    home.cli(&["update", &rule, "--category", "style"]);
    assert_eq!(home.listed("s2", &p), Vec::<String>::new());
    // What it says changed: listed.
    home.cli(&["update", &rule, "--content", "Indent with two spaces"]);
    assert_eq!(home.listed("s3", here), [id8(&rule)]);
    // Widened from its project to every project: listed.
    home.cli(&["update", &narrow, "--scope", "global", "--project", ""]);
    assert_eq!(home.listed("s4", here), [id8(&narrow)]);
}

#[test]
fn a_project_first_opened_lists_none_of_its_rules_until_one_is_added() {
    let home = Home::new();
    home.listed("s1", home.path());
    home.project_rule("Use the project's own runner", "github.com/x/p");
    let p = home.project("github.com/x/p");
    // Never covered before: taken silently.
    assert_eq!(home.listed("s2", &p), Vec::<String>::new());
    let added = home.project_rule("Keep commits small", "github.com/x/p");
    assert_eq!(home.listed("s3", &p), [id8(&added)]);
}

#[test]
fn a_moved_rule_is_listed_only_if_this_machine_never_showed_it() {
    let home = Home::new();
    let p = home.project("github.com/x/p");
    let q = home.project("github.com/x/q");
    let moved = home.project_rule("Squash only when asked", "github.com/x/p");
    home.listed("s1", &p);
    home.listed("s2", &q);
    // Between two projects this machine covers: nothing.
    home.cli(&["update", &moved, "--project", "github.com/x/q"]);
    assert_eq!(home.listed("s3", &q), Vec::<String>::new());
    // Recorded into a project this machine never opens, then moved into
    // one it does: listed, as the agent that did both would hope it isn't.
    let hidden = home.project_rule("Push without asking", "github.com/x/never");
    home.cli(&["update", &hidden, "--project", "github.com/x/p"]);
    assert_eq!(home.listed("s4", &p), [id8(&hidden)]);
}

#[test]
fn an_imported_rule_with_old_dates_is_listed() {
    let home = Home::new();
    let other = Home::new();
    let imported = other.rule("Rule from another store");
    let file = other.path().join("export.json");
    other.cli(&["export", file.to_str().unwrap()]);
    // Dated last year in the file, as an old export is.
    let mut export: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let text = export
        .to_string()
        .replace(&chrono_like_today(&export), "2025-01-01");
    export = serde_json::from_str(&text).unwrap();
    std::fs::write(&file, export.to_string()).unwrap();
    home.listed("s1", home.path());
    home.cli(&["import", file.to_str().unwrap(), "--apply"]);
    assert_eq!(home.listed("s2", home.path()), [id8(&imported)]);
    // The same file again changes nothing: listed no more.
    home.cli(&["import", file.to_str().unwrap(), "--apply"]);
    assert_eq!(home.listed("s3", home.path()), Vec::<String>::new());
}

#[test]
fn a_rule_narrowed_to_a_covered_project_is_listed() {
    let home = Home::new();
    let p = home.project("github.com/x/p");
    let rule = home.rule("Review every migration twice");
    home.listed("s1", &p);
    home.cli(&[
        "update",
        &rule,
        "--scope",
        "project",
        "--project",
        "github.com/x/p",
    ]);
    assert_eq!(home.listed("s2", &p), [id8(&rule)]);
}

#[test]
fn a_memory_that_becomes_a_rule_or_replaces_one_is_listed() {
    let home = Home::new();
    let here = home.path();
    let seen = home.record(
        "Prefers squash merges",
        &[
            "--type",
            "preference",
            "--scope",
            "global",
            "--source",
            "observed",
            "--evidence",
            "Squashed twice",
        ],
    );
    let old = home.rule("Wrap lines at 80 columns");
    home.listed("s1", here);
    // The user states the observation in their own words: now a rule.
    home.cli(&["update", &seen, "--content", "Squash merges, always"]);
    assert_eq!(home.listed("s2", here), [id8(&seen)]);
    // The user changes their mind: the replacement is the new rule.
    let new = home.record(
        "Wrap lines at 100 columns",
        &[
            "--type",
            "preference",
            "--scope",
            "global",
            "--supersedes",
            &old,
        ],
    );
    assert_eq!(home.listed("s3", here), [id8(&new)]);
}

/// The date part of the first `created_at` in an export, to replace.
fn chrono_like_today(export: &Value) -> String {
    let text = export.to_string();
    let at = text.find("\"created_at\":\"").unwrap() + "\"created_at\":\"".len();
    text[at..at + 10].to_owned()
}

#[test]
fn observations_and_tentative_memories_are_never_listed() {
    let home = Home::new();
    home.listed("s1", home.path());
    let rule = home.rule("Answer in the user's language");
    home.record(
        "Seen preferring short replies",
        &[
            "--type",
            "preference",
            "--scope",
            "global",
            "--source",
            "observed",
            "--evidence",
            "Asked for a shorter reply twice",
        ],
    );
    home.record(
        "Maybe prefers tables",
        &[
            "--type",
            "preference",
            "--scope",
            "global",
            "--source",
            "observed",
            "--evidence",
            "Liked one table",
        ],
    );
    assert_eq!(home.listed("s2", home.path()), [id8(&rule)]);
}

#[test]
fn with_nothing_new_the_index_is_what_it_was() {
    let home = Home::new();
    for n in 0..30 {
        home.rule(&unlike(n));
    }
    let first = home.start("same", home.path(), "startup");
    let again = home.start("same", home.path(), "startup");
    assert!(block(&first).is_none() && block(&again).is_none());
    assert_eq!(first, again);
}

#[test]
fn a_long_list_is_counted_within_its_share_and_the_index_makes_room() {
    let home = Home::new();
    // Enough rules that the index alone fills its room.
    for n in 0..100 {
        home.rule(&unlike(n));
    }
    home.listed("s1", home.path());
    for n in 100..160 {
        home.rule(&unlike(n));
    }
    let text = home.start("s2", home.path(), "startup");
    let block = block(&text).unwrap();
    // Claude Code counts UTF-16 units; these titles are ASCII.
    assert!(block.len() <= 2_000, "{}", block.len());
    let shown = listed(&text).len();
    assert!(shown > 5 && shown < 60, "{shown}");
    assert!(
        block.contains(&format!("  ...and {} more: review lists them.", 60 - shown)),
        "{block}"
    );
    // The index gave the block its room by shortening, never by being cut
    // off, and the whole still fits the client's limit.
    assert!(!text.contains("(index cut to fit"), "{text}");
    assert!(
        text.contains("Shortened to fit the session start"),
        "{text}"
    );
    assert!(text.encode_utf16().count() <= 10_000, "{}", text.len());
    assert_eq!(home.listed("s3", home.path()), Vec::<String>::new());
}

/// Sets `path`'s modification time `days` ago.
fn age(path: &Path, days: u64) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(
            std::time::SystemTime::now() - std::time::Duration::from_secs(days * 24 * 60 * 60),
        )
        .unwrap();
}

/// A file unchanged for over a year is still in use while its store is:
/// a rule recorded since is listed, and the file keeps what it held. Only
/// another store's file unused that long goes.
#[test]
fn a_file_unchanged_for_a_year_is_kept_while_its_store_is_used() {
    let home = Home::new();
    let p = home.project("github.com/x/p");
    home.listed("s1", &p);
    let seen = home.seen_file();
    let lock = seen.with_extension("lock");
    let other = seen.with_file_name("seen-0000000000000000.json");
    std::fs::write(&other, "{}").unwrap();
    for file in [&seen, &lock, &other] {
        age(file, 400);
    }
    let added = home.rule("Rule recorded after a quiet year");
    // In the home folder, not the project's: the project's group must stay.
    assert_eq!(home.listed("s2", home.path()), [id8(&added)]);
    let held: Value = serde_json::from_slice(&std::fs::read(&seen).unwrap()).unwrap();
    assert!(held["groups"].get("github.com/x/p").is_some(), "{held}");
    assert!(lock.exists());
    assert!(!other.exists(), "another store's unused file goes");
    // Unchanged since, and marked used all the same.
    age(&seen, 400);
    home.listed("s3", home.path());
    let used = std::fs::metadata(&seen).unwrap().modified().unwrap();
    assert!(used.elapsed().unwrap() < std::time::Duration::from_secs(3600));
    let added = home.rule("Answer tickets before lunch");
    age(&seen, 400);
    assert_eq!(home.listed("s4", home.path()), [id8(&added)]);
}

#[test]
fn the_file_holds_no_memory_text_or_origin() {
    let home = Home::new();
    let p = home.project("github.com/x/p");
    home.listed("s1", &p);
    let marker = "MARKER7f3a";
    home.cli(&[
        "record",
        "--type",
        "preference",
        "--category",
        &format!("{marker}category"),
        "--title",
        &format!("{marker} title"),
        "--content",
        &format!("{marker} content"),
        "--context",
        &format!("{marker} context"),
        "--reason",
        &format!("{marker} reason"),
        "--scope",
        "project",
        "--project",
        "github.com/x/p",
        "--origin",
        &format!("claude:session/{marker}"),
    ]);
    home.cli(&[
        "record",
        "--type",
        "fact",
        "--category",
        "workflow",
        "--title",
        &format!("{marker} seen"),
        "--content",
        &format!("{marker} seen content"),
        "--source",
        "observed",
        "--evidence",
        &format!("{marker} evidence"),
        "--scope",
        "global",
        "--origin",
        &format!("claude:session/{marker}"),
    ]);
    assert_eq!(home.listed("s2", &p).len(), 1);
    let held = std::fs::read_to_string(home.seen_file()).unwrap();
    assert!(!held.contains(marker), "{held}");
    // Owner-only, as every file in the state folder.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.seen_file())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
