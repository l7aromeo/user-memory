#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
};
use tempfile::{TempDir, tempdir};

const EMBED_ENV: [&str; 6] = [
    "USER_MEMORY_EMBED_URL",
    "USER_MEMORY_EMBED_MODEL",
    "USER_MEMORY_EMBED_DIMENSIONS",
    "USER_MEMORY_EMBED_QUERY_INSTRUCTION",
    "USER_MEMORY_EMBED_API_KEY_FILE",
    "USER_MEMORY_EMBED_ALLOW_HTTP",
];

fn run(home: &std::path::Path, args: &[&str], remote: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
    command
        .args(titled(args))
        .sandboxed(home)
        .env("USER_MEMORY_DB", home.join("memory.sqlite3"))
        .env_remove("USER_MEMORY_URL")
        .env_remove("USER_MEMORY_TOKEN_FILE");
    for key in EMBED_ENV {
        command.env_remove(key);
    }
    if remote {
        command
            .env("USER_MEMORY_URL", "https://memory.example.invalid/mcp")
            .env("USER_MEMORY_TOKEN_FILE", home.join("token"));
    }
    command.output().unwrap()
}

#[test]
fn reindex_rebuilds_the_local_store_and_refuses_on_remote_clients() {
    let home = tempdir().unwrap();
    for content in ["Prefer Rust", "Answer briefly"] {
        let out = run(
            home.path(),
            &["record", "--category", "general", "--content", content],
            false,
        );
        assert!(out.status.success());
    }
    let out = run(home.path(), &["reindex"], false);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["reindexed"], 2);

    let out = run(home.path(), &["reindex"], true);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("memory server"));
}

#[test]
fn search_and_the_index_leave_out_replaced_memories() {
    let home = tempdir().unwrap();
    let mut ids = Vec::new();
    for content in ["Active preference", "Replaced preference", "Unknown state"] {
        let out = run(
            home.path(),
            &["record", "--category", "general", "--content", content],
            false,
        );
        let saved: Value = serde_json::from_slice(&out.stdout).unwrap();
        ids.push(saved["id"].as_str().unwrap().to_owned());
    }
    let out = run(
        home.path(),
        &[
            "record",
            "--type",
            "behavior",
            "--category",
            "general",
            "--content",
            "Observed habit",
            "--source",
            "observed",
            "--evidence",
            "Seen in review",
        ],
        false,
    );
    assert!(out.status.success());
    let conn = rusqlite::Connection::open(home.path().join("memory.sqlite3")).unwrap();
    for replaced in &ids[1..] {
        conn.execute(
            "UPDATE memories SET status='superseded',superseded_by=?2 WHERE id=?1",
            [replaced, &ids[0]],
        )
        .unwrap();
    }
    drop(conn);
    let contents = |args: &[&str]| -> Vec<String> {
        let out = run(home.path(), args, false);
        let mut found: Vec<String> =
            serde_json::from_slice::<Value>(&out.stdout).unwrap()["memories"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["content"].as_str().unwrap().to_owned())
                .collect();
        found.sort();
        found
    };
    assert_eq!(
        contents(&["search", "--page-size", "20"]),
        ["Active preference", "Observed habit"]
    );
    assert_eq!(
        contents(&["search", "--page-size", "20", "--status", "all"]),
        [
            "Active preference",
            "Observed habit",
            "Replaced preference",
            "Unknown state"
        ]
    );
    let profile = String::from_utf8(run(home.path(), &["index"], false).stdout).unwrap();
    assert!(profile.contains("Active preference"));
    assert!(profile.contains("Observed habit"));
    assert!(!profile.contains("Replaced preference"));
    assert!(!profile.contains("Unknown state"));
}

/// Deterministic stand-in for an OpenAI-compatible embeddings endpoint. Each
/// vector axis is one concept, so texts about the same concept are similar
/// without sharing words.
struct Embedder {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
}

const CONCEPTS: [&[&str]; 3] = [
    &["rust", "language", "cli", "compiler"],
    &["editor", "helix", "typing"],
    &["vegetarian", "recipes", "dinner"],
];

fn concept_vector(text: &str, dimensions: Option<usize>) -> Vec<f64> {
    let text = text.to_lowercase();
    let mut vector: Vec<f64> = CONCEPTS
        .iter()
        .map(|words| words.iter().filter(|w| text.contains(*w)).count() as f64)
        .collect();
    vector.push(0.05);
    vector.resize(dimensions.unwrap_or(vector.len()).max(1), 0.0);
    vector
}

impl Embedder {
    /// Serves embeddings, or answers every request with `status` when it is not 200.
    fn start(status: u16) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/embeddings", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let mut length = 0;
                let mut authorization = Value::Null;
                reader.read_line(&mut line).unwrap();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':')
                        && key.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                    if let Some((key, value)) = line.split_once(':')
                        && key.eq_ignore_ascii_case("authorization")
                    {
                        authorization = json!(value.trim());
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let mut request: Value = serde_json::from_slice(&body).unwrap();
                request["authorization"] = authorization;
                log.lock().unwrap().push(request.clone());
                let dimensions = request["dimensions"].as_u64().map(|d| d as usize);
                let reply = if status == 200 {
                    let data: Vec<Value> = request["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .enumerate()
                        .map(|(i, text)| {
                            json!({"index":i,"embedding":concept_vector(text.as_str().unwrap(), dimensions)})
                        })
                        .collect();
                    json!({"data":data,"model":request["model"]}).to_string()
                } else {
                    json!({"error":"unavailable"}).to_string()
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        Self { url, requests }
    }
    /// Every input sent so far, in order.
    fn inputs(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .flat_map(|r| r["input"].as_array().unwrap().clone())
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    }
}

struct Store {
    root: TempDir,
}

impl Store {
    fn new() -> Self {
        Self {
            root: tempdir().unwrap(),
        }
    }
    fn db(&self) -> PathBuf {
        self.root.path().join("memory.sqlite3")
    }
    fn command(&self, embed: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .sandboxed(self.root.path())
            .env("USER_MEMORY_DB", self.db())
            .env_remove("USER_MEMORY_URL")
            .env_remove("USER_MEMORY_TOKEN_FILE");
        for key in EMBED_ENV {
            command.env_remove(key);
        }
        command.envs(embed.iter().copied());
        command
    }
    fn output(&self, embed: &[(&str, &str)], args: &[&str]) -> Output {
        let out = self.command(embed).args(titled(args)).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
    fn value(&self, embed: &[(&str, &str)], args: &[&str]) -> Value {
        let value: Value = serde_json::from_slice(&self.output(embed, args).stdout).unwrap();
        if args.first() == Some(&"search") {
            return one_page(value, args.contains(&"--page-size"));
        }
        value
    }
    fn stored(&self, args: &[&str]) -> String {
        let mut full: Vec<String> = ["record"]
            .iter()
            .chain(args)
            .map(|a| (*a).to_owned())
            .collect();
        // These fixtures are distinct memories even when their words overlap.
        // A concurrent writer can add a candidate between calls, so retry.
        for _ in 0..10 {
            let refs: Vec<&str> = full.iter().map(String::as_str).collect();
            let out = self.value(&[], &refs);
            if out["reason"] != "similar memories exist" {
                assert_eq!(out["stored"], true, "{out}");
                return out["id"].as_str().unwrap().to_owned();
            }
            for m in out["similar"].as_array().unwrap() {
                full.extend([
                    "--distinct-from".to_owned(),
                    m["id"].as_str().unwrap().to_owned(),
                ]);
            }
        }
        panic!("{args:?} kept meeting new similar memories");
    }
    fn contents(&self, embed: &[(&str, &str)], args: &[&str]) -> Vec<String> {
        let mut full = vec!["search"];
        full.extend_from_slice(args);
        self.value(embed, &full)
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["content"].as_str().unwrap().to_owned())
            .collect()
    }
    fn sql(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.db()).unwrap()
    }
    fn search_rows(&self) -> i64 {
        self.sql()
            .query_row("SELECT COUNT(*) FROM memory_search", [], |r| r.get(0))
            .unwrap()
    }
}

fn preferences(s: &Store) -> [String; 3] {
    [
        ("programming", "Generally prefer Rust over Python"),
        ("tools", "Use Helix as the text editor"),
        ("cooking", "Vegetarian recipes only"),
    ]
    .map(|(category, content)| {
        s.stored(&[
            "--type",
            "preference",
            "--category",
            category,
            "--content",
            content,
        ])
    })
}

#[test]
fn search_index_tables_are_created_once_and_follow_store_level_changes() {
    let s = Store::new();
    let [rust, ..] = preferences(&s);
    let conn = s.sql();
    let tables: Vec<String> = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND (name LIKE 'memory_search%' OR name='memory_vectors') ORDER BY name",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        tables,
        [
            "memory_search",
            "memory_search_config",
            "memory_search_content",
            "memory_search_data",
            "memory_search_docsize",
            "memory_search_idx",
            "memory_vectors"
        ]
    );
    assert_eq!(s.search_rows(), 3);
    conn.execute(
        "INSERT INTO memories(id,memory_type,category,content,title) VALUES ('00000000-0000-4000-8000-000000000001','fact','tools','Keyboard layout is Colemak','Keyboard layout is Colemak')",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE memories SET content='Prefer Zig for systems work' WHERE id=?1",
        [&rust],
    )
    .unwrap();
    drop(conn);
    assert_eq!(
        s.contents(&[], &["colemak"]),
        ["Keyboard layout is Colemak"]
    );
    assert_eq!(s.contents(&[], &["zig"]), ["Prefer Zig for systems work"]);
    assert!(s.contents(&[], &["rust"]).is_empty());
    s.value(&[], &["forget", &rust]);
    assert!(s.contents(&[], &["zig"]).is_empty());
    assert_eq!(s.search_rows(), 3);
}

#[test]
fn memories_stored_before_the_index_existed_are_indexed_on_first_use() {
    let s = Store::new();
    preferences(&s);
    let conn = s.sql();
    conn.execute_batch(
        "DROP TRIGGER memory_search_inserted; DROP TRIGGER memory_search_changed; DROP TRIGGER memory_search_deleted; DROP TABLE memory_search; DROP TABLE memory_vectors;",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memories(id,memory_type,category,content,title) VALUES ('00000000-0000-4000-8000-000000000001','fact','tools','Keyboard layout is Colemak','Keyboard layout is Colemak')",
        [],
    )
    .unwrap();
    drop(conn);
    assert_eq!(
        s.contents(&[], &["colemak"]),
        ["Keyboard layout is Colemak"]
    );
    assert_eq!(s.search_rows(), 4);
    s.value(&[], &["search", "editor"]);
    assert_eq!(s.search_rows(), 4);
}

#[test]
fn concurrent_first_use_indexes_each_memory_once() {
    let s = Store::new();
    std::thread::scope(|scope| {
        for worker in 0..8 {
            let s = &s;
            scope.spawn(move || {
                s.stored(&[
                    "--category",
                    "context",
                    "--content",
                    &format!("Parallel note {worker}"),
                ]);
            });
        }
    });
    assert_eq!(s.search_rows(), 8);
    assert_eq!(s.contents(&[], &["parallel", "--page-size", "20"]).len(), 8);
}

#[test]
fn reindex_rebuilds_a_damaged_keyword_index() {
    let s = Store::new();
    preferences(&s);
    s.sql().execute("DELETE FROM memory_search", []).unwrap();
    assert!(s.contents(&[], &["editor"]).is_empty());
    let report = s.value(&[], &["reindex"]);
    assert_eq!(report, json!({"reindexed":3,"embedded":null}));
    assert_eq!(
        s.contents(&[], &["editor"]),
        ["Use Helix as the text editor"]
    );
    s.value(&[], &["reindex"]);
    assert_eq!(
        s.search_rows(),
        3,
        "reindexing twice must not duplicate rows"
    );
}

#[test]
fn topic_words_that_are_fts_operators_are_plain_words() {
    let s = Store::new();
    s.stored(&[
        "--category",
        "style",
        "--content",
        "Do not near or mix tabs",
    ]);
    for topic in [
        "not tabs",
        "NEAR OR",
        "tabs NOT",
        "\"quoted\" tabs*",
        "a:b tabs",
    ] {
        assert_eq!(
            s.contents(&[], &[topic]),
            ["Do not near or mix tabs"],
            "{topic}"
        );
    }
}

#[test]
fn project_filters_accept_any_spelling_of_the_same_repository() {
    let s = Store::new();
    s.stored(&[
        "--category",
        "programming",
        "--content",
        "Run the linters before pushing",
        "--scope",
        "project",
        "--project",
        "https://github.com/owner/repo",
    ]);
    for spelling in [
        "git@github.com:Owner/Repo.git",
        "ssh://git@github.com/owner/repo.git",
    ] {
        assert_eq!(
            s.contents(&[], &["linters", "--project", spelling]),
            ["Run the linters before pushing"]
        );
        let index =
            String::from_utf8(s.output(&[], &["index", "--project", spelling]).stdout).unwrap();
        assert!(
            index.contains(" Run the linters before pushing [project]"),
            "{index}"
        );
    }
    assert!(
        s.contents(&[], &["linters", "--project", "other"])
            .is_empty()
    );
}

#[test]
fn project_memories_come_first_with_and_without_a_topic() {
    let s = Store::new();
    s.stored(&[
        "--category",
        "programming",
        "--content",
        "Prefer Rust everywhere",
    ]);
    s.stored(&[
        "--category",
        "notes",
        "--content",
        "Rust nightly is pinned here",
        "--scope",
        "project",
        "--project",
        "lab",
    ]);
    s.stored(&[
        "--category",
        "programming",
        "--content",
        "Prefer Rust for tools",
    ]);
    // The global memories match "rust" more strongly (category alias) and are newer.
    assert_eq!(
        s.contents(&[], &["programming rust", "--project", "lab"])[0],
        "Rust nightly is pinned here"
    );
    assert_eq!(
        s.contents(&[], &["--project", "lab"]),
        [
            "Rust nightly is pinned here",
            "Prefer Rust for tools",
            "Prefer Rust everywhere"
        ]
    );
}

#[test]
fn category_aliases_widen_topics_and_filters_but_not_unknown_labels() {
    let s = Store::new();
    s.stored(&[
        "--category",
        "Coding",
        "--content",
        "Prefer small functions",
    ]);
    s.stored(&["--category", "comms", "--content", "Answer briefly"]);
    s.stored(&["--category", "home lab", "--content", "Rootless containers"]);
    assert_eq!(
        s.contents(&[], &["development"]),
        ["Prefer small functions"]
    );
    assert_eq!(
        s.contents(&[], &["--category", "Software  Development"]),
        ["Prefer small functions"]
    );
    assert_eq!(s.contents(&[], &["writing"]), ["Answer briefly"]);
    assert_eq!(
        s.contents(&[], &["--category", "process"]),
        Vec::<String>::new()
    );
    assert_eq!(
        s.contents(&[], &["--category", "lab"]),
        Vec::<String>::new()
    );
    assert_eq!(
        s.contents(&[], &["--category", "home lab"]),
        ["Rootless containers"]
    );
}

#[test]
fn a_topic_moves_its_memories_to_the_front_of_their_groups_and_hides_nothing() {
    let s = Store::new();
    s.stored(&[
        "--type",
        "goal",
        "--category",
        "workflow",
        "--content",
        "Get the changelog reviewed before tagging",
    ]);
    for i in 0..4 {
        s.stored(&[
            "--type",
            "goal",
            "--category",
            "communication",
            "--content",
            &format!("Tone goal {i}"),
        ]);
    }
    let lines = |args: &[&str]| -> Vec<String> {
        index_text(&s, args)
            .lines()
            .filter(|l| l.starts_with("    "))
            .map(str::to_owned)
            .collect()
    };
    let plain = lines(&[]);
    assert_eq!(plain.len(), 5, "{plain:?}");
    assert!(plain[0].contains("Tone goal 3"), "{plain:?}");
    let topical = lines(&["--topic", "changelog"]);
    assert_eq!(topical.len(), 5, "{topical:?}");
    assert!(
        topical[0].contains("Get the changelog reviewed"),
        "{topical:?}"
    );
    assert!(topical[1].contains("Tone goal 3"), "{topical:?}");
}

fn index_text(s: &Store, args: &[&str]) -> String {
    let mut full = vec!["index"];
    full.extend_from_slice(args);
    String::from_utf8(s.output(&[], &full).stdout).unwrap()
}

#[test]
fn the_index_lists_every_rule_and_every_other_memory() {
    let s = Store::new();
    for i in 0..6 {
        s.stored(&[
            "--type",
            ["preference", "constraint", "value"][i % 3],
            "--category",
            "workflow",
            "--content",
            &format!("Standing rule number {i}"),
        ]);
    }
    for i in 0..4 {
        s.stored(&[
            "--type",
            "goal",
            "--category",
            "work",
            "--content",
            &format!("Goal {i}"),
        ]);
    }
    s.stored(&[
        "--type",
        "decision",
        "--category",
        "work",
        "--content",
        "Chose SQLite",
    ]);
    s.stored(&[
        "--type",
        "preference",
        "--category",
        "communication",
        "--content",
        "Skims long answers",
        "--source",
        "observed",
        "--evidence",
        "Asked for a summary after a long reply",
    ]);
    let index = index_text(&s, &[]);
    let others = index.find("\nOther memories\n").expect("other section");
    assert!(index.contains("\nRules\n  workflow (6)\n"), "{index}");
    for i in 0..6 {
        let at = index.find(&format!("Standing rule number {i}")).unwrap();
        assert!(at < others, "{index}");
    }
    // An explicit decision and a tentative observed preference are not rules.
    for other in [
        "Chose SQLite",
        "Skims long answers (tentative)",
        "Goal 0",
        "Goal 3",
    ] {
        assert!(index[others..].contains(other), "{other}: {index}");
    }
    assert_eq!(
        index.lines().filter(|l| l.starts_with("    ")).count(),
        12,
        "{index}"
    );
}

// The line after the header counts every memory the index covers, so a new
// memory can reuse an existing category instead of inventing a near twin.
#[test]
fn the_index_counts_categories_types_and_scopes() {
    let s = Store::new();
    let empty = index_text(&s, &[]);
    assert_eq!(empty.trim_end(), "No user memories recorded yet.");
    let record = |kind: &str, category: &str, content: &str, project: Option<&str>| {
        let mut args = vec!["--type", kind, "--category", category, "--content", content];
        if let Some(project) = project {
            args.extend(["--scope", "project", "--project", project]);
        }
        s.stored(&args)
    };
    let web = "github.com/acme/web";
    let api = "github.com/acme/api";
    record("preference", "workflow", "Ask before pushing to main", None);
    record(
        "preference",
        "workflow",
        "Rebase feature branches before review",
        None,
    );
    record(
        "constraint",
        "workflow",
        "Never commit secrets or tokens",
        None,
    );
    record("fact", "tools", "Uses Helix as the text editor", None);
    let brief = record("preference", "communication", "Answer briefly", None);
    s.stored(&[
        "--type",
        "preference",
        "--category",
        "communication",
        "--content",
        "Answer briefly, with one example",
        "--supersedes",
        &brief,
    ]);
    record(
        "preference",
        "workflow",
        "Use pnpm in this repository",
        Some(web),
    );
    record("decision", "tools", "Chose Postgres for the api", Some(api));
    record(
        "preference",
        "tools",
        "Format Go code with gofmt",
        Some(api),
    );
    let counts = |args: &[&str]| -> String {
        let index = index_text(&s, args);
        let line = index.lines().nth(4).unwrap_or_default().to_owned();
        assert!(line.starts_with("Categories: "), "{index}");
        line
    };
    // Active global memories and the one project asked for; counts tie on
    // count, then by name.
    let default = counts(&["--project", web]);
    assert_eq!(
        default,
        "Categories: workflow 4, communication 1, tools 1 · Types: preference 4, constraint 1, fact 1 · Scope: global 5, project 1"
    );
    // Every project names its key.
    assert_eq!(
        counts(&["--scope", "all"]),
        format!(
            "Categories: workflow 4, tools 3, communication 1 · Types: preference 5, constraint 1, decision 1, fact 1 · Scope: global 5, {api} 2, {web} 1"
        )
    );
    // The status filter counts the superseded memory too.
    assert_eq!(
        counts(&["--scope", "all", "--status", "all"]),
        format!(
            "Categories: workflow 4, tools 3, communication 2 · Types: preference 6, constraint 1, decision 1, fact 1 · Scope: global 6, {api} 2, {web} 1"
        )
    );
    // A copy that leaves the global rules out still counts them.
    let without = index_text(&s, &["--project", web, "--without-global-rules"]);
    assert_eq!(without.lines().nth(4), Some(default.as_str()), "{without}");
    assert!(!without.contains("Ask before pushing to main"), "{without}");
}

#[test]
fn the_index_shows_the_title_and_read_returns_every_word() {
    let s = Store::new();
    let content = format!("Keep every word of this rule {}", "visible ".repeat(70))
        .trim_end()
        .to_owned();
    assert!(content.chars().count() > 500 && content.chars().count() <= 600);
    let context = format!(
        "First clause stays whole; {} Last sentence.",
        "padding ".repeat(40)
    );
    let id = s.stored(&[
        "--type",
        "preference",
        "--category",
        "workflow",
        "--content",
        &content,
        "--context",
        &context,
        "--title",
        "Keep every word of a rule visible",
    ]);
    let index = index_text(&s, &[]);
    assert!(
        index.contains(&format!(
            "    {} Keep every word of a rule visible\n",
            &id[..8]
        )) || index.ends_with(&format!(
            "    {} Keep every word of a rule visible",
            &id[..8]
        )),
        "{index}"
    );
    assert!(!index.contains("padding"), "{index}");
    let read = s.value(&[], &["read", &id[..8]]);
    assert_eq!(read["memories"][0]["content"], content.as_str());
    let collapsed = context.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(read["memories"][0]["context"], collapsed);
}

#[test]
fn semantic_search_embeds_lazily_and_reuses_current_vectors() {
    let s = Store::new();
    let fake = Embedder::start(200);
    let env = [("USER_MEMORY_EMBED_URL", fake.url.as_str())];
    let [rust, ..] = preferences(&s);
    s.stored(&["--category", "notes", "--content", "Plain note"]);
    assert!(fake.inputs().is_empty(), "recording must not embed");
    assert_eq!(
        s.contents(
            &env,
            &["which language for a fast tool", "--page-size", "1"]
        ),
        ["Generally prefer Rust over Python"]
    );
    assert_eq!(fake.inputs().len(), 5, "query plus four memories");
    assert_eq!(
        s.contents(&env, &["something for typing code", "--page-size", "1"]),
        ["Use Helix as the text editor"]
    );
    assert_eq!(fake.inputs().len(), 6, "only the query is embedded again");
    s.value(
        &[],
        &["update", &rust, "--content", "Prefer the Zig language"],
    );
    s.contents(&env, &["dinner ideas"]);
    let inputs = fake.inputs();
    assert_eq!(inputs.len(), 8, "query plus the corrected memory");
    assert!(inputs[7].contains("Prefer the Zig language"));
    let other = [
        ("USER_MEMORY_EMBED_URL", fake.url.as_str()),
        ("USER_MEMORY_EMBED_MODEL", "other-model"),
    ];
    s.contents(&other, &["dinner ideas"]);
    assert_eq!(
        fake.inputs().len(),
        13,
        "another model re-embeds everything"
    );
    let models: Vec<String> = s
        .sql()
        .prepare("SELECT DISTINCT model FROM memory_vectors")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(models, ["other-model"]);
    let shorter = [
        ("USER_MEMORY_EMBED_URL", fake.url.as_str()),
        ("USER_MEMORY_EMBED_MODEL", "other-model"),
        ("USER_MEMORY_EMBED_DIMENSIONS", "6"),
    ];
    s.contents(&shorter, &["dinner ideas"]);
    assert_eq!(
        fake.inputs().len(),
        18,
        "another length re-embeds everything"
    );
}

#[test]
fn semantic_matches_are_bounded_and_fused_with_keyword_matches() {
    let s = Store::new();
    let fake = Embedder::start(200);
    let env = [("USER_MEMORY_EMBED_URL", fake.url.as_str())];
    preferences(&s);
    for i in 0..6 {
        s.stored(&[
            "--category",
            "notes",
            "--content",
            &format!("Filler note {i}"),
        ]);
    }
    // Keyword-only and semantic-only matches both appear; unrelated memories do not.
    let found = s.contents(&env, &["helix compiler", "--page-size", "2"]);
    assert_eq!(found.len(), 2);
    assert!(
        found.contains(&"Use Helix as the text editor".to_owned()),
        "{found:?}"
    );
    assert!(
        found.contains(&"Generally prefer Rust over Python".to_owned()),
        "{found:?}"
    );
    let keyword = s.contents(&[], &["helix compiler", "--page-size", "5"]);
    assert_eq!(keyword, ["Use Helix as the text editor"]);
}

#[test]
fn query_instruction_and_dimensions_reach_the_endpoint_for_the_right_inputs() {
    let s = Store::new();
    let fake = Embedder::start(200);
    preferences(&s);
    let env = [
        ("USER_MEMORY_EMBED_URL", fake.url.as_str()),
        ("USER_MEMORY_EMBED_MODEL", "qwen-test"),
        ("USER_MEMORY_EMBED_DIMENSIONS", "8"),
        (
            "USER_MEMORY_EMBED_QUERY_INSTRUCTION",
            "Instruct: find the user's preference\\nQuery:",
        ),
    ];
    assert_eq!(
        s.contents(&env, &["language", "--page-size", "1"]),
        ["Generally prefer Rust over Python"]
    );
    let requests = fake.requests.lock().unwrap().clone();
    assert!(
        requests
            .iter()
            .all(|r| r["model"] == "qwen-test" && r["dimensions"] == 8)
    );
    let inputs = fake.inputs();
    assert_eq!(
        inputs[0],
        "Instruct: find the user's preference\nQuery:language"
    );
    assert!(inputs[1..].iter().all(|i| !i.contains("Instruct")));
    let dimensions: i64 = s
        .sql()
        .query_row("SELECT MIN(dimensions) FROM memory_vectors", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(dimensions, 8);
}

#[test]
fn reindex_embeds_every_memory_without_a_current_vector() {
    let s = Store::new();
    let fake = Embedder::start(200);
    let env = [("USER_MEMORY_EMBED_URL", fake.url.as_str())];
    let [rust, ..] = preferences(&s);
    assert_eq!(
        s.value(&env, &["reindex"]),
        json!({"reindexed":3,"embedded":3})
    );
    assert_eq!(
        s.value(&env, &["reindex"]),
        json!({"reindexed":3,"embedded":0})
    );
    s.value(&[], &["forget", &rust]);
    let vectors = || -> i64 {
        s.sql()
            .query_row("SELECT COUNT(*) FROM memory_vectors", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(vectors(), 2);
    s.sql()
        .execute(
            "INSERT INTO memory_vectors (id,model,dimensions,source,vector) VALUES ('00000000-0000-4000-8000-000000000009','old',0,'gone',x'00000000')",
            [],
        )
        .unwrap();
    s.value(&env, &["reindex"]);
    assert_eq!(vectors(), 2, "reindex removes vectors of missing memories");
}

#[test]
fn unusable_embedding_endpoints_fall_back_to_keywords_with_one_warning() {
    let s = Store::new();
    preferences(&s);
    let failing = Embedder::start(503);
    let insecure = Embedder::start(200);
    let insecure_url = insecure.url.replace("127.0.0.1", "localhost.");
    let user_url = insecure.url.replace("127.0.0.1", "agent@127.0.0.1");
    let password_url = insecure.url.replace("127.0.0.1", ":secret@127.0.0.1");
    for (url, reason) in [
        (user_url.as_str(), "credentials"),
        (password_url.as_str(), "credentials"),
        (failing.url.as_str(), "refused"),
        (insecure_url.as_str(), "HTTPS"),
        ("http://127.0.0.1:9/v1/embeddings", "unreachable"),
        ("not a url", "not a URL"),
    ] {
        let out = s.output(
            &[("USER_MEMORY_EMBED_URL", url)],
            &["search", "text editor"],
        );
        let found: Vec<Value> = serde_json::from_slice::<Value>(&out.stdout).unwrap()["memories"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(found.len(), 1, "{url}");
        assert_eq!(found[0]["content"], "Use Helix as the text editor");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(stderr.lines().count(), 1, "{url}: {stderr}");
        assert!(stderr.contains(reason), "{url}: {stderr}");
    }
    assert!(
        insecure.inputs().is_empty(),
        "plain http off loopback was contacted"
    );
    for dimensions in ["many", "0", "-3"] {
        let bad = s.output(
            &[
                ("USER_MEMORY_EMBED_URL", insecure.url.as_str()),
                ("USER_MEMORY_EMBED_DIMENSIONS", dimensions),
            ],
            &["search", "editor"],
        );
        assert!(
            String::from_utf8_lossy(&bad.stderr).contains("positive integer"),
            "{dimensions}"
        );
    }
    assert!(insecure.inputs().is_empty());
    let unset = s.output(&[("USER_MEMORY_EMBED_URL", " ")], &["search", "editor"]);
    assert!(unset.stderr.is_empty(), "an empty URL means keyword-only");
}

#[test]
fn a_long_running_server_warns_once_about_a_failing_endpoint() {
    let s = Store::new();
    preferences(&s);
    let failing = Embedder::start(503);
    let mut child = s
        .command(&[("USER_MEMORY_EMBED_URL", failing.url.as_str())])
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    writeln!(input, "{}", json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}})).unwrap();
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    for id in 2..4 {
        writeln!(input, "{}", json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"search","arguments":{"topic":"text editor"}}})).unwrap();
    }
    input.flush().unwrap();
    let mut answered = 0;
    while answered < 3 {
        let mut line = String::new();
        assert!(output.read_line(&mut line).unwrap() > 0);
        let reply: Value = serde_json::from_str(&line).unwrap();
        if reply["id"].as_i64().is_some_and(|id| id >= 2) {
            assert!(
                reply["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("Use Helix as the text editor")
            );
        }
        answered += 1;
    }
    drop(input);
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.matches("semantic search unavailable").count(),
        1,
        "{stderr}"
    );
    assert_eq!(
        failing.inputs().len(),
        2,
        "each recall still tries the endpoint"
    );
}

#[test]
fn shared_function_words_do_not_outrank_a_semantic_match() {
    let s = Store::new();
    let fake = Embedder::start(200);
    let env = [("USER_MEMORY_EMBED_URL", fake.url.as_str())];
    preferences(&s);
    s.stored(&[
        "--category",
        "communication",
        "--content",
        "Answer in short paragraphs",
    ]);
    assert_eq!(
        s.contents(
            &env,
            &["which language to write a fast tool in", "--page-size", "1"]
        ),
        ["Generally prefer Rust over Python"]
    );
    assert!(s.contents(&[], &["which is it to be in"]).len() == 4);
}

#[test]
fn api_key_file_authenticates_requests_without_ever_printing_the_key() {
    let s = Store::new();
    preferences(&s);
    let fake = Embedder::start(200);
    let key_file = s.root.path().join("embed-key");
    std::fs::write(&key_file, "  sk-test-0123456789\n").unwrap();
    let out = s.output(
        &[
            ("USER_MEMORY_EMBED_URL", fake.url.as_str()),
            ("USER_MEMORY_EMBED_API_KEY_FILE", key_file.to_str().unwrap()),
        ],
        &["search", "language", "--page-size", "1"],
    );
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let requests = fake.requests.lock().unwrap().clone();
    assert!(!requests.is_empty());
    assert!(
        requests
            .iter()
            .all(|r| r["authorization"] == "Bearer sk-test-0123456789")
    );
    let plain = Embedder::start(200);
    s.output(
        &[("USER_MEMORY_EMBED_URL", plain.url.as_str())],
        &["search", "language"],
    );
    assert!(
        plain
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| r["authorization"].is_null())
    );

    std::fs::write(s.root.path().join("spaced-key"), "sk-test two").unwrap();
    for (path, reason) in [
        ("embed-key", "absolute path"),
        (
            s.root.path().join("missing-key").to_str().unwrap(),
            "cannot be read",
        ),
        (
            s.root.path().join("spaced-key").to_str().unwrap(),
            "single printable token",
        ),
    ] {
        let refused = Embedder::start(200);
        let out = s.output(
            &[
                ("USER_MEMORY_EMBED_URL", refused.url.as_str()),
                ("USER_MEMORY_EMBED_API_KEY_FILE", path),
            ],
            &["search", "text editor"],
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(reason), "{path}: {stderr}");
        assert!(!stderr.contains("sk-test"), "{stderr}");
        assert!(refused.inputs().is_empty(), "{path}");
        let found: Vec<Value> = serde_json::from_slice::<Value>(&out.stdout).unwrap()["memories"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(found[0]["content"], "Use Helix as the text editor");
    }
}

#[test]
fn each_search_embeds_a_bounded_number_of_memories_newest_first() {
    let s = Store::new();
    let fake = Embedder::start(200);
    let env = [("USER_MEMORY_EMBED_URL", fake.url.as_str())];
    for i in 0..40 {
        s.stored(&[
            "--category",
            "notes",
            "--content",
            &format!("Note number {i}"),
        ]);
    }
    let out = s.output(&env, &["search", "dinner"]);
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("8 memories are not embedded yet and are found by keyword only")
    );
    let first = fake.inputs();
    assert_eq!(first.len(), 1 + 32);
    assert!(first.iter().any(|t| t.contains("Note number 39")));
    assert!(
        !first
            .iter()
            .any(|t| t.contains("Note number 7\n") || t.ends_with("Note number 7"))
    );
    s.contents(&env, &["dinner"]);
    assert_eq!(
        fake.inputs().len(),
        33 + 1 + 8,
        "the remaining eight follow"
    );
    let out = s.output(&env, &["search", "dinner"]);
    assert!(out.stderr.is_empty());
    assert_eq!(fake.inputs().len(), 42 + 1, "nothing left to embed");
}

#[test]
fn without_keyword_matches_the_more_similar_memory_wins_over_a_newer_one() {
    let s = Store::new();
    let fake = Embedder::start(200);
    let env = [("USER_MEMORY_EMBED_URL", fake.url.as_str())];
    s.stored(&[
        "--category",
        "notes",
        "--content",
        "Rust compiler upgrades first",
    ]);
    s.stored(&[
        "--category",
        "notes",
        "--content",
        "Keep CLI flags short for typing",
    ]);
    assert_eq!(
        s.contents(&env, &["language", "--page-size", "2"]),
        [
            "Rust compiler upgrades first",
            "Keep CLI flags short for typing"
        ]
    );
}

#[test]
fn empty_optional_embedding_settings_mean_unset_as_compose_passes_them() {
    let s = Store::new();
    preferences(&s);
    let fake = Embedder::start(200);
    let out = s.output(
        &[
            ("USER_MEMORY_EMBED_URL", fake.url.as_str()),
            ("USER_MEMORY_EMBED_MODEL", ""),
            ("USER_MEMORY_EMBED_DIMENSIONS", ""),
            ("USER_MEMORY_EMBED_QUERY_INSTRUCTION", ""),
            ("USER_MEMORY_EMBED_API_KEY_FILE", ""),
        ],
        &["search", "language", "--page-size", "1"],
    );
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let requests = fake.requests.lock().unwrap().clone();
    assert!(!requests.is_empty());
    for request in &requests {
        assert_eq!(request["model"], "qwen3-embedding:4b");
        assert!(request.get("dimensions").is_none(), "{request}");
        assert!(request["authorization"].is_null());
    }
    assert_eq!(fake.inputs()[0], "language");
}

#[test]
fn plain_http_to_a_private_host_needs_an_explicit_opt_in_and_never_carries_a_key() {
    let s = Store::new();
    preferences(&s);
    let fake = Embedder::start(200);
    // "localhost." resolves to this machine but is not a loopback name, like a
    // compose sidecar such as http://ollama:11434.
    let private = fake.url.replace("127.0.0.1", "localhost.");
    for allow in ["", "0", "false", "No"] {
        let out = s.output(
            &[
                ("USER_MEMORY_EMBED_URL", private.as_str()),
                ("USER_MEMORY_EMBED_ALLOW_HTTP", allow),
            ],
            &["search", "language", "--page-size", "1"],
        );
        assert!(String::from_utf8_lossy(&out.stderr).contains("USER_MEMORY_EMBED_ALLOW_HTTP=1"));
    }
    assert!(fake.inputs().is_empty());
    for allow in ["1", "TRUE", "yes"] {
        let out = s.output(
            &[
                ("USER_MEMORY_EMBED_URL", private.as_str()),
                ("USER_MEMORY_EMBED_ALLOW_HTTP", allow),
            ],
            &["search", "language", "--page-size", "1"],
        );
        assert!(
            out.stderr.is_empty(),
            "{allow}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let found: Vec<Value> = serde_json::from_slice::<Value>(&out.stdout).unwrap()["memories"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(
            found[0]["content"], "Generally prefer Rust over Python",
            "{allow}"
        );
    }
    let contacted = fake.inputs().len();
    assert!(contacted > 0);

    let key_file = s.root.path().join("embed-key");
    std::fs::write(&key_file, "sk-test-0123456789").unwrap();
    let with_key = |url: &str| {
        s.output(
            &[
                ("USER_MEMORY_EMBED_URL", url),
                ("USER_MEMORY_EMBED_ALLOW_HTTP", "1"),
                ("USER_MEMORY_EMBED_API_KEY_FILE", key_file.to_str().unwrap()),
            ],
            &["search", "language", "--page-size", "1"],
        )
    };
    let refused = with_key(&private);
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("never sent over plain http"), "{stderr}");
    assert!(!stderr.contains("sk-test"));
    assert_eq!(
        fake.inputs().len(),
        contacted,
        "a key must not travel in clear"
    );
    assert!(
        with_key(&fake.url).stderr.is_empty(),
        "loopback http may carry a key"
    );
    assert!(fake.inputs().len() > contacted);

    let seen = fake.inputs().len();
    for (url, allow, reason) in [
        (
            private.replace("localhost.", "agent@localhost."),
            "1",
            "credentials",
        ),
        (private.replace("http://", "ftp://"), "1", "HTTPS or HTTP"),
        (private.clone(), "maybe", "must be 1, true, yes"),
    ] {
        let out = s.output(
            &[
                ("USER_MEMORY_EMBED_URL", url.as_str()),
                ("USER_MEMORY_EMBED_ALLOW_HTTP", allow),
            ],
            &["search", "text editor"],
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(reason), "{url}: {stderr}");
    }
    assert_eq!(fake.inputs().len(), seen);
}

#[test]
fn the_index_breaks_ties_by_modification_time_before_insertion_order() {
    let s = Store::new();
    let older_row = s.stored(&[
        "--type",
        "preference",
        "--category",
        "tools",
        "--content",
        "Imported preference",
    ]);
    let newer_row = s.stored(&[
        "--type",
        "preference",
        "--category",
        "tools",
        "--content",
        "Local preference",
    ]);
    // An import keeps the file's timestamps but gets new rowids.
    s.sql()
        .execute(
            "UPDATE memories SET updated_at='2030-01-01 00:00:00' WHERE id=?1",
            [&older_row],
        )
        .unwrap();
    s.sql()
        .execute(
            "UPDATE memories SET updated_at='2020-01-01 00:00:00' WHERE id=?1",
            [&newer_row],
        )
        .unwrap();
    let profile = String::from_utf8(s.output(&[], &["index"]).stdout).unwrap();
    let position = |content: &str| profile.find(content).unwrap();
    assert!(
        position("Imported preference") < position("Local preference"),
        "{profile}"
    );
}

#[test]
fn the_index_lists_memories_confirmed_in_more_conversations_first() {
    let s = Store::new();
    let once = s.stored(&[
        "--type",
        "preference",
        "--category",
        "tools",
        "--content",
        "Use ripgrep for search",
    ]);
    let twice = s.stored(&[
        "--type",
        "preference",
        "--category",
        "tools",
        "--content",
        "Use Helix for editing",
    ]);
    let conn = s.sql();
    conn.execute(
        "INSERT INTO memory_origins (memory_id,origin) VALUES (?1,'claude:session/a')",
        [&once],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memory_origins (memory_id,origin) VALUES (?1,'claude:session/a'),(?1,'codex:thread/b')",
        [&twice],
    )
    .unwrap();
    // Recency alone would list the ripgrep preference first.
    conn.execute(
        "UPDATE memories SET updated_at='2030-01-01 00:00:00' WHERE id=?1",
        [&once],
    )
    .unwrap();
    drop(conn);
    let profile = String::from_utf8(s.output(&[], &["index"]).stdout).unwrap();
    let line = |content: &str| {
        profile
            .lines()
            .find(|l| l.contains(content))
            .unwrap()
            .to_owned()
    };
    assert!(
        profile.find("Use Helix").unwrap() < profile.find("Use ripgrep").unwrap(),
        "{profile}"
    );
    assert!(
        line("Use Helix").ends_with("Use Helix for editing ×2"),
        "{profile}"
    );
    assert!(!line("Use ripgrep").contains('×'), "{profile}");
}

/// `args` with `--title` set to the content (its first 120 characters, the
/// title limit) and `--scope` to global when a record gives none: most tests
/// here are about other fields. A test about the title or scope passes its
/// own.
fn titled(args: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    if args.first().map(String::as_str) == Some("record") && !args.iter().any(|a| a == "--scope") {
        args.extend(["--scope".to_owned(), "global".to_owned()]);
    }
    if args.first().map(String::as_str) == Some("record") && !args.iter().any(|a| a == "--title") {
        let content = args
            .iter()
            .position(|a| a == "--content")
            .and_then(|i| args.get(i + 1))
            .map(|content| content.chars().take(120).collect::<String>());
        if let Some(content) = content {
            args.extend(["--title".to_owned(), content]);
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
