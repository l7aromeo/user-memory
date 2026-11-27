#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

//! The tag check (#121), through the real binary on a local store: a tag
//! condition ends once a checkout of its project has the tag, found by the
//! hooks, and a tool hook spawns git only when the refs changed. A record
//! naming a tag is checked against a checkout here. A memory server's
//! `POST /tags` lists what is pending and writes the ends its clients'
//! checkouts found.

#[path = "support/fake.rs"]
mod fake;
#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
#[path = "support/time.rs"]
mod time;
use time::{HOOK_ROOM, STUCK};

use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};
use tempfile::{TempDir, tempdir};

/// The checkout's project, from its `origin` remote.
const PROJECT: &str = "github.com/example/app";
/// The server's token file's token, which may write.
const SHARED: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A sandboxed home with its own store, a checkout of [`PROJECT`] and a
/// `git` on PATH that logs each call before running the real one: the
/// fake of tests/fake/cli.rs, so it runs on Windows too.
struct Home {
    dir: TempDir,
    /// The memory server the hooks use, once [`Home::serve`] started one.
    remote: RefCell<Option<String>>,
    /// The token file the hooks send to it: the server's own when none.
    client_token: RefCell<Option<PathBuf>>,
    /// Variables the hooks get beyond the sandbox's.
    env: RefCell<Vec<(String, PathBuf)>>,
}

/// Variables that change where git finds a folder's repository: the hooks
/// get none from the test's own environment, unless a test sets one.
const GIT_DISCOVERY_ENV: [&str; 5] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_CEILING_DIRECTORIES",
];

/// A memory server on the home's store.
struct Server {
    child: Child,
    address: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Home {
    fn new() -> Self {
        let home = Self {
            dir: tempdir().unwrap(),
            remote: RefCell::new(None),
            client_token: RefCell::new(None),
            env: RefCell::new(Vec::new()),
        };
        let shim = home.shim();
        fs::create_dir(&shim).unwrap();
        fake::fake(&shim, "git");
        fs::write(shim.join("git.real"), which_git().to_str().unwrap()).unwrap();
        fs::create_dir(home.checkout()).unwrap();
        home.git(&home.checkout(), &["init", "-q", "-b", "main"]);
        home.git(
            &home.checkout(),
            &["commit", "-q", "--allow-empty", "-m", "start"],
        );
        home.git(
            &home.checkout(),
            &["remote", "add", "origin", "git@github.com:example/app.git"],
        );
        home
    }

    fn checkout(&self) -> PathBuf {
        self.dir.path().join("app")
    }

    fn db(&self) -> PathBuf {
        self.dir.path().join("memory.sqlite3")
    }

    /// The folder of the logging git, first on the hooks' PATH.
    fn shim(&self) -> PathBuf {
        self.dir.path().join("shim")
    }

    fn log(&self) -> PathBuf {
        self.shim().join("git.calls")
    }

    /// Every git call the hooks made.
    fn git_calls(&self) -> usize {
        fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .count()
    }

    /// The git calls the hooks made after the first `before`, each by its
    /// arguments after the folder.
    fn calls_after(&self, before: usize) -> Vec<String> {
        fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .skip(before)
            .map(|line| {
                ["rev-parse", "config", "cat-file", "for-each-ref"]
                    .iter()
                    .find_map(|verb| line.find(verb).map(|at| line[at..].to_owned()))
                    .unwrap_or_else(|| line.to_owned())
            })
            .collect()
    }

    /// The lookups the hooks made: `cat-file` calls.
    fn lookups(&self) -> usize {
        fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("cat-file"))
            .count()
    }

    fn git(&self, dir: &Path, args: &[&str]) {
        let out = Command::new(which_git())
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=Test", "-c", "user.email=t@example.invalid"])
            .args(args)
            .sandboxed(self.dir.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn command(&self, args: &[&str], folder: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        let path = env::join_paths(
            std::iter::once(self.shim())
                .chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
        )
        .unwrap();
        command
            .args(args)
            .current_dir(folder)
            .sandboxed(self.dir.path())
            .env("PATH", path)
            .env("USER_MEMORY_DB", self.db())
            .env(HOOK_ROOM.0, HOOK_ROOM.1);
        for name in GIT_DISCOVERY_ENV {
            command.env_remove(name);
        }
        for (name, value) in self.env.borrow().iter() {
            command.env(name, value);
        }
        command
    }

    /// A git checkout at `dir` whose `origin` is `remote`, with a commit.
    fn repository(&self, dir: &Path, remote: &str) {
        fs::create_dir_all(dir).unwrap();
        self.git(dir, &["init", "-q", "-b", "main"]);
        self.git(dir, &["commit", "-q", "--allow-empty", "-m", "start"]);
        self.git(dir, &["remote", "add", "origin", remote]);
    }

    /// `record` of a project rule ending at `tag`, from `folder`; its reply.
    fn record(&self, tag: &str, folder: &Path) -> Value {
        self.record_about("Keep the old parser until the new one ships", tag, folder)
    }

    /// [`Home::record`] of the rule `what`, a sentence of its own, so rules
    /// about different things are not held similar.
    fn record_about(&self, what: &str, tag: &str, folder: &Path) -> Value {
        let out = self
            .command(
                &[
                    "record",
                    "--type",
                    "preference",
                    "--category",
                    "workflow",
                    "--scope",
                    "project",
                    "--project",
                    PROJECT,
                    "--title",
                    what,
                    "--content",
                    &format!("{what}."),
                    "--until-tag",
                    tag,
                ],
                folder,
            )
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// `record` of an observed note that `cargo publish` names, from
    /// `folder`: not a rule, which session start prints whole and no hook
    /// pushes again. Its id.
    fn publish_note(&self, folder: &Path) -> String {
        let out = self
            .command(
                &[
                    "record",
                    "--type",
                    "fact",
                    "--source",
                    "observed",
                    "--evidence",
                    "Seen in two sessions",
                    "--category",
                    "workflow",
                    "--scope",
                    "project",
                    "--project",
                    PROJECT,
                    "--title",
                    "Dry run before publishing",
                    "--content",
                    "Run `cargo publish --dry-run` before `cargo publish`.",
                ],
                folder,
            )
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let note: Value = serde_json::from_slice(&out.stdout).unwrap();
        note["id"].as_str().unwrap().to_owned()
    }

    /// Starts a memory server on the home's store, with `env`; from then on
    /// the hooks are its clients.
    fn serve(&self, env: &[(&str, &str)]) -> Server {
        self.serve_on("127.0.0.1:0", env)
    }

    /// [`Home::serve`], listening on `address`.
    fn serve_on(&self, address: &str, env: &[(&str, &str)]) -> Server {
        fs::write(self.token(), SHARED).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_user-memory"))
            .args(["host", "--listen", address, "--db"])
            .arg(self.db())
            .arg("--token-file")
            .arg(self.token())
            .sandboxed(self.dir.path())
            .env_remove("USER_MEMORY_DB")
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
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
        *self.remote.borrow_mut() = Some(address.clone());
        Server { child, address }
    }

    fn token(&self) -> PathBuf {
        self.dir.path().join("access-token")
    }

    /// A read-only token of the server's store.
    fn reader(&self) -> String {
        let file = self.dir.path().join("reader.token");
        let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
            .args(["host", "token", "add", "reader", "--read-only", "--out"])
            .arg(&file)
            .arg("--db")
            .arg(self.db())
            .sandboxed(self.dir.path())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        fs::read_to_string(file).unwrap()
    }

    fn hook(&self, subcommand: &str, folder: &Path, payload: &Value) -> String {
        let mut command = self.command(&[subcommand, "--client", "claude"], folder);
        if let Some(address) = self.remote.borrow().as_ref() {
            command
                .env_remove("USER_MEMORY_DB")
                .env("USER_MEMORY_URL", format!("http://{address}/mcp"))
                .env(
                    "USER_MEMORY_TOKEN_FILE",
                    self.client_token.borrow().clone().unwrap_or(self.token()),
                );
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = time::finish(child, subcommand);
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn start(&self, session: &str, folder: &Path) {
        self.hook(
            "session-start",
            folder,
            &json!({"session_id": session, "cwd": folder, "hook_event_name": "SessionStart", "source": "startup"}),
        );
    }

    /// The prompt hook in `folder`; what it printed.
    fn prompted(&self, session: &str, folder: &Path, prompt: &str) -> String {
        self.hook(
            "prompt-submit",
            folder,
            &json!({"session_id": session, "cwd": folder, "hook_event_name": "UserPromptSubmit", "prompt": prompt}),
        )
    }

    /// The tool hook after a shell command in `folder`; what it printed.
    fn ran(&self, session: &str, folder: &Path, command: &str) -> String {
        self.hook(
            "post-tool-use",
            folder,
            &json!({"session_id": session, "cwd": folder, "hook_event_name": "PostToolUse",
                    "tool_name": "Bash", "tool_input": {"command": command}, "tool_response": {"stdout": ""}}),
        )
    }

    /// The end record of memory `id`: how, evidence and origin.
    fn ended(&self, id: &str) -> Option<(String, String, String)> {
        Connection::open(self.db())
            .unwrap()
            .query_row(
                "SELECT how,evidence,origin FROM memory_ended WHERE memory_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok()
    }

    /// The end record of memory `id`: how and evidence.
    fn end(&self, id: &str) -> Option<(String, String)> {
        Connection::open(self.db())
            .unwrap()
            .query_row(
                "SELECT how,evidence FROM memory_ended WHERE memory_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()
    }
}

/// The real git, outside the shim: `git.exe` on Windows.
fn which_git() -> PathBuf {
    let name = format!("git{}", env::consts::EXE_SUFFIX);
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(&name))
        .find(|path| path.is_file())
        .unwrap()
}

/// The commit `rev` names in `dir`.
fn commit(dir: &Path, rev: &str) -> String {
    let out = Command::new(which_git())
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", rev])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

// A tag made by a command ends the condition at the next tool hook, with
// its commit in the evidence; before it, a tool hook with the refs as they
// were looks nothing up.
#[test]
fn a_tag_made_in_the_checkout_ends_the_condition_at_the_next_tool_hook() {
    let home = Home::new();
    let app = home.checkout();
    let out = home.record("v1.0.0", &app);
    let id = out["id"].as_str().unwrap().to_owned();
    assert_eq!(
        out["until"],
        format!("until tag v1.0.0 exists in {PROJECT} (no tags here yet)")
    );
    let before = home.lookups();
    home.start("s1", &app);
    let after_start = home.lookups();
    assert_eq!(after_start, before + 1, "session start looks once");
    home.ran("s1", &app, "cargo build");
    home.ran("s1", &app, "cargo test");
    assert_eq!(home.lookups(), after_start, "no ref changed, no lookup");
    assert_eq!(home.end(&id), None);
    home.git(&app, &["tag", "-a", "-m", "release", "v1.0.0"]);
    home.ran("s1", &app, "git tag -a -m release v1.0.0");
    let head = commit(&app, "HEAD");
    assert_eq!(
        home.end(&id),
        Some((
            "tag".to_owned(),
            format!("tag v1.0.0 at {} in a checkout of {PROJECT}", &head[..7])
        ))
    );
    assert_eq!(home.lookups(), after_start + 1);
    // Ended: nothing pending, so later hooks look nothing up.
    home.ran("s1", &app, "git push --tags");
    assert_eq!(home.lookups(), after_start + 1);
}

// A tag of the same name in another repository, such as a fork, never
// decides the condition; one only differing in case doesn't either.
#[test]
fn a_fork_or_a_tag_in_another_case_decides_nothing() {
    let home = Home::new();
    let app = home.checkout();
    let id = home.record("v1.0.0", &app)["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let fork = home.dir.path().join("fork");
    fs::create_dir(&fork).unwrap();
    home.git(&fork, &["init", "-q", "-b", "main"]);
    home.git(&fork, &["commit", "-q", "--allow-empty", "-m", "start"]);
    home.git(
        &fork,
        &["remote", "add", "origin", "git@github.com:someone/app.git"],
    );
    home.git(&fork, &["tag", "v1.0.0"]);
    let before = home.lookups();
    home.start("s1", &fork);
    home.ran("s1", &fork, "git tag v1.0.0");
    assert_eq!(home.end(&id), None, "a fork's tag");
    assert_eq!(home.lookups(), before, "nothing pending for the fork");
    home.git(&app, &["tag", "V1.0.0"]);
    home.start("s2", &app);
    home.ran("s2", &app, "git tag V1.0.0");
    assert_eq!(home.end(&id), None, "another case");
}

// The store busy with another writer when the tag appears: the end is
// kept, and the next hook writes it.
#[test]
fn an_end_the_store_cannot_take_is_written_by_a_later_hook() {
    let home = Home::new();
    let app = home.checkout();
    let id = home.record("v1.0.0", &app)["id"]
        .as_str()
        .unwrap()
        .to_owned();
    home.start("s1", &app);
    home.git(&app, &["tag", "v1.0.0"]);
    let writer = Connection::open(home.db()).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    home.ran("s1", &app, "git tag v1.0.0");
    writer.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        home.end(&id),
        None,
        "kept while another writer holds the store"
    );
    home.ran("s1", &app, "cargo build");
    assert!(home.end(&id).is_some());
}

// A record checks its tag against a checkout of its project here: one that
// exists is refused, the echo shows the latest tags, and a name unlike them
// is flagged; elsewhere the echo says it wasn't checked.
#[test]
fn a_record_checks_its_tag_against_a_checkout_here() {
    let home = Home::new();
    let app = home.checkout();
    home.git(&app, &["tag", "v0.9.2"]);
    home.git(&app, &["tag", "v0.9.3"]);
    let out = home
        .command(
            &[
                "record",
                "--type",
                "preference",
                "--category",
                "workflow",
                "--scope",
                "project",
                "--project",
                PROJECT,
                "--title",
                "Keep the old parser",
                "--content",
                "Keep the old parser.",
                "--until-tag",
                "v0.9.3",
            ],
            &app,
        )
        .output()
        .unwrap();
    let out: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out["stored"], false, "{out}");
    assert_eq!(
        out["reason"],
        "tag v0.9.3 already exists, so this memory would end at once; name the next release's tag"
    );
    let out = home.record_about("Pin the lexer crate to its fork", "v0.10.0", &app);
    let until = out["until"].as_str().unwrap();
    assert!(
        until.ends_with("(latest here: v0.9.3, v0.9.2)")
            || until.ends_with("(latest here: v0.9.2, v0.9.3)"),
        "{until}"
    );
    let out = home.record_about("Skip the flaky printer snapshot tests", "V1.0", &app);
    assert!(
        out["until"]
            .as_str()
            .unwrap()
            .ends_with("V1.0 has the shape of none of them, so check its name)"),
        "{out}"
    );
    let out = home.record_about(
        "Run benchmarks only on the quiet machine",
        "v2.0.0",
        home.dir.path(),
    );
    assert_eq!(
        out["until"],
        format!("until tag v2.0.0 exists in {PROJECT} (not checked here)")
    );
}

/// `POST /tags` to `server` with `token`, declaring `length` bytes and
/// sending `body`: the status and the reply.
fn post_declaring(server: &Server, token: &str, length: usize, body: &str) -> (u16, String) {
    let mut socket = TcpStream::connect(&server.address).unwrap();
    socket.set_read_timeout(Some(STUCK)).unwrap();
    write!(
        socket,
        "POST /tags HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nAuthorization: Bearer {token}\r\nConnection: close\r\nContent-Length: {length}\r\n\r\n",
        server.address,
    )
    .unwrap();
    // The server may answer and close before reading a body it refuses.
    let _ = socket.write_all(body.as_bytes());
    let mut response = Vec::new();
    let _ = socket.read_to_end(&mut response);
    let text = String::from_utf8_lossy(&response).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, body.to_owned())
}

/// `POST /tags` to `server` with `token` and `body`.
fn post(server: &Server, token: &str, body: &Value) -> (u16, String) {
    let body = body.to_string();
    post_declaring(server, token, body.len(), &body)
}

/// A tags request from session `one` for [`PROJECT`] with `ended`.
fn asked(ended: Value) -> Value {
    json!({"origin": "claude:session/one", "project": PROJECT, "ended": ended})
}

// The route lists the project's pending conditions, oldest first, and
// writes an end once, under the request's origin; one naming another tag,
// or a memory ended already, is skipped.
#[test]
fn the_tags_route_lists_what_is_pending_and_writes_each_end_once() {
    let home = Home::new();
    let app = home.checkout();
    let first = home.record("v1.0.0", &app)["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let second = home.record_about("Pin the lexer crate to its fork", "v2.0.0", &app)["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let server = home.serve(&[]);
    let (status, reply) = post(
        &server,
        SHARED,
        &json!({"origin": "claude:session/one", "project": PROJECT}),
    );
    assert_eq!(status, 200, "{reply}");
    let reply: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(
        reply,
        json!({"pending": [{"memory": first, "tag": "v1.0.0"}, {"memory": second, "tag": "v2.0.0"}], "written": 0, "skipped": 0})
    );
    let head = commit(&app, "HEAD");
    let wrong = json!({"memory": second, "tag": "v1.0.0", "commit": head});
    let right = json!({"memory": first, "tag": "v1.0.0", "commit": head});
    let (status, reply) = post(&server, SHARED, &asked(json!([wrong, right])));
    assert_eq!(status, 200, "{reply}");
    let reply: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(
        reply,
        json!({"pending": [{"memory": second, "tag": "v2.0.0"}], "written": 1, "skipped": 1})
    );
    assert_eq!(
        home.ended(&first),
        Some((
            "tag".to_owned(),
            format!("tag v1.0.0 at {} in a checkout of {PROJECT}", &head[..7]),
            "claude:session/one".to_owned()
        ))
    );
    assert_eq!(home.ended(&second), None, "another tag's end");
    let (status, reply) = post(&server, SHARED, &asked(json!([right])));
    assert_eq!(status, 200, "{reply}");
    let reply: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(
        (reply["written"].clone(), reply["skipped"].clone()),
        (json!(0), json!(1))
    );
    // An end sent for another project's checkout is skipped.
    let elsewhere = json!({"origin": "claude:session/one", "project": "github.com/someone/app",
        "ended": [{"memory": second, "tag": "v2.0.0", "commit": head}]});
    let (_, reply) = post(&server, SHARED, &elsewhere);
    let reply: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(
        (reply["written"].clone(), reply["skipped"].clone()),
        (json!(0), json!(1))
    );
    assert_eq!(home.ended(&second), None, "another project's tag");
    // Another project's checkout lists none of these.
    let other = json!({"origin": "claude:session/one", "project": "github.com/someone/app"});
    let (_, reply) = post(&server, SHARED, &other);
    assert_eq!(
        serde_json::from_str::<Value>(&reply).unwrap()["pending"],
        json!([])
    );
}

// A request the route can't take is refused with its reason and writes
// nothing; a read-only token may ask what is pending but end nothing.
#[test]
fn the_tags_route_refuses_what_it_cannot_take_and_writes_nothing() {
    let home = Home::new();
    let app = home.checkout();
    let id = home.record("v1.0.0", &app)["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let server = home.serve(&[]);
    let head = commit(&app, "HEAD");
    let end = |commit: &str| json!([{"memory": id, "tag": "v1.0.0", "commit": commit}]);
    for (body, reason) in [
        (json!({"project": PROJECT}), "Malformed tags request"),
        (
            asked(json!([{"memory": id, "tag": "v1.0.0"}])),
            "Malformed tags request",
        ),
        (
            asked(end(&head[..7])),
            "is not a commit's 40 or 64 hex characters",
        ),
        (
            asked(end(&"g".repeat(40))),
            "is not a commit's 40 or 64 hex characters",
        ),
        (
            asked(json!([{"memory": &id[..8], "tag": "v1.0.0", "commit": head}])),
            "is not a memory's full id",
        ),
        (
            json!({"origin": " ", "project": PROJECT, "ended": end(&head)}),
            "origin is empty",
        ),
        (
            json!({"origin": "x".repeat(241), "project": PROJECT, "ended": end(&head)}),
            "origin has more than 240 characters",
        ),
        (
            json!({"origin": "claude:session/one", "project": "/home/someone/app", "ended": end(&head)}),
            "project is a local path",
        ),
        (
            json!({"origin": "claude:session/one", "project": "", "ended": end(&head)}),
            "project has 0 characters; it must have 1 to 240",
        ),
    ] {
        let (status, reply) = post(&server, SHARED, &body);
        assert_eq!(status, 400, "{reason}: {reply}");
        assert!(reply.contains(reason), "{reason}: {reply}");
    }
    let (status, reply) = post_declaring(&server, SHARED, 300 * 1024, "");
    assert_eq!(status, 413, "{reply}");
    assert_eq!(reply, "A tags request is limited to 256 KiB");
    let reader = home.reader();
    let (status, reply) = post(&server, &reader, &asked(end(&head)));
    assert_eq!(status, 403, "{reply}");
    assert!(reply.contains("is read-only"), "{reply}");
    let (status, reply) = post(&server, &reader, &asked(json!([])));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(
        serde_json::from_str::<Value>(&reply).unwrap()["pending"],
        json!([{"memory": id, "tag": "v1.0.0"}])
    );
    assert_eq!(home.ended(&id), None);
}

// A session on a memory server ends the condition through `POST /tags`
// once its checkout has the tag, under the session's origin.
#[test]
fn a_session_on_a_server_ends_a_tag_made_in_its_checkout() {
    let home = Home::new();
    let app = home.checkout();
    let id = home.record("v1.0.0", &app)["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let _server = home.serve(&[]);
    home.start("s1", &app);
    home.ran("s1", &app, "cargo build");
    assert_eq!(home.ended(&id), None);
    home.git(&app, &["tag", "-a", "-m", "release", "v1.0.0"]);
    home.ran("s1", &app, "git tag -a -m release v1.0.0");
    let head = commit(&app, "HEAD");
    assert_eq!(
        home.ended(&id),
        Some((
            "tag".to_owned(),
            format!("tag v1.0.0 at {} in a checkout of {PROJECT}", &head[..7]),
            "claude:session/s1".to_owned()
        ))
    );
}

// A server from before the route answers 404: the hooks carry on, end
// nothing, and ask it nothing for the rest of the session, even once the
// server has the route; a new session asks again.
#[test]
fn a_server_without_the_tags_route_is_not_asked_again_that_session() {
    let home = Home::new();
    let app = home.checkout();
    let id = home.record("v1.0.0", &app)["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let older = home.serve(&[("USER_MEMORY_TEST_NO_TAGS_ROUTE", "1")]);
    let (status, _) = post(&older, SHARED, &asked(json!([])));
    assert_eq!(status, 404);
    let before = home.lookups();
    home.start("s1", &app);
    home.git(&app, &["tag", "v1.0.0"]);
    home.ran("s1", &app, "git tag v1.0.0");
    assert_eq!(home.lookups(), before, "nothing pending is known");
    let address = older.address.clone();
    drop(older);
    let _newer = home.serve_on(&address, &[]);
    home.prompted("s1", &app, "carry on with the release");
    home.ran("s1", &app, "cargo build");
    assert_eq!(home.ended(&id), None, "the session asks nothing again");
    home.start("s2", &app);
    assert!(home.ended(&id).is_some(), "a new session asks");
}

// A read-only token ends nothing on the server and never sends the end
// again in that session, even once the hooks may write; a session with a
// writing token ends it.
#[test]
fn a_read_only_session_never_sends_its_end() {
    let home = Home::new();
    let app = home.checkout();
    let id = home.record("v1.0.0", &app)["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let _server = home.serve(&[]);
    home.reader();
    *home.client_token.borrow_mut() = Some(home.dir.path().join("reader.token"));
    home.start("s1", &app);
    home.git(&app, &["tag", "v1.0.0"]);
    home.ran("s1", &app, "git tag v1.0.0");
    assert_eq!(home.ended(&id), None, "a read-only token");
    *home.client_token.borrow_mut() = None;
    home.ran("s1", &app, "cargo build");
    home.prompted("s1", &app, "carry on with the release");
    assert_eq!(home.ended(&id), None, "not sent again that session");
    home.start("s2", &app);
    assert!(home.ended(&id).is_some(), "a writing session ends it");
}

/// Milliseconds each call of the slow git waits: past the tool hook's limit
/// as the tests scale it (100 ms times `HOOK_ROOM`'s 10), so a push that
/// waited on git would come too late to be printed.
const SLOW_GIT_MS: u64 = 1_500;

#[test]
fn a_tool_hook_with_no_tag_waited_for_spawns_no_git_in_a_new_folder() {
    let home = Home::new();
    let checkout = home.checkout();
    let sub = checkout.join("docs");
    fs::create_dir(&sub).unwrap();
    home.start("s1", &checkout);
    let before = home.git_calls();
    home.ran("s1", &sub, "ls");
    home.ran("s1", home.dir.path(), "ls");
    assert_eq!(home.git_calls(), before, "no tag is waited for");
}

#[test]
fn a_push_never_waits_on_the_tag_check_s_git() {
    let home = Home::new();
    let checkout = home.checkout();
    let sub = checkout.join("crates");
    fs::create_dir(&sub).unwrap();
    let tag = home.record("v9.0.0", &checkout)["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = home.publish_note(&checkout);
    home.start("s1", &checkout);
    // A tag is waited for and is made, so the tool hook looks it up, with
    // git, in a folder start never saw.
    home.git(&checkout, &["tag", "v9.0.0"]);
    fs::write(home.shim().join("git.delay"), SLOW_GIT_MS.to_string()).unwrap();
    let pushed = home.ran("s1", &sub, "cargo publish");
    assert!(pushed.contains(&id[..8]), "the push arrives: {pushed:?}");
    assert!(home.end(&tag).is_some(), "the check ran after it");
}

/// Milliseconds the tag check's call for the git folder waits: past the
/// prompt hook's limit as the tests scale it (1 s times `HOOK_ROOM`'s 10).
/// Only that call is slow: the prompt hook finds a new folder's project
/// with git of its own before its push, which the tag check does not
/// change.
const SLOW_GIT_DIR_MS: u64 = 11_000;

#[test]
fn a_prompt_s_push_never_waits_on_the_tag_check_s_git() {
    let home = Home::new();
    let checkout = home.checkout();
    home.record("v9.0.0", &checkout);
    let id = home.publish_note(&checkout);
    home.start("s1", &checkout);
    // A worktree start never placed: the prompt places it with git.
    let worktree = home.dir.path().join("app-release");
    home.git(
        &checkout,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            worktree.to_str().unwrap(),
        ],
    );
    fs::write(
        home.shim().join("git.delay"),
        format!("{SLOW_GIT_DIR_MS} --git-common-dir"),
    )
    .unwrap();
    let printed = home.prompted("s1", &worktree, "Run cargo publish for the new crate");
    assert!(printed.contains(&id[..8]), "the push arrives: {printed:?}");
}

#[test]
fn a_folder_of_a_placed_checkout_is_placed_with_no_git() {
    let home = Home::new();
    let checkout = home.checkout();
    let sub = checkout.join("src").join("parser");
    fs::create_dir_all(&sub).unwrap();
    home.record("v9.0.0", &checkout);
    home.start("s1", &checkout);
    let before = home.git_calls();
    home.ran("s1", &sub, "ls");
    home.ran("s1", &checkout.join("src"), "ls");
    assert_eq!(
        home.git_calls(),
        before,
        "placed from the checkout start placed"
    );
}

#[test]
fn a_tool_hook_leaves_a_checkout_it_has_not_placed_to_the_next_prompt() {
    let home = Home::new();
    let checkout = home.checkout();
    let id = home.record("v9.0.0", &checkout)["id"]
        .as_str()
        .unwrap()
        .to_owned();
    home.start("s1", &checkout);
    // A worktree of the same repository, whose `.git` is a file: another
    // checkout of the project, at a top folder start never placed.
    let worktree = home.dir.path().join("app-release");
    home.git(
        &checkout,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            worktree.to_str().unwrap(),
        ],
    );
    home.git(&checkout, &["tag", "v9.0.0"]);
    let before = home.git_calls();
    home.ran("s1", &worktree, "cargo build");
    assert_eq!(home.git_calls(), before, "no git in a tool hook");
    assert_eq!(home.end(&id), None);
    home.prompted("s1", &worktree, "go on");
    assert!(home.end(&id).is_some(), "the prompt places it and looks");
}

#[test]
fn a_repository_inside_a_placed_checkout_is_not_taken_for_it() {
    let home = Home::new();
    let checkout = home.checkout();
    let id = home.record("v9.0.0", &checkout)["id"]
        .as_str()
        .unwrap()
        .to_owned();
    home.start("s1", &checkout);
    // A fork vendored inside the checkout has the tag; the checkout's refs
    // change too, so a hook that took the fork for the checkout would look.
    let vendored = checkout.join("vendor").join("app");
    home.repository(&vendored, "git@github.com:fork/app.git");
    home.git(&vendored, &["tag", "v9.0.0"]);
    home.git(&checkout, &["tag", "v0.1.0"]);
    home.ran("s1", &vendored, "cargo build");
    home.prompted("s1", &vendored, "go on");
    assert_eq!(home.end(&id), None);
}

#[test]
fn a_checkout_above_a_git_ceiling_is_not_taken_for_a_folder_below_it() {
    let home = Home::new();
    let checkout = home.checkout();
    let ceiling = checkout.join("work");
    let below = ceiling.join("task");
    fs::create_dir_all(&below).unwrap();
    let id = home.record("v9.0.0", &checkout)["id"]
        .as_str()
        .unwrap()
        .to_owned();
    home.start("s1", &checkout);
    home.env
        .borrow_mut()
        .push(("GIT_CEILING_DIRECTORIES".to_owned(), ceiling));
    // Git finds no repository below the ceiling, so the tag the checkout
    // now has decides nothing there.
    home.git(&checkout, &["tag", "v9.0.0"]);
    let before = home.git_calls();
    home.ran("s1", &below, "cargo build");
    assert_eq!(home.end(&id), None);
    assert_eq!(home.git_calls(), before, "the walk honours the ceiling");
}

#[test]
fn a_tool_hook_leaves_a_folder_to_git_when_git_dir_is_set() {
    let home = Home::new();
    let checkout = home.checkout();
    let sub = checkout.join("src");
    fs::create_dir(&sub).unwrap();
    let id = home.record("v9.0.0", &checkout)["id"]
        .as_str()
        .unwrap()
        .to_owned();
    home.start("s1", &checkout);
    // GIT_DIR names the repository whatever the folders say, so only git
    // can place the folder: the tool hook leaves it, and the tag waits.
    home.env
        .borrow_mut()
        .push(("GIT_DIR".to_owned(), checkout.join(".git")));
    home.git(&checkout, &["tag", "v9.0.0"]);
    let before = home.git_calls();
    home.ran("s1", &sub, "cargo build");
    assert_eq!(home.git_calls(), before, "no git in a tool hook");
    assert_eq!(home.end(&id), None);
}

#[test]
fn a_prompt_asks_git_for_its_folder_once() {
    let home = Home::new();
    let checkout = home.checkout();
    let sub = checkout.join("src");
    fs::create_dir(&sub).unwrap();
    home.start("s1", &checkout);
    // The prompt reads its folder again each time: its root, then its
    // remote for the project key. The root also finds its list file.
    let once = [
        "rev-parse --show-toplevel",
        "config --get remote.origin.url",
    ];
    for (what, folder) in [
        ("start's folder", &checkout),
        ("start's folder again", &checkout),
        ("a folder the session never saw", &sub),
        ("that folder again", &sub),
    ] {
        let before = home.git_calls();
        home.prompted("s1", folder, "go on");
        assert_eq!(home.calls_after(before), once, "{what}");
    }
}

#[test]
fn a_listed_folder_git_cannot_read_is_named_with_git_s_complaint() {
    let home = Home::new();
    let checkout = home.checkout();
    let listed = home.dir.path().join("refusedweb");
    home.repository(&listed, "git@github.com:example/web.git");
    fs::write(checkout.join(".user-memory"), "@../refusedweb\n").unwrap();
    fs::write(home.shim().join("git.fail"), "refusedweb").unwrap();
    let shown = home.hook(
        "session-start",
        &checkout,
        &json!({"session_id": "s1", "cwd": checkout, "hook_event_name": "SessionStart", "source": "startup"}),
    );
    let note = shown
        .lines()
        .find(|line| line.starts_with("Raise with the user once this session: "))
        .unwrap_or_else(|| panic!("{shown}"));
    assert!(
        note.contains("@../refusedweb: no project (git rev-parse --show-toplevel failed in ")
            && note.contains(": fatal: detected dubious ownership in repository)"),
        "{note}"
    );
    let asked = fs::read_to_string(home.log())
        .unwrap()
        .lines()
        .filter(|line| line.contains("refusedweb"))
        .count();
    assert_eq!(asked, 1, "git is asked about the folder once");
}
