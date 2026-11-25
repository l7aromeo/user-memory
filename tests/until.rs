#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

//! Conditions, through the real binary: what `record` and `update` take as
//! `until`, how each kind is checked, and the words that say so.

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
#[path = "support/session.rs"]
mod session;
#[path = "support/time.rs"]
mod time;
use session::Serve;

use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
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

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .args(args)
            .sandboxed(self.root.path())
            .env("USER_MEMORY_DB", &self.db)
            .env_remove("USER_MEMORY_URL")
            .env_remove("USER_MEMORY_TOKEN_FILE");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn value(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// The command in the user's zone for these tests, UTC+7 all year.
    fn in_jakarta(&self, args: &[&str]) -> Value {
        let out = self.command(args).env("TZ", ZONE).output().unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// The stored condition of memory `id`: kind, value and project.
    fn stored(&self, id: &str) -> (String, String, String) {
        Connection::open(&self.db)
            .unwrap()
            .query_row(
                "SELECT until_kind,until_value,until_project FROM memories WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    }

    /// Memory `id` as `read` shows it.
    fn read(&self, id: &str) -> Value {
        self.value(&["read", id, "--status", "all"])["memories"][0].clone()
    }

    fn serve(&self) -> Serve {
        let mut serve = self.command(&["serve"]);
        serve.env("TZ", ZONE).current_dir(self.root.path());
        Serve::spawn(serve)
    }

    fn help(&self, command: &str) -> String {
        String::from_utf8(self.run(&[command, "--help"]).stdout).unwrap()
    }

    /// A global rule's `record` arguments, followed by `extra`.
    fn record(&self, extra: &[&str]) -> Value {
        let mut args = vec![
            "record",
            "--type",
            "preference",
            "--category",
            "workflow",
            "--scope",
            "global",
            "--title",
            "Push the release branch without asking",
            "--content",
            "Push the release branch without asking.",
        ];
        args.extend_from_slice(extra);
        self.value(&args)
    }
}

/// A zone without daylight saving time, so its days end at 17:00 UTC.
const ZONE: &str = "Asia/Jakarta";

/// Today in [`ZONE`], and the stored instant it ends at: the next local
/// midnight, in UTC.
fn today_in_zone() -> (String, String) {
    let zone = jiff::tz::TimeZone::get(ZONE).unwrap();
    let today = jiff::Timestamp::now().to_zoned_datetime(zone).date();
    let end = format!("{}T17:00:00Z", today);
    (today.to_string(), end)
}

const TWO_KINDS: &str = "until names one kind of condition: a date, a tag or a text";

/// Each flag's help says what it does, so `--help` alone is enough to use
/// it; the texts are pinned here as the docs quote them.
#[test]
fn each_condition_flag_says_what_it_does() {
    let s = Store::new();
    let flags = [
        (
            "--until-date <DATE>",
            "End the memory after this day, YYYY-MM-DD, at the end of that day in your time zone; or at an RFC 3339 time with its offset. Give one of --until-date, --until-tag or --until-text.",
        ),
        (
            "--until-tag <TAG>",
            "End the memory when a checkout of the project has this tag: the literal name, such as v0.10.0, never a pattern.",
        ),
        (
            "--until-project <PROJECT>",
            "With --until-tag: the project whose tags count. A project memory defaults to its own; a global memory must name one.",
        ),
        (
            "--until-text <TEXT>",
            "End the memory when this happens, at most 120 characters, such as \"the parser's fix ships\".",
        ),
    ];
    for command in ["record", "update"] {
        let help = s.help(command);
        for (flag, text) in flags {
            assert!(help.contains(flag), "{command}: {flag}\n{help}");
            assert!(help.contains(text), "{command}: {text}\n{help}");
        }
    }
    let update = s.help("update");
    for (flag, text) in [
        (
            "--no-until",
            "Clear the memory's condition, so it holds until changed; reopens it if it had ended.",
        ),
        (
            "--ended <EVIDENCE>",
            "End the memory's text condition, with the evidence that it happened: what was done and seen, at most 2,000 characters.",
        ),
    ] {
        assert!(update.contains(flag), "{flag}\n{update}");
        assert!(update.contains(text), "{text}\n{update}");
    }
    let record = s.help("record");
    assert!(!record.contains("--no-until"), "{record}");
    assert!(!record.contains("--ended"), "{record}");
}

/// A condition is one kind: two in one call are refused, whichever two,
/// and nothing is stored.
#[test]
fn two_kinds_in_one_call_are_refused() {
    let s = Store::new();
    for pair in [
        ["--until-date", "2099-01-01", "--until-tag", "v1.0.0"],
        ["--until-date", "2099-01-01", "--until-text", "it ships"],
        ["--until-tag", "v1.0.0", "--until-text", "it ships"],
    ] {
        let out = s.record(&pair);
        assert_eq!(out["stored"], false, "{pair:?}: {out}");
        assert_eq!(out["reason"], TWO_KINDS, "{pair:?}: {out}");
    }
    let out = s.run(&[
        "update",
        "00000000",
        "--no-until",
        "--until-date",
        "2099-01-01",
    ]);
    assert!(!out.status.success(), "{out:?}");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("the argument '--no-until' cannot be used with '--until-date <DATE>'"),
        "{stderr}"
    );
}

/// The refusals a condition can meet, each in its own words.
#[test]
fn a_condition_that_cannot_hold_is_refused_in_its_own_words() {
    let s = Store::new();
    let long = "x".repeat(121);
    for (extra, reason) in [
        (
            vec!["--until-tag", "v1.0.0"],
            "A global memory's tag condition names the project whose tags count".to_owned(),
        ),
        (
            vec!["--until-text", "it ships", "--until-project", "github.com/x/y"],
            "until's project goes with a tag: it names the project whose tags count".to_owned(),
        ),
        (
            vec!["--until-date", "2001-01-01"],
            "until's date has already passed, so this memory would end at once; give a later date"
                .to_owned(),
        ),
        (
            vec!["--until-date", "next friday"],
            "until's date is a day, such as 2026-11-20, which ends at the end of that day in your time zone, or an RFC 3339 time with its offset, such as 2026-11-20T18:00:00+07:00".to_owned(),
        ),
        (
            vec!["--until-tag", "v1.*", "--until-project", "github.com/x/y"],
            "until's tag \"v1.*\" is not a tag name: give the literal name, such as v0.10.0, never a pattern or a refs/ path".to_owned(),
        ),
        (
            vec!["--until-text", long.as_str()],
            "until's text has 121 characters; the limit is 120; say what has to happen in fewer words".to_owned(),
        ),
    ] {
        let out = s.record(&extra);
        assert_eq!(out["stored"], false, "{extra:?}: {out}");
        assert_eq!(out["reason"], reason.as_str(), "{extra:?}: {out}");
    }
}

/// Each kind is stored in its one form and echoed in the reply: a day as
/// the next local midnight in UTC ("for today" is today's date), a tag with
/// its project's key, a text cleaned. `read` shows the stored condition.
#[test]
fn each_kind_is_stored_and_echoed() {
    let s = Store::new();
    let (today, end) = today_in_zone();
    let out = s.in_jakarta(&[
        "record",
        "--category",
        "workflow",
        "--scope",
        "global",
        "--title",
        "Skip the changelog check today",
        "--content",
        "Skip the changelog check today.",
        "--until-date",
        &today,
    ]);
    let day = out["id"].as_str().unwrap().to_owned();
    // The day ends at the next midnight in the zone, which is today's date
    // at 17:00 UTC; a run that crosses midnight there sees the next day.
    let (now_today, now_end) = today_in_zone();
    assert!(
        out["until"] == format!("until end of {today} ({end})")
            || out["until"] == format!("until end of {now_today} ({now_end})"),
        "{out}"
    );
    assert_eq!(s.stored(&day), ("date".into(), end.clone(), String::new()));
    assert_eq!(s.read(&day)["until"], json!({"date": end}));

    let out = s.record(&[
        "--until-tag",
        "v3.0.0",
        "--until-project",
        "https://GitHub.com/Example/Tool.git",
    ]);
    let tag = out["id"].as_str().unwrap().to_owned();
    assert_eq!(
        out["until"],
        "until tag v3.0.0 exists in github.com/example/tool"
    );
    assert_eq!(
        s.stored(&tag),
        (
            "tag".into(),
            "v3.0.0".into(),
            "github.com/example/tool".into()
        )
    );
    assert_eq!(
        s.read(&tag)["until"],
        json!({"tag": "v3.0.0", "project": "github.com/example/tool"})
    );

    // A project memory's tag counts in its own project.
    let out = s.value(&[
        "record",
        "--category",
        "workflow",
        "--scope",
        "project",
        "--project",
        "github.com/example/app",
        "--title",
        "Pin the parser at 2.3",
        "--content",
        "Pin the parser at 2.3.",
        "--until-tag",
        "v0.10.0",
    ]);
    let own = out["id"].as_str().unwrap().to_owned();
    assert_eq!(
        s.stored(&own),
        (
            "tag".into(),
            "v0.10.0".into(),
            "github.com/example/app".into()
        )
    );

    let out = s.value(&[
        "record",
        "--category",
        "tooling",
        "--scope",
        "global",
        "--title",
        "Keep the empty-rows workaround",
        "--content",
        "Keep the empty-rows workaround.",
        "--until-text",
        "  the parser's fix\n for empty rows ships ",
    ]);
    let text = out["id"].as_str().unwrap().to_owned();
    assert_eq!(
        out["until"],
        "until an agent reports, with evidence, that the parser's fix for empty rows ships"
    );
    assert_eq!(s.stored(&text).0, "text");

    // A memory without a condition stores none and shows none.
    let out = s.value(&[
        "record",
        "--category",
        "tooling",
        "--scope",
        "global",
        "--title",
        "Use tabs",
        "--content",
        "Use tabs.",
    ]);
    let plain = out["id"].as_str().unwrap().to_owned();
    assert!(out.get("until").is_none(), "{out}");
    assert_eq!(
        s.stored(&plain),
        (String::new(), String::new(), String::new())
    );
    assert!(s.read(&plain).get("until").is_none());
}

/// update keeps a condition it doesn't name, replaces one it gives and
/// clears one with --no-until; a condition equal to another memory's is no
/// duplicate of a memory without one.
#[test]
fn update_keeps_replaces_and_clears_a_condition() {
    let s = Store::new();
    let id = s.record(&["--until-text", "the release is out"])["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = s.value(&["update", &id, "--title", "Push the release branch freely"]);
    assert_eq!(out["updated"], true, "{out}");
    assert!(out.get("until").is_none(), "{out}");
    assert_eq!(s.stored(&id).1, "the release is out");

    let out = s.value(&["update", &id, "--until-date", "2099-01-31T12:00:00+07:00"]);
    assert_eq!(out["updated"], true, "{out}");
    assert_eq!(
        s.stored(&id),
        ("date".into(), "2099-01-31T05:00:00Z".into(), String::new())
    );

    let out = s.value(&["update", &id, "--no-until"]);
    assert_eq!(out["updated"], true, "{out}");
    assert_eq!(out["until"], Value::Null, "{out}");
    assert_eq!(s.stored(&id), (String::new(), String::new(), String::new()));

    // The same memory with and without a condition is two memories.
    let first = s.record(&[])["id"].as_str().unwrap().to_owned();
    let out = s.record(&[
        "--until-text",
        "the release is out",
        "--distinct-from",
        &first,
    ]);
    assert_eq!(out["stored"], true, "{out}");
    assert!(out.get("duplicate").is_none(), "{out}");
}

/// Said again with another condition is a change: confirms refuses it and
/// points to update; said again with the same one, or none, confirms.
#[test]
fn confirms_refuses_another_condition() {
    let s = Store::new();
    let id = s.record(&["--until-text", "the release is out"])["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = s.record(&["--confirms", &id, "--until-text", "the next release is out"]);
    assert_eq!(out["stored"], false, "{out}");
    assert_eq!(
        out["reason"],
        "until differs from the confirmed memory's condition; to change a memory's condition, update it"
    );
    for extra in [vec!["--until-text", "the release is out"], vec![]] {
        let mut args = vec!["--confirms", id.as_str()];
        args.extend(extra);
        let out = s.record(&args);
        assert_eq!(out["stored"], true, "{out}");
    }
}

/// Through the tools: a bare day ends in the zone serve runs in, null
/// clears a condition and an omitted until keeps it.
#[test]
fn the_tools_take_until_as_an_object_and_null_clears_it() {
    let s = Store::new();
    let mut serve = s.serve();
    let out = serve.call(
        "record",
        json!({"category":"workflow","scope":"global","title":"Merge on green CI","content":"Merge on green CI.","user_confirmed":true,"until":{"date":"2099-03-01"}}),
    );
    assert_eq!(out["stored"], true, "{out}");
    let id = out["id"].as_str().unwrap().to_owned();
    assert_eq!(
        out["until"],
        "until end of 2099-03-01 (2099-03-01T17:00:00Z)"
    );
    let out = serve.call(
        "record",
        json!({"category":"workflow","scope":"global","title":"Tag nightly builds","content":"Tag nightly builds.","user_confirmed":true,"until":{"date":"2099-03-01","text":"it ships"}}),
    );
    assert_eq!(out["reason"], TWO_KINDS, "{out}");
    let out = serve.call(
        "update",
        json!({"memory_id":id,"title":"Merge on green CI, no review","user_confirmed":true}),
    );
    assert_eq!(out["updated"], true, "{out}");
    assert_eq!(s.stored(&id).1, "2099-03-01T17:00:00Z");
    let out = serve.call(
        "update",
        json!({"memory_id":id,"until":null,"user_confirmed":true}),
    );
    assert_eq!(out["updated"], true, "{out}");
    assert_eq!(s.stored(&id).0, "");
    serve.close();
}

/// A project move carries the tag conditions that name the old key, so the
/// tags of the moved project still end them.
#[test]
fn a_project_move_carries_its_tag_conditions() {
    let s = Store::new();
    let global = s.record(&[
        "--until-tag",
        "v2.0.0",
        "--until-project",
        "github.com/example/old",
    ])["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = s.value(&[
        "record",
        "--category",
        "workflow",
        "--scope",
        "project",
        "--project",
        "github.com/example/old",
        "--title",
        "Pin the parser at 2.3",
        "--content",
        "Pin the parser at 2.3.",
        "--until-tag",
        "v1.0.0",
    ]);
    let own = out["id"].as_str().unwrap().to_owned();
    let out = s.run(&[
        "project",
        "move",
        "github.com/example/old",
        "github.com/example/new",
    ]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(s.stored(&global).2, "github.com/example/new");
    assert_eq!(s.stored(&own).2, "github.com/example/new");
}

/// A memory out of force, by an end record or by a date that has passed,
/// is absent from every channel that shows memories in force, and still
/// readable by asking for every status.
#[test]
fn a_memory_out_of_force_is_shown_nowhere_in_force() {
    let s = Store::new();
    let shown = s.record(&[])["id"].as_str().unwrap().to_owned();
    let ended = s.value(&[
        "record",
        "--category",
        "workflow",
        "--scope",
        "global",
        "--title",
        "Skip the slow tests until the parser ships",
        "--content",
        "Skip the slow tests until the parser ships.",
        "--until-text",
        "the parser ships",
    ])["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let dated = s.value(&[
        "record",
        "--category",
        "workflow",
        "--scope",
        "global",
        "--title",
        "Merge without review this week",
        "--content",
        "Merge without review this week.",
        "--until-date",
        "2099-01-01",
    ])["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Ends come through the store's own paths in a later step; here the
    // rows are written as those paths will write them.
    let conn = Connection::open(&s.db).unwrap();
    conn.execute(
        "INSERT INTO memory_ended (memory_id,how,evidence) VALUES (?1,'evidence','ran the release recipe: v2.0.0 tagged')",
        [&ended],
    )
    .unwrap();
    conn.execute(
        "UPDATE memories SET until_value='2001-01-01T00:00:00Z' WHERE id=?1",
        [&dated],
    )
    .unwrap();
    drop(conn);
    let out_of_force = [
        ("ended", "Skip the slow tests", &ended),
        ("dated", "Merge without review", &dated),
    ];
    let text = |args: &[&str]| {
        let out = s.command(args).current_dir(s.root.path()).output().unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    for (channel, output) in [
        ("index", text(&["index"])),
        ("read", text(&["read", "--category", "workflow"])),
        (
            "search",
            text(&["search", "merge skip slow tests review push"]),
        ),
        ("session start", text(&["session-start"])),
    ] {
        assert!(
            output.contains("Push the release branch"),
            "{channel}: {output}"
        );
        for (what, title, id) in out_of_force {
            assert!(
                !output.contains(title),
                "{channel} shows the {what} memory: {output}"
            );
            assert!(
                !output.contains(&id[..8]),
                "{channel} names the {what} memory: {output}"
            );
        }
    }
    // Asked for by id with every status, each is still there.
    for (_, title, id) in out_of_force {
        let title_read = s.read(id)["title"].as_str().unwrap().to_owned();
        assert!(title_read.starts_with(title), "{title_read}");
    }
    assert_eq!(s.read(&shown)["status"], "confirmed");
    // A memory out of force is no target for a restatement.
    let out = s.value(&[
        "record",
        "--category",
        "workflow",
        "--scope",
        "global",
        "--title",
        "Skip the slow tests until the parser ships",
        "--content",
        "Skip the slow tests until the parser ships.",
        "--confirms",
        &ended,
    ]);
    assert_eq!(out["stored"], false, "{out}");
    assert_eq!(
        out["reason"],
        "That memory's condition has ended; to put it in force again, update it with the user's yes, giving a new until or clearing it"
    );
}
