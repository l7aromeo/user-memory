//! Client tokens at the memory server: each request is checked against the
//! shared token file and the registry, live, through the real binary.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
#[path = "support/time.rs"]
mod time;
use time::STUCK;

use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tempfile::{TempDir, tempdir};

/// What every request is answered while the registry cannot be read.
const UNREADABLE: &str =
    "The server's token registry cannot be read; the server's log says why and what to do.";

/// How long a client here waits between a request's headers and its body,
/// and after the body before reading: long enough for a server answering
/// from the headers alone to have closed the connection first (#226).
const LATE: Duration = Duration::from_millis(100);

/// How many late requests the reset test sends; on a server that does not
/// read a refused request's body, each of them resets.
const LATE_ROUNDS: usize = 5;

/// The longest a refused request whose body never comes waits for its
/// answer: the server's wait for the body (2 s), and room on a loaded
/// machine.
const NO_BODY_ANSWER: Duration = Duration::from_secs(10);

/// The shared token file's token.
const SHARED: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct Host {
    root: TempDir,
    child: Option<Child>,
    address: String,
}

/// An HTTP answer: status, `WWW-Authenticate` and body.
struct Answer {
    status: u16,
    challenge: String,
    body: String,
}

impl Host {
    fn start() -> Self {
        Self::start_with(&[])
    }

    fn start_with(env: &[(&str, &str)]) -> Self {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("data")).unwrap();
        fs::write(root.path().join("access-token"), SHARED).unwrap();
        let mut host = Self {
            root,
            child: None,
            address: String::new(),
        };
        host.serve(env);
        host
    }

    /// Starts `host` on this folder's store and shared token file.
    fn serve(&mut self, env: &[(&str, &str)]) {
        let host = self;
        let mut command = host.command(&[]);
        command.args([
            "host",
            "--listen",
            "127.0.0.1:0",
            "--db",
            host.db().to_str().unwrap(),
            "--token-file",
            host.root.path().join("access-token").to_str().unwrap(),
        ]);
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(line.starts_with("Listening: http://"), "{line}");
        host.address = line
            .trim()
            .strip_prefix("Listening: http://")
            .unwrap()
            .strip_suffix("/mcp")
            .unwrap()
            .to_owned();
        host.child = Some(child);
    }

    /// Stops the server and starts it again on the same files.
    fn restart(&mut self) {
        self.stop();
        self.serve(&[]);
    }

    fn db(&self) -> PathBuf {
        self.root.path().join("data").join("memories.sqlite3")
    }

    fn registry(&self) -> PathBuf {
        self.root.path().join("data").join("accounts.sqlite3")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .args(args)
            .sandboxed(self.root.path())
            .stdin(Stdio::null());
        command
    }

    /// `host token ARGS` on this server's registry, from another process.
    fn token(&self, args: &[&str]) -> Output {
        let out = self.try_token(args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    /// `host token ARGS`, which may fail.
    fn try_token(&self, args: &[&str]) -> Output {
        self.command(
            &[
                &["host", "token"],
                args,
                &["--db", self.db().to_str().unwrap()],
            ]
            .concat(),
        )
        .output()
        .unwrap()
    }

    /// A new client token named `name` (with `extra` options), read from
    /// the file `add` wrote.
    fn add(&self, name: &str, extra: &[&str]) -> String {
        let file = self.root.path().join(format!("{name}.token"));
        let mut args = vec!["add", name, "--out", file.to_str().unwrap()];
        args.extend(extra);
        self.token(&args);
        fs::read_to_string(file).unwrap()
    }

    fn send(&self, method: &str, path: &str, token: &str, body: &str) -> Answer {
        let mut socket = TcpStream::connect(&self.address).unwrap();
        socket.set_read_timeout(Some(STUCK)).unwrap();
        write!(
            socket,
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2025-06-18\r\nAuthorization: Bearer {token}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            self.address,
            body.len()
        )
        .unwrap();
        let mut response = Vec::new();
        socket.read_to_end(&mut response).unwrap();
        let text = String::from_utf8_lossy(&response).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        let challenge = head
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("www-authenticate")
                    .then(|| value.trim().to_owned())
            })
            .unwrap_or_default();
        Answer {
            status,
            challenge,
            body: body.to_owned(),
        }
    }

    /// An MCP `initialize` with `token`.
    fn initialize(&self, token: &str) -> Answer {
        let message = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}});
        self.send("POST", "/mcp", token, &message.to_string())
    }

    /// An MCP `initialize` with `token` whose body comes [`LATE`] after its
    /// headers, or never when `send_body` is false, read [`LATE`] after the body: the
    /// answer's bytes, or the error reading them.
    fn late_initialize(&self, token: &str, send_body: bool) -> Result<String, String> {
        let message = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}).to_string();
        let head = format!(
            "POST /mcp HTTP/1.1\r\nHost: {}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2025-06-18\r\nAuthorization: Bearer {token}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
            self.address,
            message.len()
        );
        let mut socket = TcpStream::connect(&self.address).unwrap();
        socket.set_read_timeout(Some(STUCK)).unwrap();
        socket.write_all(head.as_bytes()).unwrap();
        if send_body {
            std::thread::sleep(LATE);
            // A server that closed already may refuse the body: what counts
            // is what the client reads next.
            let _ = socket.write_all(message.as_bytes());
            std::thread::sleep(LATE);
        }
        let mut response = Vec::new();
        socket
            .read_to_end(&mut response)
            .map_err(|error| error.to_string())?;
        Ok(String::from_utf8_lossy(&response).into_owned())
    }

    /// Stops the server and returns what it wrote to stderr.
    fn stop(&mut self) -> String {
        let mut child = self.child.take().unwrap();
        let _ = child.kill();
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        let _ = child.wait();
        stderr
    }

    fn registry_conn(&self) -> Connection {
        Connection::open(self.registry()).unwrap()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

const DAY: i64 = 24 * 60 * 60;

const REVOKED: &str = r#"Bearer error="invalid_token", error_description="revoked""#;
const UNUSED: &str = r#"Bearer error="invalid_token", error_description="unused""#;
const EXPIRED: &str = r#"Bearer error="invalid_token", error_description="expired""#;

#[test]
fn a_client_token_works_at_once_and_stops_at_once_when_revoked() {
    let host = Host::start();
    // The registry appears after the server started: no restart.
    let laptop = host.add("laptop", &[]);
    assert_eq!(host.initialize(&laptop).status, 200);
    // The client uses it as any token file.
    let file = host.root.path().join("laptop.token");
    let out = host
        .command(&["index"])
        .env("USER_MEMORY_URL", format!("http://{}/mcp", host.address))
        .env("USER_MEMORY_TOKEN_FILE", &file)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    host.token(&["revoke", "laptop"]);
    let refused = host.initialize(&laptop);
    assert_eq!(refused.status, 401);
    assert_eq!(refused.challenge, REVOKED);
    // The shared file and other tokens are untouched; a stranger gets the
    // plain challenge, with no reason to learn from.
    assert_eq!(host.initialize(SHARED).status, 200);
    let stranger = host.initialize(&"x".repeat(56));
    assert_eq!(stranger.status, 401);
    assert_eq!(stranger.challenge, "Bearer");
    // Once the registry says the shared file is retired, its token is
    // refused as revoked, at once.
    host.registry_conn()
        .execute(
            "INSERT INTO settings (key, value) VALUES ('shared_file_retired', '1')",
            [],
        )
        .unwrap();
    assert_eq!(host.initialize(SHARED).challenge, REVOKED);
}

#[test]
fn get_auth_token_answers_the_callers_scope() {
    let host = Host::start();
    let writer = host.add("w", &[]);
    let reader = host.add("r", &["--read-only"]);
    for (token, scope) in [
        (&writer, "write"),
        (&reader, "read"),
        (&SHARED.to_owned(), "write"),
    ] {
        let answer = host.send("GET", "/auth/token", token, "");
        assert_eq!(answer.status, 200, "{}", answer.body);
        let said: Value = serde_json::from_str(&answer.body)
            .unwrap_or_else(|err| panic!("{err}: {:?}", answer.body));
        assert_eq!(said, json!({ "scope": scope }));
    }
    assert_eq!(
        host.send("GET", "/auth/token", &"x".repeat(56), "").status,
        401
    );
    // Asking changes nothing.
    assert_eq!(host.initialize(&reader).status, 200);
}

#[test]
fn delete_auth_token_revokes_the_caller_and_no_other() {
    let host = Host::start();
    let a = host.add("a", &[]);
    let b = host.add("b", &["--read-only"]);
    let answer = host.send("DELETE", "/auth/token", &b, "");
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(host.initialize(&b).challenge, REVOKED);
    assert_eq!(host.initialize(&a).status, 200);
    // The shared file is retired otherwise, and a stranger is refused.
    let shared = host.send("DELETE", "/auth/token", SHARED, "");
    assert_eq!(shared.status, 400);
    assert!(
        shared.body.contains("host token retire-shared"),
        "{}",
        shared.body
    );
    assert_eq!(
        host.send("DELETE", "/auth/token", &"x".repeat(56), "")
            .status,
        401
    );
    let listed = String::from_utf8_lossy(&host.token(&["list"]).stdout).into_owned();
    assert!(listed.contains("\na "), "{listed}");
    assert!(!listed.contains("\nb "), "{listed}");
}

#[test]
fn a_token_expires_after_90_days_unused_or_at_its_date() {
    let host = Host::start();
    let idle = host.add("idle", &[]);
    let dated = host.add("dated", &["--expires", "5"]);
    let fresh = host.add("fresh", &[]);
    for token in [&idle, &dated, &fresh] {
        assert_eq!(host.initialize(token).status, 200);
    }
    // Time passes, as the registry sees it.
    let conn = host.registry_conn();
    conn.execute(
        "UPDATE client_tokens SET created_at = ?1, last_used_at = ?1 WHERE name = 'idle'",
        [now() - 91 * DAY],
    )
    .unwrap();
    conn.execute(
        "UPDATE client_tokens SET expires_at = ?1 WHERE name = 'dated'",
        [now() - 1],
    )
    .unwrap();
    for (token, challenge) in [(&idle, UNUSED), (&dated, EXPIRED)] {
        let answer = host.initialize(token);
        assert_eq!(answer.status, 401);
        assert_eq!(answer.challenge, challenge);
    }
    assert_eq!(host.initialize(&fresh).status, 200);
    // 89 days unused still works.
    conn.execute(
        "UPDATE client_tokens SET created_at = ?1, last_used_at = ?1 WHERE name = 'fresh'",
        [now() - 89 * DAY],
    )
    .unwrap();
    assert_eq!(host.initialize(&fresh).status, 200);
}

/// The registry is read again only when another process commits to it:
/// the server's own last-use writes cause no reload, and are written at
/// most once an hour per token.
#[test]
fn the_servers_own_last_use_write_causes_no_reload() {
    let mut host = Host::start_with(&[("USER_MEMORY_TEST_TRACE", "1")]);
    let laptop = host.add("laptop", &[]);
    assert_eq!(host.initialize(&laptop).status, 200);
    let last_used = || -> Option<i64> {
        host.registry_conn()
            .query_row(
                "SELECT last_used_at FROM client_tokens WHERE name = 'laptop'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    };
    let first = last_used().expect("the first use is written");
    std::thread::sleep(Duration::from_millis(1100));
    for _ in 0..3 {
        assert_eq!(host.initialize(&laptop).status, 200);
    }
    assert_eq!(last_used(), Some(first), "written at most once an hour");
    host.token(&["revoke", "laptop"]);
    assert_eq!(host.initialize(&laptop).status, 401);
    let log = host.stop();
    let loads = log.matches("registry loaded:").count();
    assert_eq!(
        loads, 2,
        "loaded at its first use and after the revoke only:\n{log}"
    );
}

#[test]
fn a_damaged_or_newer_registry_refuses_every_request_until_moved_aside() {
    let mut host = Host::start();
    let shared = host.root.path().join("shared.token");
    fs::write(&shared, SHARED).unwrap();
    let mut bridge = Bridge::start(&host, &shared);
    bridge.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"app","version":"1"}}}));
    assert!(bridge.reply().get("result").is_some());
    bridge.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    let (failed, text) = bridge.call(2, "index");
    assert!(!failed, "{text}");
    fs::write(
        host.registry(),
        "not a database, though long enough to be read as one by SQLite's header check",
    )
    .unwrap();
    let message = format!(
        "The server's token registry {} cannot be read (file is not a database). Move it aside and issue each client a new token with `user-memory host token add`.",
        host.registry().display()
    );
    // The agent's session is told the server answered, and goes on.
    let (failed, text) = bridge.call(3, "index");
    assert!(failed, "{text}");
    assert!(
        text.contains("answered but cannot serve requests now"),
        "{text}"
    );
    drop(bridge);
    for _ in 0..2 {
        // The shared file too: a retired one could otherwise come back.
        let answer = host.initialize(SHARED);
        assert_eq!(answer.status, 503);
        // Any caller gets it, so it names neither the file nor the cause.
        assert_eq!(answer.body, UNREADABLE);
        let own = host.send("DELETE", "/auth/token", "umt_anything", "");
        assert_eq!((own.status, own.body.as_str()), (503, UNREADABLE));
        // Its body read first, so a body sent late reads the answer too.
        let late = host.late_initialize(SHARED, true);
        assert!(
            late.as_deref().is_ok_and(|a| a.starts_with("HTTP/1.1 503")),
            "{late:?}"
        );
    }
    // A client is told its server answered and where the reason is.
    let out = client(&host, &shared, &["index"], b"");
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains(&format!(
            "The memory server at http://{}/mcp answered but cannot serve requests now; its operator should read the server's log.",
            host.address
        )),
        "{said}"
    );
    assert!(!said.contains(host.root.path().to_str().unwrap()), "{said}");
    fs::rename(host.registry(), host.root.path().join("accounts.damaged")).unwrap();
    assert_eq!(host.initialize(SHARED).status, 200);

    // A registry made newer under the running server, by a newer
    // user-memory beside it, is refused the same way, with no restart.
    let laptop = host.add("laptop", &[]);
    assert_eq!(host.initialize(&laptop).status, 200);
    host.registry_conn()
        .pragma_update(None, "user_version", 2)
        .unwrap();
    let answer = host.initialize(&laptop);
    assert_eq!((answer.status, answer.body.as_str()), (503, UNREADABLE));
    let log = host.stop();
    assert_eq!(log.matches(&message).count(), 1, "said once:\n{log}");
    assert_eq!(
        log.matches("has schema version 2, but this user-memory is older than it")
            .count(),
        1,
        "said once:\n{log}"
    );
}

/// An MCP session with the server over HTTP, as a client with `token`.
struct Session<'a> {
    host: &'a Host,
    token: String,
    id: String,
}

impl<'a> Session<'a> {
    fn open(host: &'a Host, token: &str) -> Self {
        let message = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}});
        let mut socket = TcpStream::connect(&host.address).unwrap();
        socket.set_read_timeout(Some(STUCK)).unwrap();
        let body = message.to_string();
        write!(
            socket,
            "POST /mcp HTTP/1.1\r\nHost: {}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2025-06-18\r\nAuthorization: Bearer {token}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            host.address,
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        let id = response
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("mcp-session-id")
                    .then(|| value.trim().to_owned())
            })
            .expect("a session id");
        let session = Self {
            host,
            token: token.to_owned(),
            id,
        };
        session.post(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        session
    }

    fn post(&self, message: &Value) -> String {
        let mut socket = TcpStream::connect(&self.host.address).unwrap();
        socket.set_read_timeout(Some(STUCK)).unwrap();
        let body = message.to_string();
        write!(
            socket,
            "POST /mcp HTTP/1.1\r\nHost: {}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2025-06-18\r\nMcp-Session-Id: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            self.host.address,
            self.id,
            self.token,
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap_or(("", ""));
        let head = head.to_ascii_lowercase();
        let body = if head.contains("transfer-encoding: chunked") {
            unchunked(body)
        } else {
            body.to_owned()
        };
        // An event stream's reply is the data line that holds the answer.
        body.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .find(|line| serde_json::from_str::<Value>(line).is_ok_and(|v| v.get("id").is_some()))
            .map_or(body.clone(), str::to_owned)
    }

    /// Calls `tool`: whether the server answered with a tool error, and its
    /// text.
    fn call(&self, tool: &str, arguments: Value) -> (bool, String) {
        let body = self.post(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":tool,"arguments":arguments}}));
        let reply: Value =
            serde_json::from_str(&body).unwrap_or_else(|err| panic!("{tool}: {err}: {body}"));
        let result = &reply["result"];
        (
            result["isError"] == true,
            result["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .to_owned(),
        )
    }

    fn tools(&self) -> Value {
        let body = self.post(&json!({"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}));
        serde_json::from_str::<Value>(&body).unwrap()["result"]["tools"].clone()
    }
}

/// A chunked HTTP body, joined.
fn unchunked(mut body: &str) -> String {
    let mut joined = String::new();
    while let Some((size, rest)) = body.split_once("\r\n") {
        let size = usize::from_str_radix(size.split(';').next().unwrap().trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        joined.push_str(&rest[..size]);
        body = rest[size..].strip_prefix("\r\n").unwrap_or(&rest[size..]);
    }
    joined
}

/// The client's environment for this server and the token in `file`.
fn as_client(host: &Host, command: &mut Command, file: &std::path::Path) {
    command
        .env("USER_MEMORY_URL", format!("http://{}/mcp", host.address))
        .env("USER_MEMORY_TOKEN_FILE", file)
        .env_remove("USER_MEMORY_DB");
}

fn record_args(title: &str) -> Value {
    json!({"type":"preference","category":"workflow","title":title,"content":title,"scope":"global","user_confirmed":true})
}

#[test]
fn a_read_only_token_reads_and_searches_but_changes_nothing() {
    let host = Host::start();
    let writer = Session::open(&host, SHARED);
    let (failed, stored) = writer.call("record", record_args("Prefer tabs"));
    assert!(!failed, "{stored}");
    let id = serde_json::from_str::<Value>(&stored).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let reader_token = host.add("reader", &["--read-only"]);
    let reader = Session::open(&host, &reader_token);
    // The tool list is the server's own, whatever the token.
    assert_eq!(reader.tools(), writer.tools());
    let refusal = format!(
        "This machine's token for http://{}/mcp is read-only; this needs a token with write access from the server's operator.",
        host.address
    );
    for (tool, arguments) in [
        ("record", record_args("Prefer spaces")),
        (
            "update",
            json!({"memory_id":id,"content":"Prefer spaces","user_confirmed":true}),
        ),
        ("forget", json!({"memory_id":id,"user_confirmed":true})),
        (
            "forget",
            json!({"import":"backup.json@2026-11-15T10:42:07Z.e0e0e0e0","user_confirmed":true}),
        ),
        ("import", json!({"name":"backup.json"})),
        ("review", json!({"keep":[&id[..8]],"user_confirmed":true})),
        ("export", json!({"name":"backup.json","overwrite":true})),
    ] {
        let (failed, text) = reader.call(tool, arguments);
        assert!(failed, "{tool} was let through: {text}");
        assert_eq!(text, refusal, "{tool}");
    }
    // A read-only token on a session a write token opened is refused too:
    // each request is scoped by its own token, not by the session's.
    let borrowed = Session {
        host: &host,
        token: reader_token.clone(),
        id: writer.id.clone(),
    };
    let (failed, text) = borrowed.call("record", record_args("Prefer spaces"));
    assert!(failed, "a borrowed session let a write through: {text}");
    assert_eq!(text, refusal);
    for (tool, arguments) in [
        ("index", json!({})),
        // The connector's poll for changes.
        ("index", json!({"stamps":true,"stamps_since":""})),
        // A session hook's index, fitted to its client's limit.
        (
            "index",
            json!({"projects":["github.com/acme/tools"],"fit_room":4000,"fit_whole_projects":["github.com/acme/tools"],"fit_unit":"utf16_units"}),
        ),
        ("read", json!({"ids":[&id[..8]]})),
        ("search", json!({"query":"tabs"})),
        ("review", json!({})),
        ("export", json!({"name":"reader.json"})),
        (
            "link",
            json!({"origin":"claude:session/r","projects":["github.com/acme/tools"]}),
        ),
    ] {
        let (failed, text) = reader.call(tool, arguments);
        assert!(!failed && !text.contains("read-only"), "{tool}: {text}");
    }
    // Nothing changed: the memory is as recorded.
    let (_, read) = writer.call("read", json!({"ids":[&id[..8]]}));
    assert!(
        read.contains("Prefer tabs") && !read.contains("Prefer spaces"),
        "{read}"
    );

    // /import, preview or not, is refused before anything is read.
    let answer = host.send("POST", "/import", &reader_token, "{}");
    assert_eq!(answer.status, 403);
    assert_eq!(answer.body, refusal);
    // And through the CLI: export and search work, record and import don't.
    let file = host.root.path().join("reader.token");
    let run = |args: &[&str]| {
        let mut command = host.command(args);
        as_client(&host, &mut command, &file);
        command.output().unwrap()
    };
    let exported = host.root.path().join("exported.json");
    let out = run(&["export", exported.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        fs::read_to_string(&exported)
            .unwrap()
            .contains("Prefer tabs")
    );
    let out = run(&["search", "tabs"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for args in [
        vec![
            "record",
            "--type",
            "preference",
            "--category",
            "workflow",
            "--title",
            "Prefer x",
            "--content",
            "Prefer x",
            "--scope",
            "global",
        ],
        vec!["import", exported.to_str().unwrap()],
    ] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(&refusal),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// A machine with a read-only token still starts sessions: the hooks only
/// read, and the projects a conversation linked load with it.
#[test]
fn a_read_only_clients_hooks_work_links_included() {
    let host = Host::start();
    let writer = Session::open(&host, SHARED);
    let (failed, text) = writer.call(
        "record",
        json!({"type":"preference","category":"workflow","title":"Tools use tabs","content":"Tools use tabs","scope":"project","project":"github.com/acme/tools","user_confirmed":true}),
    );
    assert!(!failed, "{text}");
    host.add("reader", &["--read-only"]);
    let file = host.root.path().join("reader.token");
    let reader = Session::open(&host, &fs::read_to_string(&file).unwrap());
    let (failed, text) = reader.call(
        "link",
        json!({"origin":"claude:session/s","projects":["github.com/acme/tools"]}),
    );
    assert!(!failed, "{text}");
    let home = tempdir().unwrap();
    let hook = |args: &[&str], event: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .args(args)
            .current_dir(home.path())
            .sandboxed(home.path())
            .env(time::HOOK_ROOM.0, time::HOOK_ROOM.1)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        as_client(&host, &mut command, &file);
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                json!({"session_id":"s","cwd":home.path(),"hook_event_name":event})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        (
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap(),
        )
    };
    let (started, said) = hook(&["session-start", "--client", "claude"], "SessionStart");
    assert!(
        started.contains("Tools use tabs"),
        "the linked project loads:\n{started}\n{said}"
    );
    assert!(
        !started.contains("read-only") && !said.contains("read-only"),
        "{started}{said}"
    );
    let (notice, said) = hook(&["prompt-submit", "--client", "claude"], "UserPromptSubmit");
    assert!(
        !notice.contains("read-only") && !said.contains("read-only"),
        "{notice}{said}"
    );
    // The read-only token fetched the scope's delivery index (#180),
    // linked project included: it holds what `read` gives that token.
    let base = delivery_bases(home.path());
    assert_eq!(base.len(), 1, "{base:?}");
    let tabs: String = "tab".bytes().map(|b| format!("{b:02x}")).collect();
    let bytes = fs::read(&base[0]).unwrap();
    assert!(bytes.windows(tabs.len()).any(|w| w == tabs.as_bytes()));
}

/// The delivery bases under `home`.
fn delivery_bases(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut dirs = vec![home.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if dir.ends_with("delivery") && path.extension().is_some_and(|e| e == "index") {
                found.push(path);
            }
        }
    }
    found
}

/// A request to the delivery route without a valid token is refused as one
/// to the export route is.
#[test]
fn the_delivery_route_refuses_a_request_without_a_valid_token_as_export_does() {
    let host = Host::start();
    let body = r#"{"projects":[],"format":"UMDELIV2","stems":""}"#;
    for token in ["", "not-a-token-of-this-server-at-all-0123456789"] {
        let delivery = host.send("POST", "/delivery", token, body);
        let export = host.send("POST", "/export", token, "{}");
        assert_eq!(delivery.status, 401, "{}", delivery.body);
        assert_eq!(
            (delivery.status, delivery.challenge),
            (export.status, export.challenge)
        );
    }
    // A valid token from another version is told so, not refused as a
    // stranger.
    let other = host.send("POST", "/delivery", SHARED, body);
    assert_eq!(other.status, 409, "{}", other.body);
}

/// A new rule's list ends, on a machine whose token is read-only, with who
/// can remove one; on a machine whose token writes, with `forget`.
#[test]
fn a_new_rule_tells_a_read_only_machine_who_can_remove_it() {
    let host = Host::start();
    let writer = Session::open(&host, SHARED);
    host.add("reader", &["--read-only"]);
    let reader = host.root.path().join("reader.token");
    let shared = host.root.path().join("shared.token");
    fs::write(&shared, SHARED).unwrap();
    let start = |file: &std::path::Path, home: &std::path::Path, session: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .args(["session-start", "--client", "claude"])
            .current_dir(home)
            .sandboxed(home)
            .env(time::HOOK_ROOM.0, time::HOOK_ROOM.1)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        as_client(&host, &mut command, file);
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                json!({"session_id":session,"cwd":home,"hook_event_name":"SessionStart","source":"resume"})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    let homes = [tempdir().unwrap(), tempdir().unwrap()];
    for (file, home) in [(&reader, &homes[0]), (&shared, &homes[1])] {
        start(file, home.path(), "s1");
    }
    let (failed, text) = writer.call(
        "record",
        json!({"type":"preference","category":"workflow","title":"Push without asking","content":"Push without asking","scope":"global","user_confirmed":true}),
    );
    assert!(!failed, "{text}");
    let read_only = start(&reader, homes[0].path(), "s2");
    assert!(
        read_only.contains("Push without asking")
            && read_only.contains("This machine's token is read-only: if one isn't the user's, the server's operator can remove it."),
        "{read_only}"
    );
    let writes = start(&shared, homes[1].path(), "s2");
    assert!(
        writes.contains("If one isn't the user's, remove it with forget on their yes"),
        "{writes}"
    );
    assert!(!writes.contains("read-only"), "{writes}");
}

/// What the client tells for each refusal reason, given its server's URL.
fn sentences(url: &str) -> [(&'static str, String); 3] {
    let fix = format!(
        "Ask its operator for a new token file, then run the installer with --url {url} --token-file FILE."
    );
    [
        (
            "revoked",
            format!("The memory server at {url} revoked this machine's token. {fix}"),
        ),
        (
            "unused",
            format!("This machine's token for {url} expired after 90 days unused. {fix}"),
        ),
        (
            "expired",
            format!("This machine's token for {url} expired on the date its operator set. {fix}"),
        ),
    ]
}

/// A token named `name` refused for `reason`: revoked, unused for 91 days,
/// or past its `--expires` date. Returns its file.
fn refused_token(host: &Host, name: &str, reason: &str) -> PathBuf {
    host.add(name, &["--expires", "30"]);
    refused_token_now(host, name, reason)
}

/// The CLI as a client of `host` with the token in `file`.
fn client(host: &Host, file: &std::path::Path, args: &[&str], stdin: &[u8]) -> Output {
    let mut command = host.command(args);
    as_client(host, &mut command, file);
    command
        .env(time::HOOK_ROOM.0, time::HOOK_ROOM.1)
        .current_dir(host.root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

/// The revoked and expired sentences reach the user on every path that
/// meets the refusal: doctor's probe, a hook's call, the CLI's calls, and
/// export and import.
#[test]
fn each_refusal_reason_is_told_on_every_cli_path() {
    let host = Host::start();
    let url = format!("http://{}/mcp", host.address);
    for (reason, sentence) in sentences(&url) {
        let file = refused_token(&host, &format!("cli-{reason}"), reason);
        let payload =
            json!({"session_id":"s","cwd":host.root.path(),"hook_event_name":"SessionStart"})
                .to_string();
        let exported = host.root.path().join(format!("{reason}.json"));
        for (path, args, stdin) in [
            // Doctor reads the endpoint from its flags, as the installer
            // passes it.
            (
                "doctor",
                vec![
                    "doctor",
                    "--url",
                    &url,
                    "--token-file",
                    file.to_str().unwrap(),
                ],
                String::new(),
            ),
            (
                "the session hook",
                vec!["session-start", "--client", "claude"],
                payload,
            ),
            ("a call", vec!["index"], String::new()),
            (
                "export",
                vec!["export", exported.to_str().unwrap()],
                String::new(),
            ),
            (
                "import",
                vec!["import", exported.to_str().unwrap()],
                String::new(),
            ),
        ] {
            if path == "import" {
                fs::write(
                    &exported,
                    r#"{"format":"user-memory-export","format_version":3,"memories":[]}"#,
                )
                .unwrap();
            }
            let out = client(&host, &file, &args, stdin.as_bytes());
            let said = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(said.contains(&sentence), "{reason}, {path}:\n{said}");
            assert!(!said.contains("login"), "{reason}, {path}:\n{said}");
        }
    }
    // A token the server does not know, as from a server too old to give a
    // reason: today's message.
    let stranger = host.root.path().join("stranger.token");
    fs::write(&stranger, "x".repeat(56)).unwrap();
    let out = client(&host, &stranger, &["index"], b"");
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("Cannot connect to memory server"), "{said}");
    assert!(!said.contains("operator"), "{said}");
}

/// The bridge an agent app talks to answers a refused call with the
/// sentence, as a tool error, and stays up: for a token revoked while the
/// session runs, and for one refused from the start.
#[test]
fn the_bridge_tells_each_refusal_and_stays_up() {
    let host = Host::start();
    let url = format!("http://{}/mcp", host.address);
    for (reason, sentence) in sentences(&url) {
        // Revoked (or expired) while the session runs.
        host.add(&format!("live-{reason}"), &["--expires", "30"]);
        let file = host.root.path().join(format!("live-{reason}.token"));
        let mut bridge = Bridge::start(&host, &file);
        bridge.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"app","version":"1"}}}));
        assert!(bridge.reply().get("result").is_some());
        bridge.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        let (failed, text) = bridge.call(2, "index");
        assert!(!failed, "{text}");
        let _ = refused_token_now(&host, &format!("live-{reason}"), reason);
        for id in [3, 4] {
            let (failed, text) = bridge.call(id, "index");
            assert!(failed, "{reason}: {text}");
            assert!(text.contains(&sentence), "{reason}: {text}");
            assert!(text.contains("don't open the token file"), "{text}");
        }
        drop(bridge);

        // Refused from the start: the session opens, and its first call is
        // told why.
        let file = refused_token(&host, &format!("start-{reason}"), reason);
        let mut bridge = Bridge::start(&host, &file);
        bridge.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"app","version":"1"}}}));
        let _ = bridge.reply();
        bridge.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        let (failed, text) = bridge.call(2, "index");
        assert!(failed, "{reason}: {text}");
        assert!(text.contains(&sentence), "{reason}: {text}");
    }
}

/// Makes the token `name`, already in use, refused for `reason`.
fn refused_token_now(host: &Host, name: &str, reason: &str) -> PathBuf {
    match reason {
        "revoked" => {
            host.token(&["revoke", name]);
        }
        "unused" => {
            host.registry_conn()
                .execute(
                    "UPDATE client_tokens SET created_at = ?1, last_used_at = ?1, expires_at = NULL WHERE name = ?2",
                    rusqlite::params![now() - 91 * DAY, name],
                )
                .unwrap();
        }
        _ => {
            host.registry_conn()
                .execute(
                    "UPDATE client_tokens SET expires_at = ?1 WHERE name = ?2",
                    rusqlite::params![now() - 1, name],
                )
                .unwrap();
        }
    }
    host.root.path().join(format!("{name}.token"))
}

/// `user-memory connect`, the stdio bridge, as an agent app runs it.
struct Bridge {
    child: Child,
    input: std::process::ChildStdin,
    output: std::sync::mpsc::Receiver<String>,
}

impl Bridge {
    fn start(host: &Host, file: &std::path::Path) -> Self {
        let mut child = host
            .command(&[
                "connect",
                "--url",
                &format!("http://{}/mcp", host.address),
                "--token-file",
                file.to_str().unwrap(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // What an agent app would keep in its log, for tests to read.
            .stderr(
                fs::File::options()
                    .create(true)
                    .append(true)
                    .open(host.root.path().join("bridge.log"))
                    .unwrap(),
            )
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, output) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            output,
        }
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.input, "{message}").unwrap();
        self.input.flush().unwrap();
    }

    fn reply(&self) -> Value {
        let line = self
            .output
            .recv_timeout(STUCK)
            .expect("the bridge sent no reply");
        serde_json::from_str(&line).unwrap()
    }

    /// Calls `tool` with no input: whether it was a tool error, and its text.
    fn call(&mut self, id: u64, tool: &str) -> (bool, String) {
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":tool,"arguments":{}}}));
        loop {
            let reply = self.reply();
            if reply["id"] == id {
                let result = &reply["result"];
                return (
                    result["isError"] == true,
                    result["content"][0]["text"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned(),
                );
            }
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `host token retire-shared` on this server's shared token file.
fn retire(host: &Host) -> Output {
    let file = host.root.path().join("access-token");
    host.try_token(&["retire-shared", "--token-file", file.to_str().unwrap()])
}

const NO_WRITER: &str = "No client token has write access; add one with `user-memory host token add NAME` first, so the shared file is not the only way in.";

/// The leftovers of an interrupted retire-shared beside the shared file.
fn leftovers(host: &Host) -> Vec<String> {
    fs::read_dir(host.root.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".access-token.retire-"))
        .collect()
}

/// The new value is 64 hex characters, which every released server and
/// client reads as a token, readable only by its owner.
fn assert_replaced(host: &Host) -> String {
    let file = host.root.path().join("access-token");
    let value = fs::read_to_string(&file).unwrap();
    assert_ne!(value, SHARED);
    assert!(
        value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()),
        "the new value has {} characters",
        value.len()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert_eq!(leftovers(host), Vec::<String>::new());
    value
}

/// retire-shared refuses until a client token can write, and refuses a
/// client's token file; then the running server refuses the shared token as
/// revoked at once, its file holds a new value that is never printed, and
/// after a restart that value is refused too.
#[test]
fn retiring_the_shared_file_needs_a_writer_and_refuses_its_token_at_once() {
    let mut host = Host::start();
    // No registry: nothing can write but the shared file.
    let out = retire(&host);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains(NO_WRITER));
    assert!(!host.registry().exists());
    // Read-only, revoked and expired tokens cannot write either.
    host.add("reader", &["--read-only"]);
    host.add("gone", &[]);
    host.token(&["revoke", "gone"]);
    host.add("late", &["--expires", "30"]);
    host.registry_conn()
        .execute(
            "UPDATE client_tokens SET expires_at = ?1 WHERE name = 'late'",
            [now() - 1],
        )
        .unwrap();
    let out = retire(&host);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains(NO_WRITER));
    assert_eq!(
        fs::read_to_string(host.root.path().join("access-token")).unwrap(),
        SHARED
    );
    assert_eq!(host.initialize(SHARED).status, 200);

    // A client's token file is not the shared file.
    let writer = host.add("writer", &[]);
    let client_file = host.root.path().join("writer.token");
    let out = host.try_token(&[
        "retire-shared",
        "--token-file",
        client_file.to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("holds a client token"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read_to_string(&client_file).unwrap(), writer);

    // Nor is any other file given by mistake: it is left as it was.
    let notes = host.root.path().join("notes.txt");
    fs::write(&notes, "line one\nline two\n").unwrap();
    let out = host.try_token(&["retire-shared", "--token-file", notes.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("is not a token file"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read_to_string(&notes).unwrap(), "line one\nline two\n");
    assert!(
        !host
            .registry_conn()
            .query_row("SELECT COUNT(*) FROM settings", [], |row| row
                .get::<_, i64>(0))
            .is_ok_and(|count| count > 0)
    );

    let out = retire(&host);
    let said = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{said}");
    assert!(out.stdout.is_empty());
    let value = assert_replaced(&host);
    assert!(!said.contains(&value), "{said}");
    let refused = host.initialize(SHARED);
    assert_eq!(refused.status, 401);
    assert!(
        refused.challenge.contains(r#"error_description="revoked""#),
        "{}",
        refused.challenge
    );
    assert_eq!(host.initialize(&writer).status, 200);
    let listed = String::from_utf8_lossy(&host.token(&["list"]).stdout).into_owned();
    assert!(!listed.contains("owner (shared file)"), "{listed}");
    assert!(listed.contains("writer"), "{listed}");

    // A restarted server reads the new value and refuses it as well.
    host.restart();
    let refused = host.initialize(&value);
    assert_eq!(refused.status, 401);
    assert!(
        refused.challenge.contains(r#"error_description="revoked""#),
        "{}",
        refused.challenge
    );
    assert_eq!(host.initialize(SHARED).status, 401);
    assert_eq!(host.initialize(&writer).status, 200);
}

/// A retire-shared stopped after writing the new value, or after
/// committing the setting, completes when it is run again.
#[test]
fn a_retire_shared_stopped_at_any_step_completes_on_a_rerun() {
    // Stopped after step 1: a new value beside the file, no setting.
    let host = Host::start();
    let writer = host.add("writer", &[]);
    fs::write(
        host.root
            .path()
            .join(".access-token.retire-00000000000000ff"),
        "f".repeat(64),
    )
    .unwrap();
    assert_eq!(host.initialize(SHARED).status, 200);
    let out = retire(&host);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_replaced(&host);
    assert_eq!(host.initialize(SHARED).status, 401);
    assert_eq!(host.initialize(&writer).status, 200);

    // Stopped after step 2: the setting committed, the file not replaced.
    // A power loss after the rename leaves the same, since the folder is
    // not synced, with the new value possibly still beside the file.
    let host = Host::start();
    let writer = host.add("writer", &[]);
    fs::write(
        host.root
            .path()
            .join(".access-token.retire-00000000000000ee"),
        "e".repeat(64),
    )
    .unwrap();
    host.registry_conn()
        .execute(
            "INSERT INTO settings (key, value) VALUES ('shared_file_retired', '1')",
            [],
        )
        .unwrap();
    assert_eq!(host.initialize(SHARED).status, 401);
    let out = retire(&host);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_replaced(&host);
    assert_eq!(host.initialize(&writer).status, 200);

    // Run again when done: a new value again, nothing else changes.
    let before = fs::read_to_string(host.root.path().join("access-token")).unwrap();
    let out = retire(&host);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_ne!(assert_replaced(&host), before);
}

/// A stand-in memory server on loopback that answers every handshake and
/// refuses every tool call with 401 `revoked`, as a server does when the
/// token is revoked between a call's handshake and its call. Returns its
/// address.
fn refusing_after_the_handshake() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            std::thread::spawn(move || answer_as_stand_in(stream));
        }
    });
    address
}

fn answer_as_stand_in(stream: TcpStream) {
    let mut reader = BufReader::new(stream);
    let mut length = 0;
    let mut line = String::new();
    let Ok(_) = reader.read_line(&mut line) else {
        return;
    };
    let method = line.split_whitespace().next().unwrap_or("").to_owned();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let message: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let (status, extra, body) = match (method.as_str(), message["method"].as_str()) {
        ("POST", Some("initialize")) => (
            "200 OK",
            "Content-Type: application/json\r\n".to_owned(),
            json!({"jsonrpc":"2.0","id":message["id"],"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"user-memory","version":env!("CARGO_PKG_VERSION")}}}).to_string(),
        ),
        ("POST", _) if message.get("id").is_none() => ("202 Accepted", String::new(), String::new()),
        ("POST", _) => (
            "401 Unauthorized",
            "WWW-Authenticate: Bearer error=\"invalid_token\", error_description=\"revoked\"\r\n"
                .to_owned(),
            String::new(),
        ),
        ("GET", _) => ("405 Method Not Allowed", String::new(), String::new()),
        _ => ("200 OK", String::new(), String::new()),
    };
    let mut stream = reader.into_inner();
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// A token revoked after a call's handshake and before its call, as a long
/// call or an import meets it: the call is told why.
#[test]
fn a_token_revoked_between_the_handshake_and_the_call_is_told() {
    let address = refusing_after_the_handshake();
    let url = format!("http://{address}/mcp");
    let root = tempdir().unwrap();
    let file = root.path().join("client.token");
    fs::write(&file, "a".repeat(64)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .arg("index")
        .sandboxed(root.path())
        .env("USER_MEMORY_URL", &url)
        .env("USER_MEMORY_TOKEN_FILE", &file)
        .env_remove("USER_MEMORY_DB")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(said.contains(&sentences(&url)[0].1), "{said}");
}

/// `user-memory server health --port` against this server.
fn health(host: &Host) -> Output {
    let port = host.address.rsplit_once(':').unwrap().1;
    host.command(&["server", "health", "--port", port])
        .output()
        .unwrap()
}

/// `/` needs no token and answers 503 while the store cannot be opened,
/// which the Docker healthcheck (`server health`) reports; a repaired store
/// answers at once, and a healthy answer is reused for a few seconds rather
/// than opening the store for every caller.
#[test]
fn the_health_check_needs_no_token_and_fails_while_the_store_cannot_open() {
    let mut host = Host::start();
    // A store from another schema version, as after a newer binary
    // migrated it, cannot be opened.
    let store = Connection::open(host.db()).unwrap();
    let version: i64 = store
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    store.pragma_update(None, "user_version", 999).unwrap();
    let broken = host.send("GET", "/", "", "");
    assert_eq!(broken.status, 503);
    assert!(
        !broken.body.contains(host.root.path().to_str().unwrap()),
        "{}",
        broken.body
    );
    let out = health(&host);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("The memory store cannot be opened"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    store.pragma_update(None, "user_version", version).unwrap();
    let out = health(&host);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(host.send("GET", "/", "", "").status, 200);
    // Broken again at once: the healthy answer stands until it is stale.
    store.pragma_update(None, "user_version", 999).unwrap();
    assert_eq!(host.send("GET", "/", "", "").status, 200);
    store.pragma_update(None, "user_version", version).unwrap();
    // The server's log names the failure once.
    let log = host.stop();
    assert_eq!(
        log.matches("The memory store cannot be opened").count(),
        1,
        "{log}"
    );

    // Nothing listening.
    assert!(!health(&host).status.success());
}

/// The health check, which needs no token, never creates a missing store or
/// changes one at another schema version.
#[test]
fn the_health_check_never_creates_or_changes_a_store() {
    let host = Host::start();
    let moved = host.root.path().join("moved.sqlite3");
    fs::rename(host.db(), &moved).unwrap();
    for side in ["-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{side}", host.db().display()));
    }
    assert_eq!(host.send("GET", "/", "", "").status, 503);
    assert!(!host.db().exists(), "the check created a store");
    fs::rename(&moved, host.db()).unwrap();

    let store = Connection::open(host.db()).unwrap();
    let version: i64 = store
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    store
        .pragma_update(None, "user_version", version - 1)
        .unwrap();
    assert_eq!(host.send("GET", "/", "", "").status, 503);
    let after: i64 = store
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(after, version - 1, "the check changed the store");
}

/// A store another connection keeps readers out of for a moment, as a
/// commit does on a store not yet on the write-ahead log, is still healthy:
/// the check waits for the lock rather than answering 503.
#[test]
fn the_health_check_waits_for_a_lock_that_keeps_readers_out() {
    let host = Host::start();
    let holder = Connection::open(host.db()).unwrap();
    // Exclusive locking mode keeps readers out from the first read until
    // the connection closes, which the thread does after a moment.
    holder
        .execute_batch("PRAGMA locking_mode = EXCLUSIVE; BEGIN EXCLUSIVE; COMMIT;")
        .unwrap();
    let released = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(1));
        drop(holder);
    });
    let answer = host.send("GET", "/", "", "");
    released.join().unwrap();
    assert_eq!(answer.status, 200, "{}", answer.body);
}

/// A token file an interrupted copy out of the Docker server's volume left
/// in the store's folder is deleted by the next `host token` and when the
/// server starts, once it is old enough that no copy can still be reading
/// it.
#[test]
fn a_copy_left_in_the_store_folder_is_deleted_once_no_copy_can_use_it() {
    let mut host = Host::start();
    let data = host.root.path().join("data");
    let leave = |name: &str, age: Duration| {
        let path = data.join(name);
        fs::write(&path, "umt_left").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() - age)
            .unwrap();
        path
    };
    let minute = Duration::from_secs(60);
    let old = leave(".token-add-00000000000000aa", 11 * minute);
    let fresh = leave(".token-add-00000000000000bb", 9 * minute);
    let other = leave("notes.txt", 11 * minute);
    host.token(&["list"]);
    assert!(!old.exists());
    assert!(fresh.exists() && other.exists());

    let old = leave(".token-add-00000000000000cc", 11 * minute);
    host.restart();
    assert!(!old.exists());
    assert!(fresh.exists() && other.exists());
}

/// A token is never shown where someone could read it: one is issued and
/// used on every path a client or the operator meets, accepted and then
/// refused, and neither the token nor the start of its secret appears in
/// any output, any answer, the connector's log or the server's log, traced.
/// Left out: the installer, which reads a token file it is given and is
/// checked in tests/install.rs, and the Docker server's copy, checked in
/// `host_token_runs_in_the_docker_server_and_retires_its_shared_file`.
#[test]
fn no_output_or_log_shows_a_token() {
    let mut host = Host::start_with(&[("USER_MEMORY_TEST_TRACE", "1")]);
    let url = format!("http://{}/mcp", host.address);
    let file = host.root.path().join("laptop.token");
    let text = |out: &Output| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    };
    let mut seen: Vec<(&str, String)> = Vec::new();
    seen.push((
        "add",
        text(&host.token(&["add", "laptop", "--out", file.to_str().unwrap()])),
    ));
    let token = fs::read_to_string(&file).unwrap();
    seen.push(("list", text(&host.token(&["list"]))));
    seen.push(("state", text(&host.token(&["state", "--name", "laptop"]))));
    let answer = host.initialize(&token);
    assert_eq!(answer.status, 200);
    seen.push((
        "an accepted request",
        format!("{}{}", answer.challenge, answer.body),
    ));
    let payload = json!({"session_id":"s","cwd":host.root.path(),"hook_event_name":"SessionStart"})
        .to_string();
    let exported = host.root.path().join("exported.json");
    let calls: [(&str, Vec<&str>, &str); 5] = [
        (
            "doctor",
            vec![
                "doctor",
                "--url",
                &url,
                "--token-file",
                file.to_str().unwrap(),
            ],
            "",
        ),
        (
            "the session hook",
            vec!["session-start", "--client", "claude"],
            &payload,
        ),
        ("a call", vec!["index"], ""),
        ("export", vec!["export", exported.to_str().unwrap()], ""),
        ("import", vec!["import", exported.to_str().unwrap()], ""),
    ];
    for (path, args, stdin) in &calls {
        seen.push((path, text(&client(&host, &file, args, stdin.as_bytes()))));
    }
    let mut bridge = Bridge::start(&host, &file);
    bridge.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"app","version":"1"}}}));
    seen.push(("the bridge's handshake", bridge.reply().to_string()));
    bridge.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    seen.push(("the bridge's call", bridge.call(2, "index").1));

    seen.push(("revoke", text(&host.token(&["revoke", "laptop"]))));
    let refused = host.initialize(&token);
    assert_eq!(refused.status, 401);
    seen.push((
        "a refused request",
        format!("{}{}", refused.challenge, refused.body),
    ));
    seen.push(("the bridge's refusal", bridge.call(3, "index").1));
    drop(bridge);
    for (path, args, stdin) in &calls {
        seen.push((path, text(&client(&host, &file, args, stdin.as_bytes()))));
    }
    seen.push((
        "the connector's log",
        fs::read_to_string(host.root.path().join("bridge.log")).unwrap_or_default(),
    ));
    let log = host.stop();
    assert!(log.contains("registry loaded"), "traced:\n{log}");
    seen.push(("the server's log", log));

    // The secret's first characters, after the public prefix.
    let start = &token[..12];
    for (path, said) in &seen {
        assert!(!said.contains(start), "{path} shows the token:\n{said}");
    }
}

#[test]
fn a_refused_request_whose_body_comes_late_still_reads_its_401() {
    let host = Host::start();
    let mut failed = Vec::new();
    for round in 0..LATE_ROUNDS {
        match host.late_initialize("not-a-token", true) {
            Ok(answer) if answer.starts_with("HTTP/1.1 401") => {}
            other => failed.push(format!("{round}: {other:?}")),
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

#[test]
fn a_refused_request_whose_body_never_comes_is_answered_after_the_wait() {
    let host = Host::start();
    let started = std::time::Instant::now();
    let answer = host.late_initialize("not-a-token", false).unwrap();
    assert!(answer.starts_with("HTTP/1.1 401"), "{answer}");
    let waited = started.elapsed();
    assert!(waited < NO_BODY_ANSWER, "{waited:?}");
}
