#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};
use tempfile::{TempDir, tempdir};

#[path = "support/fake.rs"]
mod fake;
#[path = "support/replace.rs"]
mod replace;
#[path = "support/time.rs"]
mod time;
use time::STUCK;

/// Runs a binary a test has just copied. Linux refuses to start a file that
/// any process holds open for writing ("Text file busy"), and another test
/// thread that forks while the copy is open holds it for a moment, so a
/// start that meets it is tried again.
trait RunCopied {
    fn output_retrying(&mut self) -> std::process::Output;
}

impl RunCopied for Command {
    fn output_retrying(&mut self) -> std::process::Output {
        for _ in 0..50 {
            match self.output() {
                Err(err) if err.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                result => return result.unwrap(),
            }
        }
        self.output().unwrap()
    }
}

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
struct Host {
    root: TempDir,
    child: Child,
    address: String,
    session: RefCell<Option<String>>,
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Host {
    fn start(root: TempDir) -> Self {
        Self::start_as(root, None)
    }
    /// Starts a host, as the debug build of `version` when given: a server
    /// of another release.
    fn start_as(root: TempDir, version: Option<&str>) -> Self {
        Self::start_with(root, version, &[])
    }
    /// [`Host::start_as`], with `env` added.
    fn start_with(root: TempDir, version: Option<&str>, env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        if let Some(version) = version {
            command.env("USER_MEMORY_TEST_VERSION", version);
        }
        command.envs(env.iter().copied());
        let mut child = command
            .args(["host", "--listen", "127.0.0.1:0", "--db"])
            .arg(root.path().join("shared.sqlite3"))
            .arg("--token-file")
            .arg(root.path().join("token"))
            .sandboxed(root.path())
            .env_remove("USER_MEMORY_DB")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        if !line.starts_with("Listening: http://") {
            let output = child.wait_with_output().unwrap();
            panic!(
                "Host did not start: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let address = line
            .trim()
            .strip_prefix("Listening: http://")
            .unwrap()
            .strip_suffix("/mcp")
            .unwrap()
            .to_owned();
        Self {
            root,
            child,
            address,
            session: RefCell::new(None),
        }
    }
    fn request(&self, token: Option<&str>, message: Value) -> (u16, Vec<u8>) {
        let mut socket = TcpStream::connect(&self.address).unwrap();
        socket.set_read_timeout(Some(STUCK)).unwrap();
        let body = message.to_string();
        let session = if message["method"] == "initialize" {
            String::new()
        } else {
            self.session
                .borrow()
                .as_ref()
                .map(|id| format!("Mcp-Session-Id: {id}\r\n"))
                .unwrap_or_default()
        };
        let auth = token
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        write!(socket, "POST /mcp HTTP/1.1\r\nHost: {}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2025-06-18\r\n{session}Connection: close\r\n{auth}Content-Length: {}\r\n\r\n{body}", self.address, body.len()).unwrap();
        let mut response = Vec::new();
        socket.read_to_end(&mut response).unwrap();
        let split = response.windows(4).position(|v| v == b"\r\n\r\n").unwrap();
        let header = String::from_utf8_lossy(&response[..split]);
        let status = header.split_whitespace().nth(1).unwrap().parse().unwrap();
        for line in header.lines() {
            if let Some((key, value)) = line.split_once(':')
                && key.eq_ignore_ascii_case("mcp-session-id")
            {
                *self.session.borrow_mut() = Some(value.trim().to_owned());
            }
        }
        let body = &response[split + 4..];
        let body = if header
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            decode_chunks(body)
        } else {
            body.to_vec()
        };
        let body = if header
            .to_ascii_lowercase()
            .contains("content-type: text/event-stream")
        {
            String::from_utf8_lossy(&body)
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .find(|line| {
                    serde_json::from_str::<Value>(line).is_ok_and(|v| v.get("id").is_some())
                })
                .unwrap_or("")
                .as_bytes()
                .to_vec()
        } else {
            body
        };
        (status, body)
    }
    fn restart(&mut self) {
        self.restart_as(None);
    }
    /// Restarts the host on its address with `env` added.
    fn restart_with(&mut self, env: &[(&str, &str)]) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let mut child = Command::new(env!("CARGO_BIN_EXE_user-memory"))
            .args(["host", "--listen", &self.address, "--db"])
            .arg(self.root.path().join("shared.sqlite3"))
            .arg("--token-file")
            .arg(self.root.path().join("token"))
            .sandboxed(self.root.path())
            .envs(env.iter().copied())
            .env_remove("USER_MEMORY_DB")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(line.starts_with("Listening: http://"), "{line}");
        self.child = child;
    }
    /// Restarts the host on its address, as the debug build of `version`
    /// when given: a server upgraded while down.
    fn restart_as(&mut self, version: Option<&str>) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        if let Some(version) = version {
            command.env("USER_MEMORY_TEST_VERSION", version);
        }
        let mut child = command
            .args(["host", "--listen", &self.address, "--db"])
            .arg(self.root.path().join("shared.sqlite3"))
            .arg("--token-file")
            .arg(self.root.path().join("token"))
            .sandboxed(self.root.path())
            .env_remove("USER_MEMORY_DB")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(line.starts_with("Listening: http://"), "{line}");
        self.child = child;
    }
    fn rpc(&self, message: Value) -> Value {
        let (status, body) = self.request(Some(TOKEN), message);
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        serde_json::from_slice(&body).unwrap()
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn decode_chunks(mut body: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();
    loop {
        let end = body.windows(2).position(|v| v == b"\r\n").unwrap();
        let len = usize::from_str_radix(
            std::str::from_utf8(&body[..end])
                .unwrap()
                .split(';')
                .next()
                .unwrap(),
            16,
        )
        .unwrap();
        body = &body[end + 2..];
        if len == 0 {
            return result;
        }
        result.extend_from_slice(&body[..len]);
        body = &body[len + 2..];
    }
}
/// Streams a child's stdout lines so a missing reply fails a test instead of hanging it.
fn lines(stdout: std::process::ChildStdout) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}
fn reply(lines: &std::sync::mpsc::Receiver<String>) -> Value {
    reply_to(lines, "a request", None)
}
/// The connector's next line, the reply to `what`. How long it took goes to
/// stderr, which a failed test shows; a reply that never comes names `what`
/// and shows what the connector wrote to stderr so far (`said`), so a stall
/// says which call it was and what the connector logged.
fn reply_to(lines: &std::sync::mpsc::Receiver<String>, what: &str, said: Option<&Logged>) -> Value {
    let started = std::time::Instant::now();
    let line = lines.recv_timeout(STUCK).unwrap_or_else(|err| {
        let logged = said.map_or_else(|| "not kept".to_owned(), Logged::so_far);
        panic!("Connector sent no reply to {what}: {err}; its stderr so far:\n{logged}")
    });
    eprintln!("reply to {what}: {:?}", started.elapsed());
    serde_json::from_str(&line).unwrap()
}
/// What a child writes to stderr, read on a thread as it comes, so a test
/// that stalls can show it and one that ends can check all of it.
struct Logged {
    text: std::sync::Arc<std::sync::Mutex<String>>,
    reader: std::thread::JoinHandle<()>,
}
impl Logged {
    fn of(mut stderr: std::process::ChildStderr) -> Self {
        let text = std::sync::Arc::<std::sync::Mutex<String>>::default();
        let kept = std::sync::Arc::clone(&text);
        let reader = std::thread::spawn(move || {
            let mut chunk = [0; 4096];
            while let Ok(read) = stderr.read(&mut chunk)
                && read > 0
            {
                kept.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&chunk[..read]));
            }
        });
        Self { text, reader }
    }
    /// What it wrote so far, unless that holds the token, which no output
    /// may show.
    fn so_far(&self) -> String {
        let text = self.text.lock().unwrap().clone();
        if text.contains(TOKEN) {
            "(it holds the token, so it is not shown; that alone fails the test)".to_owned()
        } else {
            text
        }
    }
    /// All it wrote, once the child has closed stderr.
    fn all(self) -> String {
        self.reader.join().unwrap();
        self.text.lock().unwrap().clone()
    }
}
fn initialize(id: u64) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test-device","version":"1"}}})
}
fn call(id: u64, name: &str, arguments: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}})
}
fn root() -> TempDir {
    let root = tempdir().unwrap();
    fs::write(root.path().join("token"), TOKEN).unwrap();
    root
}

/// The store's lock file, locked by this process, which stands in for
/// another process holding its write turn.
fn held_turn(db: &Path) -> fs::File {
    let mut lock = db.as_os_str().to_owned();
    lock.push(".lock");
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(std::path::PathBuf::from(lock))
        .unwrap();
    held.lock().unwrap();
    held
}

// A session with a call in flight is not idle: a call that runs past the
// session's idle bound, here a record waiting 3 s for a write turn another
// process holds, still gets its answer (#237).
#[test]
fn a_call_running_past_the_session_idle_bound_still_gets_its_answer() {
    let (host, _held) = busy_host();
    let (status, body) = host.request(
        Some(TOKEN),
        serde_json::from_str(&waiting_record(3)).unwrap(),
    );
    assert_eq!(status, 200);
    let refused: Value = serde_json::from_slice(&body)
        .unwrap_or_else(|_| panic!("no answer: {:?}", String::from_utf8_lossy(&body)));
    assert!(
        refused["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("kept its write turn on the store for"),
        "{refused}"
    );
}

/// The settings that make the host close an idle session after 1 s and
/// hold a write 3 s for a turn another process keeps.
const SHORT_SESSIONS: [(&str, &str); 2] = [
    ("USER_MEMORY_SESSION_IDLE_MS", "1000"),
    ("USER_MEMORY_TURN_WAIT_MS", "3000"),
];

/// Longer than the test's idle bound plus the time the host takes to find
/// an idle session (a quarter of the bound) and some slack.
const PAST_IDLE: Duration = Duration::from_millis(1800);

/// A request of `method` to the host's /mcp in its current session, with
/// `headers`, `body` and the test's token; the connection, left open.
fn send(host: &Host, method: &str, headers: &str, body: &str) -> TcpStream {
    let mut socket = TcpStream::connect(&host.address).unwrap();
    socket.set_read_timeout(Some(STUCK)).unwrap();
    let session = host.session.borrow().clone().unwrap();
    write!(socket, "{method} /mcp HTTP/1.1\r\nHost: {}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2025-06-18\r\nMcp-Session-Id: {session}\r\nAuthorization: Bearer {TOKEN}\r\n{headers}Connection: close\r\nContent-Length: {}\r\n\r\n{body}", host.address, body.len()).unwrap();
    socket
}

/// Reads `socket` until its text holds `wanted`; what it read.
fn read_until(socket: &mut TcpStream, wanted: &str) -> String {
    let mut seen = Vec::new();
    let mut chunk = [0; 4096];
    while !String::from_utf8_lossy(&seen).contains(wanted) {
        let read = socket.read(&mut chunk).unwrap();
        assert!(
            read > 0,
            "closed before {wanted:?}: {}",
            String::from_utf8_lossy(&seen)
        );
        seen.extend_from_slice(&chunk[..read]);
    }
    String::from_utf8_lossy(&seen).into_owned()
}

/// The HTTP status of a ping in the host's current session.
fn ping(host: &Host, id: u64) -> u16 {
    host.request(
        Some(TOKEN),
        json!({"jsonrpc":"2.0","id":id,"method":"ping"}),
    )
    .0
}

/// The HTTP status of a notification in the host's current session: a
/// request the server answers with no stream.
fn notify(host: &Host) -> u16 {
    host.request(
        Some(TOKEN),
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":999}}),
    )
    .0
}

/// A host with [`SHORT_SESSIONS`], one memory stored, and the store's write
/// turn held by this test, so the next record waits 3 s.
fn busy_host() -> (Host, fs::File) {
    let host = Host::start_with(root(), None, &SHORT_SESSIONS);
    host.rpc(initialize(1));
    let stored = host.rpc(call(2, "record", json!({"category":"build","title":"The quartz lantern build","content":"The quartz lantern build needs the cobalt flag.","scope":"global","user_confirmed":true})));
    assert!(
        stored["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("\"stored\":true"),
        "{stored}"
    );
    let held = held_turn(&host.root.path().join("shared.sqlite3"));
    (host, held)
}

/// A record call that waits behind the held turn.
fn waiting_record(id: u64) -> String {
    call(id, "record", json!({"category":"build","title":"The walnut gate","content":"The walnut gate opens with the ember key.","scope":"global","user_confirmed":true})).to_string()
}

// A session with no request and no stream is closed after the idle bound,
// and the next request in it is refused as for any unknown session. One
// that keeps asking stays open past it, even with requests that open no
// stream, such as notifications.
#[test]
fn a_session_is_closed_once_idle_and_kept_while_it_asks() {
    let host = Host::start_with(root(), None, &SHORT_SESSIONS);
    host.rpc(initialize(1));
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(notify(&host), 202, "closed while it asked");
    }
    std::thread::sleep(PAST_IDLE);
    assert_eq!(notify(&host), 404);
}

// A client that closes its session with DELETE has it closed at once.
#[test]
fn a_deleted_session_is_closed_at_once() {
    let host = Host::start_with(root(), None, &SHORT_SESSIONS);
    host.rpc(initialize(1));
    let mut deleted = send(&host, "DELETE", "", "");
    let said = read_until(&mut deleted, "\r\n\r\n");
    assert!(said.starts_with("HTTP/1.1 2"), "{said}");
    assert_eq!(ping(&host, 2), 404);
}

// Sessions clients opened and left are all closed, so the server's list of
// them does not grow.
#[test]
fn every_abandoned_session_is_closed() {
    let host = Host::start_with(root(), None, &SHORT_SESSIONS);
    let mut left = Vec::new();
    for id in 1..6 {
        host.rpc(initialize(id));
        left.push(host.session.borrow().clone().unwrap());
    }
    std::thread::sleep(PAST_IDLE);
    for (id, session) in (10..).zip(left) {
        *host.session.borrow_mut() = Some(session);
        assert_eq!(ping(&host, id), 404);
    }
}

// A call whose client dropped the connection keeps no stream open, so its
// session is closed after the idle bound though the call still runs.
#[test]
fn a_session_whose_call_was_dropped_is_closed_after_the_idle_bound() {
    let (host, _held) = busy_host();
    let mut posted = send(&host, "POST", "", &waiting_record(3));
    read_until(&mut posted, "id: ");
    drop(posted);
    std::thread::sleep(PAST_IDLE);
    assert_eq!(ping(&host, 4), 404);
}

// A stream resumed with Last-Event-ID counts as open: the session outlives
// the idle bound while it lives and the call's answer arrives on it. Once
// it is dropped the idle time starts.
#[test]
fn a_resumed_stream_keeps_its_session_until_it_is_dropped() {
    let (host, _held) = busy_host();
    let mut posted = send(&host, "POST", "", &waiting_record(3));
    let primed = read_until(&mut posted, "\n\n");
    let event = primed
        .lines()
        .find_map(|line| line.strip_prefix("id: "))
        .unwrap_or_else(|| panic!("no event id: {primed}"))
        .trim()
        .to_owned();
    drop(posted);
    let mut resumed = send(&host, "GET", &format!("Last-Event-ID: {event}\r\n"), "");
    let answered = read_until(&mut resumed, "kept its write turn on the store for");
    assert!(answered.contains("\"id\":3"), "{answered}");
    drop(resumed);
    // The idle time counts from the drop, not from the session's last
    // request, which was over 3 s ago.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(notify(&host), 202, "closed as the stream was dropped");
    std::thread::sleep(PAST_IDLE);
    assert_eq!(ping(&host, 4), 404);
}

#[test]
fn host_requires_authentication_and_two_devices_share_persistent_memory() {
    let host = Host::start(root());
    assert_eq!(host.request(None, initialize(1)).0, 401);
    assert_eq!(host.request(Some("wrong-token"), initialize(1)).0, 401);
    let init = host.rpc(initialize(1));
    assert!(
        init["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("index")
    );
    let record=host.rpc(call(2,"record",json!({"category":"programming languages","context":"Choosing a language","title":"Rust","content":"Rust","reason":"Performance and safety","scope":"global","user_confirmed":true})));
    assert!(
        record["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("\"stored\":true")
    );
    let saved: Value =
        serde_json::from_str(record["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let memory_id = saved["id"]
        .as_str()
        .expect("Remote memory ID must be a UUID");
    assert!(uuid::Uuid::parse_str(memory_id).is_ok());
    // An export from a server says where the file is in words: the
    // server's folders are not the client's, and no path leaves the server.
    let exported = host.rpc(call(30, "export", json!({"name":"backup.json"})));
    let text = exported["result"]["content"][0]["text"].as_str().unwrap();
    let summary: Value = serde_json::from_str(text).unwrap();
    assert_eq!(summary["exported"], true, "{text}");
    assert_eq!(
        summary["next"],
        "The file is in the memory server's exports folder, on the server, not among this machine's files. For a copy here, the user runs `user-memory export FILE`."
    );
    assert!(!text.contains(host.root.path().to_str().unwrap()), "{text}");
    // So does the copy a forget makes before it deletes: it is the server's.
    host.rpc(call(31,"record",json!({"category":"editors","context":"Setting up","title":"Uses Helix","content":"Uses Helix","reason":"Modal editing","scope":"global","user_confirmed":true,"origin":"chat/editor"})));
    let forgot = host.rpc(call(
        32,
        "forget",
        json!({"origin":"chat/editor","user_confirmed":true}),
    ));
    let text = forgot["result"]["content"][0]["text"].as_str().unwrap();
    let reply: Value = serde_json::from_str(text).unwrap();
    assert!(
        reply["backup"]
            .as_str()
            .unwrap()
            .starts_with("shared-forget-"),
        "{text}"
    );
    assert!(
        reply["next"]
            .as_str()
            .unwrap()
            .contains("is on the memory server, in the backups folder beside its store"),
        "{text}"
    );
    assert!(!text.contains(host.root.path().to_str().unwrap()), "{text}");
    let invalid = host.rpc(call(
        5,
        "forget",
        json!({"memory_id":2,"user_confirmed":true}),
    ));
    assert!(invalid.get("error").is_some() || invalid["result"]["isError"] == true);
    let update = host.rpc(call(
        6,
        "update",
        json!({"memory_id":memory_id,"reason":"Performance and safety","user_confirmed":true}),
    ));
    let updated: Value =
        serde_json::from_str(update["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(updated["id"], memory_id);
    host.rpc(initialize(3));
    let recall = host.rpc(call(4, "search", json!({"topic":"Rust"})));
    assert!(
        recall["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Rust")
    );
    let root = host.root.path().to_owned();
    let conn = rusqlite::Connection::open(root.join("shared.sqlite3")).unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM memories", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn host_creates_a_private_credential_file_without_printing_the_secret() {
    let host = Host::start(tempdir().unwrap());
    let token = fs::read_to_string(host.root.path().join("token")).unwrap();
    assert_eq!(token.trim().len(), 64);
    // Mode bits are Unix's; the owner-only ACL on Windows is checked by
    // src/private.rs's own tests.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(host.root.path().join("token"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert_eq!(host.request(Some(token.trim()), initialize(1)).0, 200);
}

#[test]
fn insecure_off_machine_url_is_rejected_before_installation() {
    let root = root();
    let output = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .args(["setup", "--url", "http://example.com/mcp", "--token-file"])
        .arg(root.path().join("token"))
        .arg("--install-dir")
        .arg(root.path().join("bin"))
        .sandboxed(root.path())
        .env("CLAUDE_CONFIG_DIR", root.path())
        .env("CODEX_HOME", root.path().join(".codex"))
        .env("USER_MEMORY_APP_DIRS", "")
        // Setup edits the global git ignore that git config names.
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_CONFIG_GLOBAL")
        .env_remove("XDG_CONFIG_HOME")
        // No client on PATH, so setup can only refuse; a folder that holds
        // nothing works on every platform.
        .env("PATH", root.path().join("no-tools"))
        .env_remove("USER_MEMORY_DB")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("HTTPS"));
    assert!(!root.path().join("bin").exists());
}

#[test]
fn connector_forwards_initialization_and_tools_without_a_local_database() {
    let host = Host::start(root());
    let client = tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .arg("connect")
        .arg("--url")
        .arg(format!("http://{}/mcp", host.address))
        .arg("--token-file")
        .arg(host.root.path().join("token"))
        .sandboxed(client.path())
        .env_remove("USER_MEMORY_DB")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let output = lines(child.stdout.take().unwrap());
    writeln!(input, "{}", initialize(1)).unwrap();
    input.flush().unwrap();
    let init = reply(&output);
    assert!(
        init["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("index")
    );
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})
    )
    .unwrap();
    input.flush().unwrap();
    let tools = reply(&output);
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 10);
    drop(input);
    assert!(child.wait().unwrap().success());
    assert!(!sandbox::folder(client.path(), "store").exists());
}

#[test]
fn connector_answers_a_tool_call_when_the_host_stops() {
    let mut host = Host::start(root());
    let client = tempdir().unwrap();
    let client_token = client.path().join("token");
    fs::write(&client_token, TOKEN).unwrap();
    let mut child = connector(&host, client.path(), &client_token);
    let mut input = child.0.stdin.take().unwrap();
    let errors = Logged::of(child.0.stderr.take().unwrap());
    let output = lines(child.0.stdout.take().unwrap());
    writeln!(input, "{}", initialize(1)).unwrap();
    input.flush().unwrap();
    assert!(
        reply_to(&output, "initialize", Some(&errors))
            .get("result")
            .is_some()
    );
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    input.flush().unwrap();
    host.child.kill().unwrap();
    host.child.wait().unwrap();
    writeln!(input, "{}", call(2, "index", json!({}))).unwrap();
    input.flush().unwrap();
    let value = reply_to(&output, "index after the host stopped", Some(&errors));
    assert_eq!(value["id"], 2);
    assert_eq!(value["result"]["isError"], true);
    assert!(
        value["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .ends_with("did not answer; the call may not have completed. Before repeating a write, search for it. Memory reconnects on the next call."),
        "{value}"
    );
    assert!(!value.to_string().contains(TOKEN));
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}})
    )
    .unwrap();
    input.flush().unwrap();
    let unavailable = reply_to(&output, "tools/list while the host is down", Some(&errors));
    assert_eq!(unavailable["id"], 3);
    assert!(unavailable.get("error").is_some());
    host.restart();
    writeln!(input, "{}", call(4, "index", json!({}))).unwrap();
    input.flush().unwrap();
    let recovered = reply_to(&output, "index after the host restarted", Some(&errors));
    assert_eq!(recovered["id"], 4);
    assert_ne!(recovered["result"]["isError"], true);
    host.child.kill().unwrap();
    host.child.wait().unwrap();
    fs::write(host.root.path().join("token"), "f".repeat(64)).unwrap();
    host.restart();
    writeln!(input, "{}", call(5, "index", json!({}))).unwrap();
    input.flush().unwrap();
    let unauthorized = reply_to(&output, "index with a refused token", Some(&errors));
    assert_eq!(unauthorized["id"], 5);
    assert_eq!(unauthorized["result"]["isError"], true);
    // The server answered: a refused token names its file, never itself.
    let said = unauthorized["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(
        said.ends_with(&format!(
            "refused this machine's access token ({}); the call was not run. Tell the user the token in that file needs replacing (user-memory doctor shows it); don't open the file. Memory tries again on the next call.",
            client_token.display()
        )),
        "{said}"
    );
    assert!(!unauthorized.to_string().contains(TOKEN));
    drop(input);
    assert!(child.0.wait().unwrap().success());
    assert_no_token(errors);
}

/// A `user-memory connect` to `host` from a client folder `home` with the
/// token file `token`; its stderr is piped for [`assert_no_token`].
fn connector(host: &Host, home: &Path, token: &Path) -> ChildGuard {
    connector_with(host, home, token, &[])
}

/// [`connector`] with `env` set as well.
fn connector_with(host: &Host, home: &Path, token: &Path, env: &[(&str, &str)]) -> ChildGuard {
    ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_user-memory"))
            .arg("connect")
            .arg("--url")
            .arg(format!("http://{}/mcp", host.address))
            .arg("--token-file")
            .arg(token)
            .sandboxed(home)
            .env_remove("USER_MEMORY_DB")
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    )
}

/// Takes a finished connector's whole stderr and fails if the token is in it.
fn assert_no_token(said: Logged) {
    assert!(
        !said.all().contains(TOKEN),
        "the token is in the connector's stderr"
    );
}

// A write through the bridge names the app, not the bridge: the connector
// relays the app's own `initialize`, and the shared server records its
// name as `host:<name>`.
#[test]
fn a_write_through_the_bridge_names_the_app_on_the_shared_server() {
    let host = Host::start(root());
    let client = tempdir().unwrap();
    let client_token = client.path().join("token");
    fs::write(&client_token, TOKEN).unwrap();
    let mut child = connector(&host, client.path(), &client_token);
    let mut input = child.0.stdin.take().unwrap();
    let output = lines(child.0.stdout.take().unwrap());
    writeln!(input, "{}", initialize(1)).unwrap();
    input.flush().unwrap();
    assert!(reply(&output).get("result").is_some());
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    writeln!(
        input,
        "{}",
        call(
            2,
            "record",
            json!({"category":"maps","title":"Uses offline maps","content":"Uses offline maps","scope":"global","user_confirmed":true})
        )
    )
    .unwrap();
    input.flush().unwrap();
    let stored = reply(&output);
    let text = stored["result"]["content"][0]["text"].as_str().unwrap();
    let id = serde_json::from_str::<Value>(text).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    drop(input);
    assert!(child.0.wait().unwrap().success());
    let history = succeeded(&on_host(&host, &["history", &id], b""));
    assert_eq!(
        history["memories"][0]["history"][0]["client"], "host:test-device",
        "{history}"
    );
}

// A server restarted between two calls keeps serving the same connector:
// the next call is answered by the restarted server, as rmcp re-initializes
// the expired session. Guards what already worked before the relay could
// recover from a failed call.
#[test]
fn connector_keeps_working_across_a_restart_between_calls() {
    let mut host = Host::start(root());
    let client = tempdir().unwrap();
    let client_token = client.path().join("token");
    fs::write(&client_token, TOKEN).unwrap();
    let mut child = connector(&host, client.path(), &client_token);
    let mut input = child.0.stdin.take().unwrap();
    let errors = Logged::of(child.0.stderr.take().unwrap());
    let output = lines(child.0.stdout.take().unwrap());
    writeln!(input, "{}", initialize(1)).unwrap();
    input.flush().unwrap();
    assert!(
        reply_to(&output, "initialize", Some(&errors))
            .get("result")
            .is_some()
    );
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    writeln!(input, "{}", call(2, "index", json!({}))).unwrap();
    input.flush().unwrap();
    let before = reply_to(&output, "index before the restart", Some(&errors));
    assert_eq!(before["id"], 2);
    assert_ne!(before["result"]["isError"], true, "{before}");
    host.child.kill().unwrap();
    host.child.wait().unwrap();
    host.restart();
    writeln!(input, "{}", call(3, "index", json!({}))).unwrap();
    input.flush().unwrap();
    let after = reply_to(&output, "index after the restart", Some(&errors));
    assert_eq!(after["id"], 3);
    assert_ne!(after["result"]["isError"], true, "{after}");
    drop(input);
    assert!(child.0.wait().unwrap().success());
    assert_no_token(errors);
}

/// The forwarded-call deadline the stall test gives the connector, in
/// milliseconds: past the relay's 10 s reconnect bound with room to tell the
/// two apart, so a call after the deadline shows it reconnects (answered at
/// the reconnect bound) rather than waiting out another deadline.
#[cfg(unix)]
const FORWARDED_CALL_TEST_LIMIT_MS: u64 = 15_000;

/// How long the stall test waits for a late reply that must not come: the
/// resumed host answers within milliseconds.
#[cfg(unix)]
const LATE_REPLY_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// The relay's reconnect bound (`RECONNECT_TIMEOUT` in src/remote.rs).
#[cfg(unix)]
const RECONNECT_BOUND: std::time::Duration = std::time::Duration::from_secs(10);

// Unix only: it stops the host with SIGSTOP, so the host keeps its
// connections open and answers nothing until SIGCONT.
#[cfg(unix)]
#[test]
fn a_call_a_stalled_host_never_answers_is_answered_at_the_deadline_and_its_late_reply_dropped() {
    let host = Host::start(root());
    let client = tempdir().unwrap();
    let client_token = client.path().join("token");
    fs::write(&client_token, TOKEN).unwrap();
    let limit = FORWARDED_CALL_TEST_LIMIT_MS.to_string();
    let mut child = connector_with(
        &host,
        client.path(),
        &client_token,
        &[("USER_MEMORY_FORWARDED_CALL_LIMIT_MS", &limit)],
    );
    let mut input = child.0.stdin.take().unwrap();
    let errors = Logged::of(child.0.stderr.take().unwrap());
    let output = lines(child.0.stdout.take().unwrap());
    writeln!(input, "{}", initialize(1)).unwrap();
    input.flush().unwrap();
    assert!(
        reply_to(&output, "initialize", Some(&errors))
            .get("result")
            .is_some()
    );
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    writeln!(input, "{}", call(2, "index", json!({}))).unwrap();
    input.flush().unwrap();
    let answered = reply_to(&output, "index before the stall", Some(&errors));
    assert_eq!(answered["id"], 2);
    assert_ne!(answered["result"]["isError"], true, "{answered}");
    let pid = host.child.id().to_string();
    let signal = |name: &str| {
        assert!(
            Command::new("kill")
                .args([format!("-{name}"), pid.clone()])
                .status()
                .unwrap()
                .success()
        );
    };
    signal("STOP");
    let started = std::time::Instant::now();
    writeln!(input, "{}", call(3, "index", json!({}))).unwrap();
    input.flush().unwrap();
    let unanswered = reply_to(&output, "index while the host is stalled", Some(&errors));
    let waited = started.elapsed();
    assert_eq!(unanswered["id"], 3);
    assert_eq!(unanswered["result"]["isError"], true, "{unanswered}");
    assert!(
        unanswered["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .ends_with("did not answer; the call may not have completed. Before repeating a write, search for it. Memory reconnects on the next call."),
        "{unanswered}"
    );
    assert!(
        waited >= std::time::Duration::from_millis(FORWARDED_CALL_TEST_LIMIT_MS),
        "answered after {waited:?}, before the deadline"
    );
    // The next call reconnects instead of waiting on the dropped
    // connection: the stalled host never answers its handshake, so it is
    // answered at the reconnect bound, before another deadline.
    let started = std::time::Instant::now();
    writeln!(input, "{}", call(4, "index", json!({}))).unwrap();
    input.flush().unwrap();
    let retried = reply_to(
        &output,
        "index while the host is still stalled",
        Some(&errors),
    );
    let waited = started.elapsed();
    assert_eq!(retried["id"], 4);
    assert_eq!(retried["result"]["isError"], true, "{retried}");
    assert!(
        waited >= RECONNECT_BOUND
            && waited < std::time::Duration::from_millis(FORWARDED_CALL_TEST_LIMIT_MS),
        "answered after {waited:?}, not at the reconnect bound"
    );
    signal("CONT");
    // The host, running again, answers calls 3 and 4 on what the relay
    // dropped: call 5 gets its own answer, and neither late one follows.
    writeln!(input, "{}", call(5, "index", json!({}))).unwrap();
    input.flush().unwrap();
    let next = reply_to(&output, "index after the stall", Some(&errors));
    assert_eq!(next["id"], 5, "{next}");
    assert_ne!(next["result"]["isError"], true, "{next}");
    assert!(
        output.recv_timeout(LATE_REPLY_WAIT).is_err(),
        "a late reply was passed on"
    );
    drop(input);
    assert!(child.0.wait().unwrap().success());
    assert_no_token(errors);
}

// Unix only: it runs setup with a Unix PATH ("<cli>:/usr/bin:/bin") and the
// fake Codex as `cli/codex`, where Windows looks clients up through PATHEXT
// (`codex.exe`) on a `;`-separated PATH.
#[cfg(unix)]
#[test]
fn remote_setup_remembers_endpoint_and_cli_commands_use_server_memory() {
    let host = Host::start(root());
    host.rpc(initialize(10));
    host.rpc(call(1,"record",json!({"category":"test","context":"Remote setup","title":"Remote-only test preference","content":"Remote-only test preference","reason":"Access across devices","type":"preference","scope":"global","user_confirmed":true})));
    let client = tempdir().unwrap();
    let bin = client.path().join("installed tools");
    let cli_dir = client.path().join("cli");
    fs::create_dir(&cli_dir).unwrap();
    let url = format!("http://{}/mcp", host.address);
    let token_file = host.root.path().join("token");
    let config = json!({"enabled":true,"transport":{"type":"stdio","command":bin.join("user-memory"),"args":["serve"],"env":{"USER_MEMORY_URL":url,"USER_MEMORY_TOKEN_FILE":token_file}}});
    fs::write(client.path().join("fixture.json"), config.to_string()).unwrap();
    // `mcp add` installs fixture.json as the registration `mcp get` shows.
    fake::fake(&cli_dir, "codex");
    let run = |binary: &std::path::Path, args: Vec<String>| {
        Command::new(binary)
            .args(args)
            .sandboxed(client.path())
            .env("CLAUDE_CONFIG_DIR", client.path())
            .env("CODEX_HOME", client.path().join(".codex"))
            .env("USER_MEMORY_APP_DIRS", "")
            // Setup edits the global git ignore that git config names.
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_GLOBAL")
            .env_remove("XDG_CONFIG_HOME")
            .env("PATH", format!("{}:/usr/bin:/bin", cli_dir.display()))
            .env_remove("USER_MEMORY_DB")
            .env_remove("USER_MEMORY_URL")
            .env_remove("USER_MEMORY_TOKEN_FILE")
            .env("USER_MEMORY_NO_UPDATE_CHECK", "1")
            .output()
            .unwrap()
    };
    let output = run(
        std::path::Path::new(env!("CARGO_BIN_EXE_user-memory")),
        vec![
            "setup".into(),
            "--client".into(),
            "codex".into(),
            "--url".into(),
            url,
            "--token-file".into(),
            token_file.to_string_lossy().into_owned(),
            "--install-dir".into(),
            bin.to_string_lossy().into_owned(),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let context = run(&bin.join("user-memory"), vec!["index".into()]);
    assert!(
        context.status.success(),
        "{}",
        String::from_utf8_lossy(&context.stderr)
    );
    assert!(String::from_utf8_lossy(&context.stdout).contains("Remote-only test preference"));
    let doctor = run(&bin.join("user-memory"), vec!["doctor".into()]);
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    assert!(!sandbox::folder(client.path(), "store").exists());
    let receipt = fs::read_to_string(bin.join(".user-memory.json")).unwrap();
    assert!(!receipt.contains(TOKEN));
    assert!(!receipt.contains("\"db\""));
    // The skill's playbooks drive the remote server's tools too.
    assert!(
        client
            .path()
            .join(".agents/skills/user-memory/SKILL.md")
            .is_file()
    );
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("Skill: OK"));
}

#[test]
fn invalid_remote_receipt_never_falls_back_to_a_local_memory_store() {
    let client = tempdir().unwrap();
    let bin = client.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let executable = bin.join("user-memory");
    fs::copy(env!("CARGO_BIN_EXE_user-memory"), &executable).unwrap();
    fs::write(bin.join(".user-memory.json"), "{\"remote\": broken}").unwrap();
    let output = Command::new(executable)
        .arg("index")
        .sandboxed(client.path())
        .env_remove("USER_MEMORY_DB")
        .env_remove("USER_MEMORY_URL")
        .output_retrying();
    assert!(!output.status.success());
    assert!(!sandbox::folder(client.path(), "store").exists());
}

#[test]
fn explicit_database_selects_local_mode_over_a_saved_remote_endpoint() {
    let client = tempdir().unwrap();
    let bin = client.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let executable = bin.join("user-memory");
    fs::copy(env!("CARGO_BIN_EXE_user-memory"), &executable).unwrap();
    fs::write(bin.join(".user-memory.json"),json!({"remote":{"url":"http://127.0.0.1:1/mcp","token_file":client.path().join("missing-token")},"clients":["codex"]}).to_string()).unwrap();
    let db = client.path().join("explicit.sqlite3");
    let output = Command::new(executable)
        .arg("index")
        .sandboxed(client.path())
        .env("USER_MEMORY_DB", &db)
        .env_remove("USER_MEMORY_URL")
        .output_retrying();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(db.exists());
}

#[test]
fn connector_responds_to_client_disconnect_during_a_slow_request() {
    let host = Host::start(root());
    let mut child = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .arg("connect")
        .arg("--url")
        .arg(format!("http://{}/mcp", host.address))
        .arg("--token-file")
        .arg(host.root.path().join("token"))
        .sandboxed(host.root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let output = lines(child.stdout.take().unwrap());
    let init = json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"test-device","version":"1"},"io.modelcontextprotocol/clientCapabilities":{}}}});
    writeln!(input, "{init}").unwrap();
    input.flush().unwrap();
    let init_reply = reply(&output);
    assert!(init_reply.get("error").is_none(), "{init_reply}");
    let lock = rusqlite::Connection::open(host.root.path().join("shared.sqlite3")).unwrap();
    lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let mut slow_request = call(
        2,
        "record",
        json!({"category":"test","context":"locked database","title":"store","content":"store","scope":"global","user_confirmed":true}),
    );
    slow_request["params"]["_meta"] = init["params"]["_meta"].clone();
    writeln!(input, "{slow_request}").unwrap();
    input.flush().unwrap();
    let arrived = output.recv_timeout(std::time::Duration::from_millis(250));
    let pending = matches!(arrived, Err(std::sync::mpsc::RecvTimeoutError::Timeout));
    drop(input);
    // The host answers the record only after its busy timeout, with an
    // error the connector would pass on: a connector that waited for it
    // prints that reply before it stops.
    let deadline = std::time::Instant::now() + STUCK;
    while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let stopped = child.try_wait().unwrap().is_some();
    if !stopped {
        let _ = child.kill();
        let _ = child.wait();
    }
    lock.execute_batch("ROLLBACK").unwrap();
    assert!(stopped, "Connector never stopped after stdin EOF");
    // Everything it printed, up to its stdout's end.
    let passed_on: Vec<String> = output.iter().collect();
    assert!(
        passed_on.is_empty(),
        "Connector ignored stdin EOF and waited for the HTTP response: {passed_on:?}"
    );
    assert!(
        pending,
        "Fixture request did not wait on the locked database: {arrived:?}"
    );
}

/// The PATH for setup in `home`: its fake clients in `cli`, and on Unix the
/// system folders, for git.
fn path(home: &std::path::Path) -> std::ffi::OsString {
    let system: &[&str] = if cfg!(unix) {
        &["/usr/bin", "/bin"]
    } else {
        &[]
    };
    let dirs = std::iter::once(home.join("cli")).chain(system.iter().map(std::path::PathBuf::from));
    std::env::join_paths(dirs).unwrap()
}

fn user_memory(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .args(args)
        .sandboxed(home)
        .env("CLAUDE_CONFIG_DIR", home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("USER_MEMORY_APP_DIRS", "")
        // Setup edits the global git ignore that git config names.
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_CONFIG_GLOBAL")
        .env_remove("XDG_CONFIG_HOME")
        .env("PATH", path(home))
        .env_remove("USER_MEMORY_DB")
        .env_remove("USER_MEMORY_URL")
        .env_remove("USER_MEMORY_TOKEN_FILE")
        .output()
        .unwrap()
}

#[test]
fn cli_memory_commands_run_against_the_configured_server() {
    let host = Host::start(root());
    let client = tempdir().unwrap();
    let run = |args: &[&str]| -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_user-memory"))
            .args(args)
            .sandboxed(client.path())
            .env("USER_MEMORY_URL", format!("http://{}/mcp", host.address))
            .env("USER_MEMORY_TOKEN_FILE", host.root.path().join("token"))
            .env_remove("USER_MEMORY_DB")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let stored = run(&[
        "record",
        "--type",
        "preference",
        "--category",
        "programming",
        "--content",
        "Prefer Rust on the server",
        "--title",
        "Prefer Rust on the server",
        "--scope",
        "global",
    ]);
    assert_eq!(stored["stored"], true, "{stored}");
    let id = stored["id"].as_str().unwrap().to_owned();
    assert_eq!(run(&["search", "server"])["memories"][0]["id"], id.as_str());
    assert_eq!(run(&["read", &id])["memories"][0]["id"], id.as_str());
    assert!(run(&["review"]).is_object());
    let updated = run(&["update", &id, "--content", "Prefer Go on the server"]);
    assert_eq!(updated["updated"], true, "{updated}");
    let shared = rusqlite::Connection::open(host.root.path().join("shared.sqlite3")).unwrap();
    let content: String = shared
        .query_row("SELECT content FROM memories WHERE id=?1", [&id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(content, "Prefer Go on the server");
    let forgotten = run(&["forget", &id]);
    assert_eq!(forgotten["deleted"], true, "{forgotten}");
    let count: i64 = shared
        .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    assert!(!sandbox::folder(client.path(), "store").exists());
}

/// A call the server fails ends the CLI with the server's own reason and a
/// failing status, as the same call on a local store does.
#[test]
fn a_failed_server_call_fails_the_cli_with_the_servers_reason() {
    let host = Host::start(root());
    let client = tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .args(["read", "dead"])
        .sandboxed(client.path())
        .env("USER_MEMORY_URL", format!("http://{}/mcp", host.address))
        .env("USER_MEMORY_TOKEN_FILE", host.root.path().join("token"))
        .env_remove("USER_MEMORY_DB")
        .output()
        .unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(!output.status.success(), "{stdout}");
    assert!(
        stderr.contains("dead is not a memory id: give a full id or its first 8 characters"),
        "{stderr}"
    );
    assert!(!stderr.contains(r#"{"error""#), "{stderr}");
}

#[test]
fn endpoint_urls_cannot_carry_credentials_queries_or_fragments() {
    let root = root();
    let token = root.path().join("token");
    for url in [
        "https://user@memory.example/mcp",
        "https://:secret@memory.example/mcp",
        "https://memory.example/mcp?token=secret",
        "https://memory.example/mcp#secret",
    ] {
        let output = user_memory(
            root.path(),
            &[
                "connect",
                "--url",
                url,
                "--token-file",
                token.to_str().unwrap(),
            ],
        );
        assert!(!output.status.success(), "{url} was accepted");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("cannot contain credentials"),
            "{url}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn access_tokens_must_be_long_and_header_safe() {
    let accepted = tempdir().unwrap();
    fs::write(accepted.path().join("token"), &TOKEN[..32]).unwrap();
    Host::start(accepted);
    for token in [&TOKEN[..31], &format!("{}!", &TOKEN[..40])] {
        let root = tempdir().unwrap();
        fs::write(root.path().join("token"), token).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_user-memory"))
            .args(["host", "--listen", "127.0.0.1:0", "--db"])
            .arg(root.path().join("shared.sqlite3"))
            .arg("--token-file")
            .arg(root.path().join("token"))
            .sandboxed(root.path())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + STUCK;
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("Host started with {token:?}");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let output = child.wait_with_output().unwrap();
        assert!(!output.status.success(), "{token:?} was accepted");
        assert!(String::from_utf8_lossy(&output.stderr).contains("at least 32 ASCII characters"));
    }
}

/// Minimal MCP endpoint that completes the handshake but lists `tools`.
/// Only the setup test uses it, which needs a Unix shell for its fake client.
#[cfg(unix)]
fn foreign_endpoint(tools: Value) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            let mut length = 0;
            reader.read_line(&mut request).unwrap();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                if let Some((key, value)) = line.split_once(':')
                    && key.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let message: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let (status, reply) = if !request.starts_with("POST") {
                (405, Value::Null)
            } else if message.get("id").is_none() {
                (202, Value::Null)
            } else {
                let result = match message["method"].as_str() {
                    Some("initialize") => {
                        json!({"result":{"protocolVersion":message["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"other-server","version":"1"}}})
                    }
                    Some("tools/list") => json!({"result":{"tools":tools}}),
                    _ => json!({"error":{"code":-32601,"message":"Method not found"}}),
                };
                let mut reply = json!({"jsonrpc":"2.0","id":message["id"]});
                reply
                    .as_object_mut()
                    .unwrap()
                    .extend(result.as_object().unwrap().clone());
                (200, reply)
            };
            let body = if reply.is_null() {
                String::new()
            } else {
                reply.to_string()
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    address
}

// Unix only: it runs setup with a Unix PATH ("<cli>:/usr/bin:/bin") and the
// fake Codex as `cli/codex`, where Windows looks clients up through PATHEXT
// (`codex.exe`) on a `;`-separated PATH.
#[cfg(unix)]
#[test]
fn setup_refuses_endpoints_that_do_not_serve_this_memory() {
    let root = root();
    let token = root.path().join("token");
    fs::create_dir(root.path().join("cli")).unwrap();
    fake::failing(&root.path().join("cli"), "codex", 1);
    let tool = |name: &str, id: Value| json!({"name":name,"inputSchema":{"type":"object","properties":{"memory_id":id}}});
    let uuid = json!({"type":"string","format":"uuid"});
    let complete = |id: Value| {
        json!([
            tool("record", json!({})),
            tool("search", json!({})),
            tool("index", json!({})),
            tool("update", id.clone()),
            tool("forget", id),
            tool("read", json!({})),
        ])
    };
    let mut missing = complete(uuid.clone());
    missing.as_array_mut().unwrap().pop();
    // A server from before duplicate resolution: UUID ids and forget by origin,
    // but record would silently ignore confirms and distinct_from, and
    // update would read distinct_from as a confirmation.
    let mut older = complete(uuid.clone());
    older[4]["inputSchema"]["properties"]["origin"] = json!({"type":["string","null"]});
    older[0]["inputSchema"]["properties"] = json!({"supersedes":uuid.clone()});
    let other_token = tempdir().unwrap();
    fs::write(other_token.path().join("token"), &TOKEN[1..]).unwrap();
    let host = Host::start(other_token);
    for (url, expected) in [
        (format!("http://{}/mcp", host.address), "access token"),
        (
            format!("http://{}/mcp", foreign_endpoint(missing)),
            "does not provide the user-memory tool set",
        ),
        (
            format!(
                "http://{}/mcp",
                foreign_endpoint(complete(json!({"type":"string"})))
            ),
            "must expose UUID memory IDs",
        ),
        (
            format!(
                "http://{}/mcp",
                foreign_endpoint(complete(json!({"type":"integer","format":"uuid"})))
            ),
            "must expose UUID memory IDs",
        ),
        (
            format!("http://{}/mcp", foreign_endpoint(older)),
            "cannot resolve similar memories",
        ),
    ] {
        let output = user_memory(
            root.path(),
            &[
                "setup",
                "--client",
                "codex",
                "--url",
                &url,
                "--token-file",
                token.to_str().unwrap(),
                "--install-dir",
                root.path().join("bin").to_str().unwrap(),
            ],
        );
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{url} was accepted");
        assert!(error.contains(expected), "{url}: {error}");
        assert!(!root.path().join("bin").exists());
    }
}

#[test]
fn unreadable_receipt_never_falls_back_to_a_local_memory_store() {
    let client = tempdir().unwrap();
    let bin = client.path().join("bin");
    fs::create_dir_all(bin.join(".user-memory.json")).unwrap();
    let executable = bin.join("user-memory");
    fs::copy(env!("CARGO_BIN_EXE_user-memory"), &executable).unwrap();
    let output = Command::new(executable)
        .arg("index")
        .sandboxed(client.path())
        .env_remove("USER_MEMORY_DB")
        .env_remove("USER_MEMORY_URL")
        .output_retrying();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Cannot read saved setup"));
    assert!(!sandbox::folder(client.path(), "store").exists());
}

#[test]
fn doctor_with_an_explicit_database_ignores_a_saved_remote_endpoint() {
    let client = tempdir().unwrap();
    let bin = client.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let executable = bin.join("user-memory");
    fs::copy(env!("CARGO_BIN_EXE_user-memory"), &executable).unwrap();
    fs::write(bin.join(".user-memory.json"),json!({"remote":{"url":"http://127.0.0.1:1/mcp","token_file":client.path().join("missing-token")},"clients":["codex"]}).to_string()).unwrap();
    let output = Command::new(executable)
        .arg("doctor")
        .env("USER_MEMORY_NO_UPDATE_CHECK", "1")
        .sandboxed(client.path())
        .env("CLAUDE_CONFIG_DIR", client.path())
        .env("CODEX_HOME", client.path().join(".codex"))
        .env("USER_MEMORY_APP_DIRS", "")
        // Setup edits the global git ignore that git config names.
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_CONFIG_GLOBAL")
        .env_remove("XDG_CONFIG_HOME")
        // No client on PATH, as above.
        .env("PATH", client.path().join("no-tools"))
        .env("USER_MEMORY_DB", client.path().join("explicit.sqlite3"))
        .env_remove("USER_MEMORY_URL")
        .output_retrying();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("Memory database is missing"), "{error}");
}

#[test]
fn host_refuses_to_serve_a_store_from_another_version() {
    let root = root();
    let db = root.path().join("shared.sqlite3");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE memories (id TEXT PRIMARY KEY, content TEXT); PRAGMA user_version = 999;",
    )
    .unwrap();
    drop(conn);
    let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .args(["host", "--listen", "127.0.0.1:0", "--db"])
        .arg(&db)
        .arg("--token-file")
        .arg(root.path().join("token"))
        .sandboxed(root.path())
        .env_remove("USER_MEMORY_DB")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(
        error.contains("has schema version 999, but this user-memory is older than the store"),
        "{error}"
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("Listening"));
}

// A write through the connector reaches this machine's instruction files:
// the server answers, then the connector writes the rules it renders.
// Unix only: it runs setup with a Unix PATH ("<cli>:/usr/bin:/bin") and the
// fake Codex as `cli/codex`, where Windows looks clients up through PATHEXT
// (`codex.exe`) on a `;`-separated PATH.
#[cfg(unix)]
#[test]
fn a_write_through_the_connector_rewrites_the_instruction_files() {
    let host = Host::start(root());
    let client = tempdir().unwrap();
    let bin = client.path().join("installed tools");
    let cli_dir = client.path().join("cli");
    fs::create_dir(&cli_dir).unwrap();
    let url = format!("http://{}/mcp", host.address);
    let token_file = host.root.path().join("token");
    let config = json!({"enabled":true,"transport":{"type":"stdio","command":bin.join("user-memory"),"args":["serve"],"env":{"USER_MEMORY_URL":url,"USER_MEMORY_TOKEN_FILE":token_file}}});
    fs::write(client.path().join("fixture.json"), config.to_string()).unwrap();
    // `mcp add` installs fixture.json as the registration `mcp get` shows.
    fake::fake(&cli_dir, "codex");
    let command = |binary: &std::path::Path| {
        let mut command = Command::new(binary);
        command
            .sandboxed(client.path())
            .env("CLAUDE_CONFIG_DIR", client.path())
            .env("CODEX_HOME", client.path().join(".codex"))
            .env("USER_MEMORY_APP_DIRS", "")
            // Setup edits the global git ignore that git config names.
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_GLOBAL")
            .env_remove("XDG_CONFIG_HOME")
            .env("PATH", format!("{}:/usr/bin:/bin", cli_dir.display()))
            .env_remove("USER_MEMORY_DB")
            .env_remove("USER_MEMORY_URL")
            .env_remove("USER_MEMORY_TOKEN_FILE");
        command
    };
    let setup = command(std::path::Path::new(env!("CARGO_BIN_EXE_user-memory")))
        .args(["setup", "--client", "codex", "--url", &url, "--token-file"])
        .arg(&token_file)
        .arg("--install-dir")
        .arg(&bin)
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let agents = client.path().join(".codex/AGENTS.md");
    assert!(
        fs::read_to_string(&agents)
            .unwrap()
            .contains("<!-- user-memory:start -->")
    );
    let mut child = command(&bin.join("user-memory"))
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let output = lines(child.stdout.take().unwrap());
    writeln!(input, "{}", initialize(1)).unwrap();
    input.flush().unwrap();
    reply(&output);
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    writeln!(
        input,
        "{}",
        call(2, "record", json!({"type":"preference","category":"workflow","title":"Recorded through the connector","content":"Recorded through the connector","scope":"global","user_confirmed":true}))
    )
    .unwrap();
    input.flush().unwrap();
    let recorded = reply(&output);
    assert!(
        recorded["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("\"stored\":true"),
        "{recorded}"
    );
    drop(input);
    assert!(child.wait().unwrap().success());
    assert!(
        fs::read_to_string(&agents)
            .unwrap()
            .contains("Recorded through the connector")
    );
}

// The server cannot see the client's folder, so the connector names its
// project in each link call: the folder's own project is never stored as a
// link, as with a local store.
#[test]
fn the_connector_names_the_folders_project_in_link_calls() {
    let host = Host::start(root());
    let client = tempdir().unwrap();
    let repo = client.path().join("notebooks");
    fs::create_dir(&repo).unwrap();
    for args in [
        &["init", "-q"][..],
        &[
            "remote",
            "add",
            "origin",
            "git@github.com:acme/notebooks.git",
        ],
    ] {
        let status = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap();
        assert!(status.success());
    }
    // A repository inside it that its .user-memory lists.
    let tools = repo.join("tools");
    fs::create_dir(&tools).unwrap();
    for args in [
        &["init", "-q"][..],
        &["remote", "add", "origin", "git@github.com:acme/tools.git"],
    ] {
        let status = Command::new("git")
            .args(args)
            .current_dir(&tools)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap();
        assert!(status.success());
    }
    fs::write(repo.join(".user-memory"), "@tools\n").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .arg("connect")
        .arg("--url")
        .arg(format!("http://{}/mcp", host.address))
        .arg("--token-file")
        .arg(host.root.path().join("token"))
        .current_dir(&repo)
        .sandboxed(client.path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CEILING_DIRECTORIES", client.path())
        .env_remove("USER_MEMORY_DB")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let output = lines(child.stdout.take().unwrap());
    writeln!(input, "{}", initialize(1)).unwrap();
    input.flush().unwrap();
    reply(&output);
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    writeln!(
        input,
        "{}",
        call(
            2,
            "link",
            json!({"origin":"claude:session/a","projects":["github.com/acme/notebooks","github.com/acme/tools","github.com/acme/compiler"]})
        )
    )
    .unwrap();
    input.flush().unwrap();
    let linked: Value = serde_json::from_str(
        reply(&output)["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        linked["projects"],
        json!(["github.com/acme/compiler"]),
        "{linked}"
    );
    assert_eq!(
        linked["notes"],
        json!([
            "github.com/acme/notebooks comes from this folder; it always loads here",
            "github.com/acme/tools is listed in .user-memory; it loads here while listed"
        ])
    );
    drop(input);
    assert!(child.wait().unwrap().success());
}

// A host serves clients in many folders, so its own folder's project means
// nothing to a link: only the folder a client names is never stored.
#[test]
fn a_host_never_takes_its_own_folders_project_for_the_clients() {
    let host = Host::start(root());
    // The folder the host runs in, as `user-memory project-id` names it.
    let here = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .arg("project-id")
        .sandboxed(host.root.path())
        .output()
        .unwrap();
    let key = String::from_utf8_lossy(&here.stdout).trim().to_owned();
    let key = if here.status.success() && !key.is_empty() {
        key
    } else {
        "github.com/acme/unrelated".to_owned()
    };
    host.rpc(initialize(1));
    let linked = host.rpc(call(
        2,
        "link",
        json!({"origin":"claude:session/a","projects":[&key]}),
    ));
    let linked: Value =
        serde_json::from_str(linked["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(linked["added"], json!([key]), "{linked}");
}

// A `link` that only lists is a read on a server too: while another
// connection holds the store's write lock, it answers as it would without
// one instead of waiting the writer out and failing.
#[test]
fn a_server_lists_a_conversations_links_during_another_write() {
    let host = Host::start(root());
    host.rpc(initialize(1));
    let text = |reply: Value| reply["result"]["content"][0]["text"].clone();
    let added = text(host.rpc(call(
        2,
        "link",
        json!({"origin":"claude:session/a","projects":["github.com/acme/tools"]}),
    )));
    assert!(added.as_str().unwrap().contains("\"added\""), "{added}");
    let listing = || text(host.rpc(call(3, "link", json!({"origin":"claude:session/a"}))));
    let free = listing();
    assert!(
        free.as_str().unwrap().contains("github.com/acme/tools"),
        "{free}"
    );
    let writer = rusqlite::Connection::open(host.root.path().join("shared.sqlite3")).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let held = listing();
    writer.execute_batch("ROLLBACK").unwrap();
    assert_eq!(held, free);
}

// A machine that switches between a server and a local store under one
// session id keeps a snapshot of each: neither one's prompts hear of the
// other's memories as changes.
#[test]
fn a_server_and_a_local_store_keep_their_own_session_snapshots() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("token"), TOKEN).unwrap();
    let host = Host::start(root);
    let home = tempdir().unwrap();
    let run = |remote: bool, args: &[&str], stdin: &[u8]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .args(args)
            .current_dir(home.path())
            .sandboxed(home.path())
            .env(time::HOOK_ROOM.0, time::HOOK_ROOM.1)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if remote {
            command
                .env("USER_MEMORY_URL", format!("http://{}/mcp", host.address))
                .env("USER_MEMORY_TOKEN_FILE", host.root.path().join("token"))
                .env_remove("USER_MEMORY_DB");
        } else {
            // The machine's own store, where it lives by default: the one a
            // native install uses before it moves to the server.
            command
                .env_remove("USER_MEMORY_DB")
                .env_remove("USER_MEMORY_URL")
                .env_remove("USER_MEMORY_TOKEN_FILE");
        }
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    for (remote, title) in [(false, "Prefer tabs here"), (true, "Prefer spaces there")] {
        let stored = run(
            remote,
            &[
                "record",
                "--type",
                "preference",
                "--category",
                "workflow",
                "--title",
                title,
                "--content",
                title,
                "--scope",
                "global",
            ],
            b"",
        );
        assert!(stored.contains("\"stored\":true"), "{stored}");
    }
    let payload = |event: &str| {
        json!({"session_id":"s","cwd":home.path(),"hook_event_name":event}).to_string()
    };
    for remote in [false, true] {
        run(
            remote,
            &["session-start", "--client", "claude"],
            payload("SessionStart").as_bytes(),
        );
    }
    for remote in [false, true] {
        let notice = run(
            remote,
            &["prompt-submit", "--client", "claude"],
            payload("UserPromptSubmit").as_bytes(),
        );
        assert_eq!(notice, "", "remote {remote}: nothing changed in its store");
    }
}

/// Runs the CLI as a client of `host`, with `home` as its home folder and
/// `stdin` as its input.
fn client(
    host: &Host,
    home: &std::path::Path,
    args: &[&str],
    stdin: &[u8],
) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .args(args)
        .sandboxed(home)
        .env("USER_MEMORY_URL", format!("http://{}/mcp", host.address))
        .env("USER_MEMORY_TOKEN_FILE", host.root.path().join("token"))
        .env_remove("USER_MEMORY_DB")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

/// Runs the CLI on the host's own store, as a local install would.
fn on_host(host: &Host, args: &[&str], stdin: &[u8]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .args(args)
        .sandboxed(host.root.path())
        .env("USER_MEMORY_DB", host.root.path().join("shared.sqlite3"))
        .env_remove("USER_MEMORY_URL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

fn succeeded(out: &std::process::Output) -> Value {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// An export without its time, which differs between two exports.
/// `document` without its time and its history: an import adds the
/// importing store's own events, and marks the file's as imported.
fn timeless(mut document: Value) -> Value {
    document.as_object_mut().unwrap().remove("exported_at");
    document.as_object_mut().unwrap().remove("history");
    document
}

fn seed(host: &Host, home: &std::path::Path) {
    for (category, content, scope) in [
        (
            "programming",
            "Prefer Rust for services",
            &["--scope", "global"][..],
        ),
        (
            "style",
            "Answer in short paragraphs",
            &["--scope", "global"],
        ),
        (
            "programming",
            "Use Python here",
            &["--scope", "project", "--project", "github.com/me/app"],
        ),
    ] {
        let mut args = vec![
            "record",
            "--type",
            "preference",
            "--category",
            category,
            "--content",
            content,
            "--title",
            content,
        ];
        args.extend_from_slice(scope);
        let stored = succeeded(&client(host, home, &args, b""));
        assert_eq!(stored["stored"], true, "{stored}");
    }
}

// A client exports from and imports into its server as it would with a
// local store: a private file, --force, filters, a preview that writes
// nothing, and every conflict choice.
#[test]
fn a_client_exports_and_imports_through_its_server_as_with_a_local_store() {
    let host = Host::start(root());
    let home = tempdir().unwrap();
    seed(&host, home.path());
    let file = home.path().join("backup.json");
    let path = file.to_str().unwrap();
    let summary = succeeded(&client(&host, home.path(), &["export", path], b""));
    assert_eq!(summary["exported"], true);
    assert_eq!(summary["file"], path);
    assert_eq!(summary["memories"]["confirmed"], 3, "{summary}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let exported: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    let local = on_host(&host, &["export", "-"], b"");
    assert!(local.status.success());
    let local_document: Value = serde_json::from_slice(&local.stdout).unwrap();
    assert_eq!(timeless(exported.clone()), timeless(local_document));
    let again = client(&host, home.path(), &["export", path], b"");
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("use --force to replace it"));
    succeeded(&client(
        &host,
        home.path(),
        &["export", path, "--force"],
        b"",
    ));

    // Filtered, through stdout, the same as on the server.
    let filtered = client(
        &host,
        home.path(),
        &["export", "-", "--category", "coding"],
        b"",
    );
    let local = on_host(&host, &["export", "-", "--category", "coding"], b"");
    let document: Value = serde_json::from_slice(&filtered.stdout).unwrap();
    assert_eq!(document["memories"].as_array().unwrap().len(), 2);
    assert_eq!(
        timeless(document),
        timeless(serde_json::from_slice(&local.stdout).unwrap())
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&filtered.stderr).unwrap(),
        serde_json::from_slice::<Value>(&local.stderr).unwrap()
    );

    // Into a second, empty server: the preview writes nothing.
    let other = Host::start(root());
    let shared = other.root.path().join("shared.sqlite3");
    let before = fs::read(&shared).unwrap();
    let preview = succeeded(&client(&other, home.path(), &["import", path], b""));
    assert_eq!(preview["applied"], false, "{preview}");
    assert_eq!(preview["added"]["confirmed"], 3, "{preview}");
    assert_eq!(
        fs::read(&shared).unwrap(),
        before,
        "a preview writes nothing"
    );
    let filtered = succeeded(&client(
        &other,
        home.path(),
        &["import", "-", "--type", "constraint"],
        &fs::read(&file).unwrap(),
    ));
    assert_eq!(filtered["selected"], 0, "{filtered}");
    for filters in [
        &["--type", "constraint"][..],
        &["--scope", "global", "--category", "style"],
    ] {
        let mut args = vec!["import", "-"];
        args.extend_from_slice(filters);
        let input = fs::read(&file).unwrap();
        assert_eq!(
            succeeded(&client(&other, home.path(), &args, &input)),
            succeeded(&on_host(&other, &args, &input)),
            "{filters:?}"
        );
    }
    let applied = succeeded(&client(
        &other,
        home.path(),
        &["import", path, "--apply"],
        b"",
    ));
    assert_eq!(applied["applied"], true, "{applied}");
    // The server names the import by this client's file: its name alone,
    // never the path to it.
    let import = applied["import"].as_str().unwrap();
    assert!(import.starts_with("backup.json@"), "{applied}");
    // This client refuses reindex, so the advice names where it runs.
    let next = applied["next"].as_str().unwrap();
    assert!(
        next.contains("optionally run `user-memory reindex` on the machine that serves http://"),
        "{next}"
    );
    // A preview's next names no command, so it reaches this client as the
    // server wrote it; one that selects nothing has none.
    assert_eq!(
        preview["next"],
        "Nothing is imported yet: show the user this preview and wait for their answer before applying.",
        "{preview}"
    );
    assert!(filtered.get("next").is_none(), "{filtered}");
    let copied = on_host(&other, &["export", "-"], b"");
    assert_eq!(
        timeless(serde_json::from_slice(&copied.stdout).unwrap()),
        timeless(exported.clone())
    );

    // A changed memory is a conflict, settled as the user chooses.
    let mut changed = exported;
    changed["memories"][0]["content"] = json!("Prefer Go for services");
    let changed = changed.to_string();
    let content = || -> String {
        let out = on_host(&other, &["export", "-"], b"");
        let document: Value = serde_json::from_slice(&out.stdout).unwrap();
        document["memories"][0]["content"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    for (choice, applied, kept) in [
        ("abort", false, "Prefer Rust for services"),
        ("keep-store", true, "Prefer Rust for services"),
        ("use-file", true, "Prefer Go for services"),
    ] {
        let summary = succeeded(&client(
            &other,
            home.path(),
            &["import", "-", "--apply", "--on-conflict", choice],
            changed.as_bytes(),
        ));
        assert_eq!(summary["applied"], applied, "{choice}: {summary}");
        assert_eq!(
            summary["conflicts"].as_array().unwrap().len(),
            1,
            "{choice}: {summary}"
        );
        assert_eq!(content(), kept, "{choice}");
    }
    // The server's refusals come through as they would locally.
    let refused = client(&other, home.path(), &["import", "-"], b"not json");
    let local = on_host(&other, &["import", "-"], b"not json");
    assert!(!refused.status.success());
    assert_eq!(
        String::from_utf8_lossy(&refused.stderr),
        String::from_utf8_lossy(&local.stderr)
    );
}

/// Sends one raw HTTP request to `host`; returns the status and the body.
fn raw(host: &Host, head: &str, body: &[u8]) -> (u16, String) {
    let mut socket = TcpStream::connect(&host.address).unwrap();
    socket.set_read_timeout(Some(STUCK)).unwrap();
    socket.write_all(head.as_bytes()).unwrap();
    // The server may answer and close before reading a body it refuses.
    let _ = socket.write_all(body);
    let mut response = Vec::new();
    let _ = socket.read_to_end(&mut response);
    let response = String::from_utf8_lossy(&response).into_owned();
    let status = response.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    (status, body)
}

// The endpoints sit behind the same checks as /mcp, and the server keeps
// the import limit whatever a client sends.
#[test]
fn the_transfer_endpoints_refuse_bad_tokens_foreign_hosts_and_oversized_imports() {
    let host = Host::start(root());
    let post = |path: &str, host_header: &str, token: Option<&str>, length: usize| {
        let auth = token
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        format!(
            "POST {path} HTTP/1.1\r\nHost: {host_header}\r\nContent-Type: application/json\r\nConnection: close\r\n{auth}Content-Length: {length}\r\n\r\n"
        )
    };
    let address = host.address.clone();
    for path in ["/export", "/import"] {
        assert_eq!(
            raw(&host, &post(path, &address, None, 2), b"{}").0,
            401,
            "{path}"
        );
        assert_eq!(
            raw(
                &host,
                &post(
                    path,
                    &address,
                    Some("wrong-token-wrong-token-wrong-token"),
                    2
                ),
                b"{}"
            )
            .0,
            401,
            "{path}"
        );
        assert_eq!(
            raw(&host, &post(path, "evil.example", Some(TOKEN), 2), b"{}").0,
            403,
            "{path}"
        );
    }
    let (status, body) = raw(&host, &post("/export", &address, Some(TOKEN), 2), b"{}");
    assert_eq!(status, 200, "{body}");

    // Over the request limit: refused before the body is read.
    let (status, body) = raw(
        &host,
        &post("/import", &address, Some(TOKEN), 2 * 64 * 1024 * 1024 + 2),
        b"",
    );
    assert_eq!(status, 413);
    assert!(body.contains("limited to 64 MiB"), "{body}");
    // A request of exactly the limit is read; the document in it, over the
    // import limit, is refused by import.
    let mut big = b"{}\n".to_vec();
    big.resize(2 * 64 * 1024 * 1024 + 1, b' ');
    let (status, body) = raw(
        &host,
        &post("/import", &address, Some(TOKEN), big.len()),
        &big,
    );
    assert_eq!(status, 400);
    assert!(body.contains("limited to 64 MiB"), "{body}");
}

// The client says what went wrong in the user's terms, and a refused
// export leaves no file behind, finished or not.
#[test]
fn a_refused_transfer_says_why_and_leaves_no_file() {
    let host = Host::start(root());
    let home = tempdir().unwrap();
    let file = home.path().join("out.json");
    let refused = client(
        &host,
        home.path(),
        &["export", file.to_str().unwrap(), "--scope", "project"],
        b"",
    );
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("scope project needs projects to list"),
        "{refused:?}"
    );
    let left: Vec<_> = fs::read_dir(home.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(left.is_empty(), "{left:?}");

    let wrong = home.path().join("wrong-token");
    fs::write(&wrong, "f".repeat(64)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .args(["export", "-"])
        .sandboxed(home.path())
        .env("USER_MEMORY_URL", format!("http://{}/mcp", host.address))
        .env("USER_MEMORY_TOKEN_FILE", &wrong)
        .env_remove("USER_MEMORY_DB")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("The memory server refused the access token"),
        "{out:?}"
    );

    // A server from before these endpoints answers 404.
    let old = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = old.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut socket, _)) = old.accept() {
            let mut request = [0; 4096];
            let _ = socket.read(&mut request);
            let _ = socket.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    });
    let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .args(["export", "-"])
        .sandboxed(home.path())
        .env("USER_MEMORY_URL", format!("http://{address}/mcp"))
        .env("USER_MEMORY_TOKEN_FILE", host.root.path().join("token"))
        .env_remove("USER_MEMORY_DB")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(
            "The memory server does not take export from clients; upgrade it to this version"
        ),
        "{out:?}"
    );
}

/// A bridge's `serve` hands over to a replaced binary too: the new process
/// re-handshakes with the memory server with the app's own initialize and
/// answers the next call on the same connection, with no new handshake
/// from the app. On Unix both ways: `exec`, and the relay Windows uses.
#[test]
fn a_bridges_serve_hands_over_and_re_handshakes_with_the_server() {
    let host = Host::start(root());
    let ways: &[bool] = if cfg!(windows) {
        &[true]
    } else {
        &[false, true]
    };
    for &relay in ways {
        let client = tempdir().unwrap();
        let token = client.path().join("token");
        fs::write(&token, TOKEN).unwrap();
        let bin = client.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let exe = bin.join(format!("user-memory{}", std::env::consts::EXE_SUFFIX));
        fs::copy(env!("CARGO_BIN_EXE_user-memory"), &exe).unwrap();
        let mut command = Command::new(&exe);
        command
            .arg("serve")
            .sandboxed(client.path())
            .env("USER_MEMORY_URL", format!("http://{}/mcp", host.address))
            .env("USER_MEMORY_TOKEN_FILE", &token)
            .env("FAKE_USER_MEMORY", env!("CARGO_BIN_EXE_user-memory"))
            .env("FAKE_USER_MEMORY_VERSION", "9.9.9")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if relay {
            command.env("USER_MEMORY_TEST_RELAY", "1");
        }
        let mut child = ChildGuard(command.spawn().unwrap());
        let mut input = child.0.stdin.take().unwrap();
        let mut errors = child.0.stderr.take().unwrap();
        let output = lines(child.0.stdout.take().unwrap());
        writeln!(input, "{}", initialize(1)).unwrap();
        input.flush().unwrap();
        assert!(reply(&output).get("result").is_some());
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .unwrap();
        writeln!(input, "{}", call(2, "index", json!({}))).unwrap();
        input.flush().unwrap();
        let before = reply(&output);
        assert_ne!(before["result"]["isError"], true, "{before}");

        let fakes = client.path().join("fakes");
        fs::create_dir_all(&fakes).unwrap();
        let staged = bin.join(format!(".new{}", std::env::consts::EXE_SUFFIX));
        fs::copy(fake::fake(&fakes, "user-memory"), &staged).unwrap();
        replace::replace_running(&exe, &staged, &bin.join("old.exe"));

        writeln!(input, "{}", call(3, "index", json!({}))).unwrap();
        input.flush().unwrap();
        let after = reply(&output);
        assert_eq!(after["id"], 3, "{after}");
        assert_ne!(after["result"]["isError"], true, "relay {relay}: {after}");
        drop(input);
        assert!(child.0.wait().unwrap().success());
        let mut said = String::new();
        errors.read_to_string(&mut said).unwrap();
        assert!(
            said.contains("user-memory 9.9.9: took over this session from"),
            "relay {relay}: {said}"
        );
        assert!(!said.contains(TOKEN));
    }
}

/// What the versioned fake server says in its handshake, and whether it
/// fails the next request, as a server going down does.
#[derive(Default)]
struct Said {
    version: String,
    min_client: Option<String>,
    fail_next: bool,
    /// The tools it lists; none when unset.
    tools: Option<Value>,
    /// Answers `tools/list` only after [`SLOW_TOOLS`], as a server under
    /// load may, while it answers other requests at once.
    slow_tools: bool,
    /// The token it accepts; any request without it gets 401. Unset, it
    /// accepts any.
    token: Option<String>,
    /// Its handshake reply in full, in place of the one built from the
    /// fields above.
    handshake: Option<Value>,
    /// Holds each handshake reply until set, as a server that has not
    /// answered yet.
    held: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// The protocol version it agrees, in place of the one asked for.
    protocol: Option<String>,
    /// How long each handshake takes it, as over a slow link.
    handshake_time: Option<std::time::Duration>,
}

type Shared = std::sync::Arc<std::sync::Mutex<Said>>;

/// How long a slow fake server takes to list its tools: long enough that a
/// call sent after the request is answered first, when nothing waits for
/// the list.
const SLOW_TOOLS: std::time::Duration = std::time::Duration::from_millis(500);

/// A memory server that answers its handshake as `said` holds at the time,
/// lists the tools it holds and answers every tool call with "served"; its
/// endpoint. Each connection is served on its own thread.
fn versioned_endpoint(said: &Shared) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    serve_fake(listener, said);
    format!("http://{address}/mcp")
}

/// An endpoint where nothing listens yet, and what starts the fake server
/// of [`versioned_endpoint`] there: a server down when a session starts.
fn endpoint_down(said: &Shared) -> (String, impl FnOnce()) {
    let address = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let said = said.clone();
    let start = move || serve_fake(std::net::TcpListener::bind(address).unwrap(), &said);
    (format!("http://{address}/mcp"), start)
}

/// Serves the fake memory server of [`versioned_endpoint`] on `listener`.
fn serve_fake(listener: std::net::TcpListener, said: &Shared) {
    let said = said.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let said = said.clone();
            std::thread::spawn(move || answer_fake(stream, &said));
        }
    });
}

/// Answers one request to the fake server of [`versioned_endpoint`].
fn answer_fake(mut stream: std::net::TcpStream, said: &Shared) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();
    let mut length = 0;
    let mut authorization = None;
    if reader.read_line(&mut request).is_err() {
        return;
    }
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            } else if key.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_owned());
            }
        }
    }
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let message: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let (held, handshake_time) = {
        let said = said.lock().unwrap();
        (said.held.clone(), said.handshake_time)
    };
    if message["method"] == "initialize" {
        if let Some(held) = held {
            while !held.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        if let Some(time) = handshake_time {
            std::thread::sleep(time);
        }
    }
    let mut said = said.lock().unwrap();
    let slow = said.slow_tools && message["method"] == "tools/list";
    let refused = said
        .token
        .as_ref()
        .is_some_and(|token| authorization != Some(format!("Bearer {token}")));
    let (status, reply) = if !request.starts_with("POST") {
        (405, Value::Null)
    } else if refused {
        (401, Value::Null)
    } else if said.fail_next && message["method"] == "tools/call" {
        said.fail_next = false;
        // The connection ends with no answer, as a server stopping.
        return;
    } else if message.get("id").is_none() {
        (202, Value::Null)
    } else {
        let result = match message["method"].as_str() {
            Some("server/discover") => {
                let mut capabilities = json!({"tools":{}});
                if let Some(min) = &said.min_client {
                    capabilities["experimental"] = json!({"user-memory": {"min_client": min}});
                }
                json!({"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":capabilities,"ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"user-memory","version":said.version}}}})
            }
            Some("initialize") if said.handshake.is_some() => {
                json!({"result": said.handshake.clone()})
            }
            Some("initialize") => {
                let mut capabilities = json!({"tools":{}});
                if let Some(min) = &said.min_client {
                    capabilities["experimental"] = json!({"user-memory": {"min_client": min}});
                }
                let protocol = said
                    .protocol
                    .clone()
                    .map_or_else(|| message["params"]["protocolVersion"].clone(), Value::from);
                json!({"result":{"protocolVersion":protocol,"capabilities":capabilities,"serverInfo":{"name":"user-memory","version":said.version}}})
            }
            // A later page holds nothing more.
            Some("tools/list") if message.pointer("/params/cursor").is_some() => {
                json!({"result":{"tools":[]}})
            }
            Some("tools/list") => {
                json!({"result":{"tools":said.tools.clone().unwrap_or_else(|| json!([]))}})
            }
            Some("tools/call") => {
                json!({"result":{"content":[{"type":"text","text":"served"}],"isError":false}})
            }
            _ => json!({"error":{"code":-32601,"message":"Method not found"}}),
        };
        let mut reply = json!({"jsonrpc":"2.0","id":message["id"]});
        reply
            .as_object_mut()
            .unwrap()
            .extend(result.as_object().unwrap().clone());
        (200, reply)
    };
    drop(said);
    if slow {
        std::thread::sleep(SLOW_TOOLS);
    }
    let body = if reply.is_null() {
        String::new()
    } else {
        reply.to_string()
    };
    // A refusal names its scheme, as the real host's does.
    let challenge = if status == 401 {
        "WWW-Authenticate: Bearer\r\n"
    } else {
        ""
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} Reply\r\n{challenge}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn said(version: &str, min_client: Option<&str>) -> Shared {
    std::sync::Arc::new(std::sync::Mutex::new(Said {
        version: version.into(),
        min_client: min_client.map(str::to_owned),
        ..Said::default()
    }))
}

/// Each row of the version table: session start says a line only when this
/// machine is below its server's minimum, or its server below this
/// machine's floor; a server without a minimum asks for nothing.
#[test]
fn session_start_says_when_either_side_is_below_the_others_floor() {
    let home = tempdir().unwrap();
    let token = home.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    for (this, version, min, expected) in [
        (
            "0.10.1",
            "0.11.2",
            Some("0.11.0"),
            Some(
                "The memory server at {url} runs user-memory 0.11.2 and needs 0.11.0 or newer on this machine (0.10.1). Upgrade: user-memory upgrade",
            ),
        ),
        ("0.11.0", "0.11.2", Some("0.11.0"), None),
        ("0.12.0", "0.11.2", Some("0.11.0"), None),
        (
            "0.12.0",
            "0.7.0",
            None,
            Some(
                "This machine runs user-memory 0.12.0, which needs the memory server at {url} to run 0.8.0 or newer (it runs 0.7.0). Upgrade the server on its machine.",
            ),
        ),
        ("0.0.1", "0.9.2", None, None),
        // This client's floor for servers, at its boundary.
        ("0.10.0", "0.8.0", None, None),
        (
            "0.10.0",
            "0.7.9",
            None,
            Some(
                "This machine runs user-memory 0.10.0, which needs the memory server at {url} to run 0.8.0 or newer (it runs 0.7.9). Upgrade the server on its machine.",
            ),
        ),
    ] {
        let url = versioned_endpoint(&said(version, min));
        let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
            .arg("session-start")
            .arg(home.path())
            .sandboxed(home.path())
            .env("USER_MEMORY_URL", &url)
            .env("USER_MEMORY_TOKEN_FILE", &token)
            .env("USER_MEMORY_TEST_VERSION", this)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stdout = String::from_utf8(out.stdout).unwrap();
        match expected {
            Some(line) => assert_eq!(
                stdout.lines().next(),
                Some(line.replace("{url}", &url).as_str()),
                "{this} with {version}: {stdout}"
            ),
            None => assert!(
                !stdout.contains("memory server at") && !stdout.contains("This machine runs"),
                "{this} with {version}: {stdout}"
            ),
        }
        assert_eq!(
            stdout.matches("Upgrade").count(),
            usize::from(expected.is_some_and(|l| l.contains("Upgrade"))),
            "said once: {stdout}"
        );
    }
}

/// A bridge, and one handed over to a new binary, says in the next tool
/// reply when a re-handshake finds its server's minimum risen above this
/// machine; once, and after the reply's own content.
#[test]
fn a_risen_minimum_is_said_in_the_next_tool_reply_after_a_re_handshake() {
    let home = tempdir().unwrap();
    let token = home.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    for handed_over in [false, true] {
        let state = said("0.10.0", Some("0.8.0"));
        let url = versioned_endpoint(&state);
        let bin = home.path().join(format!("bin-{handed_over}"));
        fs::create_dir_all(&bin).unwrap();
        let exe = bin.join(format!("user-memory{}", std::env::consts::EXE_SUFFIX));
        fs::copy(env!("CARGO_BIN_EXE_user-memory"), &exe).unwrap();
        let mut child = ChildGuard(
            Command::new(&exe)
                .arg("serve")
                .sandboxed(home.path())
                .env("USER_MEMORY_URL", &url)
                .env("USER_MEMORY_TOKEN_FILE", &token)
                .env("USER_MEMORY_TEST_VERSION", "0.10.0")
                .env("FAKE_USER_MEMORY", env!("CARGO_BIN_EXE_user-memory"))
                .env("FAKE_USER_MEMORY_VERSION", "0.10.0")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let mut input = child.0.stdin.take().unwrap();
        let output = lines(child.0.stdout.take().unwrap());
        let mut ask = |message: Value| -> Value {
            writeln!(input, "{message}").unwrap();
            input.flush().unwrap();
            // A notification gets no reply.
            if message.get("id").is_none() {
                return Value::Null;
            }
            loop {
                let reply = reply(&output);
                if reply["id"] == message["id"] {
                    return reply;
                }
            }
        };
        assert!(ask(initialize(1)).get("result").is_some());
        ask(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        let texts = |reply: &Value| -> Vec<String> {
            reply["result"]["content"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["text"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(texts(&ask(call(2, "index", json!({})))), ["served"]);
        state.lock().unwrap().min_client = Some("0.11.0".into());
        let line = format!(
            "The memory server at {url} runs user-memory 0.10.0 and needs 0.11.0 or newer on this machine (0.10.0). Upgrade: user-memory upgrade"
        );
        if handed_over {
            let fakes = home.path().join("fakes");
            fs::create_dir_all(&fakes).unwrap();
            let staged = bin.join(format!(".new{}", std::env::consts::EXE_SUFFIX));
            fs::copy(fake::fake(&fakes, "user-memory"), &staged).unwrap();
            replace::replace_running(&exe, &staged, &bin.join("old.exe"));
        } else {
            state.lock().unwrap().fail_next = true;
            let failed = ask(call(3, "index", json!({})));
            assert_eq!(failed["result"]["isError"], true, "{failed}");
        }
        assert_eq!(
            texts(&ask(call(4, "index", json!({})))),
            ["served".to_owned(), line],
            "handed over: {handed_over}"
        );
        assert_eq!(
            texts(&ask(call(5, "index", json!({})))),
            ["served"],
            "said twice"
        );
    }
}

/// The handshake says what clients compare: the server's name and
/// version in `serverInfo`, and the oldest client it supports in
/// `capabilities.experimental["user-memory"]`; and that its tool list can
/// change, which a session handed over to a new binary announces.
#[test]
fn the_handshake_names_the_version_and_the_oldest_client() {
    let host = Host::start(root());
    let reply = host.rpc(initialize(1));
    let result = &reply["result"];
    assert_eq!(result["serverInfo"]["name"], "user-memory", "{reply}");
    assert_eq!(result["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    let min = result["capabilities"]["experimental"]["user-memory"]["min_client"]
        .as_str()
        .unwrap_or_else(|| panic!("no min_client: {reply}"));
    let parts: Vec<&str> = min.split('.').collect();
    assert!(
        parts.len() == 3 && parts.iter().all(|part| part.parse::<u64>().is_ok()),
        "{min}"
    );
    assert_eq!(
        result["capabilities"]["tools"]["listChanged"], true,
        "{reply}"
    );
}

/// This server's floor for clients, at its boundary: a client of the real
/// host from 0.8.0 on is told nothing, one below it is told to upgrade.
#[test]
fn the_servers_oldest_client_is_said_to_a_client_below_it() {
    let host = Host::start(root());
    let home = tempdir().unwrap();
    let token = home.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    let url = format!("http://{}/mcp", host.address);
    for (this, told) in [("0.8.0", false), ("0.7.9", true)] {
        let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
            .arg("session-start")
            .arg(home.path())
            .sandboxed(home.path())
            .env("USER_MEMORY_URL", &url)
            .env("USER_MEMORY_TOKEN_FILE", &token)
            .env("USER_MEMORY_TEST_VERSION", this)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stdout = String::from_utf8(out.stdout).unwrap();
        let line = format!(
            "The memory server at {url} runs user-memory {} and needs 0.8.0 or newer on this machine ({this}). Upgrade: user-memory upgrade",
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(
            stdout.lines().next() == Some(line.as_str()),
            told,
            "{this}: {stdout}"
        );
    }
}

/// The release that adds history, as the clients in these tests report it.
const HISTORY_SINCE: &str = "0.10.0";

/// The release before history, as an older server reports it.
const BEFORE_HISTORY: &str = "0.9.3";

/// What a client of `url` says when asked for history from a server that
/// runs `server`, a release before history.
fn keeps_no_history(url: &str, server: &str) -> String {
    format!(
        "The memory server at {url} runs user-memory {server}, which keeps no history (history needs {HISTORY_SINCE} or newer). Upgrade the server on its machine."
    )
}

/// What the bridge adds for the agent to a refused history read.
const NOTHING_READ: &str = "Nothing was read. Tell the user this memory server is too old to show history. A read without history still returns the memory, but says nothing about whether it changed.";

/// `user-memory args` from a client of `host`, as the debug build of
/// `this` when given.
fn client_run(host: &Host, home: &Path, this: Option<&str>, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
    if let Some(this) = this {
        command.env("USER_MEMORY_TEST_VERSION", this);
    }
    command
        .args(args)
        .sandboxed(home)
        .env("USER_MEMORY_URL", format!("http://{}/mcp", host.address))
        .env("USER_MEMORY_TOKEN_FILE", host.root.path().join("token"))
        .env_remove("USER_MEMORY_DB")
        .output()
        .unwrap()
}

/// `client_run`'s reply, which must have succeeded.
fn client_reply(host: &Host, home: &Path, this: Option<&str>, args: &[&str]) -> Value {
    let out = client_run(host, home, this, args);
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// A memory recorded through `host` and corrected once, so its history has
/// two events; its id.
fn corrected_memory(host: &Host, home: &Path, this: Option<&str>) -> String {
    let stored = client_reply(
        host,
        home,
        this,
        &[
            "record",
            "--type",
            "preference",
            "--category",
            "programming",
            "--title",
            "Prefer short functions",
            "--content",
            "Prefer short functions",
            "--scope",
            "global",
        ],
    );
    let id = stored["id"].as_str().unwrap().to_owned();
    let updated = client_reply(
        host,
        home,
        this,
        &["update", &id, "--content", "Prefer short, named functions"],
    );
    assert_eq!(updated["updated"], true, "{updated}");
    id
}

/// History asked of a server from before history is refused with which side
/// to upgrade, by `history` and by `read --history`, with nothing on
/// stdout: an older server would answer with the memory and no history,
/// which reads as a memory that never changed.
#[test]
fn history_from_a_server_before_history_says_which_side_to_upgrade() {
    let host = Host::start_as(root(), Some(BEFORE_HISTORY));
    let home = tempdir().unwrap();
    let url = format!("http://{}/mcp", host.address);
    let id = corrected_memory(&host, home.path(), Some(HISTORY_SINCE));
    for args in [
        vec!["history", id.as_str()],
        vec!["read", id.as_str(), "--history"],
    ] {
        let out = client_run(&host, home.path(), Some(HISTORY_SINCE), &args);
        let (stdout, stderr) = (
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(!out.status.success(), "{args:?}: {stdout}");
        assert!(stdout.is_empty(), "{args:?}: {stdout}");
        assert!(
            stderr.contains(&keeps_no_history(&url, BEFORE_HISTORY)),
            "{args:?}: {stderr}"
        );
    }
}

/// An undo of an import asked of a server from before undo is refused with
/// which side to upgrade: an older server would ignore `import`, refuse
/// for want of a target, and the memory stays where the user expected it
/// gone, so nothing is sent.
#[test]
fn undoing_an_import_on_a_server_before_undo_says_which_side_to_upgrade() {
    let host = Host::start_as(root(), Some(BEFORE_HISTORY));
    let home = tempdir().unwrap();
    let url = format!("http://{}/mcp", host.address);
    let out = client_run(
        &host,
        home.path(),
        Some(HISTORY_SINCE),
        &[
            "forget",
            "--import",
            "shared.json@2026-01-01T00:00:00Z.0123abcd",
        ],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "The memory server at {url} runs user-memory {BEFORE_HISTORY}, which cannot undo an import (import needs {HISTORY_SINCE} or newer). Upgrade the server on its machine."
        )),
        "{stderr}"
    );
}

/// History from a server new enough is read as before: one of the release
/// that adds it, and one of this build's own version, which in a tree not
/// yet released is the release before it.
#[test]
fn history_from_a_server_new_enough_or_of_this_build_is_read() {
    for version in [Some(HISTORY_SINCE), None] {
        let host = Host::start_as(root(), version);
        let home = tempdir().unwrap();
        let id = corrected_memory(&host, home.path(), version);
        for args in [
            vec!["history", id.as_str()],
            vec!["read", id.as_str(), "--history"],
        ] {
            let read = client_reply(&host, home.path(), version, &args);
            let events = read["memories"][0]["history"]
                .as_array()
                .unwrap_or_else(|| panic!("{version:?} {args:?}: {read}"));
            assert_eq!(events.len(), 2, "{version:?} {args:?}: {read}");
        }
    }
}

/// A server from before history still serves everything that needs no newer
/// release, `update --drop-history` included: with no history kept, the
/// correction leaves the store as the option asks.
#[test]
fn a_server_before_history_still_serves_what_needs_no_newer_release() {
    let host = Host::start_as(root(), Some(BEFORE_HISTORY));
    let home = tempdir().unwrap();
    let this = Some(HISTORY_SINCE);
    let id = corrected_memory(&host, home.path(), this);
    let read = client_reply(&host, home.path(), this, &["read", &id]);
    assert_eq!(read["memories"][0]["id"], id.as_str(), "{read}");
    let found = client_reply(&host, home.path(), this, &["search", "functions"]);
    assert_eq!(found["memories"][0]["id"], id.as_str(), "{found}");
    let index = client_run(&host, home.path(), this, &["index"]);
    let listed = String::from_utf8_lossy(&index.stdout);
    assert!(
        index.status.success() && listed.contains("Prefer short functions"),
        "{listed}"
    );
    let dropped = client_reply(
        &host,
        home.path(),
        this,
        &[
            "update",
            &id,
            "--content",
            "Prefer small functions",
            "--drop-history",
        ],
    );
    assert_eq!(dropped["updated"], true, "{dropped}");
}

/// Through the bridge, a history read from a server before history is
/// answered by the bridge as a tool error the agent relays, and never sent;
/// a read without history and a correction with drop_history go through.
/// Once the server is upgraded, the next history read is the server's: on
/// Codex's opening and on Claude Code's, whose listen stream stays open.
#[test]
fn the_bridge_refuses_history_from_a_server_before_history_until_it_is_upgraded() {
    for on_2026_protocol in [false, true] {
        let mut host = Host::start_as(root(), Some(BEFORE_HISTORY));
        let home = tempdir().unwrap();
        let url = format!("http://{}/mcp", host.address);
        let id = corrected_memory(&host, home.path(), Some(HISTORY_SINCE));
        let token = host.root.path().join("token");
        let mut app = App::start(
            home.path(),
            &url,
            &token,
            &[("USER_MEMORY_TEST_VERSION", HISTORY_SINCE)],
        );
        let wrap = |message: Value| {
            if on_2026_protocol {
                on_2026(message)
            } else {
                message
            }
        };
        if on_2026_protocol {
            let discover = app.ask(&wrap(
                json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{}}),
            ));
            assert!(discover.get("result").is_some(), "{discover}");
            app.send(&wrap(
                json!({"jsonrpc":"2.0","id":"tools-stream","method":"subscriptions/listen","params":{"notifications":{"toolsListChanged":true}}}),
            ));
            let tools = app.ask(&wrap(
                json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
            ));
            assert!(tools.get("result").is_some(), "{tools}");
        } else {
            app.open();
        }
        let refused = app.ask(&wrap(call(
            3,
            "read",
            json!({"ids": [id], "history": true}),
        )));
        assert_eq!(
            refused["result"]["isError"], true,
            "{on_2026_protocol}: {refused}"
        );
        assert_eq!(
            texts(&refused),
            [format!(
                "{} {NOTHING_READ}",
                keeps_no_history(&url, BEFORE_HISTORY)
            )],
            "{on_2026_protocol}"
        );
        let plain = app.ask(&wrap(call(
            4,
            "read",
            json!({"ids": [id], "history": false}),
        )));
        assert_ne!(
            plain["result"]["isError"], true,
            "{on_2026_protocol}: {plain}"
        );
        assert!(texts(&plain)[0].contains(&id), "{plain}");
        let dropped = app.ask(&wrap(call(
            5,
            "update",
            json!({"memory_id": id, "content": "Prefer small functions", "drop_history": true, "user_confirmed": true}),
        )));
        assert_ne!(
            dropped["result"]["isError"], true,
            "{on_2026_protocol}: {dropped}"
        );
        host.child.kill().unwrap();
        host.child.wait().unwrap();
        host.restart_as(Some(HISTORY_SINCE));
        let upgraded = app.ask(&wrap(call(
            6,
            "read",
            json!({"ids": [id], "history": true}),
        )));
        assert_ne!(
            upgraded["result"]["isError"], true,
            "{on_2026_protocol}: {upgraded}"
        );
        assert!(
            texts(&upgraded)[0].contains("\"history\""),
            "{on_2026_protocol}: {upgraded}"
        );
        app.close();
    }
}

/// An app's session with `serve` on a client of the server at `url`, which
/// keeps every message that is not the reply it waits for.
struct App {
    child: ChildGuard,
    input: Option<std::process::ChildStdin>,
    output: std::sync::mpsc::Receiver<String>,
    /// Messages that came while waiting for a reply.
    others: Vec<Value>,
}

impl App {
    /// `serve` as the app starts it, with `envs` set besides the server's.
    fn start(home: &Path, url: &str, token: &Path, envs: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .arg("serve")
            .sandboxed(home)
            .env_remove("USER_MEMORY_DB")
            .env("USER_MEMORY_URL", url)
            .env("USER_MEMORY_TOKEN_FILE", token)
            .envs(envs.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Self::over(command)
    }

    /// `serve` on a local store in `home`: what a server of this build says.
    fn local(home: &Path) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .arg("serve")
            .sandboxed(home)
            .env("USER_MEMORY_DB", home.join("local.sqlite3"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Self::over(command)
    }

    fn over(mut command: Command) -> Self {
        let mut child = ChildGuard(command.spawn().unwrap());
        let input = child.0.stdin.take();
        let output = lines(child.0.stdout.take().unwrap());
        Self {
            child,
            input,
            output,
            others: Vec::new(),
        }
    }

    fn send(&mut self, message: &Value) {
        let input = self.input.as_mut().unwrap();
        writeln!(input, "{message}").unwrap();
        input.flush().unwrap();
    }

    /// The reply to the request `message`, as the line the app reads.
    fn ask_line(&mut self, message: &Value) -> String {
        self.send(message);
        loop {
            let line = self
                .output
                .recv_timeout(STUCK)
                .expect("serve sent no reply");
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["id"] == message["id"] && value.get("method").is_none() {
                return line;
            }
            self.others.push(value);
        }
    }

    fn ask(&mut self, message: &Value) -> Value {
        serde_json::from_str(&self.ask_line(message)).unwrap()
    }

    /// The handshake and the tool list, as an app opens a session.
    fn open(&mut self) -> (Value, Value) {
        let handshake = self.ask(&initialize(1));
        self.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        let tools = self.ask(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}));
        (handshake, tools)
    }

    /// How many tool list changes the app was told of.
    fn told_changed(&self) -> usize {
        self.others
            .iter()
            .filter(|message| message["method"] == "notifications/tools/list_changed")
            .count()
    }

    /// Ends the session; serve's stderr and the lines it printed meanwhile.
    fn close(mut self) -> (String, Vec<String>) {
        drop(self.input.take());
        let status = self.child.0.wait().unwrap();
        assert!(status.success(), "serve failed: {status}");
        let mut said = String::new();
        if let Some(mut errors) = self.child.0.stderr.take() {
            errors.read_to_string(&mut said).unwrap();
        }
        let rest = self.output.iter().collect();
        (said, rest)
    }
}

/// The texts of a tool reply's content.
fn texts(reply: &Value) -> Vec<String> {
    reply["result"]["content"]
        .as_array()
        .unwrap_or_else(|| panic!("not a tool reply: {reply}"))
        .iter()
        .map(|item| item["text"].as_str().unwrap().to_owned())
        .collect()
}

/// What a client of `url` says for a call its server did not answer.
fn unanswered(url: &str) -> String {
    format!(
        "The memory server at {url} did not answer; the call may not have completed. Before repeating a write, search for it. Memory reconnects on the next call."
    )
}

/// The `_meta` every 2026-07-28 request carries: its protocol version and
/// the client, in place of a handshake.
fn meta_2026() -> Value {
    json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"test-device","version":"1"},"io.modelcontextprotocol/clientCapabilities":{}})
}

/// `message` as a 2026-07-28 request.
fn on_2026(mut message: Value) -> Value {
    message["params"]["_meta"] = meta_2026();
    message
}

/// The 2026-07-28 protocol has no handshake, and `server/discover` is
/// optional: a session that never sends it reconnects after the server
/// restarts, as one that did.
#[test]
fn a_2026_session_without_discover_reconnects_after_an_outage() {
    let mut host = Host::start(root());
    let home = tempdir().unwrap();
    let token = home.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    let url = format!("http://{}/mcp", host.address);
    let mut app = App::start(home.path(), &url, &token, &[]);
    let tools = app.ask(&on_2026(
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}),
    ));
    assert!(tools.get("result").is_some(), "{tools}");
    let before = app.ask(&on_2026(call(2, "index", json!({}))));
    assert_ne!(before["result"]["isError"], true, "{before}");
    host.child.kill().unwrap();
    host.child.wait().unwrap();
    let failed = app.ask(&on_2026(call(3, "index", json!({}))));
    assert_eq!(texts(&failed), [unanswered(&url)]);
    host.restart();
    let mut replies = Vec::new();
    for id in 4..7 {
        let reply = app.ask(&on_2026(call(id, "index", json!({}))));
        let served = reply["result"]["isError"] != true;
        replies.push(reply);
        if served {
            break;
        }
    }
    assert_ne!(
        replies.last().unwrap()["result"]["isError"],
        true,
        "never reconnected: {replies:?}"
    );
    app.close();
}

/// A 2026-07-28 app, as Claude Code opens its session: `server/discover`,
/// then a `subscriptions/listen` stream for tool changes. After the server
/// was away, a change in its tools is told once on that stream, tagged
/// with its id, as the front does after a hand-over; the same tools are
/// told nothing.
#[test]
fn a_2026_session_is_told_on_its_stream_when_a_reconnect_changes_the_tools() {
    // A debug build reporting a test version lists one more tool.
    for (upgraded, told) in [(None, 0), (Some("9.9.9"), 1)] {
        let mut host = Host::start(root());
        let home = tempdir().unwrap();
        let token = home.path().join("token");
        fs::write(&token, TOKEN).unwrap();
        let url = format!("http://{}/mcp", host.address);
        let mut app = App::start(home.path(), &url, &token, &[]);
        let discover = app.ask(&on_2026(
            json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{}}),
        ));
        assert!(discover.get("result").is_some(), "{discover}");
        app.send(&on_2026(
            json!({"jsonrpc":"2.0","id":"tools-stream","method":"subscriptions/listen","params":{"notifications":{"toolsListChanged":true}}}),
        ));
        let tools = app.ask(&on_2026(
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        ));
        assert!(tools.get("result").is_some(), "{tools}");
        let before = app.ask(&on_2026(call(3, "index", json!({}))));
        assert_ne!(before["result"]["isError"], true, "{before}");
        host.child.kill().unwrap();
        host.child.wait().unwrap();
        let failed = app.ask(&on_2026(call(4, "index", json!({}))));
        assert_eq!(texts(&failed), [unanswered(&url)]);
        host.restart_as(upgraded);
        let served = app.ask(&on_2026(call(5, "index", json!({}))));
        assert_ne!(served["result"]["isError"], true, "{served}");
        // Told before the reply to the call that reconnected.
        let changes: Vec<&Value> = app
            .others
            .iter()
            .filter(|message| message["method"] == "notifications/tools/list_changed")
            .collect();
        let tagged = json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed","params":{"_meta":{"io.modelcontextprotocol/subscriptionId":"tools-stream"}}});
        assert_eq!(
            changes,
            vec![&tagged; told],
            "upgraded {upgraded:?}: {:?}",
            app.others
        );
        app.close();
    }
}

/// #100's line after a reconnect reaches a 2026-07-28 session too, whose
/// server names itself in `server/discover`'s `_meta`: once, after the
/// reply's own content.
#[test]
fn a_risen_minimum_is_said_to_a_2026_session_after_a_reconnect() {
    let home = tempdir().unwrap();
    let token = home.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    let state = said("0.10.0", Some("0.8.0"));
    let url = versioned_endpoint(&state);
    let mut app = App::start(
        home.path(),
        &url,
        &token,
        &[("USER_MEMORY_TEST_VERSION", "0.10.0")],
    );
    let discover = app.ask(&on_2026(
        json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{}}),
    ));
    assert!(discover.get("result").is_some(), "{discover}");
    assert_eq!(
        texts(&app.ask(&on_2026(call(2, "index", json!({}))))),
        ["served"]
    );
    {
        let mut state = state.lock().unwrap();
        state.min_client = Some("0.11.0".into());
        state.fail_next = true;
    }
    let failed = app.ask(&on_2026(call(3, "index", json!({}))));
    assert_eq!(failed["result"]["isError"], true, "{failed}");
    assert_eq!(
        texts(&app.ask(&on_2026(call(4, "index", json!({}))))),
        [
            "served".to_owned(),
            format!(
                "The memory server at {url} runs user-memory 0.10.0 and needs 0.11.0 or newer on this machine (0.10.0). Upgrade: user-memory upgrade"
            )
        ]
    );
    assert_eq!(
        texts(&app.ask(&on_2026(call(5, "index", json!({}))))),
        ["served"]
    );
    app.close();
}

/// A server that comes back as another version with the same tools: the
/// app is told nothing, whichever protocol it speaks; its list is still
/// right.
#[test]
fn a_reconnect_to_the_same_tools_tells_nothing() {
    let home = tempdir().unwrap();
    let token = home.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    for on_2026_protocol in [false, true] {
        let ask = |message: Value| {
            if on_2026_protocol {
                on_2026(message)
            } else {
                message
            }
        };
        let state = said("0.10.0", None);
        let url = versioned_endpoint(&state);
        let mut app = App::start(home.path(), &url, &token, &[]);
        let opened = if on_2026_protocol {
            let discover = app.ask(&ask(
                json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{}}),
            ));
            app.send(&ask(
                json!({"jsonrpc":"2.0","id":"tools-stream","method":"subscriptions/listen","params":{"notifications":{"toolsListChanged":true}}}),
            ));
            discover
        } else {
            let handshake = app.ask(&initialize(1));
            app.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
            handshake
        };
        assert!(opened.get("result").is_some(), "{opened}");
        let tools = app.ask(&ask(
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        ));
        assert!(tools.get("result").is_some(), "{tools}");
        {
            let mut state = state.lock().unwrap();
            state.version = "0.10.1".into();
            state.fail_next = true;
        }
        let failed = app.ask(&ask(call(3, "index", json!({}))));
        assert_eq!(failed["result"]["isError"], true, "{failed}");
        assert_eq!(
            texts(&app.ask(&ask(call(4, "index", json!({}))))),
            ["served"]
        );
        app.ask(&ask(call(5, "index", json!({}))));
        assert!(
            app.others
                .iter()
                .all(|message| message["method"] != "notifications/tools/list_changed"),
            "2026 {on_2026_protocol}: {:?}",
            app.others
        );
        app.close();
    }
}

/// A change in the tools after a reconnect is told before the reply to the
/// call that reconnected, even when the server lists its tools slowly: an
/// app that ends the session after that reply has heard it.
#[test]
fn a_change_is_told_before_the_reply_to_the_call_that_reconnected() {
    let home = tempdir().unwrap();
    let token = home.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    let state = said("0.10.0", None);
    let url = versioned_endpoint(&state);
    let mut app = App::start(home.path(), &url, &token, &[]);
    let discover = app.ask(&on_2026(
        json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{}}),
    ));
    assert!(discover.get("result").is_some(), "{discover}");
    app.send(&on_2026(
        json!({"jsonrpc":"2.0","id":"tools-stream","method":"subscriptions/listen","params":{"notifications":{"toolsListChanged":true}}}),
    ));
    app.ask(&on_2026(
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    ));
    {
        let mut state = state.lock().unwrap();
        state.tools = Some(json!([{"name":"added","inputSchema":{"type":"object"}}]));
        state.slow_tools = true;
        state.fail_next = true;
    }
    let failed = app.ask(&on_2026(call(3, "index", json!({}))));
    assert_eq!(failed["result"]["isError"], true, "{failed}");
    assert_eq!(
        texts(&app.ask(&on_2026(call(4, "index", json!({}))))),
        ["served"]
    );
    assert_eq!(
        app.others
            .iter()
            .filter(|message| message["method"] == "notifications/tools/list_changed")
            .count(),
        1,
        "{:?}",
        app.others
    );
    app.close();
}

/// A session whose first handshake finds the server down gets this
/// build's own handshake and tools, the outage reply for a call, and the
/// server once it is up: the next call is served on the same session, and
/// the app is told once that the tools changed.
#[test]
fn a_session_started_in_an_outage_gets_its_tools_and_recovers() {
    let home = tempdir().unwrap();
    let token = home.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    let mut local = App::local(home.path());
    let (local_handshake, local_tools) = local.open();
    local.close();
    let state = said(env!("CARGO_PKG_VERSION"), None);
    let (url, start) = endpoint_down(&state);
    let mut app = App::start(home.path(), &url, &token, &[]);
    let (handshake, tools) = app.open();
    assert_eq!(handshake, local_handshake);
    assert_eq!(tools, local_tools);
    let failed = app.ask(&call(3, "index", json!({})));
    assert_eq!(failed["result"]["isError"], true, "{failed}");
    assert_eq!(texts(&failed), [unanswered(&url)]);
    assert_eq!(app.told_changed(), 0, "told before the server answered");
    start();
    assert_eq!(texts(&app.ask(&call(4, "index", json!({})))), ["served"]);
    assert_eq!(texts(&app.ask(&call(5, "index", json!({})))), ["served"]);
    // The fake server lists no tools, unlike this build.
    assert_eq!(app.told_changed(), 1, "{:?}", app.others);
    let (errors, rest) = app.close();
    assert!(!errors.contains(TOKEN) && !rest.concat().contains(TOKEN));
}

/// A server this build serves itself, down when the session starts:
/// nothing changed, so the app is told nothing.
#[test]
fn a_session_started_in_an_outage_of_its_own_build_is_told_no_change() {
    let mut host = Host::start(root());
    host.child.kill().unwrap();
    host.child.wait().unwrap();
    let home = tempdir().unwrap();
    let token = home.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    let url = format!("http://{}/mcp", host.address);
    let mut app = App::start(home.path(), &url, &token, &[]);
    let (handshake, tools) = app.open();
    assert!(handshake.get("result").is_some(), "{handshake}");
    assert!(tools.get("result").is_some(), "{tools}");
    host.restart();
    let served = app.ask(&call(3, "index", json!({})));
    assert_ne!(served["result"]["isError"], true, "{served}");
    app.ask(&call(4, "index", json!({})));
    assert_eq!(app.told_changed(), 0, "{:?}", app.others);
    app.close();
}

/// A refused token at the first handshake: the session opens, each call
/// gets the refused-token reply naming the file, and a replaced token
/// works at the next call. Neither token is ever printed.
#[test]
fn a_refused_token_at_the_first_handshake_recovers_with_a_new_token() {
    let home = tempdir().unwrap();
    let token = home.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    let replaced = "f".repeat(64);
    let state = said(env!("CARGO_PKG_VERSION"), None);
    state.lock().unwrap().token = Some(replaced.clone());
    let url = versioned_endpoint(&state);
    let mut app = App::start(home.path(), &url, &token, &[]);
    let (handshake, tools) = app.open();
    assert!(handshake.get("result").is_some(), "{handshake}");
    assert!(tools.get("result").is_some(), "{tools}");
    let refused = app.ask(&call(3, "index", json!({})));
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert_eq!(
        texts(&refused),
        [format!(
            "The memory server at {url} refused this machine's access token ({}); the call was not run. Tell the user the token in that file needs replacing (user-memory doctor shows it); don't open the file. Memory tries again on the next call.",
            token.display()
        )]
    );
    fs::write(&token, &replaced).unwrap();
    assert_eq!(texts(&app.ask(&call(4, "index", json!({})))), ["served"]);
    let (errors, rest) = app.close();
    for secret in [TOKEN, replaced.as_str()] {
        assert!(!errors.contains(secret), "a token is in serve's stderr");
        assert!(!rest.concat().contains(secret));
        assert!(
            !refused.to_string().contains(secret),
            "a token is in a reply"
        );
    }
}

/// However the server can't be reached, the handshake is answered and the
/// tools listed: a refused connection, a name that doesn't resolve, and a
/// connection that is accepted and never answered.
#[test]
fn an_unreachable_server_never_holds_the_first_handshake() {
    let home = tempdir().unwrap();
    let token = home.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    let refused = endpoint_down(&said("0.10.0", None)).0;
    // Listening, so a connection is made, but never accepted or answered.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let silent_url = format!("http://{}/mcp", silent.local_addr().unwrap());
    for url in [
        refused.as_str(),
        "https://memory.invalid/mcp",
        silent_url.as_str(),
    ] {
        let mut app = App::start(home.path(), url, &token, &[]);
        let (handshake, tools) = app.open();
        assert!(handshake.get("result").is_some(), "{url}: {handshake}");
        assert!(
            !tools["result"]["tools"].as_array().unwrap().is_empty(),
            "{url}: {tools}"
        );
        drop(app);
    }
    drop(silent);
}

/// A reachable server's handshake reply reaches the app as it did before a
/// bridge could answer a handshake itself, field for field and in order.
#[test]
fn a_reachable_servers_handshake_reaches_the_app_unchanged() {
    let home = tempdir().unwrap();
    let token = home.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    let state = said("0.10.0", None);
    state.lock().unwrap().handshake = Some(json!({
        "protocolVersion": "2025-06-18",
        "capabilities": {"tools": {"listChanged": true}, "experimental": {"user-memory": {"min_client": "0.8.0"}}},
        "serverInfo": {"name": "user-memory", "version": "0.10.0", "title": "Shared memory"},
        "instructions": "Fake instructions."
    }));
    let url = versioned_endpoint(&state);
    let mut app = App::start(home.path(), &url, &token, &[]);
    let line = app.ask_line(&initialize(1));
    assert_eq!(
        line,
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"experimental":{"user-memory":{"min_client":"0.8.0"}},"tools":{"listChanged":true}},"serverInfo":{"name":"user-memory","title":"Shared memory","version":"0.10.0"},"instructions":"Fake instructions."}}"#
    );
    app.close();
}

/// A server that answers the first handshake only after the bridge
/// answered it itself: its reply is the real handshake, which the app never
/// sees; the app is told the tools changed, and calls are served.
#[test]
fn a_late_handshake_reply_becomes_the_real_handshake() {
    let home = tempdir().unwrap();
    let token = home.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    let state = said(env!("CARGO_PKG_VERSION"), None);
    let gate = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    state.lock().unwrap().held = Some(gate.clone());
    let url = versioned_endpoint(&state);
    let mut app = App::start(home.path(), &url, &token, &[]);
    let (handshake, tools) = app.open();
    assert!(handshake.get("result").is_some(), "{handshake}");
    assert!(tools.get("result").is_some(), "{tools}");
    gate.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(texts(&app.ask(&call(3, "index", json!({})))), ["served"]);
    app.ask(&call(4, "index", json!({})));
    assert_eq!(app.told_changed(), 1, "{:?}", app.others);
    assert!(
        app.others.iter().all(|message| message["id"] != 1),
        "the server's handshake reached the app: {:?}",
        app.others
    );
    app.close();
}

/// #100's version lines come with the first call the server answers when
/// the session started without it: once, after the call's own content.
#[test]
fn the_version_lines_come_at_the_first_real_handshake() {
    let home = tempdir().unwrap();
    let token = home.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    for (version, min, line) in [
        (
            "0.7.0",
            None,
            "This machine runs user-memory 0.10.0, which needs the memory server at {url} to run 0.8.0 or newer (it runs 0.7.0). Upgrade the server on its machine.",
        ),
        (
            "0.11.0",
            Some("0.10.1"),
            "The memory server at {url} runs user-memory 0.11.0 and needs 0.10.1 or newer on this machine (0.10.0). Upgrade: user-memory upgrade",
        ),
    ] {
        let state = said(version, min);
        let (url, start) = endpoint_down(&state);
        let mut app = App::start(
            home.path(),
            &url,
            &token,
            &[("USER_MEMORY_TEST_VERSION", "0.10.0")],
        );
        let (handshake, _) = app.open();
        assert!(handshake.get("result").is_some(), "{handshake}");
        app.ask(&call(3, "index", json!({})));
        start();
        assert_eq!(
            texts(&app.ask(&call(4, "index", json!({})))),
            ["served".to_owned(), line.replace("{url}", &url)]
        );
        assert_eq!(texts(&app.ask(&call(5, "index", json!({})))), ["served"]);
        app.close();
    }
}

/// Claude Code's opening, `server/discover` and a listen stream, with the
/// server down: discover and the tools come from this build's own server,
/// a call gets the outage reply, and once the server is up the stream is
/// told once that the tools changed.
#[test]
fn a_2026_session_started_in_an_outage_gets_its_tools_and_recovers() {
    let home = tempdir().unwrap();
    let token = home.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    let discover = on_2026(json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{}}));
    let list = on_2026(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}));
    let mut local = App::local(home.path());
    let (local_discover, local_tools) = (local.ask(&discover), local.ask(&list));
    local.close();
    let state = said(env!("CARGO_PKG_VERSION"), None);
    let (url, start) = endpoint_down(&state);
    let mut app = App::start(home.path(), &url, &token, &[]);
    assert_eq!(app.ask(&discover), local_discover);
    app.send(&on_2026(
        json!({"jsonrpc":"2.0","id":"tools-stream","method":"subscriptions/listen","params":{"notifications":{"toolsListChanged":true}}}),
    ));
    assert_eq!(app.ask(&list), local_tools);
    let failed = app.ask(&on_2026(call(3, "index", json!({}))));
    assert_eq!(texts(&failed), [unanswered(&url)]);
    start();
    assert_eq!(
        texts(&app.ask(&on_2026(call(4, "index", json!({}))))),
        ["served"]
    );
    app.ask(&on_2026(call(5, "index", json!({}))));
    let changes: Vec<&Value> = app
        .others
        .iter()
        .filter(|message| message["method"] == "notifications/tools/list_changed")
        .collect();
    assert_eq!(
        changes,
        [
            &json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed","params":{"_meta":{"io.modelcontextprotocol/subscriptionId":"tools-stream"}}})
        ],
        "{:?}",
        app.others
    );
    app.close();
}

/// A server reached after the bridge answered the handshake itself that
/// agrees an older protocol than the app's: the session goes on, as it does
/// after any reconnect, and calls are served.
#[test]
fn a_server_on_an_older_protocol_still_serves_a_session_started_without_it() {
    let home = tempdir().unwrap();
    let token = home.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    let state = said(env!("CARGO_PKG_VERSION"), None);
    state.lock().unwrap().protocol = Some("2025-03-26".into());
    let (url, start) = endpoint_down(&state);
    let mut app = App::start(home.path(), &url, &token, &[]);
    let (handshake, _) = app.open();
    assert_eq!(handshake["result"]["protocolVersion"], "2025-06-18");
    app.ask(&call(3, "index", json!({})));
    start();
    assert_eq!(texts(&app.ask(&call(4, "index", json!({})))), ["served"]);
    app.close();
}

/// `connect` has no front: what the bridge tells `serve`'s front after a
/// re-handshake never reaches its app.
#[test]
fn connect_never_prints_what_the_bridge_tells_the_front() {
    let home = tempdir().unwrap();
    let token = home.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    let state = said("0.10.0", Some("0.8.0"));
    let url = versioned_endpoint(&state);
    let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
    command
        .args(["connect", "--url", &url, "--token-file"])
        .arg(&token)
        .sandboxed(home.path())
        .env_remove("USER_MEMORY_DB")
        .env("USER_MEMORY_TEST_VERSION", "0.10.0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut app = App::over(command);
    let (handshake, _) = app.open();
    assert!(handshake.get("result").is_some(), "{handshake}");
    assert_eq!(texts(&app.ask(&call(3, "index", json!({})))), ["served"]);
    {
        let mut state = state.lock().unwrap();
        state.min_client = Some("0.11.0".into());
        state.fail_next = true;
    }
    app.ask(&call(4, "index", json!({})));
    assert_eq!(texts(&app.ask(&call(5, "index", json!({})))).len(), 2);
    let others = std::mem::take(&mut app.others);
    let (_, rest) = app.close();
    let printed: Vec<Value> = others
        .into_iter()
        .chain(rest.iter().map(|line| serde_json::from_str(line).unwrap()))
        .collect();
    assert!(
        printed.iter().all(|message| !message["method"]
            .as_str()
            .unwrap_or_default()
            .starts_with("notifications/user-memory/")),
        "{printed:?}"
    );
}

/// Requests the stand-in still holds when a call hands the session to the
/// server are answered, however the app's concurrent requests fall.
#[test]
fn what_the_stand_in_holds_is_answered_when_the_server_takes_over() {
    let home = tempdir().unwrap();
    let token = home.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    let state = said(env!("CARGO_PKG_VERSION"), None);
    let (url, start) = endpoint_down(&state);
    let mut app = App::start(home.path(), &url, &token, &[]);
    app.open();
    start();
    // Many tool lists and then the call that reaches the server, in one
    // write: some lists are still with the stand-in when the call comes.
    let lists: Vec<Value> = (100..120)
        .map(|id| json!({"jsonrpc":"2.0","id":id,"method":"tools/list","params":{}}))
        .collect();
    let batch: String = lists
        .iter()
        .chain([&call(3, "index", json!({}))])
        .map(|message| format!("{message}\n"))
        .collect();
    let input = app.input.as_mut().unwrap();
    input.write_all(batch.as_bytes()).unwrap();
    input.flush().unwrap();
    let mut answered = std::collections::HashSet::new();
    while answered.len() < lists.len() + 1 {
        let line = app
            .output
            .recv_timeout(STUCK)
            .unwrap_or_else(|_| panic!("unanswered: {:?} of 100..120 and 3", answered));
        let value: Value = serde_json::from_str(&line).unwrap();
        if value.get("method").is_none() {
            answered.insert(value["id"].as_i64().unwrap());
        }
    }
    app.close();
}

/// A server that takes longer for each handshake than a reconnect waits,
/// but less than the app would have waited at session start, still serves
/// the session's first call.
#[test]
fn a_server_slow_to_shake_hands_serves_the_first_call() {
    let home = tempdir().unwrap();
    let token = home.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    let state = said(env!("CARGO_PKG_VERSION"), None);
    // Past the 10 seconds a reconnect waits; within the apps' 30.
    state.lock().unwrap().handshake_time = Some(std::time::Duration::from_secs(12));
    let url = versioned_endpoint(&state);
    let mut app = App::start(home.path(), &url, &token, &[]);
    let (handshake, _) = app.open();
    assert!(handshake.get("result").is_some(), "{handshake}");
    assert_eq!(texts(&app.ask(&call(3, "index", json!({})))), ["served"]);
    app.close();
}

/// A reply to a later page of the tool list is not the list the app has:
/// a reconnect to the same tools tells nothing.
#[test]
fn a_later_page_is_not_taken_for_the_tool_list() {
    let home = tempdir().unwrap();
    let token = home.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    let state = said("0.10.0", None);
    state.lock().unwrap().tools = Some(json!([{"name":"index","inputSchema":{"type":"object"}}]));
    let url = versioned_endpoint(&state);
    let mut app = App::start(home.path(), &url, &token, &[]);
    app.open();
    let page =
        app.ask(&json!({"jsonrpc":"2.0","id":3,"method":"tools/list","params":{"cursor":"2"}}));
    assert_eq!(page["result"]["tools"], json!([]), "{page}");
    state.lock().unwrap().fail_next = true;
    app.ask(&call(4, "index", json!({})));
    assert_eq!(texts(&app.ask(&call(5, "index", json!({})))), ["served"]);
    assert_eq!(app.told_changed(), 0, "{:?}", app.others);
    app.close();
}

// A session that started while its server was down gets the index at the
// first prompt the server answers, once.
#[test]
fn a_session_started_while_its_server_was_down_gets_the_index_when_it_is_back() {
    let mut host = Host::start(root());
    host.rpc(initialize(1));
    host.rpc(call(
        2,
        "record",
        json!({"category":"workflow","context":"Reviewing","title":"Prefer small pull requests","content":"Prefer small pull requests","reason":"Easier review","scope":"global","user_confirmed":true}),
    ));
    host.child.kill().unwrap();
    host.child.wait().unwrap();
    let home = tempdir().unwrap();
    let url = format!("http://{}/mcp", host.address);
    let token = host.root.path().join("token");
    let hook = |subcommand: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_user-memory"))
            .args([subcommand, "--client", "claude"])
            .current_dir(home.path())
            .sandboxed(home.path())
            .env("USER_MEMORY_URL", &url)
            .env("USER_MEMORY_TOKEN_FILE", &token)
            .env_remove("USER_MEMORY_DB")
            .env(time::HOOK_ROOM.0, time::HOOK_ROOM.1)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let payload = json!({"session_id":"s","cwd":home.path(),"hook_event_name":subcommand});
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = time::finish(child, "the hook");
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    let start = hook("session-start");
    assert!(start.contains("User memory unavailable"), "{start}");
    host.restart();
    let back = hook("prompt-submit");
    assert!(
        back.contains("User memory is reachable again.")
            && back.contains("Prefer small pull requests"),
        "{back}"
    );
    assert_eq!(hook("prompt-submit"), "", "told once");
}

/// `REMOTE_MARKER_POLL` in `src/remote/marker.rs`.
const MARKER_POLL: std::time::Duration = std::time::Duration::from_secs(5);

/// A TCP relay to a host that counts the connections it takes and keeps
/// what clients send, so a test sees from the server's side whether a
/// client asked it anything, and what, and when each request came in and
/// its reply went out.
struct Counting {
    address: String,
    taken: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    sent: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    requests: std::sync::Arc<std::sync::Mutex<Vec<Request>>>,
    broken: std::sync::Arc<std::sync::Mutex<Option<Break>>>,
    holds_streams: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// One HTTP request a relay passed on.
#[derive(Clone, Debug)]
struct Request {
    /// When its first bytes came in.
    began: std::time::Instant,
    /// Its request line, headers and the start of its body.
    head: String,
    /// When its reply began to go out; none while it is on the way.
    answered: Option<std::time::Instant>,
}

impl Request {
    fn method(&self) -> &str {
        self.head.split(' ').next().unwrap_or_default()
    }
    fn session(&self) -> Option<String> {
        header(&self.head, "mcp-session-id")
    }
    /// Whether it is a marker poll: the connector's call for the stamps
    /// since a position, which asks nothing else; a hook's index call asks
    /// for stamps too, beside its projects.
    fn polls(&self) -> bool {
        self.head
            .contains("\"arguments\":{\"stamps\":true,\"stamps_since\"")
    }
    fn initializes(&self) -> bool {
        self.head.contains("\"method\":\"initialize\"")
    }
    /// Whether it is a request to the MCP endpoint: not a delivery file's
    /// or the token's own route.
    fn mcp(&self) -> bool {
        self.head.split(' ').nth(1) == Some("/mcp")
    }
}

/// One marker poll as the server's side sees it: when its call came in,
/// just after the connector took the claim, and when its reply went out,
/// just before it writes the marker; none while it is on the way.
#[derive(Clone, Debug)]
struct PollSpan {
    began: std::time::Instant,
    answered: Option<std::time::Instant>,
}

/// How a relay breaks the next POST in a session, so the server refuses
/// it: as a session it no longer knows (one a restart or an idle timeout
/// ended), or as a token it does not take.
#[derive(Clone, Copy, Debug)]
enum Break {
    Session,
    Token,
}

/// The bytes of a request a relay keeps to tell what it was: its headers
/// and the start of its JSON-RPC body.
const REQUEST_KEPT: usize = 4096;

/// The value of the HTTP header `name` in `text`, matched without case.
fn header(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

/// `chunk`, the start of a request, with the value of the header `name`
/// replaced by as many `fill` bytes, so its length and the body's stay.
fn blot(chunk: &mut [u8], name: &str, fill: u8) {
    let text = String::from_utf8_lossy(chunk).into_owned();
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        if let Some((key, value)) = line.split_once(':')
            && key.trim().eq_ignore_ascii_case(name)
        {
            let start = at + key.len() + 1 + (value.len() - value.trim_start().len());
            let len = value.trim().len();
            chunk[start..start + len].fill(fill);
            return;
        }
        at += line.len();
    }
}

/// What a relay knows of the request a connection is on.
#[derive(Default)]
struct OnConnection {
    /// The request's place in the relay's list.
    request: Option<usize>,
    /// Whether its reply began to go out: the next bytes from the client
    /// are a new request.
    replied: bool,
    /// Whether its reply is held back.
    slow: bool,
}

impl Counting {
    fn to(host: &str) -> Self {
        Self::slowed(host, std::time::Duration::ZERO)
    }
    /// A relay that holds each request's reply back by `delay` before
    /// passing it on, as a slow server or network does, on a connection
    /// kept for many requests too.
    fn slowed(host: &str, delay: std::time::Duration) -> Self {
        Self::slowed_for(host, delay, b"")
    }
    /// [`Counting::slowed`], for the requests that start with `request`
    /// only.
    fn slowed_for(host: &str, delay: std::time::Duration, request: &'static [u8]) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let taken = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = taken.clone();
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let kept = sent.clone();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let listed = requests.clone();
        let broken = std::sync::Arc::new(std::sync::Mutex::new(None));
        let breaks = broken.clone();
        let holds_streams = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let holds = holds_streams.clone();
        let host = host.to_owned();
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { break };
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let Ok(server) = TcpStream::connect(&host) else {
                    continue;
                };
                let on = std::sync::Arc::new(std::sync::Mutex::new(OnConnection::default()));
                for (mut from, mut to, asks) in [
                    (
                        client.try_clone().unwrap(),
                        server.try_clone().unwrap(),
                        true,
                    ),
                    (server, client, false),
                ] {
                    let on = on.clone();
                    let (kept, listed, breaks) = (kept.clone(), listed.clone(), breaks.clone());
                    let holds = holds.clone();
                    std::thread::spawn(move || {
                        let mut buffer = [0; 8192];
                        while let Ok(n) = from.read(&mut buffer)
                            && n > 0
                        {
                            if asks && holds.load(std::sync::atomic::Ordering::SeqCst) {
                                let fresh = {
                                    let on = on.lock().unwrap();
                                    on.request.is_none() || on.replied
                                };
                                let text = String::from_utf8_lossy(&buffer[..n]).into_owned();
                                if fresh
                                    && text.starts_with("GET ")
                                    && let Some(session) = header(&text, "mcp-session-id")
                                {
                                    hold_until_polled(&listed, &session);
                                }
                            }
                            let chunk = &mut buffer[..n];
                            if asks {
                                let mut on = on.lock().unwrap();
                                let mut listed = listed.lock().unwrap();
                                if on.request.is_none() || on.replied {
                                    let text = String::from_utf8_lossy(chunk).into_owned();
                                    if text.starts_with("POST ")
                                        && header(&text, "mcp-session-id").is_some()
                                    {
                                        match breaks.lock().unwrap().take() {
                                            Some(Break::Session) => {
                                                blot(chunk, "mcp-session-id", b'0');
                                            }
                                            Some(Break::Token) => {
                                                blot(chunk, "authorization", b'x');
                                            }
                                            None => {}
                                        }
                                    }
                                    let size = chunk.len().min(REQUEST_KEPT);
                                    listed.push(Request {
                                        began: std::time::Instant::now(),
                                        head: String::from_utf8_lossy(&chunk[..size]).into_owned(),
                                        answered: None,
                                    });
                                    *on = OnConnection {
                                        request: Some(listed.len() - 1),
                                        replied: false,
                                        slow: chunk.starts_with(request),
                                    };
                                } else if let Some(at) = on.request {
                                    let head = &mut listed[at].head;
                                    let room = REQUEST_KEPT.saturating_sub(head.len());
                                    head.push_str(&String::from_utf8_lossy(&chunk[..n.min(room)]));
                                }
                                kept.lock().unwrap().extend_from_slice(chunk);
                            } else {
                                let first = {
                                    let on = on.lock().unwrap();
                                    (!on.replied).then_some(on.slow)
                                };
                                if first == Some(true) {
                                    std::thread::sleep(delay);
                                }
                                if first.is_some() {
                                    let mut on = on.lock().unwrap();
                                    on.replied = true;
                                    if let Some(at) = on.request {
                                        listed.lock().unwrap()[at].answered =
                                            Some(std::time::Instant::now());
                                    }
                                }
                            }
                            if to.write_all(chunk).is_err() {
                                break;
                            }
                        }
                        let _ = to.shutdown(std::net::Shutdown::Write);
                    });
                }
            }
        });
        Self {
            address,
            taken,
            sent,
            requests,
            broken,
            holds_streams,
        }
    }
    fn taken(&self) -> usize {
        self.taken.load(std::sync::atomic::Ordering::SeqCst)
    }
    /// What clients sent so far, as text.
    fn sent(&self) -> String {
        String::from_utf8_lossy(&self.sent.lock().unwrap()).into_owned()
    }
    /// The requests so far, in the order they came in.
    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
    /// The marker polls so far, in the order they began.
    fn poll_spans(&self) -> Vec<PollSpan> {
        self.requests()
            .into_iter()
            .filter(Request::polls)
            .map(|request| PollSpan {
                began: request.began,
                answered: request.answered,
            })
            .collect()
    }
    /// Has the server refuse the next POST in a session, as `how` says.
    fn break_next(&self, how: Break) {
        *self.broken.lock().unwrap() = Some(how);
    }
    /// Holds each session's GET stream back until a poll in that session
    /// has been answered, so the stream opens after the calls rather than
    /// before them: an order the transport allows, as it opens the stream
    /// on a task of its own.
    fn hold_streams(&self) {
        self.holds_streams
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Waits until a marker poll in `session` among `listed` has been
/// answered, or [`STUCK`] has passed.
fn hold_until_polled(listed: &std::sync::Mutex<Vec<Request>>, session: &str) {
    let polled =
        || {
            listed.lock().unwrap().iter().any(|r| {
                r.polls() && r.session().as_deref() == Some(session) && r.answered.is_some()
            })
        };
    let deadline = std::time::Instant::now() + STUCK;
    while !polled() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// The hooks and the connector of one machine, a client of the server at
/// `url`, with `env` added.
struct Machine {
    home: TempDir,
    url: String,
    token: std::path::PathBuf,
    env: Vec<(&'static str, String)>,
}

impl Machine {
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .args(args)
            .current_dir(self.home.path())
            .sandboxed(self.home.path())
            .env("USER_MEMORY_URL", &self.url)
            .env("USER_MEMORY_TOKEN_FILE", &self.token)
            .env_remove("USER_MEMORY_DB")
            .env(time::HOOK_ROOM.0, time::HOOK_ROOM.1)
            .envs(self.env.iter().map(|(k, v)| (*k, v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
    /// The connector a client session runs, polling the server.
    fn connector(&self) -> ChildGuard {
        ChildGuard(
            self.command(&["connect", "--url", &self.url, "--token-file"])
                .arg(&self.token)
                .spawn()
                .unwrap(),
        )
    }
    /// A hook's stdout and stderr for session `s`.
    fn hook(&self, subcommand: &str) -> (String, String) {
        let mut child = self
            .command(&[subcommand, "--client", "claude"])
            .spawn()
            .unwrap();
        let payload = json!({"session_id":"s","cwd":self.home.path(),"hook_event_name":subcommand});
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = time::finish(child, "the hook");
        assert!(out.status.success(), "{out:?}");
        (
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap(),
        )
    }
    /// The stdout of hook `subcommand` for session `session`, its payload
    /// `extra` added to the session's.
    fn hook_with(&self, subcommand: &str, session: &str, extra: Value) -> String {
        let mut child = self
            .command(&[subcommand, "--client", "claude"])
            .spawn()
            .unwrap();
        let mut payload =
            json!({"session_id":session,"cwd":self.home.path(),"hook_event_name":subcommand});
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = time::finish(child, "the hook");
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    }
    /// The server's marker, once the connector wrote it.
    fn marker(&self) -> std::path::PathBuf {
        let mut found = None;
        time::until("the connector wrote a marker", || {
            found = find(self.home.path(), "changed");
            found.is_some()
        });
        found.unwrap()
    }
}

/// The file with extension `extension` in a `servers` folder under `dir`.
fn find(dir: &Path, extension: &str) -> Option<std::path::PathBuf> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find(&path, extension) {
                return Some(found);
            }
        } else if path.parent().is_some_and(|p| p.ends_with("servers"))
            && path.extension().is_some_and(|e| e == extension)
        {
            return Some(path);
        }
    }
    None
}

fn modified(path: &Path) -> std::time::SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

fn record_global(host: &Host, id: u64, title: &str) {
    host.rpc(initialize(id));
    host.rpc(call(
        id + 1,
        "record",
        json!({"category":"workflow","context":"Working","title":title,"content":title,"reason":"Because","scope":"global","user_confirmed":true}),
    ));
}

// The connector keeps the server's position in a marker on this machine: a
// change reaches it within a poll, and a prompt whose snapshot holds that
// position asks the server nothing, as the requests reaching the server
// show.
#[test]
fn a_servers_marker_takes_a_change_within_a_poll_and_spares_the_server_a_quiet_prompt() {
    let host = Host::start(root());
    let proxy = Counting::to(&host.address);
    let machine = Machine {
        home: tempdir().unwrap(),
        url: format!("http://{}/mcp", proxy.address),
        token: host.root.path().join("token"),
        env: Vec::new(),
    };
    let connector = machine.connector();
    machine.hook("session-start");
    let marker = machine.marker();
    let before = fs::read(&marker).unwrap();
    record_global(&host, 1, "Prefer small pull requests");
    let recorded = std::time::Instant::now();
    time::until("the marker moved", || fs::read(&marker).unwrap() != before);
    let took = recorded.elapsed();
    assert!(
        took <= MARKER_POLL + std::time::Duration::from_secs(1),
        "{took:?}"
    );
    let (told, _) = machine.hook("prompt-submit");
    assert!(told.contains("Prefer small pull requests"), "{told}");
    // The marker stays good for three polls after its connector stops,
    // which leaves the requests the server sees to the hook alone.
    drop(connector);
    let asked = proxy.taken();
    let (quiet, log) = machine.hook("prompt-submit");
    assert_eq!(quiet, "", "{log}");
    assert!(log.contains("none: the marker is unchanged"), "{log}");
    assert_eq!(proxy.taken(), asked, "a quiet prompt asked the server");
}

// After a quiet spell the connector polls once a quiet interval; a hook
// then finds the marker stale, asks the server itself, and brings the
// connector back to a poll's interval. With the connector stopped, the next
// prompt asks the server and tells the change.
#[test]
fn a_hook_after_a_quiet_spell_asks_the_server_and_speeds_the_connector_up() {
    // 300 ms polls: quiet after 7.2 s, a quiet poll every 3.6 s, a marker
    // stale after 0.9 s.
    let poll = std::time::Duration::from_millis(300);
    let host = Host::start(root());
    let proxy = Counting::to(&host.address);
    let machine = Machine {
        home: tempdir().unwrap(),
        url: format!("http://{}/mcp", proxy.address),
        token: host.root.path().join("token"),
        env: vec![("USER_MEMORY_MARKER_POLL_MS", poll.as_millis().to_string())],
    };
    let connector = machine.connector();
    machine.hook("session-start");
    let marker = machine.marker();
    // Quiet: two writes of the marker a quiet interval apart.
    let mut last = modified(&marker);
    time::until("the connector slowed down", || {
        let now = modified(&marker);
        let gap = now.duration_since(last).unwrap_or_default();
        last = now;
        gap >= poll * 10
    });
    time::until("the marker went stale", || {
        modified(&marker).elapsed().unwrap_or_default() > poll * 5
    });
    record_global(&host, 1, "Prefer small pull requests");
    let asked = proxy.taken();
    let (told, _) = machine.hook("prompt-submit");
    assert!(told.contains("Prefer small pull requests"), "{told}");
    assert!(proxy.taken() > asked, "the hook asked the server itself");
    // Back to a poll's interval: two writes less than a quiet poll apart.
    let mut last = modified(&marker);
    let mut gaps = Vec::new();
    time::until("the connector sped up", || {
        let now = modified(&marker);
        if now != last {
            gaps.push(now.duration_since(last).unwrap_or_default());
            last = now;
        }
        gaps.len() >= 2
    });
    assert!(gaps[1] < poll * 6, "{gaps:?}");
    // Stopped: the marker ages out and the next prompt asks the server.
    drop(connector);
    time::until("the marker went stale", || {
        modified(&marker).elapsed().unwrap_or_default() > poll * 5
    });
    record_global(&host, 3, "Prefer rebase merges");
    let (told, _) = machine.hook("prompt-submit");
    assert!(told.contains("Prefer rebase merges"), "{told}");
}

// Many sessions on one machine, each with its connector, poll the server
// about as often as one: a connector finds the marker fresh after
// another's poll and skips its own.
#[test]
fn connectors_on_one_machine_share_their_polls() {
    let poll = std::time::Duration::from_millis(500);
    let host = Host::start(root());
    let proxy = Counting::to(&host.address);
    let machine = Machine {
        home: tempdir().unwrap(),
        url: format!("http://{}/mcp", proxy.address),
        token: host.root.path().join("token"),
        env: vec![("USER_MEMORY_MARKER_POLL_MS", poll.as_millis().to_string())],
    };
    machine.hook("session-start");
    // Bounded by the interval, not by one connector's measured rate: late
    // ticks under load only slow the polls down.
    let window = poll * 8;
    let five = polls(&machine, &proxy, 5, poll, window);
    assert!(five > 0);
    assert!(
        five <= window.as_millis() / poll.as_millis() + POLL_SLACK,
        "five connectors in {window:?}: {five}"
    );
}

// A poll still waiting for a slow server's answer holds the claim, however
// long past the stale age: the other connectors wait for it instead of each
// asking the server again, and no poll starts before the one under way is
// answered.
#[test]
fn connectors_wait_for_a_poll_a_slow_server_has_not_answered() {
    let poll = std::time::Duration::from_millis(500);
    // Every reply takes `slow`, past the three polls after which a claim
    // whose connector stopped renewing it counts as gone, even for a poll
    // in a session its connector kept.
    let slow = poll * 4;
    let host = Host::start(root());
    let proxy = Counting::slowed(&host.address, slow);
    let machine = Machine {
        home: tempdir().unwrap(),
        url: format!("http://{}/mcp", proxy.address),
        token: host.root.path().join("token"),
        env: vec![("USER_MEMORY_MARKER_POLL_MS", poll.as_millis().to_string())],
    };
    machine.hook("session-start");
    // One poll at a time: each waits for its answer, one request in the
    // session its connector kept, then a poll's interval. The connectors
    // take the claim in turn, and one's first poll first opens its session,
    // three replies long: the window is long enough to hold a poll's call
    // even then.
    let window = poll * 24;
    let five = polls(&machine, &proxy, 5, poll, window);
    let spans = proxy.poll_spans();
    let first = spans.first().map(|span| span.began);
    let told = || {
        spans
            .iter()
            .map(|span| {
                let at = |time: std::time::Instant| {
                    time.duration_since(first.unwrap_or(time)).as_millis()
                };
                format!(
                    "{}..{}",
                    at(span.began),
                    span.answered
                        .map_or("?".to_owned(), |end| at(end).to_string())
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    assert!(spans.len() > 1, "polls (ms): {}", told());
    for pair in spans.windows(2) {
        assert!(
            pair[0]
                .answered
                .is_some_and(|answered| answered <= pair[1].began),
            "a poll began while another was on the way (ms): {}",
            told()
        );
    }
    assert!(five > 0);
    assert!(
        five <= window.as_millis() / (slow + poll).as_millis() + POLL_SLACK,
        "five connectors in {window:?}, answers taking {slow:?}: {five}"
    );
}

/// Waits until `proxy` has seen `polls` marker polls in all; the MCP
/// requests from the first of them on.
fn polled(proxy: &Counting, polls: usize) -> Vec<Request> {
    time::until("the polls", || {
        proxy.requests().iter().filter(|r| r.polls()).count() >= polls
    });
    let requests: Vec<Request> = proxy.requests().into_iter().filter(Request::mcp).collect();
    let first = requests.iter().position(Request::polls).unwrap();
    requests[first..].to_vec()
}

/// The calls among `requests`: the POSTs, without each session's GET
/// stream, which the transport opens on a task of its own, so it comes at
/// no fixed point among them.
fn calls(requests: &[Request]) -> Vec<Request> {
    requests
        .iter()
        .filter(|r| r.method() == "POST")
        .cloned()
        .collect()
}

/// How many GET streams `proxy` saw opened in `session`.
fn streams(proxy: &Counting, session: Option<&str>) -> usize {
    proxy
        .requests()
        .iter()
        .filter(|r| r.mcp() && r.method() == "GET" && r.session().as_deref() == session)
        .count()
}

/// What `requests` were, one per line, for a failure's message.
fn told(requests: &[Request]) -> String {
    requests
        .iter()
        .map(|r| {
            let what = if r.polls() {
                "poll"
            } else if r.initializes() {
                "initialize"
            } else {
                ""
            };
            let called = r
                .head
                .split("\"name\":\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .or_else(|| {
                    r.head
                        .split("\"method\":\"")
                        .nth(1)
                        .and_then(|rest| rest.split('"').next())
                })
                .unwrap_or_default();
            format!("{} {what} {called} session={:?}", r.method(), r.session())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// A connector keeps the session its poll was answered in, so each later
// poll is one request; a session the server no longer knows is made again
// within the poll that met it, with no poll lost.
#[test]
fn a_connector_keeps_its_session_and_makes_a_dropped_one_again() {
    keeps_its_session(false);
}

// The same when each session's stream opens only after its first poll is
// answered: the transport opens the stream on a task of its own, so it
// comes at no fixed point among the calls.
#[test]
fn a_kept_session_s_stream_may_open_after_its_polls() {
    keeps_its_session(true);
}

/// The kept session and the one made again, with each session's stream
/// `held` until its first poll is answered, or as it comes.
fn keeps_its_session(held: bool) {
    let poll = std::time::Duration::from_millis(300);
    let host = Host::start(root());
    let proxy = Counting::to(&host.address);
    if held {
        proxy.hold_streams();
    }
    let machine = Machine {
        home: tempdir().unwrap(),
        url: format!("http://{}/mcp", proxy.address),
        token: host.root.path().join("token"),
        env: vec![("USER_MEMORY_MARKER_POLL_MS", poll.as_millis().to_string())],
    };
    machine.hook("session-start");
    let _connector = machine.connector();
    let kept = calls(&polled(&proxy, 4));
    let session = kept[0].session();
    assert!(
        kept.iter().all(|r| r.polls() && r.session() == session),
        "polls after the first in its session, one request each:\n{}",
        told(&kept)
    );
    // The server takes the next call as one in a session it no longer
    // knows, as after a restart.
    let before = proxy.requests().len();
    proxy.break_next(Break::Session);
    time::until("three polls after the break", || {
        proxy.requests()[before..]
            .iter()
            .filter(|r| r.polls())
            .count()
            >= 3
    });
    let sent: Vec<Request> = proxy.requests()[before..]
        .iter()
        .filter(|r| r.mcp())
        .cloned()
        .collect();
    assert!(
        !sent.iter().any(|r| r.method() == "DELETE"),
        "the dropped session is not ended again:\n{}",
        told(&sent)
    );
    let after = calls(&sent);
    let refused = after.iter().position(Request::polls).unwrap();
    let again = refused
        + 1
        + after[refused + 1..]
            .iter()
            .position(Request::polls)
            .unwrap();
    let made = &after[refused + 1..again];
    assert!(
        made.iter().any(Request::initializes),
        "the call made again in a new session within the same poll:\n{}",
        told(&sent)
    );
    let new = after[again].session();
    assert_ne!(new, session);
    assert!(
        after[again..]
            .iter()
            .all(|r| r.polls() && r.session() == new),
        "then one request a poll in the new session:\n{}",
        told(&sent)
    );
    // Each session has one stream, whenever it opened: the new session
    // its own, and the dropped one's not opened again.
    time::until("the new session's stream", || {
        streams(&proxy, new.as_deref()) > 0
    });
    let all: Vec<Request> = proxy.requests().into_iter().filter(Request::mcp).collect();
    assert_eq!(
        (
            streams(&proxy, session.as_deref()),
            streams(&proxy, new.as_deref())
        ),
        (1, 1),
        "one stream in each session:\n{}",
        told(&all)
    );
}

// A poll the server refuses ends its session, and the next poll opens a
// new one rather than asking again in the refused one.
#[test]
fn a_refused_poll_starts_a_new_session() {
    let poll = std::time::Duration::from_millis(300);
    let host = Host::start(root());
    let proxy = Counting::to(&host.address);
    let machine = Machine {
        home: tempdir().unwrap(),
        url: format!("http://{}/mcp", proxy.address),
        token: host.root.path().join("token"),
        env: vec![("USER_MEMORY_MARKER_POLL_MS", poll.as_millis().to_string())],
    };
    machine.hook("session-start");
    let mut connector = machine.connector();
    time::until("two polls", || {
        proxy.requests().iter().filter(|r| r.polls()).count() >= 2
    });
    let kept = files_under(machine.home.path());
    let before = proxy.requests().len();
    proxy.break_next(Break::Token);
    time::until("two polls after the refusal", || {
        proxy.requests()[before..]
            .iter()
            .filter(|r| r.polls())
            .count()
            >= 2
    });
    let after: Vec<Request> = proxy.requests()[before..]
        .iter()
        .filter(|r| r.mcp())
        .cloned()
        .collect();
    let refused = after.iter().position(Request::polls).unwrap();
    let next = refused
        + 1
        + after[refused + 1..]
            .iter()
            .position(Request::polls)
            .unwrap();
    // Beside the new session, the refusal changes nothing: the connector
    // keeps no new file, says nothing, and a prompt is told nothing.
    assert_eq!(files_under(machine.home.path()), kept);
    let (quiet, log) = machine.hook("prompt-submit");
    assert_eq!(quiet, "", "{log}");
    drop(connector.0.stdin.take());
    let mut said = String::new();
    connector
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut said)
        .unwrap();
    assert_eq!(said, "");
    assert!(
        after[refused + 1..next].iter().any(Request::initializes)
            && after[next].session() != after[refused].session(),
        "a new session after the refusal:\n{}",
        told(&after)
    );
}

/// The files under `dir`, by their paths within it, sorted; a file being
/// written in place of another (`<name>.<pid>`) is left out, as it is
/// there only for a moment.
fn files_under(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(at) = dirs.pop() {
        for entry in fs::read_dir(&at).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if let Ok(inner) = path.strip_prefix(dir)
                && !path
                    .extension()
                    .is_some_and(|e| e.to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
            {
                found.push(inner.to_string_lossy().into_owned());
            }
        }
    }
    found.sort();
    found
}

// A connector whose session ends stops as soon with a kept session on a
// server slow to end it as on one that ends it at once: it does not wait
// for the session's end.
#[test]
fn a_stopping_connector_does_not_wait_for_its_kept_session_to_end() {
    let poll = std::time::Duration::from_millis(300);
    let host = Host::start(root());
    // How long it takes a connector with a kept session to exit once its
    // input closes, through `proxy`; the fastest of a few, as the machine
    // can be busy.
    let exit = |proxy: &Counting| {
        (0..STOP_RUNS)
            .map(|_| {
                let machine = Machine {
                    home: tempdir().unwrap(),
                    url: format!("http://{}/mcp", proxy.address),
                    token: host.root.path().join("token"),
                    env: vec![("USER_MEMORY_MARKER_POLL_MS", poll.as_millis().to_string())],
                };
                let before = proxy.requests().iter().filter(|r| r.polls()).count();
                let mut connector = machine.connector();
                time::until("a poll kept its session", || {
                    proxy.requests().iter().filter(|r| r.polls()).count() >= before + 2
                });
                drop(connector.0.stdin.take());
                let stopping = std::time::Instant::now();
                assert!(connector.0.wait().unwrap().success());
                stopping.elapsed()
            })
            .min()
            .unwrap()
    };
    let at_once = exit(&Counting::to(&host.address));
    // Only the session's end is slow, as on a server that answers late.
    let late = exit(&Counting::slowed_for(
        &host.address,
        std::time::Duration::from_secs(5),
        b"DELETE",
    ));
    assert!(
        late < at_once + STOP_ROOM,
        "{late:?} with a slow end against {at_once:?}"
    );
}

/// Connectors [`a_stopping_connector_does_not_wait_for_its_kept_session_to_end`]
/// stops each way, keeping the fastest, so one slowed by a busy machine
/// does not decide.
const STOP_RUNS: usize = 3;

/// How much later a connector may stop with a session slow to end: half the
/// connector's own bound on ending it (`POLLER_STOP_WAIT` in
/// src/remote/marker.rs, 500 ms), so a connector that waited out that bound
/// fails.
const STOP_ROOM: std::time::Duration = std::time::Duration::from_millis(250);

/// Polls that may start beyond the window's share: one under way as the
/// window opens, and one starting as it closes.
const POLL_SLACK: u128 = 2;

/// The polls `connectors` connectors on `machine` send through `proxy` in
/// `window`, once they are running: a poll is a request for stamps since a
/// position, and may take more than one connection.
fn polls(
    machine: &Machine,
    proxy: &Counting,
    connectors: usize,
    poll: std::time::Duration,
    window: std::time::Duration,
) -> u128 {
    let running: Vec<ChildGuard> = (0..connectors).map(|_| machine.connector()).collect();
    // The machine stays active, as while its sessions run hooks: else the
    // connectors slow to the quiet interval once the session-start hook is
    // old, which a slow server makes it soon.
    let wanted = find(machine.home.path(), "wanted").unwrap();
    let active = || {
        let file = fs::OpenOptions::new().write(true).open(&wanted).unwrap();
        file.set_modified(std::time::SystemTime::now()).unwrap();
    };
    active();
    machine.marker();
    // Counting starts once every connector is past its start and a poll
    // begun then has its answer, so the window holds the polls' steady pace.
    std::thread::sleep(poll * 2);
    let asked = || proxy.requests().iter().filter(|r| r.polls()).count();
    let start = asked();
    let opened = std::time::Instant::now();
    while opened.elapsed() < window {
        active();
        std::thread::sleep(poll.min(window.saturating_sub(opened.elapsed())));
    }
    let taken = asked() - start;
    drop(running);
    taken as u128
}

// A write one session's connector relays reaches the other sessions on the
// machine at their next prompt, not at the next poll: once the server has
// answered the write, the connector sets the marker aside as old, so the
// hooks ask the server.
#[test]
fn a_write_through_one_connector_reaches_another_sessions_next_prompt() {
    let host = Host::start(root());
    let proxy = Counting::to(&host.address);
    let machine = Machine {
        home: tempdir().unwrap(),
        url: format!("http://{}/mcp", proxy.address),
        token: host.root.path().join("token"),
        // Polls far apart, so only the marker set aside as old can tell the
        // write.
        env: vec![("USER_MEMORY_MARKER_POLL_MS", "20000".to_owned())],
    };
    let mut connector = machine.connector();
    machine.hook("session-start");
    machine.marker();
    let (quiet, log) = machine.hook("prompt-submit");
    assert_eq!(quiet, "", "{log}");
    let mut input = connector.0.stdin.take().unwrap();
    let output = lines(connector.0.stdout.take().unwrap());
    writeln!(input, "{}", initialize(1)).unwrap();
    input.flush().unwrap();
    reply(&output);
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    let before = proxy.sent().len();
    let wrote = std::time::SystemTime::now();
    writeln!(
        input,
        "{}",
        call(
            2,
            "record",
            json!({"category":"workflow","context":"Reviewing","title":"Prefer small pull requests","content":"Prefer small pull requests","reason":"Easier review","scope":"global","user_confirmed":true,"origin":"claude:session/other"})
        )
    )
    .unwrap();
    input.flush().unwrap();
    let recorded = reply(&output);
    assert!(
        recorded["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("\"stored\":true"),
        "{recorded}"
    );
    let (told, log) = machine.hook("prompt-submit");
    assert!(told.contains("Prefer small pull requests"), "{told}{log}");
    // The poll the write woke asked from the position it held, for the
    // changes since, not for every stamp.
    let marker = machine.marker();
    time::until("the poll after the write", || modified(&marker) > wrote);
    let asked = proxy.sent()[before..].to_owned();
    assert!(
        asked.contains("\"arguments\":{\"stamps\":true,\"stamps_since\":\"1."),
        "{asked}"
    );
    assert!(!asked.contains("\"stamps_since\":\"\""), "{asked}");
}

// A server that answers without positions, as one from before them does,
// gets no marker and is asked only for its version once a quiet interval;
// once its version changes, the poll asks for stamps again and the marker
// comes.
#[test]
fn a_server_without_positions_is_asked_only_for_its_version_until_it_changes() {
    // 300 ms polls: one a quiet interval of 3.6 s, as no hook runs.
    let mut host = Host::start_with(
        root(),
        Some("0.9.3"),
        &[("USER_MEMORY_TEST_NO_POSITIONS", "1")],
    );
    let proxy = Counting::to(&host.address);
    let machine = Machine {
        home: tempdir().unwrap(),
        url: format!("http://{}/mcp", proxy.address),
        token: host.root.path().join("token"),
        env: vec![("USER_MEMORY_MARKER_POLL_MS", "300".to_owned())],
    };
    let count = |text: &str, what: &str| text.matches(what).count();
    let _connector = machine.connector();
    time::until("the first poll", || {
        count(&proxy.sent(), "\"name\":\"index\"") == 1
    });
    let first = proxy.sent().len();
    time::until("two quiet polls", || {
        count(&proxy.sent()[first..], "\"method\":\"initialize\"") >= 3
    });
    let quiet = proxy.sent()[first..].to_owned();
    assert_eq!(count(&quiet, "\"name\":\"index\""), 0, "{quiet}");
    assert!(find(machine.home.path(), "changed").is_none(), "no marker");
    // No session of a server without positions is kept: each one the
    // connector opened is ended, so the next handshake names the server's
    // version as it is then.
    let sessions = |method: Option<&str>| -> std::collections::BTreeSet<String> {
        proxy
            .requests()
            .iter()
            .filter(|r| r.mcp() && method.is_none_or(|m| r.method() == m))
            .filter_map(Request::session)
            .collect()
    };
    time::until("every session the connector opened ended", || {
        sessions(None) == sessions(Some("DELETE"))
    });
    host.child.kill().unwrap();
    host.child.wait().unwrap();
    host.restart_as(Some("0.10.0"));
    machine.marker();
    // The upgraded server's positions come at each poll, and the session
    // they came in is kept: the server is met as it is now, not as the
    // first handshake named it.
    let before = proxy.requests().len();
    time::until("three polls with positions", || {
        proxy.requests()[before..]
            .iter()
            .filter(|r| r.polls())
            .count()
            >= 3
    });
    let sent: Vec<Request> = proxy.requests()[before..]
        .iter()
        .filter(|r| r.mcp())
        .cloned()
        .collect();
    let after = calls(&sent);
    let first = after.iter().position(Request::polls).unwrap();
    let session = after[first].session();
    assert!(
        after[first..]
            .iter()
            .all(|r| r.polls() && r.session() == session),
        "{}",
        told(&sent)
    );
}

/// A memory server sends every tool's hints over HTTP too, with
/// `openWorldHint` from its own embedding setting.
#[test]
fn a_server_sends_its_tools_hints_over_http() {
    let open_world = |env: &[(&str, &str)]| {
        let host = Host::start_with(root(), None, env);
        host.rpc(initialize(1));
        let listed = host.rpc(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}));
        let mut open: Vec<String> = Vec::new();
        for tool in listed["result"]["tools"].as_array().unwrap() {
            let hints = &tool["annotations"];
            for hint in ["readOnlyHint", "destructiveHint", "idempotentHint"] {
                assert!(hints[hint].is_boolean(), "{} has no {hint}", tool["name"]);
            }
            if hints["openWorldHint"].as_bool().unwrap() {
                open.push(tool["name"].as_str().unwrap().to_owned());
            }
        }
        open.sort();
        open
    };
    assert!(open_world(&[]).is_empty());
    assert_eq!(
        open_world(&[(
            "USER_MEMORY_EMBED_URL",
            "https://embed.example.com/v1/embeddings"
        )]),
        ["index", "record", "search"]
    );
}

/// The hex form a delivery file keeps `term` in.
fn hex(term: &str) -> String {
    term.bytes().map(|b| format!("{b:02x}")).collect()
}

/// The delivery files under `home` (bases, overlays and files being
/// written), however deep its state folder is.
fn delivery_files(home: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut dirs = vec![home.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if dir.ends_with("delivery")
                && path
                    .extension()
                    .is_some_and(|e| e == "index" || e == "overlay" || e == "partial")
            {
                found.push(path);
            }
        }
    }
    found
}

/// The delivery files under `home` holding `term`, in hex or as written.
fn holding(home: &Path, term: &str) -> Vec<std::path::PathBuf> {
    let (hex, raw) = (hex(term), term.as_bytes().to_vec());
    delivery_files(home)
        .into_iter()
        .filter(|path| {
            let bytes = fs::read(path).unwrap_or_default();
            [hex.as_bytes(), &raw]
                .iter()
                .any(|needle| bytes.windows(needle.len()).any(|w| w == *needle))
        })
        .collect()
}

/// Records on `host` a global procedure that runs `tool`, from `origin`;
/// its id.
fn record_tool(host: &Host, id: u64, tool: &str, origin: &str) -> String {
    host.rpc(initialize(id));
    let reply = host.rpc(call(
        id + 1,
        "record",
        json!({"type":"procedure","category":"build","context":"Releasing","title":format!("Run {tool}"),"content":format!("Run `{tool}` before the step."),"reason":"Because","scope":"global","user_confirmed":true,"origin":origin}),
    ));
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    serde_json::from_str::<Value>(text).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// What a hook said on stderr besides a debug build's traces of its work.
fn errors(said: &str) -> Vec<&str> {
    said.lines()
        .filter(|line| {
            !["user-memory: looked ", "user-memory: read "]
                .iter()
                .any(|trace| line.starts_with(trace))
        })
        .collect()
}

/// How many times the machine behind `proxy` asked for a delivery file.
fn delivery_asks(proxy: &Counting) -> usize {
    let sent = proxy.sent.lock().unwrap();
    sent.windows(b"POST /delivery".len())
        .filter(|w| *w == b"POST /delivery")
        .count()
}

/// What the host wrote to stderr, once stopped.
fn stopped_stderr(host: &mut Host) -> String {
    let mut errors = host.child.stderr.take().unwrap();
    let _ = host.child.kill();
    let _ = host.child.wait();
    let mut said = String::new();
    errors.read_to_string(&mut said).unwrap();
    said
}

// A client of a server gets the server's delivery index (#180): session
// start downloads its scope's base, the connector takes each change into
// it within a poll, and a forget or a correction on the server takes the
// words out of the client's files at the connector's next poll, the base
// rewritten without them rather than downloaded.
#[test]
fn a_forget_on_the_server_leaves_a_client_s_delivery_files_within_a_poll() {
    let poll = std::time::Duration::from_millis(300);
    let within = poll * 2 + std::time::Duration::from_secs(1);
    let host = Host::start(root());
    record_tool(&host, 1, "quokka-deploy", "chat/deploy");
    let corrected = record_tool(&host, 3, "emu-check", "chat/check");
    let proxy = Counting::to(&host.address);
    let machine = Machine {
        home: tempdir().unwrap(),
        url: format!("http://{}/mcp", proxy.address),
        token: host.root.path().join("token"),
        env: vec![("USER_MEMORY_MARKER_POLL_MS", poll.as_millis().to_string())],
    };
    let _connector = machine.connector();
    let (_, said) = machine.hook("session-start");
    let home = machine.home.path();
    assert_eq!(holding(home, "quokka-deploy").len(), 1, "{said}");
    let downloads = delivery_asks(&proxy);
    record_tool(&host, 5, "wombat-lint", "chat/lint");
    let recorded = std::time::Instant::now();
    time::until("the change reached the client", || {
        !holding(home, "wombat-lint").is_empty()
    });
    assert!(recorded.elapsed() <= within, "{:?}", recorded.elapsed());
    host.rpc(call(
        7,
        "forget",
        json!({"origin":"chat/deploy","user_confirmed":true}),
    ));
    let forgot = std::time::Instant::now();
    time::until("the forget left the client's files", || {
        holding(home, "quokka-deploy").is_empty()
    });
    assert!(forgot.elapsed() <= within, "{:?}", forgot.elapsed());
    host.rpc(call(
        8,
        "update",
        json!({"memory_id":corrected,"content":"Run `koala-check` before the step.","user_confirmed":true}),
    ));
    time::until("the correction left the client's files", || {
        holding(home, "emu-check").is_empty() && !holding(home, "koala-check").is_empty()
    });
    assert!(
        !holding(home, "wombat-lint").is_empty(),
        "folded, not dropped"
    );
    assert!(
        delivery_files(home)
            .iter()
            .all(|path| path.extension().unwrap() != "partial"),
        "no file left half written"
    );
    // The connector asked for changes; nothing downloaded a base again.
    let asked = String::from_utf8_lossy(&proxy.sent.lock().unwrap()).into_owned();
    let since = asked.matches("\"since\"").count();
    assert_eq!(delivery_asks(&proxy) - downloads, since, "{asked}");
}

// Session start with a base folds the overlay on this machine and asks the
// server nothing for it.
#[test]
fn session_start_folds_a_server_s_overlay_without_a_download() {
    let poll = std::time::Duration::from_millis(300);
    let host = Host::start(root());
    record_tool(&host, 1, "quokka-deploy", "chat/deploy");
    let proxy = Counting::to(&host.address);
    let machine = Machine {
        home: tempdir().unwrap(),
        url: format!("http://{}/mcp", proxy.address),
        token: host.root.path().join("token"),
        env: vec![("USER_MEMORY_MARKER_POLL_MS", poll.as_millis().to_string())],
    };
    let connector = machine.connector();
    machine.hook("session-start");
    let home = machine.home.path();
    record_tool(&host, 3, "wombat-lint", "chat/lint");
    time::until("the change reached the overlay", || {
        holding(home, "wombat-lint")
            .iter()
            .any(|path| path.extension().unwrap() == "overlay")
    });
    drop(connector);
    let asks = delivery_asks(&proxy);
    machine.hook("session-start");
    assert_eq!(
        delivery_asks(&proxy),
        asks,
        "session start asked the server"
    );
    let held = holding(home, "wombat-lint");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(
        held[0].extension().unwrap(),
        "index",
        "folded into the base"
    );
}

/// How long a debug build's client takes to mark a base missed in the run
/// that slows it: as a slow disk would, but within session start's 5
/// seconds, so the ask still goes out before the limit.
const MISSED_DELAY_MS: &str = "2000";

// A base that does not arrive within session start's limit leaves no file
// and prints nothing, and no prompt of the session asks for it again, also
// when marking it missed is slow.
#[test]
fn a_base_that_misses_session_start_s_limit_is_not_asked_for_at_every_prompt() {
    for slow_mark in [false, true] {
        let host = Host::start(root());
        record_tool(&host, 1, "quokka-deploy", "chat/deploy");
        // A delivery file slower than session start's 5 seconds.
        let proxy = Counting::slowed_for(
            &host.address,
            std::time::Duration::from_secs(6),
            b"POST /delivery",
        );
        let mut env = vec![(time::HOOK_LIMITS.0, time::HOOK_LIMITS.1.to_owned())];
        if slow_mark {
            env.push((
                "USER_MEMORY_TEST_MISSED_DELAY_MS",
                MISSED_DELAY_MS.to_owned(),
            ));
        }
        let machine = Machine {
            home: tempdir().unwrap(),
            url: format!("http://{}/mcp", proxy.address),
            token: host.root.path().join("token"),
            env,
        };
        let (_, said) = machine.hook("session-start");
        assert_eq!(errors(&said), Vec::<&str>::new());
        let home = machine.home.path();
        assert_eq!(delivery_files(home), Vec::<std::path::PathBuf>::new());
        let asks = delivery_asks(&proxy);
        assert_eq!(asks, 1, "slow mark: {slow_mark}");
        let (_, said) = machine.hook("prompt-submit");
        assert_eq!(errors(&said), Vec::<&str>::new());
        assert_eq!(
            delivery_asks(&proxy),
            asks,
            "a prompt asked again; slow mark: {slow_mark}"
        );
        // The next session start asks again.
        machine.hook("session-start");
        assert_eq!(delivery_asks(&proxy), asks + 1, "slow mark: {slow_mark}");
    }
}

// A download cut short is never renamed into place: the old files stay.
#[test]
fn a_delivery_file_cut_short_leaves_the_client_s_files_as_they_were() {
    let poll = std::time::Duration::from_millis(300);
    let mut host = Host::start_with(
        root(),
        None,
        &[
            ("USER_MEMORY_TEST_DELIVERY_CUT", "1"),
            // Every change makes the connector take a whole base.
            ("USER_MEMORY_TEST_OVERLAY_MEMORIES", "0"),
        ],
    );
    record_tool(&host, 1, "quokka-deploy", "chat/deploy");
    let machine = Machine {
        home: tempdir().unwrap(),
        url: format!("http://{}/mcp", host.address),
        token: host.root.path().join("token"),
        env: vec![("USER_MEMORY_MARKER_POLL_MS", poll.as_millis().to_string())],
    };
    let _connector = machine.connector();
    machine.hook("session-start");
    let home = machine.home.path();
    let before: Vec<(std::path::PathBuf, Vec<u8>)> = delivery_files(home)
        .into_iter()
        .map(|path| {
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    assert_eq!(before.len(), 2, "a base and its overlay");
    let marker = machine.marker();
    let marked = fs::read(&marker).unwrap();
    record_tool(&host, 3, "wombat-lint", "chat/lint");
    time::until("the connector polled the change", || {
        fs::read(&marker).unwrap() != marked
    });
    let after: Vec<(std::path::PathBuf, Vec<u8>)> = delivery_files(home)
        .into_iter()
        .map(|path| {
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    assert_eq!(after, before);
    // The server sends whole files again: the connector, behind, asks at
    // its next poll though the server's position stayed where it was.
    host.restart_with(&[]);
    time::until("the client caught up", || {
        !holding(home, "wombat-lint").is_empty()
    });
}

// The prompt hook downloads a base its session's scope lacks.
#[test]
fn the_prompt_hook_downloads_a_server_s_base_the_scope_lacks() {
    let host = Host::start(root());
    record_tool(&host, 1, "quokka-deploy", "chat/deploy");
    let machine = Machine {
        home: tempdir().unwrap(),
        url: format!("http://{}/mcp", host.address),
        token: host.root.path().join("token"),
        env: Vec::new(),
    };
    machine.hook("session-start");
    let home = machine.home.path();
    for path in delivery_files(home) {
        fs::remove_file(path).unwrap();
    }
    let (_, said) = machine.hook("prompt-submit");
    assert_eq!(errors(&said), Vec::<&str>::new());
    assert_eq!(holding(home, "quokka-deploy").len(), 1);
}

/// Polls after which a running connector's marker no longer counts for the
/// hooks: src/remote/marker.rs's `QUIET_POLLS` (12) times
/// `SERVED_AFTER_POLLS` (3).
const SERVED_POLLS: u32 = 36;

/// The text a hook added to the context, from its stdout; empty for none.
fn added(stdout: &str) -> String {
    if stdout.is_empty() {
        return String::new();
    }
    let reply: Value = serde_json::from_str(stdout).unwrap();
    reply["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .to_owned()
}

// On a server's store the hooks match against the files its connector
// keeps here (#117, #180), and read them only while a connector holds its
// lock, its marker is recent and the newest file is at or past the
// marker's position, so a removal the connector has heard of is never
// matched. A push names the id alone: whose word a memory is, and its
// text, are on the server, which a tool hook never asks.
#[test]
fn a_server_s_files_are_matched_only_while_a_connector_keeps_them_current() {
    let poll = std::time::Duration::from_millis(300);
    let host = Host::start(root());
    let id = record_tool(&host, 1, "quokka-deploy", "chat/deploy");
    let machine = Machine {
        home: tempdir().unwrap(),
        url: format!("http://{}/mcp", host.address),
        token: host.root.path().join("token"),
        env: vec![("USER_MEMORY_MARKER_POLL_MS", poll.as_millis().to_string())],
    };
    let home = machine.home.path();
    let sessions = ["held", "unheld", "old", "behind", "prompted"];
    {
        let _connector = machine.connector();
        for session in sessions {
            machine.hook_with("session-start", session, json!({"source": "startup"}));
        }
        machine.marker();
        assert_eq!(
            holding(home, "quokka-deploy").len(),
            1,
            "session start downloaded the base"
        );
    }
    // The connector has stopped: its lock is free, its marker still recent.
    let marker = machine.marker();
    let connected = find(home, "connected").expect("the connector's lock file");
    let call = |session: &str| {
        added(&machine.hook_with(
            "post-tool-use",
            session,
            json!({"tool_name":"Bash","tool_input":{"command":"quokka-deploy --now"},"tool_response":"done"}),
        ))
    };
    let pointer = format!(
        "From user-memory, matching this step (read one for the rest before applying it):\n- {} matches this step; read it before applying it",
        &id[..8]
    );
    assert_eq!(call("unheld"), "", "no connector runs");
    // The lock a running connector shares, held here so the marker stays
    // as the test sets it.
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&connected)
        .unwrap();
    lock.lock_shared().unwrap();
    assert_eq!(call("held"), pointer, "a connector runs, its marker recent");
    let prompted = added(&machine.hook_with(
        "prompt-submit",
        "prompted",
        json!({"prompt": "Run `quokka-deploy` for the release"}),
    ));
    assert!(prompted.contains(&pointer), "{prompted}");
    assert!(
        !prompted.contains("before the step"),
        "no text from the server: {prompted}"
    );
    let position = fs::read_to_string(&marker).unwrap();
    // A position is FORMAT.STORE.CHANGE.SEQ; one change past the files'.
    let mut parts: Vec<&str> = position.trim_end().rsplitn(3, '.').collect();
    let change = (parts[1].parse::<u64>().unwrap() + 1).to_string();
    parts[1] = &change;
    parts.reverse();
    fs::write(&marker, format!("{}\n", parts.join("."))).unwrap();
    assert_eq!(call("behind"), "", "the newest file is behind the marker");
    fs::write(&marker, &position).unwrap();
    fs::File::options()
        .write(true)
        .open(&marker)
        .unwrap()
        .set_modified(
            std::time::SystemTime::now() - poll * SERVED_POLLS - std::time::Duration::from_secs(1),
        )
        .unwrap();
    assert_eq!(call("old"), "", "the marker is past its age");
    drop(lock);
}

// A server released before the route answers 404: the client keeps no
// delivery files and says nothing.
#[test]
fn a_server_without_the_delivery_route_leaves_the_client_without_files_or_errors() {
    let host = Host::start_with(root(), None, &[("USER_MEMORY_TEST_NO_DELIVERY_ROUTE", "1")]);
    record_tool(&host, 1, "quokka-deploy", "chat/deploy");
    let machine = Machine {
        home: tempdir().unwrap(),
        url: format!("http://{}/mcp", host.address),
        token: host.root.path().join("token"),
        env: Vec::new(),
    };
    let _connector = machine.connector();
    let (started, said) = machine.hook("session-start");
    assert!(started.contains("quokka-deploy"), "{started}");
    assert_eq!(errors(&said), Vec::<&str>::new());
    let (_, said) = machine.hook("prompt-submit");
    assert_eq!(errors(&said), Vec::<&str>::new());
    assert_eq!(
        delivery_files(machine.home.path()),
        Vec::<std::path::PathBuf>::new()
    );
}

// Many clients asking for one scope's base at one position cost the
// server one build.
#[test]
fn many_clients_asking_for_one_base_cost_the_server_one_build() {
    let mut host = Host::start(root());
    record_tool(&host, 1, "quokka-deploy", "chat/deploy");
    let machines: Vec<Machine> = (0..4)
        .map(|_| Machine {
            home: tempdir().unwrap(),
            url: format!("http://{}/mcp", host.address),
            token: host.root.path().join("token"),
            env: Vec::new(),
        })
        .collect();
    std::thread::scope(|scope| {
        for machine in &machines {
            scope.spawn(|| machine.hook("session-start"));
        }
    });
    for machine in &machines {
        assert_eq!(holding(machine.home.path(), "quokka-deploy").len(), 1);
    }
    let said = stopped_stderr(&mut host);
    assert_eq!(
        said.matches("built a delivery base for clients").count(),
        1,
        "{said}"
    );
}

// Doctor on a server's client deletes the delivery files no connector kept
// current for a day, as session start does, and leaves current ones. Unix
// only, as the remote setup test above: setup finds the fake Codex as
// `cli/codex` on a `:`-separated PATH, where Windows looks clients up
// through PATHEXT on a `;`-separated one.
#[cfg(unix)]
#[test]
fn doctor_deletes_a_server_s_delivery_files_no_connector_kept_current() {
    let host = Host::start(root());
    record_tool(&host, 1, "quokka-deploy", "chat/deploy");
    let client = tempdir().unwrap();
    let bin = client.path().join("bin");
    let cli_dir = client.path().join("cli");
    fs::create_dir(&cli_dir).unwrap();
    let url = format!("http://{}/mcp", host.address);
    let token_file = host.root.path().join("token");
    let config = json!({"enabled":true,"transport":{"type":"stdio","command":bin.join("user-memory"),"args":["serve"],"env":{"USER_MEMORY_URL":url,"USER_MEMORY_TOKEN_FILE":token_file}}});
    fs::write(client.path().join("fixture.json"), config.to_string()).unwrap();
    fake::fake(&cli_dir, "codex");
    let run = |binary: &std::path::Path, args: &[&str]| {
        let out = Command::new(binary)
            .args(args)
            .current_dir(client.path())
            .sandboxed(client.path())
            .env("CLAUDE_CONFIG_DIR", client.path())
            .env("CODEX_HOME", client.path().join(".codex"))
            .env("USER_MEMORY_APP_DIRS", "")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_GLOBAL")
            .env_remove("XDG_CONFIG_HOME")
            .env("PATH", format!("{}:/usr/bin:/bin", cli_dir.display()))
            .env_remove("USER_MEMORY_DB")
            .env_remove("USER_MEMORY_URL")
            .env_remove("USER_MEMORY_TOKEN_FILE")
            .env("USER_MEMORY_NO_UPDATE_CHECK", "1")
            .env(time::HOOK_ROOM.0, time::HOOK_ROOM.1)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    let token = token_file.to_string_lossy().into_owned();
    let installed = bin.to_string_lossy().into_owned();
    run(
        std::path::Path::new(env!("CARGO_BIN_EXE_user-memory")),
        &[
            "setup",
            "--client",
            "codex",
            "--url",
            &url,
            "--token-file",
            &token,
            "--install-dir",
            &installed,
        ],
    );
    let installed = bin.join("user-memory");
    run(&installed, &["session-start"]);
    let home = client.path();
    assert_eq!(delivery_files(home).len(), 2, "a base and its overlay");
    let said = run(&installed, &["doctor"]);
    assert!(!said.contains("Delivery index"), "{said}");
    assert_eq!(delivery_files(home).len(), 2, "current files stay");
    let day_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(25 * 60 * 60);
    for path in delivery_files(home) {
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(day_ago)
            .unwrap();
    }
    let said = run(&installed, &["doctor"]);
    assert!(
        said.contains(
            "Delivery index: deleted 2 files that may hold words the memory server removed."
        ),
        "{said}"
    );
    assert_eq!(delivery_files(home), Vec::<std::path::PathBuf>::new());
}

// A server that no longer answers the delivery route, as one put back to
// an older release: the connector deletes the files no one keeps current.
#[test]
fn a_server_that_stops_serving_the_delivery_index_leaves_the_client_no_files() {
    let poll = std::time::Duration::from_millis(300);
    let mut host = Host::start(root());
    record_tool(&host, 1, "quokka-deploy", "chat/deploy");
    let machine = Machine {
        home: tempdir().unwrap(),
        url: format!("http://{}/mcp", host.address),
        token: host.root.path().join("token"),
        env: vec![("USER_MEMORY_MARKER_POLL_MS", poll.as_millis().to_string())],
    };
    let _connector = machine.connector();
    machine.hook("session-start");
    let home = machine.home.path();
    assert_eq!(delivery_files(home).len(), 2);
    host.restart_with(&[("USER_MEMORY_TEST_NO_DELIVERY_ROUTE", "1")]);
    record_tool(&host, 3, "wombat-lint", "chat/lint");
    time::until("the client deleted its files", || {
        delivery_files(home).is_empty()
    });
}

// A client whose connector never ran keeps no removed word past a day:
// session start deletes files not written for a day and downloads the
// base again, without what the server forgot meanwhile.
#[test]
fn session_start_replaces_a_server_s_files_no_connector_kept_current() {
    let host = Host::start(root());
    record_tool(&host, 1, "quokka-deploy", "chat/deploy");
    record_tool(&host, 3, "wombat-lint", "chat/lint");
    let machine = Machine {
        home: tempdir().unwrap(),
        url: format!("http://{}/mcp", host.address),
        token: host.root.path().join("token"),
        env: Vec::new(),
    };
    machine.hook("session-start");
    let home = machine.home.path();
    assert_eq!(holding(home, "quokka-deploy").len(), 1);
    host.rpc(call(
        5,
        "forget",
        json!({"origin":"chat/deploy","user_confirmed":true}),
    ));
    machine.hook("session-start");
    assert_eq!(holding(home, "quokka-deploy").len(), 1, "within a day");
    let day_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(25 * 60 * 60);
    for path in delivery_files(home) {
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(day_ago)
            .unwrap();
    }
    machine.hook("session-start");
    assert_eq!(
        holding(home, "quokka-deploy"),
        Vec::<std::path::PathBuf>::new()
    );
    assert_eq!(holding(home, "wombat-lint").len(), 1, "downloaded again");
}
