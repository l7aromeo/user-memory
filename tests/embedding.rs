//! Choosing an embedding model: a new model is probed, calibrated on the
//! labelled pairs that ship in the binary, and every memory re-embedded.

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
    sync::{Arc, Mutex},
};
use tempfile::{TempDir, tempdir};

/// Every embedding setting, cleared so the test's own environment never leaks in.
const EMBED_ENV: [&str; 6] = [
    "USER_MEMORY_EMBED_URL",
    "USER_MEMORY_EMBED_MODEL",
    "USER_MEMORY_EMBED_DIMENSIONS",
    "USER_MEMORY_EMBED_QUERY_INSTRUCTION",
    "USER_MEMORY_EMBED_API_KEY_FILE",
    "USER_MEMORY_EMBED_ALLOW_HTTP",
];

/// The labelled pairs in the order calibration embeds them: short, then long.
fn classes() -> Vec<String> {
    [
        include_str!("../src/memory/calibration/similarity-pairs.json"),
        include_str!("../src/memory/calibration/similarity-long-pairs.json"),
    ]
    .iter()
    .flat_map(|text| {
        let set: Value = serde_json::from_str(text).unwrap();
        set["pairs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["class"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    })
    .collect()
}

/// Cosine a model that separates the classes gives a pair of `class`,
/// spread by its place `n` so the extremes matter.
fn separating(class: &str, n: usize) -> f32 {
    let spread = (n % 7) as f32 * 0.01;
    match class {
        "same" | "contradiction" => 0.86 + spread,
        "exception" => 0.62 + spread,
        "related" => 0.64 + spread,
        _ => 0.40 + spread,
    }
}

/// How the fake endpoint answers.
#[derive(Clone, Copy)]
enum Answer {
    /// Vectors that give each labelled pair its class's cosine.
    Separating,
    /// Vectors that give every labelled pair the same cosine.
    Alike,
    /// HTTP 500 for every request.
    Failing,
}

/// Stand-in embeddings endpoint. The labelled pairs arrive in order, after
/// the probe text: it counts them and gives pair i the vectors
/// a = e(2i) and b = s·e(2i) + √(1−s²)·e(2i+1), whose cosine is s. Any other
/// text gets one shared vector.
struct Endpoint {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl Endpoint {
    fn start(answer: Answer) -> Self {
        let classes = classes();
        let length = 2 * classes.len() + 1;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let counted = Arc::new(Mutex::new(0_usize));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut length_header = 0;
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length_header = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length_header];
                reader.read_exact(&mut body).unwrap();
                let request: Value = serde_json::from_slice(&body).unwrap();
                seen.lock()
                    .unwrap()
                    .push(json!({"path": first.split(' ').nth(1), "body": request}));
                if matches!(answer, Answer::Failing) {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                    );
                    continue;
                }
                let mut count = counted.lock().unwrap();
                let data: Vec<Value> = request["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .enumerate()
                    .map(|(index, text)| {
                        let text = text.as_str().unwrap();
                        let mut vector = vec![0.0_f32; length];
                        if text.contains("user-memory embedding probe")
                            || *count >= 2 * classes.len()
                        {
                            vector[length - 1] = 1.0;
                        } else {
                            let pair = *count / 2;
                            let cosine = match answer {
                                Answer::Separating => separating(&classes[pair], pair),
                                _ => 0.5,
                            };
                            if (*count).is_multiple_of(2) {
                                vector[2 * pair] = 1.0;
                            } else {
                                vector[2 * pair] = cosine;
                                vector[2 * pair + 1] = (1.0 - cosine * cosine).sqrt();
                            }
                            *count += 1;
                        }
                        json!({"index": index, "embedding": vector})
                    })
                    .collect();
                let reply = json!({"data": data}).to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        Self { url, requests }
    }
}

struct Store {
    _root: TempDir,
    home: PathBuf,
    db: PathBuf,
}

impl Store {
    fn new() -> Self {
        let root = tempdir().unwrap();
        Self {
            home: root.path().to_owned(),
            db: root.path().join("memory.sqlite3"),
            _root: root,
        }
    }
    fn run(&self, endpoint: Option<&Endpoint>, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .args(args)
            .sandboxed(&self.home)
            .env("USER_MEMORY_DB", &self.db)
            .env_remove("USER_MEMORY_URL")
            .env_remove("USER_MEMORY_TOKEN_FILE");
        for name in EMBED_ENV {
            command.env_remove(name);
        }
        if let Some(endpoint) = endpoint {
            command
                .env(
                    "USER_MEMORY_EMBED_URL",
                    format!("{}/embeddings", endpoint.url),
                )
                .env("USER_MEMORY_EMBED_MODEL", "fake-model");
        }
        command.output().unwrap()
    }
    fn record(&self, content: &str) {
        let out = self.run(
            None,
            &[
                "record",
                "--type",
                "preference",
                "--category",
                "workflow",
                "--title",
                content,
                "--content",
                content,
                "--scope",
                "global",
            ],
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("\"stored\":true"),
            "{out:?}"
        );
    }
    fn query<T: rusqlite::types::FromSql>(&self, sql: &str) -> T {
        rusqlite::Connection::open(&self.db)
            .unwrap()
            .query_row(sql, [], |r| r.get(0))
            .unwrap()
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

// A model whose vectors separate the labelled pairs gets cutoffs computed
// and stored, every memory is embedded with it, and show reports it.
#[test]
fn a_model_that_separates_the_pairs_is_calibrated_and_every_memory_embedded() {
    let s = Store::new();
    for content in [
        "Keep commits small",
        "Answer in short paragraphs",
        "Use Helix",
    ] {
        s.record(content);
    }
    let endpoint = Endpoint::start(Answer::Separating);
    let out = s.run(Some(&endpoint), &["embedding", "sync"]);
    let said = text(&out);
    assert!(out.status.success(), "{said}");
    assert!(said.contains("calibrated on the labelled pairs"), "{said}");
    assert!(
        said.contains("Embedded 3 memories with fake-model"),
        "{said}"
    );
    let (duplicate, related): (f64, f64) = rusqlite::Connection::open(&s.db)
        .unwrap()
        .query_row(
            "SELECT duplicate, related FROM embedding_cutoffs WHERE model='fake-model'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(related < duplicate, "{related} {duplicate}");
    let vectors: i64 = s.query("SELECT count(*) FROM memory_vectors WHERE model='fake-model'");
    assert_eq!(vectors, 3);
    let shown = text(&s.run(Some(&endpoint), &["embedding", "show"]));
    assert!(shown.contains("Model: fake-model"), "{shown}");
    assert!(
        shown.contains("computed from the labelled pairs"),
        "{shown}"
    );
}

// A model that scores every pair alike cannot be calibrated: it says so,
// records that, and duplicate detection stays keyword-only.
#[test]
fn a_model_that_cannot_separate_the_pairs_stays_keyword_only() {
    let s = Store::new();
    s.record("Keep commits small");
    let endpoint = Endpoint::start(Answer::Alike);
    let out = s.run(Some(&endpoint), &["embedding", "sync"]);
    let said = text(&out);
    assert!(out.status.success(), "{said}");
    assert!(
        said.contains("does not separate the labelled pairs well enough"),
        "{said}"
    );
    assert!(said.contains("duplicate detection uses keywords"), "{said}");
    let stored: Option<f64> =
        s.query("SELECT duplicate FROM embedding_cutoffs WHERE model='fake-model'");
    assert_eq!(stored, None);
    let shown = text(&s.run(Some(&endpoint), &["embedding", "show"]));
    assert!(
        shown.contains("cannot separate the labelled pairs"),
        "{shown}"
    );
}

// An endpoint that fails the probe is refused before anything is
// calibrated or embedded.
#[test]
fn an_endpoint_that_fails_the_probe_changes_nothing() {
    let s = Store::new();
    s.record("Keep commits small");
    let endpoint = Endpoint::start(Answer::Failing);
    let out = s.run(Some(&endpoint), &["embedding", "sync"]);
    let said = text(&out);
    assert!(!out.status.success(), "{said}");
    assert!(said.contains("did not answer the probe"), "{said}");
    let rows: i64 = s.query("SELECT count(*) FROM embedding_cutoffs");
    let vectors: i64 = s.query("SELECT count(*) FROM memory_vectors");
    assert_eq!((rows, vectors), (0, 0));
    assert_eq!(endpoint.requests.lock().unwrap().len(), 1);
}

// With semantic search off, sync drops the stored vectors: they belong to
// the model no longer in use and can be made again.
#[test]
fn with_embedding_off_sync_drops_the_vectors() {
    let s = Store::new();
    s.record("Keep commits small");
    let endpoint = Endpoint::start(Answer::Separating);
    assert!(
        s.run(Some(&endpoint), &["embedding", "sync"])
            .status
            .success()
    );
    let vectors: i64 = s.query("SELECT count(*) FROM memory_vectors");
    assert_eq!(vectors, 1);
    let out = s.run(None, &["embedding", "sync"]);
    assert!(
        text(&out).contains("Embedding: off; dropped 1 stored vectors"),
        "{}",
        text(&out)
    );
    let vectors: i64 = s.query("SELECT count(*) FROM memory_vectors");
    assert_eq!(vectors, 0);
    let shown = text(&s.run(None, &["embedding", "show"]));
    assert!(shown.contains("Embedding: off"), "{shown}");
}

// A client of a memory server runs nothing locally: the store, and so the
// calibration, live on the server.
#[test]
fn a_client_of_a_server_is_sent_to_the_server() {
    let s = Store::new();
    let token = s.home.join("token");
    std::fs::write(&token, "x".repeat(40)).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
    let out = command
        .args(["embedding", "show"])
        .sandboxed(&s.home)
        .env("USER_MEMORY_URL", "http://127.0.0.1:9/mcp")
        .env("USER_MEMORY_TOKEN_FILE", &token)
        .output()
        .unwrap();
    let said = text(&out);
    assert!(!out.status.success(), "{said}");
    assert!(
        said.contains("docker compose exec memory user-memory embedding"),
        "{said}"
    );
    // A server the installer did not set up here owns its setting.
    let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .args([
            "embedding",
            "set",
            "--url",
            "https://embed.example.com/v1",
            "--model",
            "other-model",
        ])
        .sandboxed(&s.home)
        .env("USER_MEMORY_URL", "http://127.0.0.1:9/mcp")
        .env("USER_MEMORY_TOKEN_FILE", &token)
        .output()
        .unwrap();
    let said = text(&out);
    assert!(!out.status.success(), "{said}");
    assert!(said.contains("owns its embedding setting"), "{said}");
}

/// A local install: the binary copied beside a saved setup, as setup leaves
/// it, so `embedding set` and `off` have a setting to save.
struct Installed {
    store: Store,
    bin: PathBuf,
}

impl Installed {
    fn new() -> Self {
        let store = Store::new();
        let dir = store.home.join("bin");
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("user-memory");
        std::fs::copy(env!("CARGO_BIN_EXE_user-memory"), &bin).unwrap();
        std::fs::write(dir.join(".user-memory.json"), r#"{"clients":[]}"#).unwrap();
        Self { store, bin }
    }
    fn run(&self, args: &[&str]) -> Output {
        // Linux refuses to start a file another thread's child still holds
        // open for writing just after the copy; that passes at once.
        for _ in 0..50 {
            let mut command = Command::new(&self.bin);
            command
                .args(args)
                .sandboxed(&self.store.home)
                .env("USER_MEMORY_DB", &self.store.db)
                .env_remove("USER_MEMORY_URL")
                .env_remove("USER_MEMORY_TOKEN_FILE");
            for name in EMBED_ENV {
                command.env_remove(name);
            }
            match command.output() {
                Err(err) if err.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                out => return out.unwrap(),
            }
        }
        panic!("the copied binary stayed busy")
    }
    fn receipt(&self) -> Value {
        let text = std::fs::read_to_string(self.bin.with_file_name(".user-memory.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }
}

// `set` checks the endpoint, calibrates its model, saves the setting beside
// the binary and embeds every memory; later commands use the saved setting.
// `off` removes the setting and drops the vectors.
#[test]
fn set_saves_a_checked_calibrated_setting_and_off_removes_it() {
    let i = Installed::new();
    i.store.record("Keep commits small");
    i.store.record("Answer in short paragraphs");
    let endpoint = Endpoint::start(Answer::Separating);
    let out = i.run(&[
        "embedding",
        "set",
        "--url",
        &endpoint.url,
        "--model",
        "fake-model",
    ]);
    let said = text(&out);
    assert!(out.status.success(), "{said}");
    assert!(said.contains("calibrated on the labelled pairs"), "{said}");
    assert!(said.contains("Saved in"), "{said}");
    assert!(
        said.contains("Embedded 2 memories with fake-model"),
        "{said}"
    );
    assert_eq!(
        i.receipt()["embedding"],
        json!({
            "USER_MEMORY_EMBED_MODEL": "fake-model",
            "USER_MEMORY_EMBED_URL": format!("{}/embeddings", endpoint.url),
        })
    );
    let shown = text(&i.run(&["embedding", "show"]));
    assert!(shown.contains("Model: fake-model"), "{shown}");
    assert!(
        shown.contains("computed from the labelled pairs"),
        "{shown}"
    );
    let out = i.run(&["embedding", "off"]);
    let said = text(&out);
    assert!(out.status.success(), "{said}");
    assert!(said.contains("dropped 2 stored vectors"), "{said}");
    assert!(i.receipt().get("embedding").is_none(), "{}", i.receipt());
    assert!(text(&i.run(&["embedding", "show"])).contains("Embedding: off"));
}

// An endpoint that fails the probe leaves the saved setting as it was.
#[test]
fn set_with_a_failing_endpoint_saves_nothing() {
    let i = Installed::new();
    let endpoint = Endpoint::start(Answer::Failing);
    let out = i.run(&[
        "embedding",
        "set",
        "--url",
        &endpoint.url,
        "--model",
        "fake-model",
    ]);
    let said = text(&out);
    assert!(!out.status.success(), "{said}");
    assert!(said.contains("did not answer the probe"), "{said}");
    assert!(i.receipt().get("embedding").is_none(), "{}", i.receipt());
}
