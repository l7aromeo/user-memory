//! `serve` handing itself over to a replaced binary: the session goes on,
//! on the same connection and with no new handshake, answered by the new
//! code. Every case runs through the real binary, copied into a folder of
//! its own; the "newer build" is the fake user-memory, which runs the real
//! test build with a debug build's test version (tests/fake/cli.rs). On
//! Unix each case runs twice: handing over by `exec`, and by the relay
//! Windows uses, which `USER_MEMORY_TEST_RELAY` turns on there.

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
            let aside = self.bin().join(format!(
                ".user-memory-old-{}{}",
                Instant::now().elapsed().as_nanos(),
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
        let mut command = Command::new(self.binary());
        command
            .arg("serve")
            .current_dir(self.dir.path())
            .env("USER_MEMORY_DB", self.db())
            .env("FAKE_USER_MEMORY", env!("CARGO_BIN_EXE_user-memory"))
            .env("FAKE_USER_MEMORY_VERSION", NEWER)
            .envs(env.iter().copied())
            .sandboxed(self.dir.path());
        if matches!(way, Way::Relay) {
            command.env("USER_MEMORY_TEST_RELAY", "1");
        }
        Session::start(command)
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
}

impl Session {
    fn start(mut command: Command) -> Self {
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
        };
        session.write(&[
            json!({"jsonrpc":"2.0","id":"init","method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":"tools","method":"tools/list","params":{}}),
        ]);
        session.reply("init");
        session.reply("tools");
        session
    }
    /// Writes `messages` to serve in one write.
    fn write(&mut self, messages: &[Value]) {
        let bytes: String = messages.iter().map(|m| format!("{m}\n")).collect();
        let input = self.input.as_mut().unwrap();
        input.write_all(bytes.as_bytes()).unwrap();
        input.flush().unwrap();
    }
    fn call_message(id: &str, tool: &str) -> Value {
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":tool,"arguments":{}}})
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
        self.write(&[Self::call_message(id, tool)]);
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
            Session::call_message("first", "debug_version"),
            Session::call_message("second", "index"),
        ]);
        let first = session.reply("first");
        let answer: Value = serde_json::from_str(&texts(&first).concat()).unwrap_or_default();
        assert_eq!(answer["version"], NEWER, "{way:?}: {first}");
        let second = session.reply("second");
        assert_eq!(second["result"]["isError"], false, "{way:?}: {second}");
        // The new build adds a tool, so the app is told to reload them.
        assert_eq!(
            session.notified("notifications/tools/list_changed"),
            1,
            "{way:?}: {:?}",
            session.seen
        );
        assert_eq!(session.version("after"), Some(NEWER.into()), "{way:?}");
        assert_eq!(session.child.id(), pid);
        let stderr = session.close();
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
        fs::File::options()
            .write(true)
            .open(&copy)
            .unwrap()
            .set_modified(before.modified().unwrap())
            .unwrap();
        place.replace_with(&copy);
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
        session.write(&[Session::call_message("waits", "index")]);
        let reply = session.reply("waits");
        let migrated = migration.join().unwrap();
        assert_eq!(reply["result"]["isError"], false, "{way:?}: {reply}");
        assert!(Instant::now() >= migrated, "answered before the migration");
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
        assert_eq!(session.version("serving"), None, "{way:?}");

        place.break_binary();
        let broken = session.call("broken", "index");
        let notice = texts(&broken).last().cloned().unwrap_or_default();
        assert!(
            notice.starts_with("user-memory was replaced on disk, but the new binary can't take over this session (it ended with"),
            "{way:?}: {broken}"
        );
        assert_eq!(texts(&session.call("quiet", "index")).len(), 1, "{way:?}");
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
        session.write(&[Session::call_message("slow", "index")]);
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
