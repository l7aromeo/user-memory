#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

//! A session's use of memories (#123), through the real binary: what
//! `POST /use` writes to a memory server's store, what it refuses, and how
//! the rows are merged, pruned and forgotten.

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
    process::{Child, Command, Stdio},
};
use tempfile::{TempDir, tempdir};

/// The shared token file's token, which may write.
const SHARED: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
/// The project the tests' sessions work in.
const PROJECT: &str = "github.com/example/app";
/// Sessions per project whose use counts (`USE_WINDOW`).
const USE_WINDOW: usize = 40;

/// A use row: memory, project, origin, read, recorded, shown, held,
/// overridden.
type UseRow = (
    String,
    String,
    String,
    i64,
    i64,
    i64,
    Option<i64>,
    Option<i64>,
);

struct Host {
    root: TempDir,
    child: Child,
    address: String,
}

impl Host {
    fn start(env: &[(&str, &str)]) -> Self {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("data")).unwrap();
        fs::write(root.path().join("access-token"), SHARED).unwrap();
        let db = root.path().join("data").join("memories.sqlite3");
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .args(["host", "--listen", "127.0.0.1:0", "--db"])
            .arg(&db)
            .arg("--token-file")
            .arg(root.path().join("access-token"))
            .sandboxed(root.path())
            .env_remove("USER_MEMORY_DB")
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(line.starts_with("Listening: http://"), "{line}");
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
        }
    }

    fn db(&self) -> PathBuf {
        self.root.path().join("data").join("memories.sqlite3")
    }

    /// `user-memory ARGS` on the server's store, as its operator would.
    fn cli(&self, args: &[&str]) -> Value {
        let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
            .args(args)
            .sandboxed(self.root.path())
            .env("USER_MEMORY_DB", self.db())
            .env_remove("USER_MEMORY_URL")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// A memory recorded in `project`, or global without one; its id.
    fn record(&self, title: &str, project: Option<&str>) -> String {
        let mut args = vec![
            "record",
            "--category",
            "workflow",
            "--title",
            title,
            "--content",
            title,
        ];
        match project {
            Some(project) => args.extend(["--scope", "project", "--project", project]),
            None => args.extend(["--scope", "global"]),
        }
        self.cli(&args)["id"].as_str().unwrap().to_owned()
    }

    /// A read-only client token, read from the file `host token add` wrote.
    fn reader(&self) -> String {
        let file = self.root.path().join("reader.token");
        let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
            .args(["host", "token", "add", "reader", "--read-only", "--out"])
            .arg(&file)
            .arg("--db")
            .arg(self.db())
            .sandboxed(self.root.path())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        fs::read_to_string(file).unwrap()
    }

    /// `POST /use` with `token` and `body`: the status and the body.
    fn post(&self, token: &str, body: &str) -> (u16, String) {
        self.post_declaring(token, body.len(), body)
    }

    /// `POST /use` declaring a body of `length` bytes and sending `body`.
    fn post_declaring(&self, token: &str, length: usize, body: &str) -> (u16, String) {
        let mut socket = TcpStream::connect(&self.address).unwrap();
        socket.set_read_timeout(Some(STUCK)).unwrap();
        write!(
            socket,
            "POST /use HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nAuthorization: Bearer {token}\r\nConnection: close\r\nContent-Length: {length}\r\n\r\n{body}",
            self.address,
        )
        .unwrap();
        let mut response = Vec::new();
        socket.read_to_end(&mut response).unwrap();
        let text = String::from_utf8_lossy(&response).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, body.to_owned())
    }

    /// Every use row: memory, project, origin, read, recorded, shown, held,
    /// overridden.
    fn rows(&self) -> Vec<UseRow> {
        let conn = Connection::open(self.db()).unwrap();
        let mut statement = conn
            .prepare("SELECT memory,project,origin,read,recorded,shown,held,overridden FROM memory_use ORDER BY 1,2,3")
            .unwrap();
        statement
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// The store's latest change, which use must leave alone (#119).
    fn last_change(&self) -> i64 {
        Connection::open(self.db())
            .unwrap()
            .query_row("SELECT last_change FROM store_changes", [], |r| r.get(0))
            .unwrap()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One use row's JSON.
fn used(memory: &str, project: &str, read: bool, recorded: bool, shown: bool) -> Value {
    json!({"memory": memory, "project": project, "read": read, "recorded": recorded, "shown": shown})
}

// A session's rows are upserted by memory, project and origin, their flags
// merged, so sending again only adds; a memory not stored, or a project
// memory's row for another project, is skipped. No change marker moves.
// Forgetting a memory removes its rows.
#[test]
fn a_session_s_use_is_merged_by_memory_project_and_origin() {
    let host = Host::start(&[]);
    let global = host.record("Run the fast tests first", None);
    let own = host.record("Build with the release profile", Some(PROJECT));
    let other = host.record(
        "Deploy from the main branch",
        Some("github.com/example/site"),
    );
    let missing = "1a2b3c4d-9999-4aaa-8aaa-000000000099";
    let before = host.last_change();
    let body = json!({"origin": "claude:session/one", "uses": [
        used(&global, PROJECT, true, false, false),
        used(&own, PROJECT, false, true, false),
        used(&other, PROJECT, true, false, false),
        used(missing, PROJECT, true, false, false),
    ]});
    let (status, reply) = host.post(SHARED, &body.to_string());
    assert_eq!(status, 200, "{reply}");
    assert_eq!(
        serde_json::from_str::<Value>(&reply).unwrap(),
        json!({"written": 2, "skipped": 2})
    );
    // Sent again with other flags: they are merged, never cleared.
    let mut again = used(&global, PROJECT, false, false, true);
    again["held"] = json!(true);
    let body = json!({"origin": "claude:session/one", "uses": [again]});
    assert_eq!(host.post(SHARED, &body.to_string()).0, 200);
    let one = "claude:session/one".to_owned();
    let mut expected = vec![
        (
            global.clone(),
            PROJECT.to_owned(),
            one.clone(),
            1,
            0,
            1,
            Some(1),
            None,
        ),
        (
            own.clone(),
            PROJECT.to_owned(),
            one.clone(),
            0,
            1,
            0,
            None,
            None,
        ),
    ];
    expected.sort();
    assert_eq!(host.rows(), expected);
    assert_eq!(host.last_change(), before, "use moved the change marker");
    host.cli(&["forget", &global]);
    assert_eq!(
        host.rows(),
        [(own, PROJECT.to_owned(), one, 0, 1, 0, None, None)]
    );
}

// A project keeps the use of its newest USE_WINDOW sessions: a session
// past them drops the oldest.
#[test]
fn a_project_keeps_its_newest_sessions_use() {
    let host = Host::start(&[]);
    let memory = host.record("Run the fast tests first", None);
    for n in 0..=USE_WINDOW {
        let body = json!({"origin": format!("claude:session/{n:02}"), "uses": [used(&memory, PROJECT, true, false, false)]});
        assert_eq!(host.post(SHARED, &body.to_string()).0, 200);
        // Each session a second later than the last, as real sessions are.
        Connection::open(host.db())
            .unwrap()
            .execute("UPDATE memory_use SET at=datetime(at,'-1 second')", [])
            .unwrap();
    }
    let origins: Vec<String> = host.rows().into_iter().map(|row| row.2).collect();
    assert_eq!(origins.len(), USE_WINDOW);
    assert!(
        !origins.contains(&"claude:session/00".to_owned()),
        "{origins:?}"
    );
    assert!(origins.contains(&format!("claude:session/{USE_WINDOW:02}")));
}

// A malformed request is refused with its reason and writes nothing; a
// body over 256 KiB is refused before it is read; a read-only token is
// refused as read-only refusals are, so its sessions never count.
#[test]
fn use_is_refused_with_its_reason_and_writes_nothing() {
    let host = Host::start(&[]);
    let memory = host.record("Run the fast tests first", None);
    let row = used(&memory, PROJECT, true, false, false);
    let many: Vec<Value> = (0..1001).map(|_| row.clone()).collect();
    for (body, reason) in [
        ("{not json".to_owned(), "Malformed use request"),
        (json!({"origin": "claude:session/one"}).to_string(), "Malformed use request"),
        (
            json!({"origin": "claude:session/one", "uses": [{"memory": memory, "project": PROJECT, "read": "yes", "recorded": false, "shown": false}]}).to_string(),
            "Malformed use request",
        ),
        (
            json!({"origin": "claude:session/one", "uses": []}).to_string(),
            "uses has 0 rows; send 1 to 1000 per request",
        ),
        (
            json!({"origin": "claude:session/one", "uses": many}).to_string(),
            "uses has 1001 rows; send 1 to 1000 per request",
        ),
        (
            json!({"origin": "x".repeat(241), "uses": [row]}).to_string(),
            "origin has 241 characters; it must have 1 to 240",
        ),
        (
            json!({"origin": "claude:session/one", "uses": [{"memory": "e0000001", "project": PROJECT, "read": true, "recorded": false, "shown": false}]}).to_string(),
            "memory \"e0000001\" is not a memory's full id",
        ),
        (
            json!({"origin": "claude:session/one", "uses": [used(&memory, "/home/someone/app", true, false, false)]}).to_string(),
            "local path",
        ),
    ] {
        let (status, reply) = host.post(SHARED, &body);
        assert_eq!(status, 400, "{reason}: {reply}");
        assert!(reply.contains(reason), "{reason}: {reply}");
    }
    // Declared over the limit, the body is refused before it is sent.
    let (status, reply) = host.post_declaring(SHARED, 300 * 1024, "");
    assert_eq!(status, 413, "{reply}");
    assert_eq!(reply, "A use request is limited to 256 KiB");
    let reader = host.reader();
    let body = json!({"origin": "claude:session/one", "uses": [row]});
    let (status, reply) = host.post(&reader, &body.to_string());
    assert_eq!(status, 403, "{reply}");
    assert_eq!(
        reply,
        format!(
            "This machine's token for http://{}/mcp is read-only; this needs a token with write access from the server's operator.",
            host.address
        )
    );
    assert!(host.rows().is_empty());
}
