//! The keyword index answers every search exactly as reading every memory
//! does. After writes through each path that changes memories (record,
//! update, forget, merge, import, another program's SQL, VACUUM, reindex
//! and migrate), a store reading its current index and a copy stamped by
//! another version, which reads and stems every memory itself, return the
//! same output for random topics under random filters, keyword-only and
//! fused with meaning.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex},
};
use tempfile::{TempDir, tempdir};

const PROJECTS: [&str; 2] = ["github.com/example/ledger", "github.com/example/site"];
const CATEGORIES: [&str; 9] = [
    "coding",
    "programming",
    "tools",
    "communication",
    "writing style",
    "workflow",
    "cooking",
    "Café",
    "release",
];
const TYPES: [&str; 5] = ["preference", "fact", "decision", "goal", "constraint"];
/// Words memories and topics are made of: inflections of one stem, words
/// that stem alike only in Snowball, stop words, other scripts, digits and
/// hyphens, and words FTS5 reads as operators.
const WORDS: [&str; 48] = [
    "release",
    "releases",
    "released",
    "releasing",
    "deploy",
    "deploys",
    "rollback",
    "hotfix",
    "invoice",
    "invoices",
    "rounding",
    "customer",
    "editor",
    "editing",
    "helix",
    "rust",
    "compiler",
    "generous",
    "generously",
    "generic",
    "news",
    "new",
    "skies",
    "sky",
    "dying",
    "the",
    "and",
    "of",
    "straße",
    "strasse",
    "İstanbul",
    "café",
    "naïve",
    "ﬁle",
    "file",
    "x86",
    "v2",
    "2026",
    "über-fast",
    "co-op",
    "NEAR",
    "NOT",
    "OR",
    "tabs",
    "spaces",
    "recipes",
    "vegetarian",
    "workflow",
];

/// A seeded xorshift generator, so a failure reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
    fn words(&mut self, low: usize, high: usize) -> String {
        let n = low + self.below(high - low + 1);
        (0..n)
            .map(|_| self.pick(&WORDS))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A deterministic embeddings endpoint: each word adds to an axis picked by
/// its letters, so texts sharing words are near and others spread out. A
/// request it fails to answer is recorded and answered with an error, so
/// the endpoint keeps serving and [`Embedder::check`] fails the test.
struct Embedder {
    url: String,
    failures: Arc<Mutex<Vec<String>>>,
}

fn vector(text: &str) -> Vec<f64> {
    let mut vector = vec![0.01; 16];
    for word in text.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
        let axis = word.bytes().fold(7usize, |h, b| {
            h.wrapping_mul(31).wrapping_add(usize::from(b))
        }) % 16;
        vector[axis] += 1.0;
    }
    vector
}

/// The embeddings for one request on `stream`, read as an HTTP POST.
fn answer(stream: &std::net::TcpStream) -> Result<String, String> {
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut line = String::new();
    let mut length = 0;
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    loop {
        line.clear();
        if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 || line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case("content-length")
        {
            length = value
                .trim()
                .parse()
                .map_err(|_| format!("bad length {value}"))?;
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).map_err(|e| e.to_string())?;
    let request: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    let data = request["input"]
        .as_array()
        .ok_or("no input")?
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let text = text.as_str().ok_or("an input is not text")?;
            Ok(json!({"index":i,"embedding":vector(text)}))
        })
        .collect::<Result<Vec<Value>, String>>()?;
    Ok(json!({"data":data,"model":request["model"]}).to_string())
}

impl Embedder {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/embeddings", listener.local_addr().unwrap());
        let failures = Arc::new(Mutex::new(Vec::new()));
        let failed = failures.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let answered =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| answer(&stream)));
                let (status, reply) = match answered {
                    Ok(Ok(reply)) => ("200 OK", reply),
                    Ok(Err(error)) => ("500 Error", error),
                    Err(panic) => (
                        "500 Error",
                        panic
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_owned())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "it panicked".to_owned()),
                    ),
                };
                if status != "200 OK" {
                    failed.lock().unwrap().push(reply.clone());
                }
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        Self { url, failures }
    }
    /// Fails the test if a request went unanswered.
    fn check(&self) {
        let failures = self.failures.lock().unwrap();
        assert!(
            failures.is_empty(),
            "the embeddings endpoint failed: {failures:?}"
        );
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
        self.root.path().join("memories.sqlite3")
    }
    fn command(&self, db: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .sandboxed(self.root.path())
            .env("USER_MEMORY_DB", db)
            .env_remove("USER_MEMORY_URL")
            .env_remove("USER_MEMORY_TOKEN_FILE")
            .env_remove("USER_MEMORY_EMBED_URL");
        command
    }
    fn run(&self, args: &[&str]) -> Value {
        let out = self.command(&self.db()).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
    fn sql(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.db()).unwrap()
    }
    fn count(&self, table: &str) -> i64 {
        self.sql()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }
    fn stamp(&self) -> String {
        self.sql()
            .query_row("SELECT stemmed_by FROM search_state", [], |r| r.get(0))
            .unwrap()
    }
    /// Ids of the memories `sql` selects.
    fn ids(&self, sql: &str) -> Vec<String> {
        self.sql()
            .prepare(sql)
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
    /// Records a memory of random words, telling it apart from any similar
    /// one, and returns its id.
    fn record(&self, rng: &mut Rng) -> String {
        let title = rng.words(2, 6);
        let content = rng.words(6, 24);
        let category = rng.pick(&CATEGORIES).to_owned();
        let kind = rng.pick(&TYPES).to_owned();
        let mut args: Vec<String> = [
            "record",
            "--type",
            &kind,
            "--category",
            &category,
            "--title",
            &title,
            "--content",
            &content,
        ]
        .map(str::to_owned)
        .to_vec();
        if rng.chance(30) {
            args.extend(
                ["--scope", "project", "--project", rng.pick(&PROJECTS)].map(str::to_owned),
            );
        } else {
            args.extend(["--scope", "global"].map(str::to_owned));
        }
        if rng.chance(25) {
            args.extend(["--context".to_owned(), rng.words(2, 8)]);
        }
        if rng.chance(20) {
            args.extend(
                ["--source", "observed", "--evidence", "Seen in a review"].map(str::to_owned),
            );
        }
        self.store(args)
    }
    /// Runs a record command until it stores, marking every similar memory
    /// as distinct, and returns the new memory's id.
    fn store(&self, mut args: Vec<String>) -> String {
        for _ in 0..20 {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let out = self.run(&refs);
            if out["stored"] == true {
                return out["id"].as_str().unwrap().to_owned();
            }
            for m in out["similar"].as_array().unwrap() {
                args.extend([
                    "--distinct-from".to_owned(),
                    m["id"].as_str().unwrap().to_owned(),
                ]);
            }
        }
        panic!("{args:?} kept meeting new similar memories");
    }
}

/// A `user-memory serve` answering many tool calls over one stdio session.
struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next: u64,
}

impl Server {
    fn start(mut command: Command) -> Self {
        let mut child = command
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut server = Self {
            child,
            input,
            output,
            next: 1,
        };
        server.request("initialize", json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}));
        writeln!(
            server.input,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .unwrap();
        server
    }
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        writeln!(
            self.input,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )
        .unwrap();
        self.input.flush().unwrap();
        loop {
            let mut line = String::new();
            assert!(
                self.output.read_line(&mut line).unwrap() > 0,
                "the server stopped"
            );
            let reply: Value = serde_json::from_str(&line).unwrap();
            if reply["id"] == id {
                return reply["result"].clone();
            }
        }
    }
    /// The search tool's whole answer, as text.
    fn search(&mut self, arguments: &Value) -> String {
        let result = self.request("tools/call", json!({"name":"search","arguments":arguments}));
        result["content"][0]["text"].as_str().unwrap().to_owned()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Random search arguments: a topic of stored words, inflections and edge
/// words, under random filters, on one page.
fn arguments(rng: &mut Rng, memory_words: &[String]) -> Value {
    let mut topic: Vec<String> = (0..1 + rng.below(3))
        .map(|_| {
            if !memory_words.is_empty() && rng.chance(60) {
                memory_words[rng.below(memory_words.len())].clone()
            } else {
                rng.pick(&WORDS).to_owned()
            }
        })
        .collect();
    if rng.chance(15) {
        topic.push(rng.pick(&CATEGORIES).to_owned());
    }
    let mut arguments = json!({"topic": topic.join(" "), "page_size": 200});
    if rng.chance(70) {
        let count = 1 + rng.below(2);
        arguments["projects"] = json!(PROJECTS[..count]);
    }
    for (key, values, percent) in [
        ("scope", &["global", "project", "all"][..], 25),
        ("category", &CATEGORIES[..], 15),
        ("type", &TYPES[..], 15),
        ("source", &["explicit", "observed", "inferred"][..], 10),
        ("status", &["active", "stale", "superseded", "all"][..], 30),
    ] {
        if rng.chance(percent) {
            arguments[key] = json!(rng.pick(values));
        }
    }
    if arguments["scope"] == "project" && arguments.get("projects").is_none() {
        arguments["projects"] = json!([PROJECTS[0]]);
    }
    arguments
}

/// The keyword index of the store at `db`, by memory id: each key's
/// columns in key order, and each stored stem with its column. A stem whose
/// key has no memory shows with a null id.
fn keyword_index(db: &Path) -> [Vec<String>; 2] {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute_batch(
        "CREATE VIRTUAL TABLE temp.stems USING fts5vocab(main,'search_words','instance')",
    )
    .unwrap();
    let rows = |sql: &str| -> Vec<String> {
        conn.prepare(sql)
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    [
        rows(
            "SELECT id||memory_type||category||scope||project||source||status||updated_at FROM search_keys ORDER BY key",
        ),
        rows(
            "SELECT ifnull(k.id,'none')||' '||s.col||' '||s.term FROM temp.stems s LEFT JOIN search_keys k ON k.key=s.doc ORDER BY 1",
        ),
    ]
}

/// Searches the store and a copy stamped by another version with `queries`
/// random arguments each, keyword-only and fused, and checks they answer
/// alike. The store's index must be current and fully indexed: the copy is
/// the oracle only while the store reads its index.
fn compare(store: &Store, embedder: &Embedder, stamp: &str, rng: &mut Rng, phase: &str) {
    assert_eq!(store.stamp(), stamp, "{phase}: the index is not current");
    assert_eq!(
        store.count("search_pending"),
        0,
        "{phase}: memories left queued"
    );
    let oracle = store
        .root
        .path()
        .join(format!("oracle-{}.sqlite3", rng.next()));
    store
        .sql()
        .backup(rusqlite::MAIN_DB, &oracle, None)
        .unwrap();
    rusqlite::Connection::open(&oracle)
        .unwrap()
        .execute("UPDATE search_state SET stemmed_by='another version'", [])
        .unwrap();
    // The index holds what a rebuild makes, and keys follow stored order.
    let [keys, stems] = keyword_index(&store.db());
    assert_eq!(
        keys,
        store.ids("SELECT id||memory_type||category||scope||project||source||status||updated_at FROM memories ORDER BY rowid"),
        "{phase}: keys"
    );
    let rebuilt = store.root.path().join("rebuilt.sqlite3");
    let _ = fs::remove_file(&rebuilt);
    store
        .sql()
        .backup(rusqlite::MAIN_DB, &rebuilt, None)
        .unwrap();
    let out = store.command(&rebuilt).arg("reindex").output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stems, keyword_index(&rebuilt)[1], "{phase}: stems");
    let memory_words: Vec<String> = store
        .ids("SELECT title || ' ' || content FROM memories")
        .iter()
        .flat_map(|text| {
            text.split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect();
    for fused in [false, true] {
        let start = |db: &Path| {
            let mut command = store.command(db);
            if fused {
                command
                    .env("USER_MEMORY_EMBED_URL", &embedder.url)
                    .env("USER_MEMORY_EMBED_MODEL", "test-model")
                    .env("USER_MEMORY_EMBED_ALLOW_HTTP", "1");
            }
            Server::start(command)
        };
        let (mut indexed, mut full) = (start(&store.db()), start(&oracle));
        let (mut answered, mut semantic) = (0, 0);
        for _ in 0..60 {
            let arguments = arguments(rng, &memory_words);
            let answer = indexed.search(&arguments);
            assert_eq!(
                answer,
                full.search(&arguments),
                "{phase}, fused {fused}: {arguments}"
            );
            embedder.check();
            semantic +=
                usize::from(serde_json::from_str::<Value>(&answer).unwrap()["semantic"] == true);
            answered += usize::from(!answer.contains(r#""memories":[]"#));
        }
        // Meaning ranked fused searches, not keywords after a failure; only
        // a topic of stop words has nothing to embed.
        if fused {
            assert!(
                semantic >= 40,
                "{phase}: only {semantic} searches used meaning"
            );
        } else {
            assert_eq!(semantic, 0, "{phase}");
        }
        assert!(
            answered >= 20,
            "{phase}, fused {fused}: only {answered} answers found memories"
        );
    }
    assert_eq!(store.stamp(), stamp, "{phase}: search changed the stamp");
    fs::remove_file(&oracle).unwrap();
}

#[test]
fn the_index_answers_as_reading_every_memory_does_after_every_kind_of_write() {
    let mut rng = Rng(0x5eed_1d3a_u64);
    let embedder = Embedder::start();
    let store = Store::new();
    let first = store.record(&mut rng);
    let stamp = store.stamp();
    let mut ids = vec![first];
    for _ in 0..34 {
        ids.push(store.record(&mut rng));
    }
    compare(&store, &embedder, &stamp, &mut rng, "record");

    for id in ids.iter().take(10) {
        let words = rng.words(4, 16);
        let field = ["--content", "--title", "--category", "--type", "--context"][rng.below(5)];
        let value = match field {
            "--category" => rng.pick(&CATEGORIES).to_owned(),
            "--type" => rng.pick(&TYPES).to_owned(),
            _ => words,
        };
        let mut args = vec!["update", id.as_str(), field, value.as_str()];
        if field == "--content" {
            args.extend(["--reason", ""]);
        }
        store.run(&args);
    }
    compare(&store, &embedder, &stamp, &mut rng, "update");

    for id in ids.drain(10..14) {
        store.run(&["forget", &id]);
    }
    compare(&store, &embedder, &stamp, &mut rng, "forget");

    let pair = store.ids(
        "SELECT id FROM memories WHERE scope='global' AND status='confirmed' AND source='explicit' ORDER BY rowid LIMIT 2",
    );
    let merged = store.store(
        [
            "record",
            "--type",
            "preference",
            "--category",
            "release",
            "--scope",
            "global",
            "--title",
            "Releases need notes and a rollback plan",
            "--content",
            "Write release notes and a rollback plan before each release",
            "--supersedes",
            &pair[0],
            "--supersedes",
            &pair[1],
            "--merge",
        ]
        .map(str::to_owned)
        .to_vec(),
    );
    assert_eq!(store.count("memories WHERE status='superseded'"), 2);
    ids.push(merged);
    compare(&store, &embedder, &stamp, &mut rng, "merge");

    let other = Store::new();
    for _ in 0..12 {
        other.record(&mut rng);
    }
    let export = store.root.path().join("export.json");
    other.run(&["export", export.to_str().unwrap()]);
    store.run(&["import", export.to_str().unwrap(), "--apply"]);
    compare(&store, &embedder, &stamp, &mut rng, "import");

    // Another program's writes: queued by the triggers, indexed before the
    // first search reads the index.
    let conn = store.sql();
    for n in 0..4 {
        conn.execute(
            "INSERT INTO memories(id,memory_type,category,title,content) VALUES (?1,'fact',?2,?3,?4)",
            [
                format!("00000000-0000-4000-8000-00000000000{n}"),
                rng.pick(&CATEGORIES).to_owned(),
                rng.words(2, 5),
                rng.words(5, 15),
            ],
        )
        .unwrap();
    }
    let active = store.ids(
        "SELECT id FROM memories WHERE status='confirmed' AND id NOT LIKE '00000000-%' ORDER BY rowid DESC LIMIT 3",
    );
    conn.execute(
        "UPDATE memories SET content=?2 WHERE id=?1",
        [active[0].clone(), rng.words(5, 12)],
    )
    .unwrap();
    conn.execute(
        "UPDATE memories SET status='stale' WHERE id=?1",
        [active[1].clone()],
    )
    .unwrap();
    conn.execute("DELETE FROM memories WHERE id=?1", [active[2].clone()])
        .unwrap();
    // SQLite fires no delete trigger for the row a REPLACE removes.
    conn.execute(
        "INSERT OR REPLACE INTO memories(id,memory_type,category,title,content) VALUES ('00000000-0000-4000-8000-000000000000','fact','tools',?1,?2)",
        [rng.words(2, 5), rng.words(5, 15)],
    )
    .unwrap();
    drop(conn);
    assert_eq!(store.count("search_pending"), 5);
    store.run(&["search", "release"]);
    compare(&store, &embedder, &stamp, &mut rng, "another program's SQL");

    // VACUUM may renumber the memories' rowids; keys keep their order.
    store.sql().execute_batch("VACUUM").unwrap();
    compare(&store, &embedder, &stamp, &mut rng, "VACUUM");

    store.run(&["reindex"]);
    compare(&store, &embedder, &stamp, &mut rng, "reindex");
}

/// A store at schema version 5 from its snapshot and seed rows, with
/// `extra` random memories added by SQL.
fn version_5_store(store: &Store, rng: &mut Rng, extra: usize) {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/schema");
    let conn = store.sql();
    conn.execute_batch(&fs::read_to_string(fixtures.join("v5.sql")).unwrap())
        .unwrap();
    conn.pragma_update(None, "user_version", 5).unwrap();
    conn.execute_batch(&fs::read_to_string(fixtures.join("v5.seed.sql")).unwrap())
        .unwrap();
    for n in 0..extra {
        let scope = if rng.chance(30) { "project" } else { "global" };
        conn.execute(
            "INSERT INTO memories(id,memory_type,category,title,content,scope,project) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            [
                format!("00000000-0000-4000-8000-{n:012}"),
                rng.pick(&TYPES).to_owned(),
                rng.pick(&CATEGORIES).to_owned(),
                rng.words(2, 6),
                rng.words(6, 20),
                scope.to_owned(),
                if scope == "project" { rng.pick(&PROJECTS).to_owned() } else { String::new() },
            ],
        )
        .unwrap();
    }
}

#[test]
fn the_index_migrate_builds_answers_as_reading_every_memory_does() {
    let mut rng = Rng(0x0006_5eed);
    let embedder = Embedder::start();
    let fresh = Store::new();
    fresh.run(&["search"]);
    let stamp = fresh.stamp();
    let store = Store::new();
    version_5_store(&store, &mut rng, 40);
    let out = store
        .command(&store.db())
        .args(["migrate"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    compare(&store, &embedder, &stamp, &mut rng, "migrate");
}

#[test]
fn a_stale_stamp_reads_every_memory_until_migrate_rebuilds_the_index() {
    let mut rng = Rng(0x57a1_e000);
    let store = Store::new();
    for _ in 0..8 {
        store.record(&mut rng);
    }
    store.run(&[
        "record",
        "--scope",
        "global",
        "--category",
        "release",
        "--title",
        "Tag releases on main",
        "--content",
        "Tag releases on main only",
    ]);
    let stamp = store.stamp();
    let titles = |value: Value| -> Vec<String> {
        value["memories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["title"].as_str().unwrap().to_owned())
            .collect()
    };
    let before = titles(store.run(&["search", "tagging", "--page-size", "200"]));
    assert!(before.contains(&"Tag releases on main".to_owned()));
    store
        .sql()
        .execute_batch(
            "UPDATE search_state SET stemmed_by='another version'; INSERT INTO search_words(search_words) VALUES('delete-all');",
        )
        .unwrap();
    // The emptied index is not read: search stems every memory itself.
    assert_eq!(
        titles(store.run(&["search", "tagging", "--page-size", "200"])),
        before
    );
    // Writes leave a stale index as it is, for the rebuild to replace.
    store.record(&mut rng);
    assert_eq!(store.stamp(), "another version");
    assert!(store.count("search_pending") > 0);
    let out = store.command(&store.db()).arg("migrate").output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(store.stamp(), stamp);
    assert_eq!(store.count("search_pending"), 0);
    assert_eq!(
        titles(store.run(&["search", "tagging", "--page-size", "200"])),
        before
    );
}

#[test]
fn a_memory_too_long_to_index_leaves_search_reading_every_memory() {
    let store = Store::new();
    store.run(&[
        "record",
        "--scope",
        "global",
        "--category",
        "tools",
        "--title",
        "Use Helix",
        "--content",
        "Use Helix as the editor",
    ]);
    // A word past FTS5's token limit, written past recording's own limit
    // by another program.
    let word = "q".repeat(20_000);
    store
        .sql()
        .execute(
            "INSERT INTO memories(id,memory_type,category,title,content) VALUES ('00000000-0000-4000-8000-000000000001','fact','tools','A long word',?1)",
            [format!("editor {word}")],
        )
        .unwrap();
    let found = store.run(&["search", &word]);
    assert_eq!(found["memories"][0]["title"], "A long word");
    let found = store.run(&["search", "editor"]);
    assert_eq!(found["memories"].as_array().unwrap().len(), 2);
    assert_eq!(store.count("search_pending"), 1);
}

// SQLite removes a WAL store's log and shared memory when its last
// connection closes; a server that held none paid for that on every call.
#[test]
fn a_server_keeps_the_store_open_between_calls() {
    let store = Store::new();
    store.run(&["search"]);
    let shared = |db: PathBuf| {
        let mut name = db.into_os_string();
        name.push("-shm");
        PathBuf::from(name)
    };
    assert!(!shared(store.db()).exists(), "no connection is open");
    let mut server = Server::start(store.command(&store.db()));
    server.search(&json!({"topic": "release"}));
    assert!(
        shared(store.db()).exists(),
        "the server keeps the store open"
    );
    drop(server);
}
