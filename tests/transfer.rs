//! Export and import through the CLI and the MCP tools.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
use tempfile::{TempDir, tempdir};

struct Store {
    root: TempDir,
    db: PathBuf,
}

impl Store {
    fn new() -> Self {
        let root = tempdir().unwrap();
        let db = root.path().join("store/memory.sqlite3");
        Self { root, db }
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .args(titled(args))
            .sandboxed(self.root.path())
            .env("USER_MEMORY_DB", &self.db)
            .env_remove("USER_MEMORY_URL")
            .env_remove("USER_MEMORY_TOKEN_FILE");
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> Output {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
    fn value(&self, args: &[&str]) -> Value {
        let value: Value = serde_json::from_slice(&self.ok(args).stdout).unwrap();
        if args.first() == Some(&"search") {
            return one_page(value, args.contains(&"--page-size"));
        }
        value
    }
    fn with_stdin(&self, args: &[&str], input: &[u8]) -> Output {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }
    fn seed(&self) {
        for args in [
            &["--content", "Prefer Rust", "--origin", "chat/x"][..],
            &["--content", "Answer briefly"],
        ] {
            let mut full = vec!["record", "--type", "preference", "--category", "style"];
            full.extend_from_slice(args);
            assert_eq!(self.value(&full)["stored"], true);
        }
    }
    fn memories(&self) -> Value {
        let out = self.ok(&["export", "-"]);
        let document: Value = serde_json::from_slice(&out.stdout).unwrap();
        document["memories"].clone()
    }
}

// The user backs up their memory to a file and restores it on a new machine.
#[test]
fn a_file_export_restores_into_a_fresh_store_after_a_preview() {
    let s = Store::new();
    s.seed();
    let file = s.root.path().join("backup.json");
    let summary = s.value(&["export", file.to_str().unwrap()]);
    assert_eq!(summary["exported"], true);
    assert_eq!(summary["memories"]["confirmed"], 2);
    assert_eq!(summary["file"], file.to_str().unwrap());
    // Mode bits are Unix's; on Windows the file is made private by
    // private::create_file, whose Windows tests cover it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let refused = s.run(&["export", file.to_str().unwrap()]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--force"));
    s.ok(&["export", file.to_str().unwrap(), "--force"]);

    let fresh = Store::new();
    fresh.value(&["search"]);
    let before = fs::read(&fresh.db).unwrap();
    let preview = fresh.value(&["import", file.to_str().unwrap()]);
    assert_eq!(preview["applied"], false, "{preview}");
    assert!(
        preview.get("next").is_none(),
        "a preview has no next step: {preview}"
    );
    assert_eq!(
        fs::read(&fresh.db).unwrap(),
        before,
        "a preview writes nothing"
    );
    let applied = fresh.value(&["import", file.to_str().unwrap(), "--apply"]);
    assert_eq!(applied["applied"], true, "{applied}");
    assert_eq!(
        applied["next"],
        "optionally run `user-memory reindex` to embed the new memories now; search embeds them as it goes"
    );
    assert_eq!(fresh.memories(), s.memories());
}

// In Docker the export travels through stdout and back in through stdin.
#[test]
fn stdio_export_and_import_round_trip_with_the_summary_on_stderr() {
    let s = Store::new();
    s.seed();
    let out = s.ok(&["export", "-"]);
    let document: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(document["format"], "user-memory-export");
    let summary: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(summary["memories"]["confirmed"], 2);
    let fresh = Store::new();
    let imported = fresh.with_stdin(&["import", "-", "--apply"], &out.stdout);
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let result: Value = serde_json::from_slice(&imported.stdout).unwrap();
    assert_eq!(result["applied"], true);
    assert_eq!(fresh.memories(), s.memories());
    let garbage = fresh.with_stdin(&["import", "-"], b"\xff\xfe not text");
    assert!(!garbage.status.success());
    assert!(String::from_utf8_lossy(&garbage.stderr).contains("UTF-8"));
}

// A client connected to a shared server has no store of its own to export.
/// Drives `user-memory serve` and returns each tool reply's text.
fn tools(store: &Store, calls: &[(&str, Value)]) -> Vec<Value> {
    let mut child = store
        .command(&["serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut send = |id: usize, method: &str, params: Value| {
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )
        .unwrap();
        input.flush().unwrap();
        loop {
            let mut line = String::new();
            assert!(output.read_line(&mut line).unwrap() > 0);
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == id {
                return message;
            }
        }
    };
    send(
        0,
        "initialize",
        json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}),
    );
    let mut replies = Vec::new();
    for (i, (name, arguments)) in calls.iter().enumerate() {
        let reply = send(
            i + 1,
            "tools/call",
            json!({"name":name,"arguments":arguments}),
        );
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        replies.push(serde_json::from_str(text).unwrap());
    }
    drop(input);
    let _ = child.wait();
    replies
}

fn exports(store: &Store) -> PathBuf {
    store.db.parent().unwrap().join("exports")
}

/// Creates `store`'s exports directory as private as an import needs it,
/// through the public `secure-file` command, so it holds on every platform.
fn private_exports(store: &Store) {
    let dir = exports(store);
    fs::create_dir(&dir).unwrap();
    let out = store.run(&["secure-file", dir.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
}

// An agent exports and imports only inside the exports directory, and only
// summaries reach its context.
// The tools take the same filters as the CLI, and still answer with
// summaries only.
#[test]
fn agents_filter_what_they_export_and_import() {
    let s = Store::new();
    s.seed();
    let replies = tools(
        &s,
        &[("export", json!({"name":"chat.json","origin":"chat/x"}))],
    );
    assert_eq!(replies[0]["memories"]["confirmed"], 1, "{}", replies[0]);
    assert_eq!(replies[0]["selection"], json!({"origin": "chat/x"}));
    let fresh = Store::new();
    fresh.value(&["search"]);
    private_exports(&fresh);
    fs::copy(
        exports(&s).join("chat.json"),
        exports(&fresh).join("chat.json"),
    )
    .unwrap();
    let replies = tools(
        &fresh,
        &[(
            "import",
            json!({"name":"chat.json","apply":true,"user_confirmed":true,"type":"fact"}),
        )],
    );
    assert_eq!(replies[0]["selected"], 0, "{}", replies[0]);
    assert_eq!(replies[0]["applied"], false);
    assert!(!replies[0].to_string().contains("Prefer Rust"));
}

#[test]
fn agents_export_and_import_by_name_and_see_only_summaries() {
    let s = Store::new();
    s.seed();
    let replies = tools(
        &s,
        &[
            ("export", json!({"name":"backup.json"})),
            ("export", json!({"name":"backup.json"})),
            ("export", json!({"name":"../escape.json"})),
            ("export", json!({"name":"/tmp/escape.json"})),
            ("review", json!({})),
        ],
    );
    assert_eq!(replies[4], json!({"groups":[]}));
    assert_eq!(replies[0]["exported"], true, "{}", replies[0]);
    assert_eq!(replies[0]["file"], "backup.json");
    assert!(
        replies[1]["error"]
            .as_str()
            .unwrap()
            .contains("already exists")
    );
    for refused in &replies[2..4] {
        assert!(refused.get("error").is_some(), "{refused}");
    }
    let dir = exports(&s);
    // As for the file above: mode bits on Unix, private.rs's tests on Windows.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    assert!(dir.join("backup.json").is_file());
    assert!(!s.root.path().join("escape.json").exists());
    for reply in &replies {
        let text = reply.to_string();
        assert!(
            !text.contains("Prefer Rust") && !text.contains(s.root.path().to_str().unwrap()),
            "{text}"
        );
    }

    let fresh = Store::new();
    fresh.value(&["search"]);
    private_exports(&fresh);
    fs::copy(dir.join("backup.json"), exports(&fresh).join("backup.json")).unwrap();
    let replies = tools(
        &fresh,
        &[
            ("import", json!({"name":"backup.json","apply":true})),
            (
                "import",
                json!({"name":"backup.json","user_confirmed":true}),
            ),
            (
                "import",
                json!({"name":"backup.json","apply":true,"user_confirmed":true}),
            ),
        ],
    );
    assert_eq!(replies[0]["applied"], false, "apply alone is a preview");
    assert_eq!(
        replies[1]["applied"], false,
        "confirmation alone is a preview"
    );
    assert_eq!(replies[2]["applied"], true, "{}", replies[2]);
    for reply in &replies {
        assert!(!reply.to_string().contains("Prefer Rust"), "{reply}");
    }
    assert_eq!(fresh.memories(), s.memories());

    // A backup from before titles existed: the tool lists the ids that need
    // titles, not the memories, and imports with a titles map.
    let document: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("backup.json")).unwrap()).unwrap();
    let ids: Vec<&str> = document["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    let older = Store::new();
    older.value(&["search"]);
    private_exports(&older);
    fs::write(
        exports(&older).join("old.json"),
        untitled(&document).to_string(),
    )
    .unwrap();
    let apply = json!({"name":"old.json","apply":true,"user_confirmed":true});
    let mut titled = apply.clone();
    titled["titles"] = json!({ ids[0]: "Rust first", ids[1]: "Brief answers" });
    let replies = tools(&older, &[("import", apply), ("import", titled)]);
    let refusal = replies[0]["error"].as_str().unwrap();
    assert!(
        refusal.contains("needs a title for every memory"),
        "{refusal}"
    );
    assert!(
        refusal.contains(&format!(
            "Untitled memories (2):\n  {}\n  {}",
            ids[0], ids[1]
        )),
        "{refusal}"
    );
    assert!(!refusal.contains("Prefer Rust"), "{refusal}");
    // The agent cannot title what it cannot see: the user runs the CLI.
    assert!(
        refusal.contains(
            "once the user has run `user-memory import FILE` to see each memory's content"
        ),
        "{refusal}"
    );
    assert_eq!(replies[1]["applied"], true, "{}", replies[1]);
    let titles: Vec<Value> = older
        .memories()
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["title"].clone())
        .collect();
    assert_eq!(titles, [json!("Rust first"), json!("Brief answers")]);
}

// Moving the store to a server in Docker, run against a real compose stack.
// Set USER_MEMORY_TEST_DOCKER=1 with the memory service running to try it.
#[test]
fn docker_export_round_trips_through_stdout() {
    if std::env::var_os("USER_MEMORY_TEST_DOCKER").is_none() {
        eprintln!("skipped: set USER_MEMORY_TEST_DOCKER=1 with the compose memory service running");
        return;
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let exported = Command::new("docker")
        .args([
            "compose",
            "exec",
            "-T",
            "memory",
            "user-memory",
            "export",
            "-",
        ])
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let local = Store::new();
    let imported = local.with_stdin(&["import", "-", "--apply"], &exported.stdout);
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let server: Value = serde_json::from_slice(&exported.stdout).unwrap();
    assert_eq!(local.memories(), server["memories"]);
}

// An export of exactly the size limit is read; one byte more is refused before
// anything is parsed, from a file or from stdin.
#[test]
fn imports_are_read_up_to_exactly_the_size_limit() {
    const LIMIT: usize = 64 * 1024 * 1024;
    let s = Store::new();
    s.seed();
    let mut document = s.ok(&["export", "-"]).stdout;
    document.resize(LIMIT, b' ');
    let file = s.root.path().join("exact.json");
    fs::write(&file, &document).unwrap();
    let preview = s.value(&["import", file.to_str().unwrap()]);
    assert_eq!(preview["applied"], false, "{preview}");
    document.push(b' ');
    fs::write(&file, &document).unwrap();
    let too_big = s.run(&["import", file.to_str().unwrap()]);
    assert!(!too_big.status.success());
    assert!(String::from_utf8_lossy(&too_big.stderr).contains("limited to 64 MiB"));
    let piped = s.with_stdin(&["import", "-"], &document);
    assert!(String::from_utf8_lossy(&piped.stderr).contains("limited to 64 MiB"));
}

// Exports carry each memory's title (format version 2), and a written
// title is cleaned like any other text on the way in.
#[test]
fn titles_travel_in_exports() {
    let s = Store::new();
    s.seed();
    let out = s.ok(&["export", "-"]);
    let document: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(document["format_version"], 2);
    assert_eq!(document["memories"][0]["title"], "Prefer Rust");
    let mut messy = document.clone();
    messy["memories"][0]["title"] = json!("  Prefer \t Rust ");
    let preview = Store::new().with_stdin(&["import", "-"], &serde_json::to_vec(&messy).unwrap());
    let report: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(
        report["normalized"],
        json!([document["memories"][0]["id"]]),
        "{report}"
    );
}

/// `document` as a format version 1 export, written before titles existed.
fn untitled(document: &Value) -> Value {
    let mut older = document.clone();
    older["format_version"] = json!(1);
    for memory in older["memories"].as_array_mut().unwrap() {
        memory.as_object_mut().unwrap().remove("title");
    }
    older
}

/// A titles map file in `dir`.
fn map_file(dir: &Path, map: &Value) -> PathBuf {
    let path = dir.join("titles.json");
    fs::write(&path, map.to_string()).unwrap();
    path
}

// Backups written before titles existed (format version 1) restore only
// with a title, from a map the user approved, for each memory the store
// lacks; memories the store holds keep its titles. Without a complete, valid
// map nothing changes; with one each title is stored as written, its
// whitespace tidied.
#[test]
fn older_backups_need_a_title_for_each_memory_the_store_lacks() {
    let s = Store::new();
    s.seed();
    let out = s.ok(&["export", "-"]);
    let document: Value = serde_json::from_slice(&out.stdout).unwrap();
    let older = serde_json::to_vec(&untitled(&document)).unwrap();
    let ids: Vec<&str> = document["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    let titles = [
        "Rust — for every new tool".to_owned(),
        format!("Brief {}", "b".repeat(114)),
    ];
    assert_eq!(titles[1].chars().count(), 120);
    let full = json!({ ids[0]: titles[0], ids[1]: titles[1] });
    let fresh = Store::new();
    let maps = fresh.root.path();
    let import = |map: Option<&Value>, extra: &[&str]| {
        let mut args = vec!["import", "-", "--apply"];
        args.extend_from_slice(extra);
        let path = map.map(|m| map_file(maps, m));
        if let Some(path) = &path {
            args.extend(["--titles", path.to_str().unwrap()]);
        }
        fresh.with_stdin(&args, &older)
    };
    let with = |id: &str, title: Value| {
        let mut map = full.clone();
        map[id] = title;
        map
    };
    let stray = "6f1c3a52-7d0e-4c1b-9a2e-3b4c5d6e7f80";
    for (case, map, expected) in [
        (
            "no map",
            None,
            format!(
                "Untitled memories (2):\n  {}  Prefer Rust\n  {}  Answer briefly",
                ids[0], ids[1]
            ),
        ),
        (
            "partial",
            Some(json!({ ids[0]: titles[0] })),
            format!("Untitled memories (1):\n  {}  Answer briefly", ids[1]),
        ),
        (
            "unknown id",
            Some(with(stray, json!("Stray"))),
            format!("not memories here (1):\n  {stray}"),
        ),
        (
            "empty",
            Some(with(ids[0], json!(""))),
            format!("  {}: the title is empty", ids[0]),
        ),
        (
            "over 120",
            Some(with(ids[0], json!("x".repeat(121)))),
            format!(
                "  {}: the title has 121 characters; the limit is 120",
                ids[0]
            ),
        ),
    ] {
        let out = import(map.as_ref(), &[]);
        assert!(!out.status.success(), "{case}");
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(
            error.contains("Importing an export from before titles (format version 1) needs a title for every memory"),
            "{case}: {error}"
        );
        assert!(error.contains(&expected), "{case}: {error}");
        assert!(
            error.contains("Give `import FILE --titles FILE` (or the import tool's `titles`)"),
            "{case}: {error}"
        );
        assert_eq!(fresh.memories(), json!([]), "{case}");
    }

    let applied = import(Some(&full), &[]);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let report: Value = serde_json::from_slice(&applied.stdout).unwrap();
    assert_eq!(report["normalized"], json!([]), "{report}");
    let restored = fresh.memories();
    assert_eq!(restored[0]["title"], titles[0]);
    assert_eq!(restored[1]["title"], titles[1]);

    // Memories the store already holds keep its titles: their map entries
    // are not used, and said so, and they need none.
    fresh.value(&["update", ids[0], "--title", "Rust for new tools"]);
    let again = import(Some(&full), &[]);
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    let result: Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(result["applied"], true, "{result}");
    assert_eq!(result["conflicts"], json!([]), "{result}");
    assert_eq!(result["titles_ignored"]["ids"], json!(ids), "{result}");
    assert!(
        result["titles_ignored"]["note"]
            .as_str()
            .unwrap()
            .contains("keeps its titles"),
        "{result}"
    );
    assert_eq!(fresh.memories()[0]["title"], "Rust for new tools");
    let without = import(None, &[]);
    let result: Value = serde_json::from_slice(&without.stdout).unwrap();
    assert_eq!(result["applied"], true, "{result}");
    assert!(result.get("titles_ignored").is_none(), "{result}");
    // Only the memories the store lacks are listed as untitled.
    fresh.ok(&["forget", ids[1]]);
    let partial = import(None, &[]);
    let error = String::from_utf8_lossy(&partial.stderr);
    assert!(
        error.contains(&format!(
            "Untitled memories (1):\n  {}  Answer briefly",
            ids[1]
        )),
        "{error}"
    );
    assert!(!error.contains(ids[0]), "{error}");
    let restored = import(Some(&json!({ ids[1]: "Brief" })), &[]);
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert_eq!(fresh.memories()[1]["title"], "Brief");

    // A newer backup's title is a field like any other: use-file takes it.
    let mut newer = document.clone();
    newer["memories"][0]["title"] = json!("Rust everywhere");
    let replaced = fresh.with_stdin(
        &["import", "-", "--apply", "--on-conflict", "use-file"],
        &serde_json::to_vec(&newer).unwrap(),
    );
    assert!(replaced.status.success());
    assert_eq!(fresh.memories()[0]["title"], "Rust everywhere");
}

// Replacing a memory from a file keeps the conversations the file shares
// with the store, so a pattern built on it does not go stale when only its
// reason changes, even for a moment while origins are rewritten.
#[test]
fn use_file_keeps_shared_conversations_and_the_patterns_on_them() {
    let s = Store::new();
    let record = |args: &[&str]| -> String {
        let mut full = vec!["record", "--type", "preference", "--category", "style"];
        full.extend_from_slice(args);
        let out = s.value(&full);
        assert_eq!(out["stored"], true, "{out}");
        out["id"].as_str().unwrap().to_owned()
    };
    let x = record(&["--content", "Prefer concrete examples", "--origin", "c:1"]);
    let y = record(&["--content", "Prefer runnable demos", "--origin", "c:2"]);
    // X was also stated in c:2, after c:1.
    rusqlite::Connection::open(&s.db)
        .unwrap()
        .execute(
            "INSERT INTO memory_origins (memory_id,origin) VALUES (?1,'c:2')",
            [&x],
        )
        .unwrap();
    let p = s.value(&[
        "record",
        "--type",
        "pattern",
        "--category",
        "learning",
        "--content",
        "Learns from concrete material",
        "--source",
        "inferred",
        "--evidence-id",
        &x,
        "--evidence-id",
        &y,
    ]);
    assert_eq!(p["stored"], true, "{p}");
    let mut document: Value = serde_json::from_slice(&s.ok(&["export", "-"]).stdout).unwrap();
    for memory in document["memories"].as_array_mut().unwrap() {
        if memory["id"] == x.as_str() {
            memory["reason"] = json!("Examples stick better");
        }
    }
    // The preview predicts what applying does: nothing goes stale.
    let preview = s.with_stdin(
        &["import", "-", "--on-conflict", "use-file"],
        &serde_json::to_vec(&document).unwrap(),
    );
    let preview: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(preview["will_stale"], json!([]), "{preview}");
    let applied = s.with_stdin(
        &["import", "-", "--apply", "--on-conflict", "use-file"],
        &serde_json::to_vec(&document).unwrap(),
    );
    let result: Value = serde_json::from_slice(&applied.stdout).unwrap();
    assert_eq!(result["applied"], true, "{result}");
    let memories = s.memories();
    let pattern = memories
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == p["id"])
        .unwrap();
    assert_eq!(pattern["status"], "tentative", "{memories}");
}

// A use-file import that swaps one of a support's conversations for another
// adds the new one before removing the old, so a pattern that keeps two
// independent conversations throughout never goes stale on the way.
#[test]
fn use_file_adds_conversations_before_removing_others() {
    let s = Store::new();
    let record = |args: &[&str]| -> String {
        let mut full = vec!["record", "--type", "preference", "--category", "style"];
        full.extend_from_slice(args);
        let out = s.value(&full);
        assert_eq!(out["stored"], true, "{out}");
        out["id"].as_str().unwrap().to_owned()
    };
    let a = record(&["--content", "Prefer concrete examples", "--origin", "c:1"]);
    let b = record(&["--content", "Prefer runnable demos", "--origin", "c:1"]);
    rusqlite::Connection::open(&s.db)
        .unwrap()
        .execute(
            "INSERT INTO memory_origins (memory_id,origin) VALUES (?1,'c:2')",
            [&b],
        )
        .unwrap();
    let p = s.value(&[
        "record",
        "--type",
        "pattern",
        "--category",
        "learning",
        "--content",
        "Learns from concrete material",
        "--source",
        "inferred",
        "--evidence-id",
        &a,
        "--evidence-id",
        &b,
    ]);
    assert_eq!(p["stored"], true, "{p}");
    let mut document: Value = serde_json::from_slice(&s.ok(&["export", "-"]).stdout).unwrap();
    for memory in document["memories"].as_array_mut().unwrap() {
        if memory["id"] == b.as_str() {
            memory["reason"] = json!("Demos stick better");
            memory["origins"] = json!([
                {"origin": "c:1", "first_seen": "2026-10-01 10:00:00"},
                {"origin": "c:5", "first_seen": "2026-10-02 10:00:00"},
            ]);
        }
    }
    // The preview predicts what applying does: nothing goes stale.
    let preview = s.with_stdin(
        &["import", "-", "--on-conflict", "use-file"],
        &serde_json::to_vec(&document).unwrap(),
    );
    let preview: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(preview["will_stale"], json!([]), "{preview}");
    let applied = s.with_stdin(
        &["import", "-", "--apply", "--on-conflict", "use-file"],
        &serde_json::to_vec(&document).unwrap(),
    );
    let result: Value = serde_json::from_slice(&applied.stdout).unwrap();
    assert_eq!(result["applied"], true, "{result}");
    let memories = s.memories();
    let find = |id: &Value| {
        memories
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == *id)
            .unwrap()
            .clone()
    };
    let support = find(&json!(b));
    let origins: Vec<&str> = support["origins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["origin"].as_str().unwrap())
        .collect();
    assert_eq!(origins, ["c:1", "c:5"]);
    assert_eq!(find(&p["id"])["status"], "tentative", "{memories}");
}

// An observation made explicit while its evidence note is rewritten is not a
// confirmation, which drops the note: the pattern it supports goes stale.
// Only an import can produce that change; update always drops the note.
#[test]
fn an_observation_made_explicit_with_a_new_note_stales_its_patterns() {
    let s = Store::new();
    let observed = s.value(&[
        "record",
        "--type",
        "behavior",
        "--category",
        "style",
        "--source",
        "observed",
        "--evidence",
        "Saw it once",
        "--content",
        "Asks for runnable demos",
        "--origin",
        "c:1",
    ]);
    let a = observed["id"].as_str().unwrap().to_owned();
    let stated = s.value(&[
        "record",
        "--type",
        "preference",
        "--category",
        "style",
        "--content",
        "Prefer examples over theory",
        "--origin",
        "c:2",
    ]);
    let b = stated["id"].as_str().unwrap().to_owned();
    let p = s.value(&[
        "record",
        "--type",
        "pattern",
        "--category",
        "learning",
        "--content",
        "Learns from concrete material",
        "--source",
        "inferred",
        "--evidence-id",
        &a,
        "--evidence-id",
        &b,
    ]);
    assert_eq!(p["stored"], true, "{p}");
    let mut document: Value = serde_json::from_slice(&s.ok(&["export", "-"]).stdout).unwrap();
    for memory in document["memories"].as_array_mut().unwrap() {
        if memory["id"] == a.as_str() {
            // Explicit memories are confirmed; that part only strengthens.
            memory["source"] = json!("explicit");
            memory["status"] = json!("confirmed");
            memory["evidence"] = json!("Saw it twice, then the user said so");
        }
    }
    let document = serde_json::to_vec(&document).unwrap();
    let preview = s.with_stdin(&["import", "-", "--on-conflict", "use-file"], &document);
    let preview: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(preview["will_stale"], json!([p["id"]]), "{preview}");
    let applied = s.with_stdin(
        &["import", "-", "--apply", "--on-conflict", "use-file"],
        &document,
    );
    let result: Value = serde_json::from_slice(&applied.stdout).unwrap();
    assert_eq!(result["staled"], json!([p["id"]]), "{result}");
}

// The export and the titles map cannot both come from stdin.
#[test]
fn only_one_input_comes_from_stdin() {
    let s = Store::new();
    let out = s.with_stdin(&["import", "-", "--titles", "-"], b"{}");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("Only one of the export and the titles map can come from stdin")
    );
}

// An export with text over the limit is refused per field, in either format
// version, instead of being cut on the way in.
#[test]
fn imports_refuse_over_long_fields_by_name() {
    let s = Store::new();
    s.seed();
    let out = s.ok(&["export", "-"]);
    let document: Value = serde_json::from_slice(&out.stdout).unwrap();
    for (version, field, limit) in [
        (2, "content", 4000),
        (2, "reason", 2000),
        (1, "context", 2000),
        (1, "content", 4000),
        (2, "evidence", 2000),
    ] {
        let mut file = document.clone();
        file["format_version"] = json!(version);
        if version == 1 {
            for memory in file["memories"].as_array_mut().unwrap() {
                memory.as_object_mut().unwrap().remove("title");
            }
        }
        file["memories"][1][field] = json!("w".repeat(limit + 1));
        let fresh = Store::new();
        let map: serde_json::Map<String, Value> = file["memories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| (m["id"].as_str().unwrap().to_owned(), json!("A title")))
            .collect();
        let map = map_file(fresh.root.path(), &map.into());
        let result = fresh.with_stdin(
            &["import", "-", "--apply", "--titles", map.to_str().unwrap()],
            &serde_json::to_vec(&file).unwrap(),
        );
        let result: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(result["applied"], false, "{version} {field}: {result}");
        assert_eq!(
            result["invalid"][0]["rule"],
            format!(
                "{field} has {} characters; the limit is {limit}; shorten it or split the memory",
                limit + 1
            ),
            "{version} {field}"
        );
        assert_eq!(result["invalid"][0]["index"], 1);
        assert_eq!(fresh.memories(), json!([]));
    }
    // Each limit itself imports, whole.
    let mut file = document;
    file["memories"][1]["content"] = json!("w".repeat(4000));
    file["memories"][1]["reason"] = json!("w".repeat(2000));
    let fresh = Store::new();
    let result = fresh.with_stdin(
        &["import", "-", "--apply"],
        &serde_json::to_vec(&file).unwrap(),
    );
    let result: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(result["applied"], true, "{result}");
    let stored = fresh.memories();
    let id = &file["memories"][1]["id"];
    let long = stored
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == *id)
        .unwrap();
    assert_eq!(long["content"].as_str().unwrap().len(), 4000);
    assert_eq!(long["reason"].as_str().unwrap().len(), 2000);
}

/// The store every filter test exports from, with the ids of its memories:
/// global g1 (programming, from chat/a) and g2 (style), distinct from each
/// other; p1 in project app (programming, from chat/b); an observation o1;
/// a pattern i1 inferred from g1 and o1; g3 superseded by g4.
struct Fixture {
    store: Store,
    ids: HashMap<&'static str, String>,
}

const APP: &str = "github.com/me/app";

impl Fixture {
    fn new() -> Self {
        let store = Store::new();
        let mut ids = HashMap::new();
        ids.insert(
            "g1",
            record(
                &store,
                &[
                    "--type",
                    "preference",
                    "--category",
                    "programming",
                    "--content",
                    "Prefer Rust for command line tools",
                    "--origin",
                    "chat/a",
                ],
            ),
        );
        ids.insert(
            "g2",
            record(
                &store,
                &[
                    "--type",
                    "preference",
                    "--category",
                    "style",
                    "--content",
                    "Answer in short paragraphs",
                ],
            ),
        );
        ids.insert(
            "p1",
            record(
                &store,
                &[
                    "--type",
                    "constraint",
                    "--category",
                    "programming",
                    "--content",
                    "Use Python 3.12 in this repository",
                    "--scope",
                    "project",
                    "--project",
                    APP,
                    "--origin",
                    "chat/b",
                ],
            ),
        );
        ids.insert(
            "o1",
            record(
                &store,
                &[
                    "--type",
                    "behavior",
                    "--category",
                    "learning",
                    "--content",
                    "Asks for runnable examples",
                    "--source",
                    "observed",
                    "--evidence",
                    "Asked for an example twice",
                ],
            ),
        );
        let (g1, o1) = (ids["g1"].clone(), ids["o1"].clone());
        ids.insert(
            "i1",
            record(
                &store,
                &[
                    "--type",
                    "pattern",
                    "--category",
                    "learning",
                    "--content",
                    "Learns by trying code",
                    "--source",
                    "inferred",
                    "--evidence-id",
                    &g1,
                    "--evidence-id",
                    &o1,
                ],
            ),
        );
        ids.insert(
            "g3",
            record(
                &store,
                &[
                    "--type",
                    "preference",
                    "--category",
                    "editor",
                    "--content",
                    "Indent with tabs",
                ],
            ),
        );
        let g3 = ids["g3"].clone();
        ids.insert(
            "g4",
            record(
                &store,
                &[
                    "--type",
                    "preference",
                    "--category",
                    "editor",
                    "--content",
                    "Indent with four spaces",
                    "--supersedes",
                    &g3,
                ],
            ),
        );
        let marked = store.value(&["update", &ids["g1"], "--distinct-from", &ids["g2"]]);
        assert_eq!(marked["distinct_marked"], 1, "{marked}");
        Self { store, ids }
    }
    /// The names of the memories in `memories`, sorted.
    fn names(&self, memories: &Value) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = memories
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                *self
                    .ids
                    .iter()
                    .find(|(_, id)| m["id"] == id.as_str())
                    .unwrap()
                    .0
            })
            .collect();
        names.sort_unstable();
        names
    }
    /// Exports with `filters` to stdout: the document and the summary.
    fn export(&self, filters: &[&str]) -> (Value, Value) {
        let mut args = vec!["export", "-"];
        args.extend_from_slice(filters);
        let out = self.store.ok(&args);
        (
            serde_json::from_slice(&out.stdout).unwrap(),
            serde_json::from_slice(&out.stderr).unwrap(),
        )
    }
}

/// Records a memory, distinct from any similar one, and returns its id.
fn record(store: &Store, args: &[&str]) -> String {
    let mut full: Vec<String> = std::iter::once("record")
        .chain(args.iter().copied())
        .map(str::to_owned)
        .collect();
    fn refs(full: &[String]) -> Vec<&str> {
        full.iter().map(String::as_str).collect()
    }
    let mut out = store.value(&refs(&full));
    if out["reason"] == "similar memories exist" {
        for m in out["similar"].as_array().unwrap() {
            full.extend([
                "--distinct-from".to_owned(),
                m["id"].as_str().unwrap().to_owned(),
            ]);
        }
        out = store.value(&refs(&full));
    }
    assert_eq!(out["stored"], true, "{args:?}: {out}");
    out["id"].as_str().unwrap().to_owned()
}

/// Every filter and the memories it selects from the fixture, for export
/// and import alike. No filter is everything; a filter given keeps read's
/// meaning, except that an unset status is every status and an unset scope
/// with no project is every scope.
const FILTERS: &[(&[&str], &[&str])] = &[
    (&[], &["g1", "g2", "g3", "g4", "i1", "o1", "p1"]),
    (
        &["--scope", "global"],
        &["g1", "g2", "g3", "g4", "i1", "o1"],
    ),
    (&["--scope", "project", "--project", APP], &["p1"]),
    (
        &["--project", APP],
        &["g1", "g2", "g3", "g4", "i1", "o1", "p1"],
    ),
    (&["--category", "coding"], &["g1", "p1"]),
    (&["--type", "preference"], &["g1", "g2", "g3", "g4"]),
    (&["--source", "inferred"], &["i1"]),
    (&["--status", "superseded"], &["g3"]),
    (
        &["--status", "active"],
        &["g1", "g2", "g4", "i1", "o1", "p1"],
    ),
    (&["--origin", "chat/b"], &["p1"]),
    (&["--category", "programming", "--scope", "global"], &["g1"]),
    (
        &[
            "--type",
            "preference",
            "--status",
            "active",
            "--category",
            "editor",
        ],
        &["g4"],
    ),
];

#[test]
fn each_export_filter_selects_what_read_would() {
    let f = Fixture::new();
    for (filters, expected) in FILTERS {
        let (document, summary) = f.export(filters);
        assert_eq!(f.names(&document["memories"]), *expected, "{filters:?}");
        if filters.is_empty() {
            assert!(document.get("selection").is_none());
            assert!(summary.get("selection").is_none(), "{summary}");
        } else {
            assert!(document["selection"].is_object(), "{filters:?}");
            assert_eq!(document["selection"], summary["selection"]);
        }
    }
    let (document, _) = f.export(&["--category", "coding"]);
    assert_eq!(
        document["selection"],
        json!({"category": "coding"}),
        "the file records the filters as given"
    );
}

#[test]
fn each_import_filter_selects_what_the_export_filter_would() {
    let f = Fixture::new();
    let file = f.store.root.path().join("all.json");
    f.store.ok(&["export", file.to_str().unwrap()]);
    for (filters, expected) in FILTERS {
        let fresh = Store::new();
        let mut args = vec!["import", file.to_str().unwrap(), "--apply"];
        args.extend_from_slice(filters);
        let summary = fresh.value(&args);
        assert_eq!(summary["applied"], true, "{filters:?}: {summary}");
        if filters.is_empty() {
            // An unfiltered import prints what it always printed.
            for key in ["selection", "selected", "skipped", "replacements_missing"] {
                assert!(summary.get(key).is_none(), "{key}: {summary}");
            }
        } else {
            assert_eq!(
                summary["selected"],
                expected.len(),
                "{filters:?}: {summary}"
            );
        }
        // The pattern comes in only with both its supports.
        let mut imported: Vec<&str> = expected.to_vec();
        if !["g1", "o1"].iter().all(|s| expected.contains(s)) {
            imported.retain(|n| *n != "i1");
        }
        let (document, _) = Fixture::export_of(&fresh);
        assert_eq!(f.names(&document["memories"]), imported, "{filters:?}");
    }
}

impl Fixture {
    fn export_of(store: &Store) -> (Value, Value) {
        let out = store.ok(&["export", "-"]);
        (
            serde_json::from_slice(&out.stdout).unwrap(),
            serde_json::from_slice(&out.stderr).unwrap(),
        )
    }
}

// A filter selects memories and never edits one: their links leave the file
// as stored, the summary counts those whose other end stayed behind, and an
// import decides by what is there.
#[test]
fn links_leaving_a_filtered_export_are_counted_and_settled_on_import() {
    let f = Fixture::new();
    let (_, summary) = f.export(&["--source", "inferred"]);
    assert_eq!(
        summary["links_outside"],
        json!({"supports": 2, "replacements": 0, "distinct_pairs": 0})
    );
    let (_, summary) = f.export(&["--status", "superseded"]);
    assert_eq!(
        summary["links_outside"],
        json!({"supports": 0, "replacements": 1, "distinct_pairs": 0})
    );
    let (document, summary) = f.export(&["--category", "programming", "--scope", "global"]);
    assert_eq!(
        summary["links_outside"],
        json!({"supports": 0, "replacements": 0, "distinct_pairs": 1})
    );
    assert_eq!(document["distinct_pairs"].as_array().unwrap().len(), 1);

    // Into a fresh store: the pattern without its supports is skipped, the
    // superseded memory keeps its link, the pair is dropped.
    for (filters, check) in [
        (&["--source", "inferred"][..], "unsupported"),
        (&["--status", "superseded"], "replacement"),
        (&["--category", "programming", "--scope", "global"], "pair"),
    ] {
        let (document, _) = f.export(filters);
        let fresh = Store::new();
        let out = fresh.with_stdin(&["import", "-", "--apply"], document.to_string().as_bytes());
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let summary: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(summary["applied"], true, "{summary}");
        assert_eq!(summary["file_selection"], document["selection"]);
        let (imported, _) = Fixture::export_of(&fresh);
        match check {
            "unsupported" => {
                assert_eq!(summary["skipped"]["unsupported_inferences"], 1, "{summary}");
                assert_eq!(f.names(&imported["memories"]), Vec::<&str>::new());
            }
            "replacement" => {
                assert_eq!(summary["replacements_missing"], 1, "{summary}");
                assert_eq!(
                    imported["memories"][0]["superseded_by"],
                    f.ids["g4"].as_str()
                );
            }
            _ => {
                assert_eq!(summary["dropped_distinct_pairs"], 1, "{summary}");
                assert_eq!(f.names(&imported["memories"]), ["g1"]);
            }
        }
    }

    // Into a store that holds the supports, the pattern comes in whole.
    let (supports, _) = f.export(&["--category", "programming", "--scope", "global"]);
    let (learning, _) = f.export(&["--category", "learning"]);
    let fresh = Store::new();
    for document in [&supports, &learning] {
        let out = fresh.with_stdin(&["import", "-", "--apply"], document.to_string().as_bytes());
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let (imported, _) = Fixture::export_of(&fresh);
    assert_eq!(f.names(&imported["memories"]), ["g1", "i1", "o1"]);
}

// Only a filter leaves a support behind: in a whole-store file, an active
// pattern citing a memory that is nowhere is still refused.
#[test]
fn an_unfiltered_file_citing_a_missing_support_is_still_refused() {
    let f = Fixture::new();
    let (mut document, _) = f.export(&[]);
    let memories = document["memories"].as_array_mut().unwrap();
    memories.retain(|m| m["id"] != f.ids["o1"].as_str());
    let fresh = Store::new();
    let out = fresh.with_stdin(&["import", "-"], document.to_string().as_bytes());
    let summary: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        summary["invalid"][0]["rule"], "Supporting memory does not exist",
        "{summary}"
    );
}

// The store itself keeps a superseded memory whose replacement was
// forgotten; its export imports elsewhere.
#[test]
fn a_store_whose_replacement_was_forgotten_exports_and_imports_whole() {
    let f = Fixture::new();
    f.store.ok(&["forget", &f.ids["g4"]]);
    let (document, _) = f.export(&[]);
    let fresh = Store::new();
    let out = fresh.with_stdin(&["import", "-", "--apply"], document.to_string().as_bytes());
    let summary: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(summary["applied"], true, "{summary}");
    assert_eq!(summary["replacements_missing"], 1, "{summary}");
}

// A filtered import selects among the file's rows; the preview counts them
// and writes nothing, and a filter selecting nothing says so.
#[test]
fn a_filtered_import_previews_counts_and_can_select_nothing() {
    let f = Fixture::new();
    let file = f.store.root.path().join("all.json");
    f.store.ok(&["export", file.to_str().unwrap()]);
    let fresh = Store::new();
    fresh.value(&["search"]);
    let before = fs::read(&fresh.db).unwrap();
    let preview = fresh.value(&["import", file.to_str().unwrap(), "--type", "preference"]);
    assert_eq!(preview["applied"], false);
    assert_eq!(preview["selection"], json!({"type": "preference"}));
    assert_eq!(preview["selected"], 4, "{preview}");
    assert_eq!(preview["skipped"]["not_selected"], 3, "{preview}");
    assert_eq!(preview["added"]["confirmed"], 3, "{preview}");
    assert_eq!(preview["added"]["superseded"], 1, "{preview}");
    assert_eq!(
        fs::read(&fresh.db).unwrap(),
        before,
        "a preview writes nothing"
    );
    // A row the import tidies (a timestamp in ISO form) is reported only
    // when it is imported.
    let mut document: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    let g2 = document["memories"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|m| m["id"] == f.ids["g2"].as_str())
        .unwrap();
    g2["created_at"] = json!("2026-10-18T09:00:00Z");
    let tidied = document.to_string();
    let read = |filters: &[&str]| -> Value {
        let mut args = vec!["import", "-"];
        args.extend_from_slice(filters);
        let out = fresh.with_stdin(&args, tidied.as_bytes());
        serde_json::from_slice(&out.stdout).unwrap()
    };
    assert_eq!(
        read(&["--category", "style"])["normalized"],
        json!([f.ids["g2"]])
    );
    assert_eq!(read(&["--category", "editor"])["normalized"], json!([]));
    let none = fresh.value(&[
        "import",
        file.to_str().unwrap(),
        "--apply",
        "--category",
        "gardening",
    ]);
    assert_eq!(none["applied"], false, "{none}");
    assert_eq!(none["selected"], 0);
    assert_eq!(
        none["note"],
        "The filter selected no memories; nothing was imported"
    );
    assert_eq!(fs::read(&fresh.db).unwrap(), before);
}

/// `args` with `--title` set to the content and `--scope` to global when a
/// record gives none: most tests here are about other fields. A test about
/// the title or scope passes its own.
fn titled<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut args = args.to_vec();
    if args.first() == Some(&"record") && !args.contains(&"--scope") {
        args.extend(["--scope", "global"]);
    }
    if args.first() == Some(&"record") && !args.contains(&"--title") {
        let content = args
            .iter()
            .position(|a| *a == "--content")
            .and_then(|i| args.get(i + 1).copied());
        if let Some(content) = content {
            args.extend(["--title", content]);
        }
    }
    args
}

/// The memories of a search result's page. Unless a test asks for a smaller
/// page, its results fit one, so a next_cursor would mean memories went
/// missing.
fn one_page(value: Value, page_size_given: bool) -> Value {
    assert!(
        page_size_given || value.get("next_cursor").is_none(),
        "{value}"
    );
    value["memories"].clone()
}

/// A git checkout in `store`'s sandbox whose remote names the project
/// github.com/me/app.
fn checkout(store: &Store) -> PathBuf {
    let repo = store.root.path().join("app");
    fs::create_dir_all(&repo).unwrap();
    for args in [
        &["init", "-q"][..],
        &["remote", "add", "origin", "https://github.com/me/app.git"],
    ] {
        let out = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .sandboxed(store.root.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }
    repo
}

// Inside a checkout, `--project .` names its project for export and import,
// as it does for read: the CLI resolves the folder to its key before the
// filter applies.
#[test]
fn a_folder_names_its_project_for_export_and_import() {
    let store = Store::new();
    let repo = checkout(&store);
    for (scope, project, content) in [
        ("project", "github.com/me/app", "Ship the app weekly"),
        ("project", "github.com/me/other", "Ship the other monthly"),
        ("global", "", "Prefer small commits"),
    ] {
        let mut args = vec![
            "--type",
            "preference",
            "--category",
            "workflow",
            "--scope",
            scope,
            "--content",
            content,
        ];
        if !project.is_empty() {
            args.extend(["--project", project]);
        }
        record(&store, &args);
    }
    let file = store.root.path().join("app.json");
    let out = store
        .command(&[
            "export",
            file.to_str().unwrap(),
            "--scope",
            "project",
            "--project",
            ".",
        ])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let document: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    let contents: Vec<&str> = document["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(contents, ["Ship the app weekly"]);
    assert_eq!(
        document["selection"]["projects"],
        json!(["github.com/me/app"])
    );

    // Importing every memory from a whole export, `.` selects this folder's.
    let all = store.root.path().join("all.json");
    store.ok(&["export", all.to_str().unwrap()]);
    let fresh = Store::new();
    let fresh_repo = checkout(&fresh);
    let out = fresh
        .command(&[
            "import",
            all.to_str().unwrap(),
            "--apply",
            "--scope",
            "project",
            "--project",
            ".",
        ])
        .current_dir(&fresh_repo)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let summary: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(summary["selected"], 1, "{summary}");
    assert_eq!(
        summary["selection"]["projects"],
        json!(["github.com/me/app"])
    );
}
