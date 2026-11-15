//! Undoing an import with `forget --import`: what it deletes, gives back
//! and keeps, by what happened to each memory since the import.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::PathBuf, process::Command};
use tempfile::{TempDir, tempdir};

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

    fn value(&self, args: &[&str]) -> Value {
        let out = self.command(args).output().unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// Records a global memory and returns its id.
    fn record(&self, category: &str, content: &str, extra: &[&str]) -> String {
        let mut args = vec![
            "record",
            "--category",
            category,
            "--scope",
            "global",
            "--title",
            content,
            "--content",
            content,
        ];
        args.extend_from_slice(extra);
        let out = self.value(&args);
        assert_eq!(out["stored"], true, "{out}");
        out["id"].as_str().unwrap().to_owned()
    }

    fn export(&self) -> Value {
        let out = self.command(&["export", "-"]).output().unwrap();
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// Applies `document` as the file `name`, taking the file's side of a
    /// conflict; returns the import's name.
    fn import(&self, name: &str, document: &Value) -> String {
        let file = self.root.path().join(name);
        fs::write(&file, document.to_string()).unwrap();
        let summary = self.value(&[
            "import",
            file.to_str().unwrap(),
            "--apply",
            "--on-conflict",
            "use-file",
        ]);
        assert_eq!(summary["applied"], true, "{summary}");
        summary["import"].as_str().unwrap_or_default().to_owned()
    }

    fn undo(&self, named: &str) -> Value {
        self.value(&["forget", "--import", named])
    }

    /// The stored memory `id`, if any.
    fn memory(&self, id: &str) -> Option<Value> {
        let read = self.value(&["read", id, "--status", "all"]);
        read["memories"].get(0).cloned()
    }

    fn history(&self, id: &str) -> Vec<Value> {
        let shown = self.value(&["history", id]);
        let memory = shown["memories"]
            .get(0)
            .or_else(|| shown["forgotten"].get(0))
            .unwrap();
        memory["history"].as_array().unwrap().clone()
    }
}

/// The id of test memory `n`.
fn id(n: u32) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}

/// A memory for a file: `template` as a new memory `n` titled `text`,
/// stated in `origins`.
fn new(template: &Value, n: u32, text: &str, origins: &[&str]) -> Value {
    let mut memory = template.clone();
    memory["id"] = json!(id(n));
    memory["title"] = json!(text);
    memory["content"] = json!(text);
    memory["origins"] = json!(
        origins
            .iter()
            .map(|o| json!({"origin": o, "first_seen": "2026-01-01 00:00:00"}))
            .collect::<Vec<_>>()
    );
    memory
}

/// `document` with memory `id` changed by `edit`.
fn edited(document: &Value, id: &str, edit: impl Fn(&mut Value)) -> Value {
    let mut document = document.clone();
    for memory in document["memories"].as_array_mut().unwrap() {
        if memory["id"] == id {
            edit(memory);
        }
    }
    document
}

/// `document` with only the memories `ids`.
fn only(document: &Value, ids: &[&str]) -> Value {
    let mut document = document.clone();
    document["memories"]
        .as_array_mut()
        .unwrap()
        .retain(|m| ids.contains(&m["id"].as_str().unwrap()));
    document.as_object_mut().unwrap().remove("history");
    document
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

// Each memory an import added or replaced, by what happened to it since:
// the acceptance cases and the table of what an undo keeps.
#[test]
fn an_undo_keeps_what_changed_since_and_undoes_the_rest() {
    let s = Store::new();
    let replaced = s.record("tea", "Drinks green tea", &["--origin", "chat/one"]);
    let first = s.export();
    let template = first["memories"][0].clone();
    let mut file = edited(&first, &replaced, |m| {
        m["content"] = json!("Drinks black tea");
        m["origins"] = json!([{"origin": "chat/x", "first_seen": "2026-01-01 00:00:00"}]);
    });
    let added = [
        (1, "Rows on Sundays", &["chat/x", "chat/w"][..]),
        (2, "Keeps a paper diary", &["chat/x"]),
        (3, "Bakes sourdough bread", &["chat/x"]),
        (4, "Uses a fountain pen", &["chat/x"]),
        (5, "Collects old maps", &["chat/x"]),
        (6, "Plays the cello", &["chat/x", "chat/p"]),
        (7, "Grows tomatoes", &["chat/x"]),
        (8, "Reads poetry at night", &["chat/x"]),
        (9, "Swims in cold water", &["chat/x"]),
        (10, "Builds with make", &["chat/x"]),
        (11, "Writes in pencil", &["chat/x"]),
        (12, "Writes in ink", &["chat/x"]),
    ];
    for (n, text, origins) in added {
        file["memories"]
            .as_array_mut()
            .unwrap()
            .push(new(&template, n, text, origins));
    }
    // Memory 10 belongs to a project; memory 11 came replaced by 12.
    file = edited(&file, &id(10), |m| {
        m["scope"] = json!("project");
        m["project"] = json!("github.com/acme/old");
    });
    file = edited(&file, &id(11), |m| {
        m["status"] = json!("superseded");
        m["superseded_by"] = json!(id(12));
    });
    let named = s.import("shared.json", &file);
    let after = s.export();

    // What happens to each since.
    s.value(&["update", &id(2)]);
    s.value(&["update", &id(3), "--content", "Bakes rye bread"]);
    s.record("pens", "Uses a gel pen", &["--supersedes", &id(4)]);
    let other = s.record("maps", "Draws maps by hand", &[]);
    s.value(&["update", &id(5), "--distinct-from", &other]);
    s.value(&["forget", "--origin", "chat/p"]);
    // A later import finds memory 7 identical but stated in another
    // conversation too; another replaces memory 8.
    let seven = edited(&only(&after, &[&id(7)]), &id(7), |m| {
        m["origins"] = json!([
            {"origin": "chat/x", "first_seen": "2026-01-01 00:00:00"},
            {"origin": "chat/z", "first_seen": "2026-01-03 00:00:00"}
        ]);
    });
    s.import("garden.json", &seven);
    let eight = edited(&only(&after, &[&id(8)]), &id(8), |m| {
        m["content"] = json!("Reads novels at night");
    });
    let later = s.import("reading.json", &eight);
    assert!(!later.is_empty());
    s.value(&[
        "update",
        &id(9),
        "--content",
        "Swims in the lake",
        "--drop-history",
    ]);
    let moved = s
        .command(&[
            "project",
            "move",
            "github.com/acme/old",
            "github.com/acme/new",
        ])
        .output()
        .unwrap();
    assert!(moved.status.success(), "{moved:?}");
    // Its replacement forgotten, the user takes memory 11 back.
    s.value(&["forget", &id(12)]);
    s.value(&["update", &id(11)]);

    let reply = s.undo(&named);
    assert_eq!(reply["import"], named.as_str(), "{reply}");
    assert_eq!(strings(&reply["ids"]), [id(1), id(6)], "{reply}");
    assert_eq!(
        strings(&reply["restored"]),
        std::slice::from_ref(&replaced),
        "{reply}"
    );
    let kept: BTreeMap<String, String> =
        serde_json::from_value(reply["kept_since"].clone()).unwrap();
    let expected: BTreeMap<String, String> = [
        (2, "confirm"),
        (3, "update"),
        (4, "supersede"),
        (5, "distinct"),
        (7, "origin"),
        (8, "import"),
        (9, "update"),
        (10, "move"),
        (11, "revive"),
    ]
    .into_iter()
    .map(|(n, op)| (id(n), op.to_owned()))
    .collect();
    assert_eq!(kept, expected, "{reply}");
    assert!(
        reply["next"]
            .as_str()
            .unwrap()
            .contains("undo that one first"),
        "{reply}"
    );
    // The memories it deleted are in the copy made just before.
    let backup = reply["backup"].as_str().unwrap();
    let copy = rusqlite::Connection::open(s.db.with_file_name("backups").join(backup)).unwrap();
    let held: i64 = copy
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE id IN (?1,?2)",
            [id(1), id(6)],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(held, 2);
    assert!(s.memory(&id(1)).is_none());
    // The replaced memory is back as it was, with its own conversation.
    let back = s.memory(&replaced).unwrap();
    assert_eq!(back["content"], "Drinks green tea", "{back}");
    assert_eq!(back["origins"], json!(["chat/one"]), "{back}");
    let created = first["memories"][0]["created_at"].as_str().unwrap();
    assert_eq!(
        s.export()["memories"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == replaced.as_str())
            .unwrap()["created_at"],
        created
    );
    // Its history says what the undo did, by the import's name.
    let undone: Vec<Value> = s
        .history(&replaced)
        .into_iter()
        .filter(|e| e.get("undoes").is_some())
        .collect();
    assert!(!undone.is_empty());
    assert!(undone.iter().all(|e| e["undoes"] == named.as_str()));
}

// Patterns: one built on an added memory goes stale when the undo deletes
// it, as when a memory is forgotten; an added pattern that went stale since
// is deleted with it; a replaced pattern whose earlier support was
// forgotten since is kept.
#[test]
fn an_undo_stales_dependents_and_keeps_a_pattern_it_cannot_give_back() {
    let s = Store::new();
    let observed = |text: &str, origin: &str| {
        s.record(
            "habits",
            text,
            &[
                "--type",
                "behavior",
                "--source",
                "observed",
                "--evidence",
                "Seen in a session",
                "--origin",
                origin,
            ],
        )
    };
    let b1 = observed("Opens the terminal first", "chat/b1");
    let b2 = observed("Reads the diff before pushing", "chat/b2");
    let b3 = observed("Runs the tests twice", "chat/b3");
    let b4 = observed("Keeps commits small", "chat/b4");
    let pattern = |text: &str, supports: [&str; 2]| {
        s.record(
            "habits",
            text,
            &[
                "--type",
                "pattern",
                "--source",
                "inferred",
                "--evidence-id",
                supports[0],
                "--evidence-id",
                supports[1],
            ],
        )
    };
    let q = pattern("Works carefully before sharing", [&b3, &b4]);
    let first = s.export();
    let observation = first["memories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == b1.as_str())
        .unwrap()
        .clone();
    let mut file = edited(&first, &q, |m| {
        m["evidence_ids"] = json!([b3, b2]);
    });
    let a1 = new(&observation, 1, "Writes notes in markdown", &["chat/x"]);
    let a3 = new(&observation, 3, "Names branches by type", &["chat/x3"]);
    let mut a2 = new(&observation, 2, "Keeps tidy notes", &[]);
    a2["type"] = json!("pattern");
    a2["source"] = json!("inferred");
    a2["evidence"] = json!("");
    a2["evidence_ids"] = json!([id(1), b1]);
    for memory in [a1, a3, a2] {
        file["memories"].as_array_mut().unwrap().push(memory);
    }
    let named = s.import("habits.json", &file);
    // A pattern the user builds on an added memory.
    let p = pattern("Organises work by name", [&id(3), &b2]);
    // The added pattern loses a support's wording: stale.
    s.value(&["update", &b1, "--content", "Opens the editor first"]);
    // The replaced pattern's earlier support goes.
    s.value(&["forget", &b4]);

    let reply = s.undo(&named);
    let mut deleted = strings(&reply["ids"]);
    deleted.sort();
    assert_eq!(deleted, [id(1), id(2), id(3)], "{reply}");
    assert_eq!(
        reply["kept_since"],
        json!({q.clone(): "support gone"}),
        "{reply}"
    );
    assert!(strings(&reply["staled"]).contains(&p), "{reply}");
}

// A name no import here gave, or one from before imports had names, is
// refused with the reason, and nothing is written.
#[test]
fn an_unknown_import_is_refused_with_the_reason() {
    let s = Store::new();
    s.record("tea", "Drinks green tea", &[]);
    let unknown = s.undo("shared.json@2026-01-01T00:00:00Z.0123abcd");
    assert_eq!(unknown["deleted"], false, "{unknown}");
    assert!(
        unknown["reason"]
            .as_str()
            .unwrap()
            .starts_with("No memory carries this import"),
        "{unknown}"
    );
    let old = s.undo("2026-01-01 00:00:00");
    assert!(
        old["reason"]
            .as_str()
            .unwrap()
            .contains("before imports had names"),
        "{old}"
    );
    assert!(old.get("backup").is_none());
}

// A restore into an empty store is an import like any other: undoing it
// empties the store, after a copy that holds every memory.
#[test]
fn undoing_a_restore_leaves_a_copy_of_everything_it_deleted() {
    let s = Store::new();
    for text in ["Drinks green tea", "Walks at dawn", "Reads on trains"] {
        s.record("life", text, &[]);
    }
    let backup = s.export();
    let fresh = Store::new();
    let named = fresh.import("backup.json", &backup);
    let reply = fresh.undo(&named);
    assert_eq!(reply["count"], 3, "{reply}");
    assert!(fresh.export()["memories"].as_array().unwrap().is_empty());
    let copy = rusqlite::Connection::open(
        fresh
            .db
            .with_file_name("backups")
            .join(reply["backup"].as_str().unwrap()),
    )
    .unwrap();
    let held: i64 = copy
        .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
        .unwrap();
    assert_eq!(held, 3);
    // Again: done already, nothing left to do.
    let again = fresh.undo(&named);
    assert_eq!(again["deleted"], false, "{again}");
    assert!(
        again["reason"].as_str().unwrap().contains("undone already"),
        "{again}"
    );
}

// Two imports changed one memory and the user undoes the earlier first:
// it is kept for the later one; undoing the later gives back the earlier's
// version and says so; undoing the earlier again ends where the user began.
#[test]
fn undoing_two_imports_in_either_order_ends_before_both() {
    for kind in ["replaced", "added"] {
        let s = Store::new();
        let seed = s.record("tea", "Drinks green tea", &["--origin", "chat/one"]);
        let first = s.export();
        let target = if kind == "replaced" {
            seed.clone()
        } else {
            id(1)
        };
        let mut x = first.clone();
        if kind == "replaced" {
            x = edited(&x, &seed, |m| m["content"] = json!("Drinks black tea"));
        } else {
            let memory = new(&first["memories"][0], 1, "Drinks oolong tea", &["chat/one"]);
            x["memories"].as_array_mut().unwrap().push(memory);
        }
        let named_x = s.import("x.json", &x);
        let y = edited(&s.export(), &target, |m| {
            m["content"] = json!("Drinks white tea")
        });
        let named_y = s.import("y.json", &y);

        let undo_x = s.undo(&named_x);
        assert_eq!(
            undo_x["kept_since"],
            json!({target.clone(): "import"}),
            "{kind}: {undo_x}"
        );
        let undo_y = s.undo(&named_y);
        assert_eq!(
            strings(&undo_y["restored"]),
            std::slice::from_ref(&target),
            "{kind}: {undo_y}"
        );
        let next = undo_y["next"].as_str().unwrap();
        assert!(
            next.contains(&format!("user-memory forget --import \"{named_x}\"")),
            "{kind}: {next}"
        );
        let again = s.undo(&named_x);
        assert_eq!(again["kept"], json!([]), "{kind}: {again}");
        if kind == "replaced" {
            assert_eq!(
                strings(&again["restored"]),
                std::slice::from_ref(&seed),
                "{kind}: {again}"
            );
            assert_eq!(s.memory(&seed).unwrap()["content"], "Drinks green tea");
        } else {
            assert_eq!(strings(&again["ids"]), [id(1)], "{kind}: {again}");
            assert!(s.memory(&id(1)).is_none());
        }
    }
}

// History shows the import's name and the origins it brought, and no
// replaced values for a memory it added; an undo's events name it as
// undone. An export leaves the names and what only an undo here reads out.
#[test]
fn history_and_exports_show_an_import_and_its_undo_as_they_are() {
    let s = Store::new();
    let seed = s.record("tea", "Drinks green tea", &["--origin", "chat/one"]);
    let first = s.export();
    let mut file = edited(&first, &seed, |m| m["content"] = json!("Drinks black tea"));
    let memory = new(&first["memories"][0], 1, "Walks at dawn", &["chat/x"]);
    file["memories"].as_array_mut().unwrap().push(memory);
    let named = s.import("shared.json", &file);

    let added = s.history(&id(1));
    let own = added.iter().find(|e| e.get("import").is_some()).unwrap();
    assert!(own.get("before").is_none(), "{own}");
    assert_eq!(own["import_origins"], json!(["chat/x"]), "{own}");
    let replaced = s.history(&seed);
    let own = replaced.iter().find(|e| e.get("import").is_some()).unwrap();
    assert_eq!(own["before"]["content"], "Drinks green tea", "{own}");

    let exported = s.export();
    let history = exported["history"].to_string();
    for key in ["shared.json", "import_origins", "undo_kept"] {
        assert!(!history.contains(key), "{key}: {history}");
    }
    let events = exported["history"].as_array().unwrap();
    for event in events.iter().filter(|e| e["op"] == "import") {
        for key in [
            "import_origins",
            "origins",
            "created_at",
            "last_confirmed_at",
        ] {
            assert!(event["before"].get(key).is_none(), "{key}: {event}");
        }
        assert!(event["imported"].is_null(), "{event}");
    }

    s.undo(&named);
    let undone = s.history(&seed);
    assert!(
        undone.iter().any(|e| e["undoes"] == named.as_str()),
        "{undone:?}"
    );
    let forgotten = s.history(&id(1));
    assert_eq!(forgotten.last().unwrap()["op"], "forget");
    assert_eq!(forgotten.last().unwrap()["undoes"], named.as_str());
}

// The command an applied import prints to undo it runs as printed, for a
// file whose name has spaces and characters a shell would take apart.
#[cfg(unix)]
#[test]
fn the_printed_undo_command_runs_as_printed() {
    let s = Store::new();
    s.record("tea", "Drinks green tea", &[]);
    let first = s.export();
    let mut file = first.clone();
    file["memories"].as_array_mut().unwrap().push(new(
        &first["memories"][0],
        1,
        "Walks at dawn",
        &["chat/x"],
    ));
    let path = s.root.path().join("my rules @ 'v2' $HOME.json");
    fs::write(&path, file.to_string()).unwrap();
    let summary = s.value(&["import", path.to_str().unwrap(), "--apply"]);
    let next = summary["next"].as_str().unwrap();
    let command = next
        .split('`')
        .find(|part| part.starts_with("user-memory forget --import"))
        .unwrap();
    let binary = env!("CARGO_BIN_EXE_user-memory");
    let line = command.replacen("user-memory", &format!("'{binary}'"), 1);
    let out = s.command(&[]).arg0_shell(&line);
    assert!(out.status.success(), "{line}: {out:?}");
    let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(strings(&reply["ids"]), [id(1)], "{reply}");
}

/// Runs a shell line with the store's environment.
trait Shell {
    fn arg0_shell(self, line: &str) -> std::process::Output;
}

impl Shell for Command {
    fn arg0_shell(self, line: &str) -> std::process::Output {
        let mut shell = Command::new("sh");
        shell.arg("-c").arg(line);
        for (key, value) in self.get_envs() {
            match value {
                Some(value) => shell.env(key, value),
                None => shell.env_remove(key),
            };
        }
        if let Some(dir) = self.get_current_dir() {
            shell.current_dir(dir);
        }
        shell.output().unwrap()
    }
}

// Whether the import added or replaced a memory is what the import recorded,
// not what history is left: a memory replaced long after it was recorded,
// whose earlier events went with the year history keeps, is given back, and
// a memory forgotten and then brought back by the import is deleted again.
#[test]
fn an_undo_tells_added_from_replaced_by_the_import_alone() {
    let s = Store::new();
    let old = s.record("editor", "Uses Helix", &["--origin", "chat/a"]);
    let gone = s.record("shell", "Uses fish", &["--origin", "chat/b"]);
    let before = s.export();
    s.value(&["forget", &gone]);
    let file = edited(&before, &old, |memory| {
        memory["title"] = json!("Uses Zed");
        memory["content"] = json!("Uses Zed");
    });
    let named = s.import("tools.json", &file);
    // As history::prune drops the events past a year.
    rusqlite::Connection::open(&s.db)
        .unwrap()
        .execute("DELETE FROM memory_events WHERE imported IS NULL", [])
        .unwrap();
    let undo = s.undo(&named);
    assert_eq!(
        strings(&undo["restored"]),
        std::slice::from_ref(&old),
        "{undo}"
    );
    assert_eq!(strings(&undo["ids"]), std::slice::from_ref(&gone), "{undo}");
    assert_eq!(s.memory(&old).unwrap()["content"], "Uses Helix");
    assert!(s.memory(&gone).is_none(), "{undo}");
}

// A later import that found a memory identical and only confirmed it again
// left nothing to undo: the memory is kept as that import also carried it,
// and the reply sends the user to no other undo.
#[test]
fn a_memory_a_later_import_only_confirmed_is_kept_with_no_undo_to_run_first() {
    let s = Store::new();
    s.record("tea", "Drinks green tea", &[]);
    let template = s.export()["memories"][0].clone();
    let mut file = s.export();
    file["memories"] = json!([new(&template, 1, "Knits scarves", &["chat/x"])]);
    let named = s.import("craft.json", &file);
    let again = edited(&only(&s.export(), &[&id(1)]), &id(1), |m| {
        m["last_confirmed_at"] = json!("2099-01-01 00:00:00");
    });
    let later = s.import("craft-again.json", &again);
    assert!(later.is_empty(), "the later import changed nothing to undo");
    let reply = s.undo(&named);
    assert_eq!(reply["kept_since"], json!({id(1): "import"}), "{reply}");
    assert!(reply.get("next").is_none(), "{reply}");
    assert!(s.memory(&id(1)).is_some());
}
