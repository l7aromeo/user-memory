//! `serve` handing itself over to a replaced binary: the session goes on,
//! on the same connection and with no new handshake, answered by the new
//! code. Every case runs through the real binary, copied into a folder of
//! its own; the "newer build" is the fake user-memory, which runs the real
//! test build with a debug build's test version (tests/fake/cli.rs). On
//! Unix each case runs twice: handing over by `exec`, and by the relay
//! Windows uses, which `USER_MEMORY_TEST_RELAY` turns on there.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/fake.rs"]
mod fake;
#[path = "support/port.rs"]
mod port;
#[path = "support/replace.rs"]
mod replace;
#[path = "support/sandbox.rs"]
mod sandbox;

use sandbox::Sandboxed;
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tempfile::{TempDir, tempdir};

/// How long a test waits for a reply: a hand-over checks the new binary by
/// running it twice, which a loaded machine can take seconds to start.
const REPLY_WAIT: Duration = Duration::from_secs(60);

/// The version the newer build reports.
const NEWER: &str = "9.9.9";

/// The listen stream's id in the tests' 2026-07-28 sessions.
const LISTEN: &str = "listen";

/// How the app opens the session.
#[derive(Clone, Copy, Debug)]
enum Opening {
    /// `initialize` on 2025-06-18, as Codex does.
    Initialize,
    /// `server/discover` on 2026-07-28, as Claude Code does.
    Discover,
    /// No handshake at all, which 2026-07-28 allows: every request names
    /// its protocol in `_meta`.
    Bare,
}

/// The `_meta` every 2026-07-28 request carries.
fn meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1"},
        "io.modelcontextprotocol/clientCapabilities": {},
    })
}

/// How `serve` hands over on this platform, as the tests run it.
#[derive(Clone, Copy, Debug)]
enum Way {
    /// `exec`, on Unix.
    Exec,
    /// The relay: Windows' own way, forced on Unix by the debug build.
    Relay,
}

fn ways() -> Vec<Way> {
    if cfg!(windows) {
        vec![Way::Relay]
    } else {
        vec![Way::Exec, Way::Relay]
    }
}

/// A folder holding the copy of user-memory a session runs from, its store
/// and its sandbox.
struct Place {
    dir: TempDir,
}

impl Place {
    fn new() -> Self {
        let place = Self {
            dir: tempdir().unwrap(),
        };
        fs::create_dir_all(place.bin()).unwrap();
        fs::copy(env!("CARGO_BIN_EXE_user-memory"), place.binary()).unwrap();
        place
    }
    fn bin(&self) -> PathBuf {
        self.dir.path().join("bin")
    }
    fn binary(&self) -> PathBuf {
        self.bin()
            .join(format!("user-memory{}", std::env::consts::EXE_SUFFIX))
    }
    fn db(&self) -> PathBuf {
        self.dir.path().join("memories.sqlite3")
    }
    /// The hand-over state files left in the sandbox's state folder: none,
    /// once each hand-over is over, whatever its outcome.
    fn state_files(&self) -> Vec<String> {
        let state = sandbox::folder(self.dir.path(), "state");
        fs::read_dir(state)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.starts_with("handover-"))
                    .collect()
            })
            .unwrap_or_default()
    }
    /// Replaces the binary with `new` as an upgrade does: in one rename, the
    /// running one renamed aside first on Windows, which can't replace a
    /// running program.
    fn replace_with(&self, new: &Path) {
        let staged = self
            .bin()
            .join(format!(".new{}", std::env::consts::EXE_SUFFIX));
        let _ = fs::remove_file(&staged);
        fs::copy(new, &staged).unwrap();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let aside = self.bin().join(format!(
            ".user-memory-old-{nanos}{}",
            std::env::consts::EXE_SUFFIX
        ));
        replace::replace_running(&self.binary(), &staged, &aside);
    }
    /// Replaces the binary with the newer build, reporting `version`.
    fn upgrade(&self) {
        let fakes = self.dir.path().join("fakes");
        fs::create_dir_all(&fakes).unwrap();
        self.replace_with(&fake::fake(&fakes, "user-memory"));
    }
    /// The newer build, which fails at once: a binary that is there but
    /// does not run.
    fn break_binary(&self) {
        let fakes = self.dir.path().join("broken");
        fs::create_dir_all(&fakes).unwrap();
        self.replace_with(&fake::fake(&fakes, "user-memory"));
        // Read beside the copy that runs.
        fs::write(self.bin().join("user-memory.exit"), "1").unwrap();
    }
    fn serve(&self, way: Way, env: &[(&str, &str)]) -> Session {
        self.serve_as(way, env, Opening::Initialize)
    }
    /// Replaces the binary with another copy of the newer build, which
    /// reports `version`: a second upgrade in the same session.
    fn upgrade_again(&self, version: &str) {
        let fakes = self.dir.path().join("fakes-again");
        fs::create_dir_all(&fakes).unwrap();
        let fake = fake::fake(&fakes, "user-memory");
        fs::write(self.bin().join("user-memory.version"), version).unwrap();
        self.replace_with(&fake);
    }
    fn serve_as(&self, way: Way, env: &[(&str, &str)], opening: Opening) -> Session {
        let mut command = Command::new(self.binary());
        command
            .arg("serve")
            .current_dir(self.dir.path())
            .env("USER_MEMORY_DB", self.db())
            .env("FAKE_USER_MEMORY", env!("CARGO_BIN_EXE_user-memory"))
            .env("FAKE_USER_MEMORY_VERSION", NEWER)
            // Each hand-over decision on stderr, for a failure to show.
            .env("USER_MEMORY_TEST_TRACE", "1")
            .envs(env.iter().copied())
            .sandboxed(self.dir.path());
        if matches!(way, Way::Relay) {
            command.env("USER_MEMORY_TEST_RELAY", "1");
        }
        Session::start(command, opening)
    }
}

/// A `serve` session as an app holds it: one connection, read line by line
/// on a thread so a missing reply fails the test instead of hanging it.
struct Session {
    child: Child,
    input: Option<ChildStdin>,
    lines: mpsc::Receiver<Value>,
    errors: mpsc::Receiver<String>,
    /// Messages read while waiting for another, such as notifications.
    seen: Vec<Value>,
    /// The `_meta` each request carries: on 2026-07-28 only.
    meta: Option<Value>,
}

impl Session {
    fn start(mut command: Command, opening: Opening) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (to_lines, lines) = mpsc::channel();
        let output = BufReader::new(child.stdout.take().unwrap());
        thread::spawn(move || {
            for line in output.lines() {
                let Ok(line) = line else { return };
                let message = serde_json::from_str(&line)
                    .unwrap_or_else(|_| panic!("serve wrote a line that isn't JSON: {line}"));
                if to_lines.send(message).is_err() {
                    return;
                }
            }
        });
        let (to_errors, errors) = mpsc::channel();
        let mut stderr = child.stderr.take().unwrap();
        thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            let _ = to_errors.send(text);
        });
        let mut session = Self {
            input: child.stdin.take(),
            child,
            lines,
            errors,
            seen: Vec::new(),
            meta: None,
        };
        match opening {
            Opening::Initialize => {
                session.write(&[
                    json!({"jsonrpc":"2.0","id":"init","method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
                    json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                    json!({"jsonrpc":"2.0","id":"tools","method":"tools/list","params":{}}),
                ]);
                session.reply("init");
            }
            Opening::Discover => {
                session.meta = Some(meta());
                session.write(&[
                    json!({"jsonrpc":"2.0","id":"init","method":"server/discover","params":{"_meta":meta()}}),
                    json!({"jsonrpc":"2.0","id":"tools","method":"tools/list","params":{"_meta":meta()}}),
                ]);
                session.reply("init");
            }
            Opening::Bare => {
                session.meta = Some(meta());
                session.write(&[
                    json!({"jsonrpc":"2.0","id":"tools","method":"tools/list","params":{"_meta":meta()}}),
                ]);
            }
        }
        session.reply("tools");
        session
    }
    /// Opens a listen stream with `notifications` as its filter.
    fn listen_message(&self, notifications: Value) -> Value {
        json!({"jsonrpc":"2.0","id":LISTEN,"method":"subscriptions/listen","params":{"_meta":self.meta,"notifications":notifications}})
    }
    /// Opens the listen stream for tool list changes and returns what
    /// acknowledged it.
    fn listen(&mut self) -> Value {
        self.write(&[self.listen_message(json!({"toolsListChanged": true}))]);
        // The acknowledgement is a notification: it comes before the reply.
        let marker = self.call("after-listen", "index");
        assert_eq!(marker["result"]["isError"], false, "{marker}");
        self.seen
            .iter()
            .find(|m| m["method"] == "notifications/subscriptions/acknowledged")
            .cloned()
            .unwrap_or_else(|| panic!("no acknowledgement; seen {:?}", self.seen))
    }
    /// Closes the session as [`Session::close`] does and returns serve's
    /// stderr and every message it wrote that no reply was awaited for: a
    /// notification the new code sends once its tools are in may come
    /// after the next reply, but always before serve ends.
    fn finish(mut self) -> (String, Vec<Value>) {
        drop(self.input.take());
        let deadline = Instant::now() + REPLY_WAIT;
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "serve did not end");
            thread::sleep(Duration::from_millis(20));
        }
        let mut seen = std::mem::take(&mut self.seen);
        seen.extend(self.lines.iter());
        let stderr = self.errors.recv_timeout(REPLY_WAIT).unwrap_or_default();
        (stderr, seen)
    }
    /// Writes `messages` to serve in one write.
    fn write(&mut self, messages: &[Value]) {
        let bytes: String = messages.iter().map(|m| format!("{m}\n")).collect();
        let input = self.input.as_mut().unwrap();
        input.write_all(bytes.as_bytes()).unwrap();
        input.flush().unwrap();
    }
    fn call_message(&self, id: &str, tool: &str) -> Value {
        let mut message = json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":tool,"arguments":{}}});
        if let Some(meta) = &self.meta {
            message["params"]["_meta"] = meta.clone();
        }
        message
    }
    /// The reply to request `id`, keeping what came before it in `seen`.
    fn reply(&mut self, id: &str) -> Value {
        let deadline = Instant::now() + REPLY_WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(message) if message["id"] == id => return message,
                Ok(message) => self.seen.push(message),
                Err(_) => panic!(
                    "no reply to {id}; seen {:?}; stderr: {}",
                    self.seen,
                    self.stderr_now()
                ),
            }
        }
    }
    /// Calls `tool` and returns its whole reply.
    fn call(&mut self, id: &str, tool: &str) -> Value {
        self.write(&[self.call_message(id, tool)]);
        self.reply(id)
    }
    /// Who answers: the version `debug_version` names, or `None` when the
    /// code serving has no such tool (the real build).
    fn version(&mut self, id: &str) -> Option<String> {
        let reply = self.call(id, "debug_version");
        let text = reply.pointer("/result/content/0/text")?.as_str()?;
        Some(serde_json::from_str::<Value>(text).map_or_else(
            |_| text.to_owned(),
            |answer| answer["version"].as_str().unwrap_or_default().to_owned(),
        ))
    }
    fn notified(&self, method: &str) -> usize {
        self.seen.iter().filter(|m| m["method"] == method).count()
    }
    fn stderr_now(&self) -> String {
        self.errors.try_recv().unwrap_or_default()
    }
    /// Closes the session as the app does and returns what serve wrote to
    /// stderr; serve must end on its own.
    fn close(mut self) -> String {
        drop(self.input.take());
        let deadline = Instant::now() + REPLY_WAIT;
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "serve did not end");
            thread::sleep(Duration::from_millis(20));
        }
        self.errors.recv_timeout(REPLY_WAIT).unwrap_or_default()
    }
}

/// The messages in `seen` on the listen stream: those carrying its id.
fn on_listen(seen: &[Value]) -> Vec<&Value> {
    seen.iter()
        .filter(|m| {
            m.pointer("/params/_meta/io.modelcontextprotocol~1subscriptionId")
                == Some(&json!(LISTEN))
        })
        .collect()
}

/// How many messages in `seen` are `method`.
fn count(seen: &[Value], method: &str) -> usize {
    seen.iter().filter(|m| m["method"] == method).count()
}

fn texts(reply: &Value) -> Vec<String> {
    reply
        .pointer("/result/content")
        .and_then(Value::as_array)
        .map(|content| {
            content
                .iter()
                .filter_map(|item| item["text"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_replaced_binary_answers_the_next_calls_on_the_same_connection() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve(way, &[]);
        let pid = session.child.id();
        assert_eq!(session.version("before"), None, "{way:?}");
        place.upgrade();
        // Two messages in one write around the hand-over: the first finds
        // the binary replaced, and both reach the new code.
        session.write(&[
            session.call_message("first", "debug_version"),
            session.call_message("second", "index"),
        ]);
        let first = session.reply("first");
        let answer: Value = serde_json::from_str(&texts(&first).concat()).unwrap_or_default();
        assert_eq!(answer["version"], NEWER, "{way:?}: {first}");
        let second = session.reply("second");
        assert_eq!(second["result"]["isError"], false, "{way:?}: {second}");
        assert_eq!(session.version("after"), Some(NEWER.into()), "{way:?}");
        assert_eq!(session.child.id(), pid);
        assert_eq!(place.state_files(), Vec::<String>::new(), "{way:?}");
        let (stderr, seen) = session.finish();
        // The new build adds a tool, so the app is told to reload them, as
        // a session opened by `initialize` always was: with no stream.
        let changed: Vec<&Value> = seen
            .iter()
            .filter(|m| m["method"] == "notifications/tools/list_changed")
            .collect();
        assert_eq!(
            changed,
            [&json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})],
            "{way:?}: {seen:?}"
        );
        assert!(
            stderr.contains(&format!(
                "user-memory {NEWER}: took over this session from {}",
                env!("CARGO_PKG_VERSION")
            )),
            "{way:?}: {stderr}"
        );
    }
}

/// A replacement with the same tools changes nothing the app sees.
#[test]
fn an_unchanged_tool_list_is_not_announced() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve(
            way,
            &[("FAKE_USER_MEMORY_VERSION", env!("CARGO_PKG_VERSION"))],
        );
        place.upgrade();
        let reply = session.call("call", "index");
        assert_eq!(reply["result"]["isError"], false, "{way:?}: {reply}");
        assert_eq!(
            session.notified("notifications/tools/list_changed"),
            0,
            "{way:?}"
        );
        let stderr = session.close();
        assert!(
            stderr.contains("took over this session"),
            "{way:?}: {stderr}\nreply: {reply}"
        );
    }
}

/// An upgrade replaces the file by rename, so a new file with the old size
/// and modified time is still a new binary.
#[test]
fn a_replacement_with_the_old_size_and_time_is_noticed_by_its_file_id() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve(way, &[]);
        let copy = place
            .dir
            .path()
            .join(format!("copy{}", std::env::consts::EXE_SUFFIX));
        fs::copy(place.binary(), &copy).unwrap();
        let before = fs::metadata(place.binary()).unwrap();
        place.replace_with(&copy);
        // Set after the rename: a copy keeps the old time on macOS but not
        // on Linux.
        fs::File::options()
            .write(true)
            .open(place.binary())
            .unwrap()
            .set_modified(before.modified().unwrap())
            .unwrap();
        let after = fs::metadata(place.binary()).unwrap();
        assert_eq!(
            (after.len(), after.modified().unwrap()),
            (before.len(), before.modified().unwrap())
        );
        let reply = session.call("call", "index");
        assert_eq!(reply["result"]["isError"], false, "{way:?}: {reply}");
        let stderr = session.close();
        assert!(
            stderr.contains("took over this session"),
            "{way:?}: {stderr}\nreply: {reply}"
        );
    }
}

/// The store migrated to a newer schema between two calls, as an upgrade
/// does: the old code refuses it, the new code it hands over to serves it.
#[test]
fn a_store_migrated_between_two_calls_is_served_after_the_hand_over() {
    let newer = newer_schema();
    let schema = newer.to_string();
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve(way, &[("FAKE_USER_MEMORY_SCHEMA", &schema)]);
        let first = session.call("first", "index");
        assert_eq!(first["result"]["isError"], false, "{way:?}: {first}");
        set_schema_version(&place.db(), newer);
        let refused = session.call("refused", "index");
        assert!(
            texts(&refused)
                .concat()
                .contains("this user-memory is older than the store"),
            "{way:?}: {refused}"
        );
        place.upgrade();
        let second = session.call("second", "index");
        assert_eq!(second["result"]["isError"], false, "{way:?}: {second}");
        session.close();
    }
}

/// A schema version past this build's, as an upgrade's newer build has:
/// one more than a store this build creates gets.
fn newer_schema() -> i64 {
    let dir = tempdir().unwrap();
    let db = dir.path().join("memories.sqlite3");
    let status = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .arg("search")
        .env("USER_MEMORY_DB", &db)
        .sandboxed(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "creating a store failed");
    let current: i64 = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    current + 1
}

fn set_schema_version(db: &Path, version: i64) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.pragma_update(None, "user_version", version).unwrap();
}

/// Setup replaces the binary, then migrates the store: a call in between
/// waits for the migration instead of being refused.
#[test]
fn a_handed_over_serve_waits_for_a_running_migration_then_answers() {
    let newer = newer_schema();
    let schema = newer.to_string();
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve(way, &[("FAKE_USER_MEMORY_SCHEMA", &schema)]);
        let first = session.call("first", "index");
        assert_eq!(first["result"]["isError"], false, "{way:?}: {first}");
        place.upgrade();
        let db = place.db();
        let migration = thread::spawn(move || {
            thread::sleep(Duration::from_secs(1));
            set_schema_version(&db, newer);
            Instant::now()
        });
        session.write(&[session.call_message("waits", "index")]);
        let reply = session.reply("waits");
        let migrated = migration.join().unwrap();
        assert_eq!(reply["result"]["isError"], false, "{way:?}: {reply}");
        assert!(Instant::now() >= migrated, "answered before the migration");
        // The new code, which needs the migrated store, answered it.
        assert_eq!(session.version("after"), Some(NEWER.into()), "{way:?}");
        session.close();
    }
}

/// A migration that never comes is waited for only as long as the call's
/// budget (#227), not the hand-over's own 50 s.
#[test]
fn a_handed_over_serve_waits_for_a_migration_no_longer_than_the_calls_budget() {
    let schema = newer_schema().to_string();
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve(
            way,
            &[
                ("FAKE_USER_MEMORY_SCHEMA", &schema),
                ("USER_MEMORY_TOOL_CALL_BUDGET_MS", "2000"),
            ],
        );
        let first = session.call("first", "index");
        assert_eq!(first["result"]["isError"], false, "{way:?}: {first}");
        place.upgrade();
        let started = Instant::now();
        session.write(&[session.call_message("waits", "index")]);
        let reply = session.reply("waits");
        let took = started.elapsed();
        assert_eq!(reply["result"]["isError"], true, "{way:?}: {reply}");
        // The hand-over and the call's own work on a loaded machine, far
        // under the 50 s wait.
        assert!(
            took < Duration::from_millis(2000 + 5000),
            "{way:?}: answered after {took:?}"
        );
        session.close();
    }
}

/// A replacement that is older, or doesn't run, leaves the session on the
/// code serving it, which says so once in the next tool reply.
#[test]
fn an_older_or_broken_replacement_keeps_the_session_and_says_so_once() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve(way, &[("FAKE_USER_MEMORY_VERSION", "0.0.1")]);
        place.upgrade();
        let told = session.call("told", "index");
        assert_eq!(told["result"]["isError"], false, "{way:?}: {told}");
        let notice = texts(&told).last().cloned().unwrap_or_default();
        assert_eq!(
            notice,
            format!(
                "user-memory 0.0.1 replaced the binary this session runs ({0}), but it is older, so this session keeps {0}; restart your client to run 0.0.1.",
                env!("CARGO_PKG_VERSION")
            ),
            "{way:?}"
        );
        let again = session.call("again", "index");
        assert_eq!(texts(&again).len(), 1, "{way:?}: said twice: {again}");
        // The notice is one more text item: the call's own reply, which a
        // client reads from the first item, is unchanged.
        let mut own = told["result"].clone();
        own["content"].as_array_mut().unwrap().pop();
        assert_eq!(own, again["result"], "{way:?}");
        assert_eq!(session.version("serving"), None, "{way:?}");

        place.break_binary();
        let broken = session.call("broken", "index");
        let notice = texts(&broken).last().cloned().unwrap_or_default();
        assert!(
            notice.starts_with("user-memory was replaced on disk, but the new binary can't take over this session (it ended with"),
            "{way:?}: {broken}"
        );
        assert_eq!(texts(&session.call("quiet", "index")).len(), 1, "{way:?}");
        assert_eq!(place.state_files(), Vec::<String>::new(), "{way:?}");
        let stderr = session.close();
        assert!(!stderr.contains("took over"), "{way:?}: {stderr}");
    }
}

/// A message that arrives while a request is in flight goes to the code
/// handling it; the hand-over waits until nothing is in flight.
#[test]
fn no_hand_over_while_a_request_is_in_flight() {
    for way in ways() {
        let place = Place::new();
        // A new store's first open pauses until the file is removed.
        let pause = place.dir.path().join("pause");
        fs::write(&pause, "").unwrap();
        let mut session = place.serve(way, &[("USER_MEMORY_PAUSE_OPEN", pause.to_str().unwrap())]);
        session.write(&[session.call_message("slow", "index")]);
        let reached = place.dir.path().join("pause.reached");
        let deadline = Instant::now() + REPLY_WAIT;
        while !reached.exists() {
            assert!(
                Instant::now() < deadline,
                "{way:?}: the call never opened the store"
            );
            thread::sleep(Duration::from_millis(20));
        }
        place.upgrade();
        assert_eq!(
            session.version("during"),
            None,
            "{way:?}: handed over in flight"
        );
        fs::remove_file(&pause).unwrap();
        let slow = session.reply("slow");
        assert_eq!(slow["result"]["isError"], false, "{way:?}: {slow}");
        assert_eq!(session.version("after"), Some(NEWER.into()), "{way:?}");
        session.close();
    }
}

/// Windows has no `exec`: the relay serves through a child, and a second
/// upgrade replaces that child rather than relaying to a relay.
#[test]
fn a_second_upgrade_restarts_the_relays_child_without_a_chain() {
    let place = Place::new();
    let mut session = place.serve(Way::Relay, &[]);
    place.upgrade();
    let first = serving(&mut session, "first");
    assert_eq!(first["version"], NEWER);
    let relay = session.child.id();
    let child = first["pid"].as_u64().unwrap();
    assert_ne!(child, u64::from(relay));
    // The second upgrade: another newer build, a file of its own.
    let fakes = place.dir.path().join("fakes2");
    fs::create_dir_all(&fakes).unwrap();
    place.replace_with(&fake::fake(&fakes, "user-memory"));
    let second = serving(&mut session, "second");
    assert_ne!(second["pid"], first["pid"], "the same child serves");
    let deadline = Instant::now() + REPLY_WAIT;
    while running(child) {
        assert!(
            Instant::now() < deadline,
            "the first child still runs: a relay chain"
        );
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(session.child.id(), relay);
    let stderr = session.close();
    assert_eq!(
        stderr.matches("took over this session").count(),
        2,
        "{stderr}"
    );
}

/// What `debug_version` answers: the version and process serving.
fn serving(session: &mut Session, id: &str) -> Value {
    let reply = session.call(id, "debug_version");
    let text = texts(&reply).concat();
    serde_json::from_str(&text).unwrap_or_else(|_| panic!("{reply}"))
}

/// Whether process `pid` still runs.
fn running(pid: u64) -> bool {
    if cfg!(windows) {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains(&pid.to_string()))
    } else {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}

/// A replacement that passed its checks but couldn't be started leaves the
/// session on its code, says so once, and leaves no state file behind.
#[test]
fn a_failed_hand_over_keeps_the_session_and_its_state_file_goes() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve(way, &[("USER_MEMORY_TEST_EXEC_FAIL", "1")]);
        place.upgrade();
        let told = session.call("told", "index");
        assert_eq!(told["result"]["isError"], false, "{way:?}: {told}");
        let notice = texts(&told).last().cloned().unwrap_or_default();
        assert!(
            notice.starts_with("user-memory was replaced on disk, but the new binary can't take over this session ("),
            "{way:?}: {told}"
        );
        assert_eq!(place.state_files(), Vec::<String>::new(), "{way:?}");
        assert_eq!(session.version("serving"), None, "{way:?}");
        assert_eq!(texts(&session.call("quiet", "index")).len(), 1, "{way:?}");
        session.close();
    }
}

/// macOS can kill a new program while it assesses it: a check ended by a
/// signal is tried again at the next message, without a notice, and the
/// hand-over happens once the program runs.
#[cfg(unix)]
#[test]
fn a_check_ended_by_a_signal_is_tried_again_without_a_notice() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve(way, &[]);
        place.upgrade();
        fs::write(place.bin().join("user-memory.killed-once"), "").unwrap();
        let first = session.call("first", "index");
        assert_eq!(texts(&first).len(), 1, "{way:?}: {first}");
        assert!(!place.bin().join("user-memory.killed-once").exists());
        assert_eq!(session.version("second"), Some(NEWER.into()), "{way:?}");
        let stderr = session.close();
        assert!(
            stderr.contains(
                "a check of the replaced binary failed (it ended with signal: 6 (SIGABRT)"
            ) && stderr.contains("trying again at the next message"),
            "{way:?}: {stderr}"
        );
    }
}

/// The probe timeout these tests give, in milliseconds: the two starts of
/// a debug build that a check makes fit in it on a loaded machine, and the
/// fake's slow run, ten times longer, never answers within it. The slow run
/// is ended at the timeout, so it costs the test nothing more.
const SHORT_PROBE: &str = "3000";

/// A replacement slow to answer on a busy machine is checked again at the
/// next message, without a notice, and the hand-over happens once it
/// answers in time (#277).
#[test]
fn a_check_that_did_not_answer_is_tried_again_without_a_notice() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve(way, &[("USER_MEMORY_TEST_PROBE_TIMEOUT_MS", SHORT_PROBE)]);
        place.upgrade();
        fs::write(place.bin().join("user-memory.slow"), "1").unwrap();
        let first = session.call("first", "index");
        assert_eq!(texts(&first).len(), 1, "{way:?}: {first}");
        assert!(!place.bin().join("user-memory.slow").exists());
        assert_eq!(session.version("second"), Some(NEWER.into()), "{way:?}");
        let stderr = session.close();
        assert!(
            stderr.contains("a check of the replaced binary failed (it did not answer)")
                && stderr.contains("trying again at the next message")
                && stderr.contains("took over this session"),
            "{way:?}: {stderr}"
        );
    }
}

/// A replacement that never answers in time is refused after its last
/// check, with the notice it always had, and the session keeps its build.
#[test]
fn a_replacement_that_never_answers_is_refused_after_its_checks() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve(way, &[("USER_MEMORY_TEST_PROBE_TIMEOUT_MS", SHORT_PROBE)]);
        place.upgrade();
        fs::write(place.bin().join("user-memory.slow"), "10").unwrap();
        let first = session.call("first", "index");
        let second = session.call("second", "index");
        assert_eq!(texts(&first).len(), 1, "{way:?}: {first}");
        assert_eq!(texts(&second).len(), 1, "{way:?}: {second}");
        let third = session.call("third", "index");
        let said = texts(&third);
        assert!(
            said.iter().any(|text| text.starts_with(
                "user-memory was replaced on disk, but the new binary can't take over this session (it did not answer), so this session keeps "
            )),
            "{way:?}: {third}"
        );
        // The session's own build still serves: it has no debug tool.
        assert_eq!(session.version("fourth"), None, "{way:?}");
        session.close();
    }
}

/// The probe timeout the tests of a held check give, in milliseconds: long
/// enough that a check the test holds (`user-memory.wait`) never times out
/// before the test ends it.
const LONG_PROBE: &str = "60000";

/// How long a test waits for a reply that must come while a check is held:
/// far less than [`LONG_PROBE`], so a reply that waits for the check fails.
const DURING_CHECK: Duration = Duration::from_secs(20);

/// Holds the next check's probe run until [`go_on`]; the fake writes the
/// run's process id once it waits.
fn hold_check(place: &Place) {
    fs::write(place.bin().join("user-memory.wait"), "").unwrap();
}

/// The process id of the probe run a held check waits on, once it waits.
fn held_check(place: &Place, way: Way) -> u64 {
    let waiting = place.bin().join("user-memory.waiting");
    let deadline = Instant::now() + REPLY_WAIT;
    loop {
        if let Some(pid) = fs::read_to_string(&waiting)
            .ok()
            .and_then(|pid| pid.trim().parse().ok())
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "{way:?}: no check started");
        thread::sleep(Duration::from_millis(20));
    }
}

/// Lets a held check go on.
fn go_on(place: &Place) {
    fs::remove_file(place.bin().join("user-memory.wait")).unwrap();
}

impl Session {
    /// The reply to `id` if it came or comes within `wait`.
    fn reply_within(&mut self, id: &str, wait: Duration) -> Option<Value> {
        if let Some(at) = self.seen.iter().position(|m| m["id"] == id) {
            return Some(self.seen.remove(at));
        }
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(message) if message["id"] == id => return Some(message),
                Ok(message) => self.seen.push(message),
                Err(_) => return None,
            }
        }
    }
    /// The reply to `id`, which may have come before another awaited: a
    /// server answers requests that came together in any order.
    fn reply_any(&mut self, id: &str) -> Value {
        match self.seen.iter().position(|m| m["id"] == id) {
            Some(at) => self.seen.remove(at),
            None => self.reply(id),
        }
    }
}

/// While a check runs, the server's answer to a call already in flight
/// reaches the app; the message that started the check waits for it and
/// is then handed over (#279).
#[test]
fn a_reply_written_during_a_check_reaches_the_app_at_once() {
    for way in ways() {
        let place = Place::new();
        // A new store's first open pauses until the file is removed.
        let pause = place.dir.path().join("pause");
        fs::write(&pause, "").unwrap();
        let mut session = place.serve(
            way,
            &[
                ("USER_MEMORY_PAUSE_OPEN", pause.to_str().unwrap()),
                ("USER_MEMORY_TEST_PROBE_TIMEOUT_MS", LONG_PROBE),
            ],
        );
        session.write(&[session.call_message("slow", "index")]);
        let reached = place.dir.path().join("pause.reached");
        let deadline = Instant::now() + REPLY_WAIT;
        while !reached.exists() {
            assert!(
                Instant::now() < deadline,
                "{way:?}: the call never opened the store"
            );
            thread::sleep(Duration::from_millis(20));
        }
        place.upgrade();
        hold_check(&place);
        session.write(&[session.call_message("trigger", "debug_version")]);
        held_check(&place, way);
        fs::remove_file(&pause).unwrap();
        let slow = session.reply_within("slow", DURING_CHECK);
        assert!(
            slow.as_ref()
                .is_some_and(|slow| slow["result"]["isError"] == false),
            "{way:?}: the reply waited for the check: {slow:?}; stderr: {}",
            session.stderr_now()
        );
        go_on(&place);
        let trigger = session.reply("trigger");
        let answer: Value = serde_json::from_str(&texts(&trigger).concat()).unwrap_or_default();
        assert_eq!(answer["version"], NEWER, "{way:?}: {trigger}");
        session.close();
    }
}

/// Messages that come together wait for one check, not one each: a
/// replacement that never answers costs a burst one timeout, and the
/// refusal is said once its last check failed (#279).
#[test]
fn a_burst_of_messages_waits_for_one_check() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve(way, &[("USER_MEMORY_TEST_PROBE_TIMEOUT_MS", SHORT_PROBE)]);
        place.upgrade();
        let slow = place.bin().join("user-memory.slow");
        fs::write(&slow, "10").unwrap();
        session.write(&[
            session.call_message("one", "index"),
            session.call_message("two", "index"),
            session.call_message("three", "index"),
        ]);
        for id in ["one", "two", "three"] {
            let reply = session.reply_any(id);
            assert_eq!(texts(&reply).len(), 1, "{way:?}: {reply}");
        }
        // Each check starts the replacement once, counting the file down.
        assert_eq!(
            fs::read_to_string(&slow).unwrap(),
            "9",
            "{way:?}: more than one check"
        );
        let fourth = session.call("four", "index");
        assert_eq!(texts(&fourth).len(), 1, "{way:?}: {fourth}");
        let fifth = session.call("five", "index");
        assert!(
            texts(&fifth).iter().any(|text| text.starts_with(
                "user-memory was replaced on disk, but the new binary can't take over this session (it did not answer), so this session keeps "
            )),
            "{way:?}: {fifth}"
        );
        assert_eq!(fs::read_to_string(&slow).unwrap(), "7", "{way:?}");
        session.close();
    }
}

/// A request cancelled while a check holds it never reaches a server and
/// gets no answer; the request after it is served (#279).
#[test]
fn a_request_cancelled_during_a_check_gets_no_answer() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve(way, &[("USER_MEMORY_TEST_PROBE_TIMEOUT_MS", LONG_PROBE)]);
        place.upgrade();
        hold_check(&place);
        session.write(&[session.call_message("cancelled", "index")]);
        held_check(&place, way);
        session.write(&[
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"cancelled","reason":"stopped by the user"}}),
            session.call_message("after", "debug_version"),
        ]);
        go_on(&place);
        let after = session.reply("after");
        let answer: Value = serde_json::from_str(&texts(&after).concat()).unwrap_or_default();
        assert_eq!(answer["version"], NEWER, "{way:?}: {after}");
        let (stderr, seen) = session.finish();
        assert!(
            seen.iter().all(|m| m["id"] != "cancelled"),
            "{way:?}: {seen:?}\n{stderr}"
        );
    }
}

/// A message that comes while a check runs waits for it and goes to the
/// new code when the check passes, as the message that started it does.
#[test]
fn a_message_sent_during_a_passing_check_is_served_by_the_new_binary() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve(way, &[("USER_MEMORY_TEST_PROBE_TIMEOUT_MS", LONG_PROBE)]);
        place.upgrade();
        hold_check(&place);
        session.write(&[session.call_message("first", "debug_version")]);
        held_check(&place, way);
        session.write(&[session.call_message("second", "debug_version")]);
        go_on(&place);
        for id in ["first", "second"] {
            let reply = session.reply_any(id);
            let answer: Value = serde_json::from_str(&texts(&reply).concat()).unwrap_or_default();
            assert_eq!(answer["version"], NEWER, "{way:?}: {reply}");
        }
        let stderr = session.close();
        assert_eq!(
            stderr.matches("took over this session").count(),
            1,
            "{way:?}: {stderr}"
        );
    }
}

/// One deadline covers a whole check: a replacement whose two starts each
/// take most of it does not answer in time (#279).
#[test]
fn a_check_has_one_deadline_for_both_of_its_starts() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve(way, &[("USER_MEMORY_TEST_PROBE_TIMEOUT_MS", SHORT_PROBE)]);
        place.upgrade();
        // Three fifths of the timeout, each start: both fit one each.
        fs::write(place.bin().join("user-memory.delay"), "1800").unwrap();
        let first = session.call("first", "index");
        assert_eq!(texts(&first).len(), 1, "{way:?}: {first}");
        let second = session.call("second", "index");
        assert_eq!(texts(&second).len(), 1, "{way:?}: {second}");
        let third = session.call("third", "index");
        assert!(
            texts(&third).iter().any(|text| text.starts_with(
                "user-memory was replaced on disk, but the new binary can't take over this session (it did not answer), so this session keeps "
            )),
            "{way:?}: {third}"
        );
        assert_eq!(session.version("fourth"), None, "{way:?}");
        session.close();
    }
}

/// A binary replaced again while its check runs: the verdict is for the
/// file checked, so the one now on disk is checked before anything is
/// handed to it, and a broken one is refused (#279).
#[test]
fn a_binary_replaced_during_its_check_is_checked_again() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve(way, &[("USER_MEMORY_TEST_PROBE_TIMEOUT_MS", LONG_PROBE)]);
        place.upgrade();
        hold_check(&place);
        session.write(&[session.call_message("first", "index")]);
        held_check(&place, way);
        // The held probe already read its files, so it passes.
        place.break_binary();
        go_on(&place);
        let first = session.reply("first");
        let notice = texts(&first).last().cloned().unwrap_or_default();
        assert!(
            notice.starts_with(
                "user-memory was replaced on disk, but the new binary can't take over this session (it ended with"
            ),
            "{way:?}: {first}"
        );
        assert_eq!(session.version("serving"), None, "{way:?}");
        let stderr = session.close();
        assert!(!stderr.contains("took over"), "{way:?}: {stderr}");
    }
}

/// A check that failed for a file replaced again while it ran says
/// nothing of it: the file now on disk is checked, and a good one takes
/// over with no notice (#279).
#[test]
fn a_failed_check_of_a_file_replaced_during_it_is_dropped() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve(way, &[("USER_MEMORY_TEST_PROBE_TIMEOUT_MS", LONG_PROBE)]);
        place.upgrade();
        // The held probe fails once it goes on.
        fs::write(place.bin().join("user-memory.wait"), "1").unwrap();
        session.write(&[session.call_message("first", "debug_version")]);
        held_check(&place, way);
        place.upgrade_again(LATER);
        go_on(&place);
        let first = session.reply("first");
        assert_eq!(texts(&first).len(), 1, "{way:?}: {first}");
        let answer: Value = serde_json::from_str(&texts(&first).concat()).unwrap_or_default();
        assert_eq!(answer["version"], LATER, "{way:?}: {first}");
        session.close();
    }
}

/// The server's request to the app, made while a check runs, gets the
/// app's answer once the check ends: a check that passes while a call is in
/// flight passes what it held to the server serving, so a call waiting on
/// the app never waits on the hand-over (#279).
#[test]
fn an_answer_to_the_servers_request_reaches_it_after_a_check() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve(way, &[("USER_MEMORY_TEST_PROBE_TIMEOUT_MS", LONG_PROBE)]);
        // The newer build has the debug tools, the real one doesn't.
        place.upgrade();
        assert_eq!(session.version("first"), Some(NEWER.into()), "{way:?}");
        session.write(&[session.call_message("ping", "debug_ping")]);
        let deadline = Instant::now() + REPLY_WAIT;
        let ping = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let message = session
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("{way:?}: no ping; seen {:?}", session.seen));
            if message["method"] == "ping" {
                break message;
            }
            session.seen.push(message);
        };
        hold_check(&place);
        place.upgrade_again(LATER);
        session.write(&[session.call_message("trigger", "index")]);
        held_check(&place, way);
        session.write(&[json!({"jsonrpc":"2.0","id":ping["id"],"result":{}})]);
        go_on(&place);
        let pinged = session.reply("ping");
        assert_eq!(texts(&pinged), ["answered"], "{way:?}: {pinged}");
        let trigger = session.reply("trigger");
        assert_eq!(trigger["result"]["isError"], false, "{way:?}: {trigger}");
        assert_eq!(session.version("after"), Some(LATER.into()), "{way:?}");
        session.close();
    }
}

/// The app closing its input during a check ends the check and its run,
/// and the server serving answers what the check held (#279).
#[test]
fn input_closed_during_a_check_answers_what_it_held() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve(way, &[("USER_MEMORY_TEST_PROBE_TIMEOUT_MS", LONG_PROBE)]);
        place.upgrade();
        hold_check(&place);
        session.write(&[session.call_message("held", "index")]);
        let run = held_check(&place, way);
        let closed = Instant::now();
        let (stderr, seen) = session.finish();
        assert!(
            closed.elapsed() < DURING_CHECK,
            "{way:?}: serve ended only with the check: {stderr}"
        );
        let held = seen.iter().find(|m| m["id"] == "held");
        assert!(
            held.is_some_and(|held| held["result"]["isError"] == false),
            "{way:?}: {seen:?}\n{stderr}"
        );
        ended(run, way);
    }
}

/// A stop signal during a check ends it the same way: what it held is
/// answered, a listen stream it held is opened and completed, and its run
/// is ended (#279).
#[cfg(unix)]
#[test]
fn a_signal_during_a_check_answers_what_it_held() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve_as(
            way,
            &[("USER_MEMORY_TEST_PROBE_TIMEOUT_MS", LONG_PROBE)],
            Opening::Discover,
        );
        place.upgrade();
        hold_check(&place);
        session.write(&[session.call_message("held", "index")]);
        let run = held_check(&place, way);
        session.write(&[session.listen_message(json!({"toolsListChanged": true}))]);
        let stopped = Command::new("kill")
            .args(["-TERM", &session.child.id().to_string()])
            .status()
            .unwrap();
        assert!(stopped.success());
        let held = session.reply_within("held", DURING_CHECK);
        assert!(
            held.as_ref()
                .is_some_and(|held| held["result"]["isError"] == false),
            "{way:?}: the answer waited for the check: {held:?}"
        );
        let complete = session.reply_within(LISTEN, DURING_CHECK);
        assert_eq!(
            complete.map(|complete| complete["result"].clone()),
            Some(
                json!({"resultType": "complete", "_meta": {"io.modelcontextprotocol/subscriptionId": LISTEN}})
            ),
            "{way:?}: {:?}",
            session.seen
        );
        session.close();
        ended(run, way);
    }
}

/// Waits for process `pid` to end: a check's run never outlives it.
fn ended(pid: u64, way: Way) {
    let deadline = Instant::now() + REPLY_WAIT;
    while running(pid) {
        assert!(
            Instant::now() < deadline,
            "{way:?}: the check's run {pid} still runs"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// On 2026-07-28 the app hears of changed tools only on a listen stream it
/// opened for them: the stream is acknowledged first, told nothing of a
/// hand-over that left the tools as they were, told once of one that
/// changed them, kept open across both without holding them back, and
/// kept out of the hand-over files.
#[test]
fn a_listen_is_told_only_of_a_changed_tool_list_across_two_hand_overs() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve_as(
            way,
            &[("FAKE_USER_MEMORY_VERSION", env!("CARGO_PKG_VERSION"))],
            Opening::Discover,
        );
        let acknowledged = session.listen();
        assert_eq!(
            acknowledged["params"],
            json!({"_meta": {"io.modelcontextprotocol/subscriptionId": LISTEN}, "notifications": {"toolsListChanged": true}}),
            "{way:?}"
        );
        // The same tools: a hand-over the app is told nothing of.
        place.upgrade();
        assert_eq!(session.version("same"), None, "{way:?}");
        // The newer build adds a tool.
        place.upgrade_again(NEWER);
        let newer = session.version("newer");
        let (stderr, seen) = session.finish();
        assert_eq!(newer, Some(NEWER.into()), "{way:?}: {stderr}");
        assert_eq!(place.state_files(), Vec::<String>::new(), "{way:?}");
        assert_eq!(
            stderr.matches("took over this session").count(),
            2,
            "{way:?}: {stderr}"
        );
        let on_listen = on_listen(&seen);
        assert_eq!(on_listen.len(), 2, "{way:?}: {seen:?}");
        assert_eq!(
            on_listen[0]["method"],
            "notifications/subscriptions/acknowledged"
        );
        assert_eq!(on_listen[1]["method"], "notifications/tools/list_changed");
        assert_eq!(
            count(&seen, "notifications/tools/list_changed"),
            1,
            "{way:?}: {seen:?}"
        );
    }
}

/// A cancelled stream is told nothing more.
#[test]
fn a_cancelled_listen_is_told_nothing() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve_as(way, &[], Opening::Discover);
        session.listen();
        session.write(&[
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":LISTEN}}),
        ]);
        place.upgrade();
        assert_eq!(session.version("after"), Some(NEWER.into()), "{way:?}");
        let (_, seen) = session.finish();
        assert_eq!(on_listen(&seen).len(), 1, "{way:?}: {seen:?}");
        assert_eq!(
            count(&seen, "notifications/tools/list_changed"),
            0,
            "{way:?}: {seen:?}"
        );
    }
}

/// A 2026-07-28 app that opened no stream is told of no change: the
/// protocol has no other way to say it.
#[test]
fn a_session_without_a_listen_is_told_nothing() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve_as(way, &[], Opening::Discover);
        place.upgrade();
        assert_eq!(session.version("after"), Some(NEWER.into()), "{way:?}");
        let (_, seen) = session.finish();
        assert!(seen.is_empty(), "{way:?}: {seen:?}");
    }
}

/// With no handshake at all, the protocol comes from each request's
/// `_meta`, and the new code asks for its tools the way the app did.
#[test]
fn a_session_with_no_handshake_hands_over_and_hears_of_new_tools() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve_as(way, &[], Opening::Bare);
        session.listen();
        place.upgrade();
        assert_eq!(session.version("after"), Some(NEWER.into()), "{way:?}");
        let (stderr, seen) = session.finish();
        assert!(
            stderr.contains("took over this session"),
            "{way:?}: {stderr}"
        );
        let on_listen = on_listen(&seen);
        assert_eq!(on_listen.len(), 2, "{way:?}: {seen:?}");
        assert_eq!(on_listen[1]["method"], "notifications/tools/list_changed");
    }
}

/// A listen the front was not written for gets the server's own answer, an
/// error, as before streams were supported: the cost is only that new
/// tools appear at the next session.
#[test]
fn a_listen_the_front_does_not_know_gets_the_old_answer() {
    let place = Place::new();
    let mut session = place.serve_as(Way::Exec, &[], Opening::Discover);
    let mut later = meta();
    later["io.modelcontextprotocol/protocolVersion"] = json!("2027-01-01");
    let cases = [
        (
            "another kind",
            session.listen_message(json!({"toolsListChanged": true, "tasksChanged": true})),
        ),
        (
            "no tools",
            session.listen_message(json!({"promptsListChanged": true})),
        ),
        (
            "tools off",
            session.listen_message(json!({"toolsListChanged": false})),
        ),
        ("another parameter", {
            let mut message = session.listen_message(json!({"toolsListChanged": true}));
            message["params"]["cursor"] = json!("x");
            message
        }),
        (
            "another protocol",
            json!({"jsonrpc":"2.0","id":LISTEN,"method":"subscriptions/listen","params":{"_meta":later,"notifications":{"toolsListChanged":true}}}),
        ),
    ];
    for (case, message) in cases {
        session.write(&[message]);
        let reply = session.reply(LISTEN);
        assert!(reply["error"]["code"].is_i64(), "{case}: {reply}");
    }
    place.upgrade();
    assert_eq!(session.version("after"), Some(NEWER.into()));
    let (_, seen) = session.finish();
    assert!(seen.is_empty(), "{seen:?}");
}

/// A serve stopped by a signal ends its open streams gracefully.
#[cfg(unix)]
#[test]
fn a_stopped_serve_completes_its_open_listen() {
    let place = Place::new();
    let mut session = place.serve_as(Way::Exec, &[], Opening::Discover);
    session.listen();
    let stopped = Command::new("kill")
        .args(["-TERM", &session.child.id().to_string()])
        .status()
        .unwrap();
    assert!(stopped.success());
    let complete = session.reply(LISTEN);
    assert_eq!(
        complete["result"],
        json!({"resultType": "complete", "_meta": {"io.modelcontextprotocol/subscriptionId": LISTEN}}),
        "{complete}"
    );
    session.close();
}

/// A version after the newer build's, for a second upgrade.
const LATER: &str = "9.9.10";

/// A second upgrade that lands as the new code starts, before it has
/// answered anything, is handed over to: the baseline is the build that was
/// checked and started, not the file found, and the front's own replayed
/// requests don't hold the hand-over back. After an `exec` the new code
/// sees it at the call that started the hand-over; a relay may already
/// have passed that call to its child, and sees it at the next one.
#[test]
fn an_upgrade_landing_as_the_new_code_starts_is_handed_over_to() {
    for way in ways() {
        let place = Place::new();
        let mut session = place.serve(way, &[]);
        let fakes = place.dir.path().join("fakes-then");
        fs::create_dir_all(&fakes).unwrap();
        fs::copy(
            fake::fake(&fakes, "user-memory"),
            place.bin().join("user-memory.then"),
        )
        .unwrap();
        fs::write(place.bin().join("user-memory.then-version"), LATER).unwrap();
        place.upgrade();
        let first = session.version("first");
        let second = session.version("second");
        let (stderr, _) = session.finish();
        if matches!(way, Way::Exec) {
            assert_eq!(first, Some(LATER.into()), "{way:?}: {stderr}");
            // After an `exec` the newer build handed the session on; a relay
            // starts each child itself.
            assert!(
                stderr.contains(&format!(
                    "user-memory {LATER}: took over this session from {NEWER}"
                )),
                "{way:?}: {stderr}"
            );
        }
        assert_eq!(second, Some(LATER.into()), "{way:?}: {stderr}");
        assert!(
            !place.bin().join("user-memory.then").exists(),
            "{way:?}: the second build never took the first's place"
        );
        assert_eq!(
            stderr.matches("took over this session").count(),
            2,
            "{way:?}: {stderr}"
        );
        assert_eq!(place.state_files(), Vec::<String>::new(), "{way:?}");
    }
}

/// A relay whose child ends serves with its own code again, and knows it:
/// the next call goes to the newer build once more, not to the old code
/// taken for the new one.
#[cfg(unix)]
#[test]
fn a_relays_child_that_ends_is_started_again_from_the_newer_build() {
    let place = Place::new();
    let mut session = place.serve(Way::Relay, &[]);
    place.upgrade();
    let first = serving(&mut session, "first");
    assert_eq!(first["version"], NEWER);
    let child = first["pid"].as_u64().unwrap();
    let killed = Command::new("kill")
        .args(["-KILL", &child.to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let deadline = Instant::now() + REPLY_WAIT;
    while running(child) {
        assert!(Instant::now() < deadline, "the child still runs");
        thread::sleep(Duration::from_millis(20));
    }
    // The relay may notice the child's end with this call in flight, which
    // then gets the restart's error.
    session.call("noticed", "index");
    let after = session.version("after");
    let (stderr, _) = session.finish();
    assert_eq!(after, Some(NEWER.into()), "{stderr}");
}

/// A call the app cancelled gets no answer, so it doesn't hold a hand-over
/// back: the next call after an upgrade is answered by the newer build.
#[test]
fn a_cancelled_call_does_not_hold_a_hand_over_back() {
    for way in ways() {
        let place = Place::new();
        // A new store's first open pauses until the file is removed.
        let pause = place.dir.path().join("pause");
        fs::write(&pause, "").unwrap();
        let mut session = place.serve(way, &[("USER_MEMORY_PAUSE_OPEN", pause.to_str().unwrap())]);
        session.write(&[session.call_message("stopped", "index")]);
        let reached = place.dir.path().join("pause.reached");
        let deadline = Instant::now() + REPLY_WAIT;
        while !reached.exists() {
            assert!(
                Instant::now() < deadline,
                "{way:?}: the call never opened the store"
            );
            thread::sleep(Duration::from_millis(20));
        }
        // The user stops the turn: the app cancels the call.
        session.write(&[
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"stopped","reason":"stopped by the user"}}),
        ]);
        fs::remove_file(&pause).unwrap();
        place.upgrade();
        let after = session.version("after");
        let (stderr, seen) = session.finish();
        assert_eq!(after, Some(NEWER.into()), "{way:?}: {stderr}");
        // Whatever the server did with the cancelled call, the app saw at
        // most one reply to it.
        assert!(
            seen.iter().filter(|m| m["id"] == "stopped").count() <= 1,
            "{way:?}: {seen:?}"
        );
    }
}

/// `serve` takes a hand-over only from its own state files: a file
/// elsewhere, or of another name in the state folder, is neither read nor
/// deleted, and the session starts as a new one.
#[test]
fn a_hand_over_names_only_its_own_state_files() {
    let place = Place::new();
    let state = sandbox::folder(place.dir.path(), "state");
    fs::create_dir_all(&state).unwrap();
    for (case, file) in [
        ("elsewhere", place.dir.path().join("handover-1-1.json")),
        ("another name", state.join("notes.json")),
    ] {
        fs::write(&file, "not a session\n").unwrap();
        let mut session = place.serve(
            Way::Exec,
            &[("USER_MEMORY_HANDOVER", file.to_str().unwrap())],
        );
        let reply = session.call("call", "index");
        assert_eq!(reply["result"]["isError"], false, "{case}: {reply}");
        let stderr = session.close();
        assert!(file.exists(), "{case}: the file was deleted");
        assert!(
            stderr.contains(&format!(
                "user-memory: {} is not a hand-over file in user-memory's state folder; serving this session as a new one",
                file.display()
            )),
            "{case}: {stderr}"
        );
    }
}

/// A hand-over file a dying process left unread holds the app's input. A
/// starting `serve` removes one older than ten minutes and leaves a recent
/// one, which a hand-over may be about to read, and any other file.
#[test]
fn a_starting_serve_removes_hand_over_files_left_behind() {
    let place = Place::new();
    let state = sandbox::folder(place.dir.path(), "state");
    fs::create_dir_all(&state).unwrap();
    let old = state.join("handover-1-1.json");
    let recent = state.join("handover-2-2.json");
    let other = state.join("handover-notes.json");
    for file in [&old, &recent, &other] {
        fs::write(file, "{}\n").unwrap();
    }
    let hour_ago = std::time::SystemTime::now() - Duration::from_secs(60 * 60);
    for file in [&old, &other] {
        fs::File::options()
            .write(true)
            .open(file)
            .unwrap()
            .set_modified(hour_ago)
            .unwrap();
    }
    let session = place.serve(Way::Exec, &[]);
    session.close();
    assert!(!old.exists(), "a file left behind stays");
    assert!(recent.exists(), "a recent hand-over file went");
    assert!(other.exists(), "another file went");
}

/// The token the tests' memory server accepts.
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// An address on loopback where nothing listens yet: a test port, so no
/// other test takes it while this one starts and stops servers there.
fn free_address() -> String {
    format!("127.0.0.1:{}", port::closed_port())
}

/// A memory server of this build on `address`, its store and token in
/// `dir`; killed when dropped.
struct Host(Child);

impl Host {
    fn start(dir: &Path, address: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_user-memory"))
            .args(["host", "--listen", address, "--db"])
            .arg(dir.join("shared.sqlite3"))
            .arg("--token-file")
            .arg(dir.join("token"))
            .sandboxed(dir)
            .env_remove("USER_MEMORY_DB")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(line.starts_with("Listening: http://"), "{line}");
        Self(child)
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A bridge's `serve` whose memory server is not up yet, at `address`.
fn bridge_env(place: &Place, address: &str) -> Vec<(&'static str, String)> {
    let token = place.dir.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    vec![
        ("USER_MEMORY_URL", format!("http://{address}/mcp")),
        (
            "USER_MEMORY_TOKEN_FILE",
            token.to_string_lossy().into_owned(),
        ),
    ]
}

/// A session that started while its memory server was down hands over to
/// a replaced binary like any other: the new code answers the handshake
/// itself too, the app is told of its other tools, and once the server is
/// up the next call is served.
#[test]
fn a_session_started_in_an_outage_hands_over() {
    for way in ways() {
        let place = Place::new();
        let address = free_address();
        let env = bridge_env(&place, &address);
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let mut session = place.serve(way, &env);
        place.upgrade();
        let down = session.call("down", "index");
        assert_eq!(down["result"]["isError"], true, "{way:?}: {down}");
        assert!(
            texts(&down)[0].ends_with("did not answer; the call may not have completed. Before repeating a write, search for it. Memory reconnects on the next call."),
            "{way:?}: {down}"
        );
        let host = Host::start(place.dir.path(), &address);
        let up = session.call("up", "index");
        assert_eq!(up["result"]["isError"], false, "{way:?}: {up}");
        drop(host);
        // Told twice, each on its own schedule: the newer build adds a tool,
        // and the server, a build without it, takes it away again.
        let (stderr, seen) = session.finish();
        assert_eq!(
            count(&seen, "notifications/tools/list_changed"),
            2,
            "{way:?}: {seen:?}"
        );
        assert!(
            stderr.contains(&format!("user-memory {NEWER}: took over this session")),
            "{way:?}: {stderr}"
        );
        assert!(!stderr.contains(TOKEN), "{way:?}: the token is in stderr");
    }
}

/// A version line the first real handshake said is not said again when the
/// session hands over to a build of the same version, as a reinstall does:
/// the bridge gives the front its line.
#[test]
fn a_version_line_is_said_once_across_a_hand_over_to_the_same_version() {
    for way in ways() {
        let place = Place::new();
        let address = free_address();
        let mut env = bridge_env(&place, &address);
        // Below the host's oldest client, before and after the hand-over.
        env.push(("USER_MEMORY_TEST_VERSION", "0.7.9".into()));
        env.push(("FAKE_USER_MEMORY_VERSION", "0.7.9".into()));
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let mut session = place.serve(way, &env);
        session.call("down", "index");
        let host = Host::start(place.dir.path(), &address);
        let line = format!(
            "The memory server at http://{address}/mcp runs user-memory {} and needs 0.8.0 or newer on this machine (0.7.9). Upgrade: user-memory upgrade",
            env!("CARGO_PKG_VERSION")
        );
        let first = session.call("first", "index");
        assert_eq!(texts(&first).last(), Some(&line), "{way:?}: {first}");
        place.upgrade();
        let after = session.call("after", "index");
        assert_eq!(after["result"]["isError"], false, "{way:?}: {after}");
        assert!(
            !texts(&after).contains(&line),
            "{way:?}: said again: {after}"
        );
        drop(host);
        let stderr = session.close();
        assert!(
            stderr.contains("user-memory 0.7.9: took over this session"),
            "{way:?}: {stderr}"
        );
    }
}
