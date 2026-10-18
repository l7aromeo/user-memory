//! Guards for schema changes: every schema version has a committed snapshot,
//! and a store built from any earlier snapshot migrates to exactly the
//! current schema without losing data.
//!
//! Bumping the schema: add `vN.sql` (this test writes it for you when it is
//! missing) and `vN.seed.sql` with rows of every kind for that version, and
//! never edit the snapshot of an earlier version.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
#[path = "support/damage.rs"]
#[allow(dead_code, reason = "each test file uses part of the shared damage")]
mod damage;

use rusqlite::{Connection, types::ValueRef};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::{TempDir, tempdir};

fn snapshots() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/schema")
}

fn user_memory(home: &Path, db: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .args(titled(args))
        .sandboxed(home)
        .env("USER_MEMORY_DB", db)
        .env_remove("USER_MEMORY_URL")
        .env_remove("USER_MEMORY_TOKEN_FILE")
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A store created by this binary, in its own directory.
fn fresh() -> (TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    let db = dir.path().join("memories.sqlite3");
    let out = user_memory(dir.path(), &db, &["search"]);
    assert!(out.status.success(), "{}", stderr(&out));
    (dir, db)
}

/// The schema version this binary creates.
fn current_version() -> i64 {
    let (_dir, db) = fresh();
    version(&db)
}

fn version(db: &Path) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

/// The schema's statements in creation order, leaving out what SQLite
/// derives itself: automatic indexes and the shadow tables of virtual tables.
fn statements(db: &Path) -> Vec<String> {
    let conn = Connection::open(db).unwrap();
    let virtual_tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE sql LIKE 'CREATE VIRTUAL TABLE%'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    conn.prepare("SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY rowid")
        .unwrap()
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .filter(|(kind, name, _)| {
            let shadow = kind == "table"
                && virtual_tables
                    .iter()
                    .any(|table| name.starts_with(&format!("{table}_")));
            !shadow && !name.starts_with("sqlite_")
        })
        .map(|(_, _, sql)| sql)
        .collect()
}

fn render(version: i64, statements: &[String]) -> String {
    let mut text = format!(
        "-- Schema version {version} of a fresh user-memory store: sqlite_master in\n-- creation order, without automatic indexes or FTS5 shadow tables.\n-- Written by tests/schema.rs; never edit the snapshot of a released version.\n"
    );
    for statement in statements {
        text.push_str(statement);
        text.push_str("\n;\n");
    }
    text
}

/// The statements of a snapshot file.
fn parse(text: &str) -> Vec<String> {
    let body: String = text
        .lines()
        .skip_while(|line| line.starts_with("--"))
        .map(|line| format!("{line}\n"))
        .collect();
    body.split("\n;\n")
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

/// A statement with layout and identifier quoting removed outside string
/// literals: ALTER TABLE quotes renamed tables and reflows added columns.
fn normalize(sql: &str) -> String {
    let mut out = String::new();
    let mut literal = false;
    let mut space = false;
    for c in sql.chars() {
        if literal {
            out.push(c);
            literal = c != '\'';
            continue;
        }
        match c {
            '\'' => {
                if space && !out.is_empty() && !out.ends_with(['(', ',']) {
                    out.push(' ');
                }
                space = false;
                literal = true;
                out.push(c);
            }
            '"' => {}
            c if c.is_whitespace() => space = true,
            '(' | ')' | ',' | ';' => {
                space = false;
                out.push(c);
            }
            c => {
                if space && !out.is_empty() && !out.ends_with(['(', ',']) {
                    out.push(' ');
                }
                space = false;
                out.push(c);
            }
        }
    }
    out
}

fn normalized(statements: &[String]) -> Vec<String> {
    let mut found: Vec<String> = statements.iter().map(|s| normalize(s)).collect();
    found.sort();
    found
}

fn snapshot_versions() -> Vec<i64> {
    let mut found: Vec<i64> = fs::read_dir(snapshots())
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().into_string().unwrap();
            name.strip_prefix('v')?.strip_suffix(".sql")?.parse().ok()
        })
        .collect();
    found.sort();
    found
}

/// A store at schema version `n`, built from its snapshot and seed rows.
fn store_from_snapshot(dir: &Path, n: i64) -> PathBuf {
    let db = dir.join("memories.sqlite3");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(&fs::read_to_string(snapshots().join(format!("v{n}.sql"))).unwrap())
        .unwrap();
    conn.pragma_update(None, "user_version", n).unwrap();
    conn.execute_batch(&fs::read_to_string(snapshots().join(format!("v{n}.seed.sql"))).unwrap())
        .unwrap();
    db
}

/// Every row of `sql`, every column, as text.
fn rows(conn: &Connection, sql: &str) -> Vec<String> {
    let mut statement = conn.prepare(sql).unwrap();
    let columns = statement.column_count();
    let mut rows = statement.query([]).unwrap();
    let mut found = Vec::new();
    while let Some(row) = rows.next().unwrap() {
        found.push(
            (0..columns)
                .map(|i| match row.get_ref(i).unwrap() {
                    ValueRef::Null => "NULL".to_owned(),
                    ValueRef::Integer(n) => n.to_string(),
                    ValueRef::Real(x) => x.to_string(),
                    ValueRef::Text(t) => format!("'{}'", String::from_utf8_lossy(t)),
                    ValueRef::Blob(b) => format!("{b:02x?}"),
                })
                .collect::<Vec<_>>()
                .join(" | "),
        );
    }
    found
}

/// What a migration must keep. A step that renames or reshapes one of these
/// columns updates the query here along with it.
fn data(db: &Path) -> Vec<Vec<String>> {
    let conn = Connection::open(db).unwrap();
    [
        "SELECT id,created_at,updated_at,memory_type,category,context,content,reason,scope,project,source,status,evidence,evidence_ids,superseded_by,last_confirmed_at FROM memories ORDER BY id",
        "SELECT memory_id,origin,first_seen FROM memory_origins ORDER BY 1,2",
        "SELECT a,b FROM memory_distinct ORDER BY 1,2",
        "SELECT id,model,dimensions,source,hex(vector) FROM memory_vectors ORDER BY id",
        "SELECT id FROM memory_search WHERE memory_search MATCH 'rust OR answers' ORDER BY id",
    ]
    .iter()
    .map(|sql| rows(&conn, sql))
    .collect()
}

/// The first schema version whose memories carry titles.
const TITLED_VERSION: i64 = 3;

/// A titles map beside `db` giving each of its memories a title, for
/// migrations of stores from before titles existed.
fn titles_file(db: &Path) -> PathBuf {
    let conn = Connection::open(db).unwrap();
    let map: serde_json::Map<String, serde_json::Value> = conn
        .prepare("SELECT id, rowid FROM memories NOT INDEXED")
        .unwrap()
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .unwrap()
        .map(|row| {
            let (id, rowid) = row.unwrap();
            (id, format!("Memory {rowid}").into())
        })
        .collect();
    let path = db.with_file_name("titles.json");
    fs::write(&path, serde_json::Value::from(map).to_string()).unwrap();
    path
}

/// `migrate` for a store at version `n`, with a titles map when it needs one.
fn migrate(dir: &Path, db: &Path, n: i64) -> Output {
    let map = titles_file(db);
    let mut args = vec!["migrate", "--db", db.to_str().unwrap()];
    if n < TITLED_VERSION {
        args.extend(["--titles", map.to_str().unwrap()]);
    }
    user_memory(dir, db, &args)
}

fn backups(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "bak"))
        .collect()
}

// Changing the schema without a snapshot for its version fails here.
#[test]
fn the_current_schema_matches_its_committed_snapshot() {
    let (_dir, db) = fresh();
    let current = version(&db);
    let expected = render(current, &statements(&db));
    let path = snapshots().join(format!("v{current}.sql"));
    let committed = fs::read_to_string(&path).unwrap_or_default();
    if committed != expected {
        let written = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("v{current}.sql"));
        fs::write(&written, &expected).unwrap();
        panic!(
            "{} does not match schema version {current}. If the version is new, copy {} there and add v{current}.seed.sql; if not, bump SCHEMA_VERSION and add a migration step.",
            path.display(),
            written.display()
        );
    }
    assert_eq!(parse(&committed), statements(&db));
    assert!(
        snapshots().join(format!("v{current}.seed.sql")).is_file(),
        "every snapshot needs seed rows"
    );
}

// A damaged store is refused until it is repaired: migrating copies its
// rows, and copying them through a damaged index could lose some.
#[test]
fn a_damaged_store_migrates_only_after_repair_and_keeps_every_memory() {
    let (_fresh_dir, fresh_db) = fresh();
    let current = version(&fresh_db);
    for from in snapshot_versions().into_iter().filter(|n| *n < current) {
        for (name, damage) in damage::ALL {
            let dir = tempdir().unwrap();
            let db = store_from_snapshot(dir.path(), from);
            let memories = damage::rows(&db);
            damage(&db);
            let damaged = fs::read(&db).unwrap();
            let refused = migrate(dir.path(), &db, from);
            assert!(!refused.status.success(), "v{from} {name}");
            assert!(
                stderr(&refused).contains("run `user-memory repair` first"),
                "v{from} {name}: {}",
                stderr(&refused)
            );
            assert_eq!(fs::read(&db).unwrap(), damaged, "v{from} {name}");
            assert!(backups(dir.path()).is_empty(), "v{from} {name}");
            let repaired = user_memory(dir.path(), &db, &["repair", "--db", db.to_str().unwrap()]);
            assert!(
                repaired.status.success(),
                "v{from} {name}: {}",
                stderr(&repaired)
            );
            let migrated = migrate(dir.path(), &db, from);
            assert!(
                migrated.status.success(),
                "v{from} {name}: {}",
                stderr(&migrated)
            );
            assert_eq!(version(&db), current, "v{from} {name}");
            assert_eq!(damage::rows(&db), memories, "v{from} {name}: memories lost");
            assert_eq!(
                damage::check(&db, "integrity_check"),
                ["ok"],
                "v{from} {name}"
            );
        }
    }
}

// Bumping the version without a migration, or with a lossy one, fails here.
#[test]
fn every_snapshot_migrates_to_the_current_schema_and_keeps_its_data() {
    let (_fresh_dir, fresh_db) = fresh();
    let current = version(&fresh_db);
    let target = normalized(&statements(&fresh_db));
    let versions = snapshot_versions();
    assert_eq!(versions.last(), Some(&current));
    for n in versions {
        let dir = tempdir().unwrap();
        let db = store_from_snapshot(dir.path(), n);
        let before = data(&db);
        assert!(
            before.iter().all(|rows| !rows.is_empty()),
            "v{n}.seed.sql must seed every table: {before:?}"
        );
        let out = migrate(dir.path(), &db, n);
        assert!(out.status.success(), "v{n}: {}", stderr(&out));
        if n == current {
            assert!(stdout(&out).contains("is up to date"), "{}", stdout(&out));
            assert!(backups(dir.path()).is_empty());
        } else {
            assert!(
                stdout(&out).contains(&format!("from schema version {n} to {current}")),
                "{}",
                stdout(&out)
            );
            assert_eq!(backups(dir.path()).len(), 1);
        }
        assert_eq!(version(&db), current);
        assert_eq!(normalized(&statements(&db)), target, "v{n}");
        assert_eq!(data(&db), before, "v{n} lost data");
        Connection::open(&db)
            .unwrap()
            .execute(
                "INSERT INTO memory_search(memory_search) VALUES('integrity-check')",
                [],
            )
            .unwrap();
        let recalled = user_memory(
            dir.path(),
            &db,
            &[
                "search",
                "--status",
                "all",
                "--page-size",
                "20",
                "--project",
                "github.com/x/y",
            ],
        );
        assert!(recalled.status.success(), "v{n}: {}", stderr(&recalled));
        let recalled: serde_json::Value = serde_json::from_slice(&recalled.stdout).unwrap();
        assert_eq!(
            recalled["memories"].as_array().unwrap().len(),
            before[0].len(),
            "v{n}"
        );
    }
}

#[test]
fn normalizing_ignores_layout_and_quoting_but_not_literals() {
    assert_eq!(
        normalize("CREATE TABLE \"t\" (x TEXT,\n    y TEXT DEFAULT 'a  b')"),
        normalize("CREATE TABLE t(x TEXT, y TEXT DEFAULT 'a  b')")
    );
    assert_ne!(
        normalize("CREATE TABLE t(y TEXT DEFAULT 'a  b')"),
        normalize("CREATE TABLE t(y TEXT DEFAULT 'a b')")
    );
    assert_ne!(
        normalize("CREATE TABLE t(x INT)"),
        normalize("CREATE TABLE t(x TEXT)")
    );
    assert_eq!(
        parse(
            "-- c\nCREATE TABLE a(x)\n;\nCREATE TRIGGER t AFTER DELETE ON a BEGIN\n    DELETE FROM a;\nEND\n;\n"
        ),
        [
            "CREATE TABLE a(x)",
            "CREATE TRIGGER t AFTER DELETE ON a BEGIN\n    DELETE FROM a;\nEND"
        ]
    );
}

// A store from a later version is refused everywhere and left as it is.
#[test]
fn a_store_from_a_newer_version_is_refused_and_left_unchanged() {
    let (dir, db) = fresh();
    let newer = version(&db) + 1;
    Connection::open(&db)
        .unwrap()
        .pragma_update(None, "user_version", newer)
        .unwrap();
    let before = fs::read(&db).unwrap();
    for args in [
        &["migrate"][..],
        &["search"],
        &["index"],
        &["record", "--category", "x", "--content", "y"],
        &["export", "-"],
    ] {
        let out = user_memory(dir.path(), &db, args);
        assert!(!out.status.success(), "{args:?}");
        assert!(
            stderr(&out).contains(&format!(
                "has schema version {newer}, but this user-memory is older than the store"
            )) && stderr(&out).contains("update user-memory and restart your client"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
    let hook = user_memory(
        dir.path(),
        &db,
        &["session-start", dir.path().to_str().unwrap()],
    );
    assert!(
        stdout(&hook).contains("update user-memory and restart your client"),
        "{}",
        stdout(&hook)
    );
    let token = dir.path().join("token");
    fs::write(&token, "a".repeat(40)).unwrap();
    let host = user_memory(
        dir.path(),
        &db,
        &[
            "host",
            "--listen",
            "127.0.0.1:0",
            "--db",
            db.to_str().unwrap(),
            "--token-file",
            token.to_str().unwrap(),
        ],
    );
    assert!(!host.status.success());
    assert!(
        stderr(&host).contains("older than the store"),
        "{}",
        stderr(&host)
    );
    assert_eq!(fs::read(&db).unwrap(), before);
    assert!(backups(dir.path()).is_empty());
}

// Servers and the CLI never migrate: an earlier store points to setup.
#[test]
fn a_store_from_an_earlier_version_is_refused_until_migrated() {
    let (_fresh_dir, fresh_db) = fresh();
    let current = version(&fresh_db);
    for n in snapshot_versions().into_iter().filter(|&n| n < current) {
        let dir = tempdir().unwrap();
        let db = store_from_snapshot(dir.path(), n);
        let before = fs::read(&db).unwrap();
        for args in [&["search"][..], &["index"]] {
            let out = user_memory(dir.path(), &db, args);
            assert!(!out.status.success(), "v{n} {args:?}");
            assert!(
                stderr(&out).contains(&format!(
                    "has schema version {n}; run the installer again (or ./setup, or `user-memory migrate`) to upgrade it"
                )),
                "v{n} {args:?}: {}",
                stderr(&out)
            );
        }
        assert_eq!(fs::read(&db).unwrap(), before, "v{n}");
        let out = migrate(dir.path(), &db, n);
        assert!(out.status.success(), "v{n}: {}", stderr(&out));
        assert!(stdout(&out).contains("Backed up"), "{}", stdout(&out));
        assert!(user_memory(dir.path(), &db, &["search"]).status.success());
    }
}

#[test]
fn migrate_needs_an_existing_local_store() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("missing.sqlite3");
    let out = user_memory(dir.path(), &db, &["migrate"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("run setup to create one"),
        "{}",
        stderr(&out)
    );
    assert!(!db.exists());
    let remote = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .args(["migrate"])
        .sandboxed(dir.path())
        .env_remove("USER_MEMORY_DB")
        .env("USER_MEMORY_URL", "https://memory.example.invalid/mcp")
        .env("USER_MEMORY_TOKEN_FILE", dir.path().join("token"))
        .output()
        .unwrap();
    assert!(!remote.status.success());
    assert!(
        stderr(&remote).contains("Run migrate on the memory server"),
        "{}",
        stderr(&remote)
    );
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

// Version 3 adds a title per memory, taken from a titles map the user
// approved. Without a complete, valid map the migration refuses and changes
// nothing, not even a backup; with one it stores each title as written once
// its whitespace is tidied, and changes nothing else, including recency and
// insertion order.
#[test]
fn migrating_to_version_3_takes_every_title_from_the_map() {
    let dir = tempdir().unwrap();
    let db = store_from_snapshot(dir.path(), 2);
    let ids: Vec<String> = Connection::open(&db)
        .unwrap()
        .prepare("SELECT id FROM memories ORDER BY rowid")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let titles: Vec<String> = [
        "Prefer Rust — memory safety first",
        "Tabs, once",
        "Indent with four spaces in github.com/x/y",
        "Keep answers short 👍",
        "Direct, concise work",
    ]
    .iter()
    .map(|t| (*t).to_owned())
    .chain([format!("Consistent {}", "f".repeat(109))])
    .collect();
    assert_eq!(titles[5].chars().count(), 120);
    let full: serde_json::Map<String, serde_json::Value> = ids
        .iter()
        .zip(&titles)
        .map(|(id, title)| (id.clone(), title.as_str().into()))
        .collect();
    // A title's whitespace is tidied, never refused.
    let mut full = full;
    full.insert(ids[1].clone(), "  Tabs,\n  once ".into());
    let map_file = dir.path().join("titles.json");
    let migrate = |map: Option<serde_json::Value>| {
        let mut args = vec!["migrate"];
        if let Some(map) = map {
            fs::write(&map_file, map.to_string()).unwrap();
            args.extend(["--titles", map_file.to_str().unwrap()]);
        }
        user_memory(dir.path(), &db, &args)
    };
    let before = fs::read(&db).unwrap();
    let with = |id: &str, title: serde_json::Value| {
        let mut map = full.clone();
        map.insert(id.into(), title);
        Some(map.into())
    };
    let mut partial = full.clone();
    partial.remove(&ids[2]);
    for (case, map, expected) in [
        (
            "no map",
            None,
            format!("Untitled memories (6):\n  {}  Prefer Rust\n", ids[0]),
        ),
        (
            "partial",
            Some(partial.into()),
            format!(
                "Untitled memories (1):\n  {}  Indent with four spaces",
                ids[2]
            ),
        ),
        (
            "unknown id",
            with("1a2b3c4d-9999-4aaa-8aaa-000000000009", "Stray".into()),
            "not memories here (1):\n  1a2b3c4d-9999-4aaa-8aaa-000000000009".into(),
        ),
        (
            "empty",
            with(&ids[1], "".into()),
            format!("  {}: the title is empty", ids[1]),
        ),
        (
            "over 120",
            with(&ids[1], "x".repeat(121).into()),
            format!(
                "  {}: the title has 121 characters; the limit is 120",
                ids[1]
            ),
        ),
    ] {
        let out = migrate(map);
        assert!(!out.status.success(), "{case}");
        let error = stderr(&out);
        assert!(
            error.contains("Migrating to schema version 3 needs a title for every memory"),
            "{case}: {error}"
        );
        assert!(error.contains(&expected), "{case}: {error}");
        assert_eq!(fs::read(&db).unwrap(), before, "{case}");
        assert!(backups(dir.path()).is_empty(), "{case}");
    }

    let kept = "SELECT rowid,id,status,source,updated_at,content FROM memories ORDER BY rowid";
    let rows_before = rows(&Connection::open(&db).unwrap(), kept);
    let out = migrate(Some(full.clone().into()));
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        !stdout(&out).contains("No titles map was needed"),
        "{}",
        stdout(&out)
    );
    let conn = Connection::open(&db).unwrap();
    assert_eq!(rows(&conn, kept), rows_before);
    let stored: Vec<String> = conn
        .prepare("SELECT title FROM memories ORDER BY rowid")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(stored, titles);
    // The search index finds memories by their title words too.
    let found = rows(
        &conn,
        "SELECT id FROM memory_search WHERE memory_search MATCH 'title:once'",
    );
    assert_eq!(found, [format!("'{}'", ids[1])]);
    // A map given for a store that needs none is reported, never ignored.
    let again = migrate(Some(full.into()));
    assert!(again.status.success(), "{}", stderr(&again));
    let current = current_version();
    assert!(
        stdout(&again).contains(&format!(
            "No titles map was needed: the store is already at schema version {current}."
        )),
        "{}",
        stdout(&again)
    );
}

// A check refuses as the migration would and changes nothing either way, so
// an upgrade can learn what it needs before it stops a running server.
#[test]
fn a_migration_check_changes_nothing() {
    let dir = tempdir().unwrap();
    let db = store_from_snapshot(dir.path(), 2);
    let map = titles_file(&db);
    let before = fs::read(&db).unwrap();
    let refused = user_memory(dir.path(), &db, &["migrate", "--check"]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("needs a title for every memory"),
        "{}",
        stderr(&refused)
    );
    let ready = user_memory(
        dir.path(),
        &db,
        &["migrate", "--check", "--titles", map.to_str().unwrap()],
    );
    assert!(ready.status.success(), "{}", stderr(&ready));
    let current = current_version();
    assert!(
        stdout(&ready).contains(&format!(
            "can be migrated from schema version 2 to {current}; the check changed nothing."
        )),
        "{}",
        stdout(&ready)
    );
    assert_eq!(fs::read(&db).unwrap(), before);
    assert!(backups(dir.path()).is_empty());
}

// The map can come on stdin, as the Docker upgrade passes it.
#[test]
fn a_titles_map_can_come_from_stdin() {
    let dir = tempdir().unwrap();
    let db = store_from_snapshot(dir.path(), 2);
    let map = fs::read_to_string(titles_file(&db)).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .args(["migrate", "--titles", "-"])
        .sandboxed(dir.path())
        .env("USER_MEMORY_DB", &db)
        .env_remove("USER_MEMORY_URL")
        .env_remove("USER_MEMORY_TOKEN_FILE")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(map.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(version(&db), current_version());
}
