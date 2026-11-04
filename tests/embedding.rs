//! Choosing an embedding model: a new model is probed, calibrated on the
//! labelled pairs that ship in the binary, and every memory re-embedded.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/fake.rs"]
mod fake;
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
    /// HTTP 500 for every request, with this body.
    Failing(&'static str),
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
                if let Answer::Failing(reply) = answer {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
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
        self.run_with(endpoint, &[], args)
    }
    /// `run` with extra environment settings.
    fn run_with(&self, endpoint: Option<&Endpoint>, env: &[(&str, &str)], args: &[&str]) -> Output {
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
        command.envs(env.iter().copied()).output().unwrap()
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
    let endpoint = Endpoint::start(Answer::Failing(""));
    let out = s.run(Some(&endpoint), &["embedding", "sync"]);
    let said = text(&out);
    assert!(!out.status.success(), "{said}");
    assert!(said.contains("did not answer the probe"), "{said}");
    // An empty error body adds no reason.
    assert!(!said.contains("The endpoint said"), "{said}");
    let rows: i64 = s.query("SELECT count(*) FROM embedding_cutoffs");
    let vectors: i64 = s.query("SELECT count(*) FROM memory_vectors");
    assert_eq!((rows, vectors), (0, 0));
    assert_eq!(endpoint.requests.lock().unwrap().len(), 1);
}

// An endpoint's own reason for refusing, from its error body, follows the
// status, with the API key it repeats masked.
#[test]
fn a_refusing_endpoint_says_why() {
    let s = Store::new();
    let key = s.home.join("embed-key");
    std::fs::write(&key, "sk-test-0123456789").unwrap();
    let endpoint = Endpoint::start(Answer::Failing(
        r#"{"error":{"message":"error starting llama-server: binary not found (key sk-test-0123456789)"}}"#,
    ));
    let out = s.run_with(
        Some(&endpoint),
        &[("USER_MEMORY_EMBED_API_KEY_FILE", key.to_str().unwrap())],
        &["embedding", "sync"],
    );
    let said = text(&out);
    assert!(!out.status.success(), "{said}");
    let status = said.find("HTTP status server error (500").expect(&said);
    let reason = said
        .find("The endpoint said: error starting llama-server: binary not found (key [key])")
        .expect(&said);
    assert!(status < reason, "{said}");
    assert!(!said.contains("sk-test-0123456789"), "{said}");
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
        said.contains("Run embedding show on the memory server that owns the database: on the machine that serves http://127.0.0.1:9/mcp, run: user-memory embedding show"),
        "{said}"
    );
    let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .args(["embedding", "sync"])
        .sandboxed(&s.home)
        .env("USER_MEMORY_URL", "http://127.0.0.1:9/mcp")
        .env("USER_MEMORY_TOKEN_FILE", &token)
        .output()
        .unwrap();
    let said = text(&out);
    assert!(!out.status.success(), "{said}");
    assert!(
        said.contains(
            "on the machine that serves http://127.0.0.1:9/mcp, run: user-memory embedding sync"
        ),
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
    let endpoint = Endpoint::start(Answer::Failing(""));
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

/// The port the test Docker server's compose.yaml publishes; nothing
/// listens there, as these commands never reach the server itself.
const DOCKER_PORT: &str = "18765";

/// A client of the installer's Docker server on this machine: its project
/// folder with compose.yaml and embedding.env, the fake `docker` on PATH,
/// and the server's URL as this machine's remote.
struct DockerClient {
    store: Store,
    dir: PathBuf,
    cli: PathBuf,
    token: PathBuf,
}

impl DockerClient {
    fn new(embedding_env: &str) -> Self {
        let store = Store::new();
        let data = if cfg!(windows) {
            store.home.join("AppData").join("Local")
        } else {
            store.home.join(".local").join("share")
        };
        let dir = data.join("user-memory").join("docker");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("compose.yaml"),
            format!("    ports:\n      - \"127.0.0.1:{DOCKER_PORT}:8765\"\n"),
        )
        .unwrap();
        std::fs::write(dir.join("embedding.env"), embedding_env).unwrap();
        let cli = store.home.join("cli");
        std::fs::create_dir_all(&cli).unwrap();
        fake::fake(&cli, "docker");
        let token = store.home.join("token");
        std::fs::write(&token, "x".repeat(40)).unwrap();
        Self {
            store,
            dir,
            cli,
            token,
        }
    }
    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .args(args)
            .sandboxed(&self.store.home)
            .env("PATH", &self.cli)
            .env(
                "USER_MEMORY_URL",
                format!("http://127.0.0.1:{DOCKER_PORT}/mcp"),
            )
            .env("USER_MEMORY_TOKEN_FILE", &self.token)
            .env("FAKE_HOST", env!("CARGO_BIN_EXE_user-memory"));
        for name in EMBED_ENV {
            command.env_remove(name);
        }
        command.output().unwrap()
    }
    fn docker_log(&self) -> String {
        std::fs::read_to_string(self.store.home.join("docker.log")).unwrap_or_default()
    }
}

// On the installer's Docker server's machine, show prints the setting from
// embedding.env; with it off, there's nothing to ask the container.
#[test]
fn show_on_the_docker_servers_machine_reads_its_setting() {
    let c = DockerClient::new("");
    let out = c.run(&["embedding", "show"]);
    let said = text(&out);
    assert!(out.status.success(), "{said}");
    assert!(
        said.contains("Embedding: off; search uses keywords only"),
        "{said}"
    );
    assert!(!c.docker_log().contains(" exec "), "{}", c.docker_log());
}

// With a model set, show prints the setting, then the container's own
// probe and calibration, without its repeat of the endpoint.
#[test]
fn show_on_the_docker_servers_machine_asks_the_container() {
    let endpoint = Endpoint::start(Answer::Separating);
    let c = DockerClient::new(&format!(
        "USER_MEMORY_EMBED_URL={}/embeddings\nUSER_MEMORY_EMBED_MODEL=fake-model\n",
        endpoint.url
    ));
    let out = c.run(&["embedding", "show"]);
    let said = text(&out);
    assert!(out.status.success(), "{said}");
    assert!(
        said.contains(&format!("Endpoint: {}/embeddings\n", endpoint.url)),
        "{said}"
    );
    assert!(said.contains("Model: fake-model"), "{said}");
    assert!(said.contains("Vector length: "), "{said}");
    assert_eq!(said.matches("Endpoint: ").count(), 1, "{said}");
    assert_eq!(said.matches("Model: ").count(), 1, "{said}");
    assert!(
        c.docker_log()
            .contains("exec -T memory user-memory embedding show"),
        "{}",
        c.docker_log()
    );
}

// A container that isn't running still leaves the setting shown, with the
// command that gives the rest once it runs.
#[test]
fn show_names_the_container_command_when_docker_cannot_answer() {
    let c = DockerClient::new(
        "USER_MEMORY_EMBED_URL=http://host.docker.internal:11434/v1/embeddings\nUSER_MEMORY_EMBED_MODEL=fake-model\nUSER_MEMORY_EMBED_API_KEY_FILE=/data/embedding-key\nUSER_MEMORY_EMBED_ALLOW_HTTP=1\n",
    );
    std::fs::write(c.store.home.join("container-down"), "").unwrap();
    let out = c.run(&["embedding", "show"]);
    let said = text(&out);
    assert!(out.status.success(), "{said}");
    assert!(
        said.contains("Endpoint: http://host.docker.internal:11434/v1/embeddings (this machine, as the Docker server reaches it)"),
        "{said}"
    );
    assert!(said.contains("Model: fake-model"), "{said}");
    // The key is named by where it is, never by its path or value.
    assert!(said.contains("API key: in the server's volume"), "{said}");
    assert!(!said.contains("embedding-key"), "{said}");
    assert!(said.contains("service \"memory\" is not running"), "{said}");
    assert!(
        said.contains(&format!(
            "Probe and stored vectors: docker compose -f '{}' exec memory user-memory embedding show",
            c.dir.join("compose.yaml").display()
        )),
        "{said}"
    );
}

// sync on the Docker server's machine runs in the container, and fails
// when it fails there.
#[test]
fn sync_on_the_docker_servers_machine_runs_in_the_container() {
    let c = DockerClient::new(
        "USER_MEMORY_EMBED_URL=http://host.docker.internal:11434/v1/embeddings\n",
    );
    let out = c.run(&["embedding", "sync"]);
    let said = text(&out);
    assert!(out.status.success(), "{said}");
    assert!(
        c.docker_log()
            .contains("exec -T memory user-memory embedding sync"),
        "{}",
        c.docker_log()
    );
    std::fs::write(c.store.home.join("sync-fails"), "").unwrap();
    let out = c.run(&["embedding", "sync"]);
    let said = text(&out);
    assert!(!out.status.success(), "{said}");
    assert!(said.contains("did not answer the probe"), "{said}");
    assert!(
        said.contains("docker compose exec -T memory user-memory embedding sync failed"),
        "{said}"
    );
}

// Without a saved setup there's nowhere to keep a setting: set says where
// to run it before the slow probe and calibration, and off before dropping
// any vector.
#[test]
fn set_and_off_without_a_saved_setup_say_where_to_run_them() {
    let s = Store::new();
    s.record("Keep commits small");
    let endpoint = Endpoint::start(Answer::Separating);
    let out = s.run(Some(&endpoint), &["embedding", "sync"]);
    assert!(out.status.success(), "{}", text(&out));
    let vectors: i64 = s.query("SELECT count(*) FROM memory_vectors");
    assert_eq!(vectors, 1);
    let probed = endpoint.requests.lock().unwrap().len();
    let wanted = "On the installer's Docker server, run `user-memory embedding set` on the machine that runs the container";
    let out = s.run(
        None,
        &[
            "embedding",
            "set",
            "--url",
            &endpoint.url,
            "--model",
            "fake-model",
        ],
    );
    let said = text(&out);
    assert!(!out.status.success(), "{said}");
    assert!(said.contains(wanted), "{said}");
    assert_eq!(endpoint.requests.lock().unwrap().len(), probed, "{said}");
    let out = s.run(None, &["embedding", "off"]);
    let said = text(&out);
    assert!(!out.status.success(), "{said}");
    assert!(said.contains(wanted), "{said}");
    let vectors: i64 = s.query("SELECT count(*) FROM memory_vectors");
    assert_eq!(vectors, 1);
}
