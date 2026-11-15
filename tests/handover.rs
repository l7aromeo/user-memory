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
        if cfg!(windows) {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let aside = self.bin().join(format!(
                ".user-memory-old-{nanos}{}",
                std::env::consts::EXE_SUFFIX
            ));
            fs::rename(self.binary(), aside).unwrap();
        }
        fs::rename(&staged, self.binary()).unwrap();
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
            "{way:?}: {stderr}"
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
            "{way:?}: {stderr}"
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
