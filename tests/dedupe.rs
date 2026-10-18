//! Duplicate prevention at record time: similar memories hold a record back
//! until the agent says how it relates to them.

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
    net::TcpListener,
    path::PathBuf,
    process::{Command, Output},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[path = "support/time.rs"]
mod time;
use tempfile::{TempDir, tempdir};
use time::STUCK;

const EMBED_ENV: [&str; 6] = [
    "USER_MEMORY_EMBED_URL",
    "USER_MEMORY_EMBED_MODEL",
    "USER_MEMORY_EMBED_DIMENSIONS",
    "USER_MEMORY_EMBED_QUERY_INSTRUCTION",
    "USER_MEMORY_EMBED_API_KEY_FILE",
    "USER_MEMORY_EMBED_ALLOW_HTTP",
];

/// Stand-in embeddings endpoint for a calibrated model: one axis per concept,
/// so paraphrases without shared words land on the same vector. It can be
/// switched off (errors), or told to hold each request until a number of
/// them have arrived.
struct Embedder {
    url: String,
    up: Arc<AtomicBool>,
    gather: Arc<Gather>,
}

/// Requests held until `want` of them have arrived (none held at 0), or
/// until [`STUCK`].
#[derive(Default)]
struct Gather {
    want: AtomicUsize,
    arrived: AtomicUsize,
}

const CONCEPTS: [&[&str]; 4] = [
    &["rust", "cargo", "borrow"],
    &["python", "pip", "notebook"],
    &["helix", "vim", "editor", "modal"],
    &["tabs", "indent", "indentation", "whitespace"],
];

fn vector(text: &str) -> Vec<f32> {
    let words = text.to_lowercase();
    let mut v = vec![0.0; 1024];
    let axis = CONCEPTS
        .iter()
        .position(|words_of| words_of.iter().any(|w| words.contains(w)))
        .unwrap_or_else(|| 10 + words.len() % 900);
    v[axis] = 1.0;
    v
}

impl Embedder {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/embeddings", listener.local_addr().unwrap());
        let up = Arc::new(AtomicBool::new(true));
        let gather = Arc::new(Gather::default());
        let (serving, gathering) = (up.clone(), gather.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (serving, gathering) = (serving.clone(), gathering.clone());
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            length = v.trim().parse().unwrap();
                        }
                    }
                    let mut body = vec![0; length];
                    if reader.read_exact(&mut body).is_err() {
                        return;
                    }
                    gathering.arrived.fetch_add(1, Ordering::SeqCst);
                    let deadline = Instant::now() + STUCK;
                    while gathering.arrived.load(Ordering::SeqCst)
                        < gathering.want.load(Ordering::SeqCst)
                        && Instant::now() < deadline
                    {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let (status, reply) = if serving.load(Ordering::SeqCst) {
                        let request: Value = serde_json::from_slice(&body).unwrap();
                        let data: Vec<Value> = request["input"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .enumerate()
                            .map(|(i, text)| json!({"index":i,"embedding":vector(text.as_str().unwrap())}))
                            .collect();
                        ("200 OK", json!({"data":data}).to_string())
                    } else {
                        ("503 Unavailable", "{}".to_owned())
                    };
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                });
            }
        });
        Self { url, up, gather }
    }
}

struct Store {
    _root: TempDir,
    home: PathBuf,
    db: PathBuf,
    embed: Vec<(String, String)>,
}

impl Store {
    fn new() -> Self {
        let root = tempdir().unwrap();
        Self {
            home: root.path().to_owned(),
            db: root.path().join("memory.sqlite3"),
            _root: root,
            embed: Vec::new(),
        }
    }
    fn with_embedder(embedder: &Embedder, model: &str) -> Self {
        let mut s = Self::new();
        s.embed = vec![
            ("USER_MEMORY_EMBED_URL".into(), embedder.url.clone()),
            ("USER_MEMORY_EMBED_MODEL".into(), model.into()),
            ("USER_MEMORY_EMBED_DIMENSIONS".into(), "1024".into()),
        ];
        s
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .args(titled(args))
            .sandboxed(&self.home)
            .env("USER_MEMORY_DB", &self.db)
            .env_remove("USER_MEMORY_URL");
        for key in EMBED_ENV {
            command.env_remove(key);
        }
        command.envs(self.embed.iter().map(|(k, v)| (k, v)));
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
    fn value(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        if args.first() == Some(&"search") {
            return one_page(value, args.contains(&"--page-size"));
        }
        value
    }
    fn record(&self, extra: &[&str]) -> Value {
        let mut args = vec![
            "record",
            "--type",
            "preference",
            "--category",
            "programming",
        ];
        args.extend_from_slice(extra);
        self.value(&args)
    }
    fn stored(&self, extra: &[&str]) -> String {
        let out = self.record(extra);
        assert_eq!(out["stored"], true, "{out}");
        out["id"].as_str().unwrap().to_owned()
    }
    fn memory(&self, id: &str) -> Value {
        self.value(&["search", "--status", "all", "--page-size", "20"])
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap_or_else(|| panic!("{id} missing"))
            .clone()
    }
    fn count(&self, sql: &str) -> i64 {
        rusqlite::Connection::open(&self.db)
            .unwrap()
            .query_row(sql, [], |r| r.get(0))
            .unwrap()
    }
}

fn similar_ids(out: &Value) -> Vec<String> {
    assert_eq!(out["stored"], false, "{out}");
    assert_eq!(out["reason"], "similar memories exist", "{out}");
    out["similar"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_owned())
        .collect()
}

// The user says "I prefer Rust" again in a new chat: one memory, two conversations.
#[test]
fn a_restatement_is_held_back_then_recorded_as_a_confirmation() {
    let s = Store::new();
    let rust = s.stored(&["--content", "Prefer Rust", "--origin", "chat/x"]);
    let other = s.stored(&["--content", "Answer briefly"]);
    let out = s.record(&["--content", "Prefer Rust", "--origin", "chat/y"]);
    assert_eq!(similar_ids(&out), vec![rust.clone()]);
    assert_eq!(
        out["similar"][0],
        json!({"id":rust,"content":"Prefer Rust","type":"preference","category":"programming",
            "scope":"global","project":"","status":"confirmed","source":"explicit","match":"keyword 1.00"})
    );
    assert_eq!(
        out["resolve"],
        "call record again with confirms, supersedes or distinct_from"
    );
    let confirmed = s.record(&[
        "--content",
        "Prefer Rust",
        "--origin",
        "chat/y",
        "--confirms",
        &rust,
    ]);
    assert_eq!(
        confirmed,
        json!({"stored":true,"id":rust,"confirmed":true,"confirmations":2})
    );
    let memory = s.memory(&rust);
    assert_eq!(memory["origins"], json!(["chat/x", "chat/y"]));
    assert_ne!(memory["last_confirmed_at"], Value::Null);
    assert_eq!(s.memory(&other)["last_confirmed_at"], Value::Null);
    assert_eq!(
        s.count("SELECT COUNT(*) FROM memories"),
        2,
        "no copy of the restatement"
    );
    // Confirming again from a known conversation adds nothing; without one it only stamps the time.
    assert_eq!(
        s.record(&[
            "--content",
            "Prefer Rust",
            "--origin",
            "chat/y",
            "--confirms",
            &rust
        ])["confirmations"],
        2
    );
    assert_eq!(
        s.record(&["--content", "Prefer Rust", "--confirms", &rust])["confirmations"],
        2
    );
    // An exact repeat from a conversation that already stated it stays a harmless no-op.
    let again = s.record(&["--content", "Prefer Rust", "--origin", "chat/x"]);
    assert_eq!(
        again,
        json!({"stored":true,"id":rust,"status":"confirmed","duplicate":true})
    );
}

#[test]
fn confirmations_need_an_explicit_statement_about_an_active_explicit_memory_in_place() {
    let s = Store::new();
    let rust = s.stored(&["--content", "Prefer Rust", "--origin", "chat/x"]);
    let local = s.stored(&[
        "--content",
        "Use Go here",
        "--scope",
        "project",
        "--project",
        "repo",
    ]);
    let habit = s.value(&[
        "record",
        "--type",
        "behavior",
        "--category",
        "programming",
        "--content",
        "Ran cargo fmt before committing",
        "--source",
        "observed",
        "--evidence",
        "Seen in review",
    ])["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let refusal = "confirms must name an active explicit memory with the same scope and project";
    for (target, extra) in [
        (local.as_str(), &[][..]),
        (habit.as_str(), &[][..]),
        ("3d9f829a-c594-45d2-84b6-1469b20d653b", &[][..]),
        (
            rust.as_str(),
            &["--scope", "project", "--project", "repo"][..],
        ),
    ] {
        let mut args = vec!["--content", "Prefer Rust", "--confirms", target];
        args.extend_from_slice(extra);
        assert_eq!(s.record(&args)["reason"], refusal, "{target} {extra:?}");
    }
    let observed = s.value(&[
        "record",
        "--type",
        "preference",
        "--category",
        "programming",
        "--content",
        "Prefer Rust",
        "--source",
        "observed",
        "--evidence",
        "Chose Rust",
        "--confirms",
        &rust,
    ]);
    assert_eq!(
        observed["reason"],
        "Only the user's explicit statement can confirm a memory"
    );
    let newer = s.stored(&["--content", "Prefer Zig", "--supersedes", &rust]);
    assert_eq!(
        s.record(&["--content", "Prefer Rust", "--confirms", &rust])["reason"],
        refusal
    );
    assert_eq!(
        s.record(&[
            "--content",
            "x",
            "--confirms",
            &newer,
            "--supersedes",
            &newer
        ])["reason"],
        "Give at most one of confirms, supersedes or distinct_from"
    );
}

// A changed mind replaces the old memory; only the new conversation stated it.
#[test]
fn a_changed_mind_supersedes_with_only_the_new_conversation() {
    let s = Store::new();
    let tabs = s.stored(&["--content", "Indent with tabs", "--origin", "chat/x"]);
    let out = s.record(&[
        "--content",
        "Indent with tabs, width 4",
        "--origin",
        "chat/y",
    ]);
    assert_eq!(similar_ids(&out), vec![tabs.clone()]);
    let spaces = s.record(&[
        "--content",
        "Indent with tabs, width 4",
        "--origin",
        "chat/y",
        "--supersedes",
        &tabs,
    ]);
    assert_eq!(spaces["superseded"], json!([tabs]));
    let id = spaces["id"].as_str().unwrap();
    assert_eq!(s.memory(id)["origins"], json!(["chat/y"]));
    assert_eq!(s.memory(&tabs)["status"], "superseded");
    let forgotten = s.value(&["forget", "--origin", "chat/y"]);
    assert_eq!(forgotten["ids"], json!([id]));
}

// The user approves merging two copies of the same preference into one.
#[test]
fn a_merge_keeps_every_conversation_of_the_duplicates() {
    let s = Store::new();
    let unrelated = s.stored(&["--content", "Answer briefly", "--origin", "chat/w"]);
    let a = s.stored(&["--content", "Prefer Rust", "--origin", "chat/x"]);
    let b = s.record(&["--content", "Prefer Rust", "--origin", "chat/y"]);
    let b = s.record(&[
        "--content",
        "Prefer Rust",
        "--origin",
        "chat/y",
        "--distinct-from",
        &similar_ids(&b)[0],
    ])["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        s.record(&[
            "--content",
            "Prefer Rust always",
            "--supersedes",
            &a,
            "--merge"
        ])["reason"],
        "merge needs at least two memories in supersedes"
    );
    assert_eq!(
        s.record(&["--content", "Prefer Rust always", "--merge"])["reason"],
        "merge needs at least two memories in supersedes"
    );
    let merged = s.record(&[
        "--content",
        "Prefer Rust always",
        "--origin",
        "chat/z",
        "--supersedes",
        &a,
        "--supersedes",
        &b,
        "--merge",
    ]);
    assert_eq!(merged["stored"], true, "{merged}");
    let mut targets = vec![a.clone(), b.clone()];
    targets.sort();
    assert_eq!(merged["superseded"], json!(targets));
    let id = merged["id"].as_str().unwrap();
    assert_eq!(
        s.memory(id)["origins"],
        json!(["chat/x", "chat/y", "chat/z"])
    );
    assert_eq!(s.memory(id)["confirmations"], 3);
    assert_eq!(s.memory(&a)["origins"], json!(["chat/x"]), "history kept");
    assert_eq!(s.memory(&unrelated)["origins"], json!(["chat/w"]));
}

// A genuinely different memory is recorded once the agent lists every reported one.
#[test]
fn distinct_from_must_cover_todays_top_candidates_and_is_remembered() {
    let s = Store::new();
    let a = s.stored(&["--content", "Prefer Rust for tools"]);
    let out = s.record(&["--content", "Prefer Rust for servers"]);
    assert_eq!(similar_ids(&out), vec![a.clone()]);
    assert_eq!(
        s.record(&[
            "--content",
            "Prefer Rust for servers",
            "--distinct-from",
            "3d9f829a-c594-45d2-84b6-1469b20d653b"
        ])["reason"],
        "distinct_from names a memory that does not exist"
    );
    let b = s.record(&[
        "--content",
        "Prefer Rust for servers",
        "--distinct-from",
        &a,
    ]);
    assert_eq!(b["stored"], true, "{b}");
    let b = b["id"].as_str().unwrap().to_owned();
    assert_eq!(s.count("SELECT COUNT(*) FROM memory_distinct"), 1);
    // A third memory similar to both: naming only the first leaves the second unresolved.
    let third = ["--content", "Prefer Rust for scripts"];
    let out = s.record(&third);
    let mut both = vec![a.clone(), b.clone()];
    both.sort();
    let mut shown = similar_ids(&out);
    shown.sort();
    assert_eq!(shown, both);
    let mut partial = third.to_vec();
    partial.extend(["--distinct-from", &a]);
    let again = s.record(&partial);
    assert_eq!(similar_ids(&again).len(), 2, "still held back");
    partial.extend(["--distinct-from", &b]);
    assert_eq!(s.record(&partial)["stored"], true);
    assert_eq!(s.count("SELECT COUNT(*) FROM memory_distinct"), 3);
}

// Observations from different conversations are independent evidence, never duplicates.
#[test]
fn observations_hold_each_other_back_only_within_one_conversation() {
    let s = Store::new();
    let observe = |origin: &str| {
        s.value(&[
            "record",
            "--type",
            "behavior",
            "--category",
            "review",
            "--content",
            "Asked for the diff first",
            "--source",
            "observed",
            "--evidence",
            "User asked for the diff before reading code",
            "--origin",
            origin,
        ])
    };
    let first = observe("chat/x");
    assert_eq!(first["stored"], true);
    assert_eq!(observe("chat/y")["stored"], true, "another conversation");
    let first_id = first["id"].as_str().unwrap();
    // The same conversation repeating it exactly is the idempotent duplicate.
    assert_eq!(observe("chat/x")["id"], first_id);
    let reworded = s.value(&[
        "record",
        "--type",
        "behavior",
        "--category",
        "review",
        "--content",
        "Asked for the diff first again",
        "--source",
        "observed",
        "--evidence",
        "User asked for the diff a second time",
        "--origin",
        "chat/x",
    ]);
    assert!(similar_ids(&reworded).contains(&first_id.to_owned()));
}

// A project memory is compared with global ones; a global memory never with a project's.
#[test]
fn the_pool_is_where_the_new_memory_applies() {
    let s = Store::new();
    s.stored(&[
        "--content",
        "Use Go here",
        "--scope",
        "project",
        "--project",
        "repo",
    ]);
    assert_eq!(s.record(&["--content", "Use Go here"])["stored"], true);
    let global = s.stored(&["--content", "Prefer tabs always"]);
    let out = s.record(&[
        "--content",
        "Prefer tabs always",
        "--scope",
        "project",
        "--project",
        "other",
    ]);
    assert_eq!(similar_ids(&out), [global]);
}

// "Go in this repo only" next to a general Rust preference is recorded directly.
#[test]
fn a_narrower_project_exception_is_not_held_back() {
    let s = Store::new();
    s.stored(&["--content", "Prefer Rust for command-line tools"]);
    let exception = s.record(&[
        "--content",
        "Use Go in this repository only",
        "--scope",
        "project",
        "--project",
        "repo",
    ]);
    assert_eq!(exception["stored"], true, "{exception}");
}

// Paraphrases without shared words are caught by the calibrated semantic signal,
// and the vector saved at record time is what later checks compare against.
#[test]
fn paraphrases_are_held_back_through_saved_vectors() {
    let embedder = Embedder::start();
    let s = Store::with_embedder(&embedder, "qwen3-embedding:4b");
    let rust = s.stored(&["--content", "Prefer Rust"]);
    assert_eq!(s.count("SELECT COUNT(*) FROM memory_vectors"), 1);
    let out = s.record(&["--content", "Cargo projects are my default"]);
    assert_eq!(similar_ids(&out), vec![rust.clone()]);
    assert_eq!(out["similar"][0]["match"], "semantic 1.00");
    // An uncalibrated model gives no semantic signal: keyword only.
    let other = Store::with_embedder(&embedder, "some-other-model");
    other.stored(&["--content", "Prefer Rust"]);
    assert_eq!(
        other.record(&["--content", "Cargo projects are my default"])["stored"],
        true
    );
}

// The endpoint goes away between the agent's two calls.
#[test]
fn an_embedder_flap_never_blocks_a_resolution() {
    let embedder = Embedder::start();
    let s = Store::with_embedder(&embedder, "qwen3-embedding:4b");
    let rust = s.stored(&["--content", "Prefer Rust", "--origin", "chat/x"]);
    let paraphrase = [
        "--content",
        "Cargo projects are my default",
        "--origin",
        "chat/y",
    ];
    assert_eq!(similar_ids(&s.record(&paraphrase)), vec![rust.clone()]);
    embedder.up.store(false, Ordering::SeqCst);
    // Without the semantic signal nothing is similar, yet both resolutions still work.
    let mut confirm = paraphrase.to_vec();
    confirm.extend(["--confirms", &rust]);
    assert_eq!(s.record(&confirm)["confirmations"], 2);
    let mut distinct = paraphrase.to_vec();
    distinct.extend(["--distinct-from", &rust]);
    assert_eq!(s.record(&distinct)["stored"], true);
    // With the endpoint down, recording falls back to keyword checks and still works.
    let keyword = s.record(&["--content", "Prefer Rust", "--origin", "chat/z"]);
    assert_eq!(similar_ids(&keyword), [rust]);
}

// Clients record the same preference at the same moment: one row, the others
// held back, and no "database is locked". The model answers no record until
// two are embedding at once, which a record embedding under the write lock
// would never allow: the others would wait for that lock instead.
#[test]
fn concurrent_records_never_hold_the_write_lock_while_embedding() {
    let embedder = Embedder::start();
    embedder.gather.want.store(2, Ordering::SeqCst);
    let s = Store::with_embedder(&embedder, "qwen3-embedding:4b");
    let results: Vec<Output> = std::thread::scope(|scope| {
        let handles: Vec<_> = ["chat/x", "chat/y", "chat/z"]
            .map(|origin| {
                let s = &s;
                scope.spawn(move || {
                    s.run(&[
                        "record",
                        "--type",
                        "preference",
                        "--category",
                        "programming",
                        "--content",
                        "Prefer Rust",
                        "--origin",
                        origin,
                    ])
                })
            })
            .into_iter()
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut stored = 0;
    for out in &results {
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        assert!(!stderr.contains("locked"), "{stderr}");
        let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
        if reply["stored"] == true {
            stored += 1;
        } else {
            similar_ids(&reply);
        }
    }
    assert_eq!(stored, 1);
    assert_eq!(s.count("SELECT COUNT(*) FROM memories"), 1);
}

// Memories recorded before embeddings were enabled get vectors from reindex;
// after that, a paraphrase of one is held back.
#[test]
fn reindexed_memories_catch_paraphrases_recorded_later() {
    let embedder = Embedder::start();
    let mut s = Store::new();
    let rust = s.stored(&["--content", "Prefer Rust"]);
    assert_eq!(s.count("SELECT COUNT(*) FROM memory_vectors"), 0);
    s.embed = Store::with_embedder(&embedder, "qwen3-embedding:4b").embed;
    assert_eq!(s.value(&["reindex"])["embedded"], 1);
    let out = s.record(&["--content", "Cargo projects are my default"]);
    assert_eq!(similar_ids(&out), [rust]);
}

// An agent repeating a memory exactly while naming that same memory as
// distinct gets the harmless duplicate, not a storage error.
#[test]
fn an_exact_repeat_listing_itself_as_distinct_stays_a_duplicate() {
    let s = Store::new();
    let rust = s.stored(&["--content", "Prefer Rust", "--origin", "chat/x"]);
    let again = s.record(&[
        "--content",
        "Prefer Rust",
        "--origin",
        "chat/x",
        "--distinct-from",
        &rust,
    ]);
    assert_eq!(again["id"], rust.as_str());
    assert_eq!(again["duplicate"], true);
    assert_eq!(s.count("SELECT COUNT(*) FROM memory_distinct"), 0);
}

// The user asks to clean up: duplicates recorded before the check existed are
// listed for review, and the approved merge consolidates them.
#[test]
fn review_lists_duplicates_and_an_approved_merge_consolidates_them() {
    let s = Store::new();
    let a = s.stored(&["--content", "Prefer Rust", "--origin", "chat/x"]);
    let b = s.stored(&["--content", "Use Go here", "--origin", "chat/y"]);
    // Make the second a copy of the first, as an older version could have stored it.
    rusqlite::Connection::open(&s.db)
        .unwrap()
        .execute(
            "UPDATE memories SET content='Prefer Rust' WHERE id=?1",
            [&b],
        )
        .unwrap();
    let review = s.value(&["review"]);
    let group = &review["groups"][0];
    assert_eq!(group["kind"], "duplicate", "{review}");
    let mut members: Vec<&str> = group["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    members.sort();
    let mut expected = vec![a.as_str(), b.as_str()];
    expected.sort();
    assert_eq!(members, expected);
    let before = std::fs::read(&s.db).unwrap();
    s.value(&["review", "--max-groups", "1"]);
    assert_eq!(std::fs::read(&s.db).unwrap(), before, "review never writes");
    let merged = s.record(&[
        "--content",
        "Prefer Rust",
        "--origin",
        "chat/z",
        "--supersedes",
        &a,
        "--supersedes",
        &b,
        "--merge",
    ]);
    assert_eq!(merged["stored"], true, "{merged}");
    assert_eq!(s.value(&["review"])["groups"], json!([]));
}

/// Every row of the memory tables, every column, in a fixed order.
fn rows(s: &Store) -> Vec<String> {
    let conn = rusqlite::Connection::open(&s.db).unwrap();
    let mut found = Vec::new();
    for table in ["memories", "memory_origins"] {
        let mut statement = conn
            .prepare(&format!("SELECT * FROM {table} ORDER BY 1, 2"))
            .unwrap();
        let columns = statement.column_count();
        let mut rows = statement.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            let values: Vec<String> = (0..columns)
                .map(|i| format!("{:?}", row.get_ref(i).unwrap()))
                .collect();
            found.push(format!("{table}: {}", values.join(" | ")));
        }
    }
    found
}

// The user says two memories review grouped mean different things: review stops
// grouping them, and no memory changes, so nothing built on them goes stale.
#[test]
fn marking_existing_memories_distinct_changes_no_memory_and_ends_their_group() {
    let s = Store::new();
    let a = s.stored(&["--content", "Prefer Rust", "--origin", "chat/x"]);
    let b = s.stored(&["--content", "Use Go here", "--origin", "chat/y"]);
    let pattern = s.value(&[
        "record",
        "--type",
        "pattern",
        "--category",
        "programming",
        "--content",
        "Weighs language choice per project",
        "--source",
        "inferred",
        "--evidence-id",
        &a,
        "--evidence-id",
        &b,
    ]);
    assert_eq!(pattern["stored"], true, "{pattern}");
    // An older copy, with timestamps old enough that any rewrite would show.
    rusqlite::Connection::open(&s.db)
        .unwrap()
        .execute_batch(&format!(
            "UPDATE memories SET content='Prefer Rust' WHERE id='{b}';
             UPDATE memories SET updated_at='2000-01-01 00:00:00', created_at='2000-01-01 00:00:00';
             UPDATE memories SET status='tentative' WHERE id='{}';",
            pattern["id"].as_str().unwrap()
        ))
        .unwrap();
    assert_eq!(
        s.memory(pattern["id"].as_str().unwrap())["status"],
        "tentative"
    );
    assert_eq!(s.value(&["review"])["groups"][0]["kind"], "duplicate");
    let before = rows(&s);
    let marked = s.value(&["update", &a, "--distinct-from", &b]);
    assert_eq!(marked, json!({"updated":false,"distinct_marked":1,"id":a}));
    assert_eq!(rows(&s), before, "marking distinct rewrites no memory");
    assert_eq!(
        s.memory(pattern["id"].as_str().unwrap())["status"],
        "tentative"
    );
    assert_eq!(s.value(&["review"])["groups"], json!([]));
    let again = s.value(&["update", &b, "--distinct-from", &a]);
    assert_eq!(again["distinct_marked"], 0, "the pair is unordered");
    let combined = s.value(&[
        "update",
        &a,
        "--distinct-from",
        &b,
        "--content",
        "Prefer Rust!",
    ]);
    assert_eq!(
        combined["reason"],
        "Mark memories distinct separately from changing one"
    );
    assert_eq!(rows(&s), before);
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
