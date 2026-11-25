#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

//! A session's use of memories (#123), through the real binary: what
//! `POST /use` writes to a memory server's store, what it refuses, how the
//! rows are merged, pruned and forgotten, and what the hooks show from
//! them.

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
#[path = "support/time.rs"]
mod time;
use time::{HOOK_ROOM, STUCK};

use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
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

    /// `user-memory ARGS` on the server's store: its text output.
    fn text(&self, args: &[&str]) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_user-memory"))
            .args(args)
            .sandboxed(self.root.path())
            .env("USER_MEMORY_DB", self.db())
            .env_remove("USER_MEMORY_URL")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    /// A memory of `project` recorded with `args` after its title and
    /// content: its short id.
    fn noted(&self, project: &str, title: &str, content: &str, args: &[&str]) -> String {
        let mut all = vec![
            "record",
            "--category",
            "build",
            "--scope",
            "project",
            "--project",
            project,
            "--title",
            title,
            "--content",
            content,
        ];
        all.extend_from_slice(args);
        let reply = self.cli(&all);
        let id = reply["id"].as_str();
        id.unwrap_or_else(|| panic!("{title}: {reply}"))[..8].to_owned()
    }

    /// A git repository in the server's folder whose remote is `remote`.
    fn repository(&self, name: &str, remote: &str) -> PathBuf {
        let repo = self.root.path().join(name);
        fs::create_dir(&repo).unwrap();
        for args in [&["init", "-q"][..], &["remote", "add", "origin", remote]] {
            let status = Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", self.git_config())
                .status()
                .unwrap();
            assert!(status.success());
        }
        repo
    }

    /// An empty git config, so the user's own never reaches a test.
    fn git_config(&self) -> PathBuf {
        let file = self.root.path().join(".gitconfig-empty");
        if !file.exists() {
            fs::write(&file, "").unwrap();
        }
        file
    }

    /// The text the `hook` (session-start or subagent-start) of `client`
    /// gives the agent for `payload` in `folder`, over the server's store,
    /// with the variables `vars`; "" when it prints nothing.
    fn hook(
        &self,
        hook: &str,
        client: &str,
        folder: &Path,
        vars: &[(&str, &str)],
        payload: &Value,
    ) -> String {
        let mut child = Command::new(env!("CARGO_BIN_EXE_user-memory"))
            .args([hook, "--client", client])
            .current_dir(folder)
            .sandboxed(self.root.path())
            .env("USER_MEMORY_DB", self.db())
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.git_config())
            .env("GIT_CEILING_DIRECTORIES", self.root.path())
            .env_remove("USER_MEMORY_URL")
            .env_remove("USER_MEMORY_TOKEN_FILE")
            .env(HOOK_ROOM.0, HOOK_ROOM.1)
            .envs(vars.iter().copied())
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
        let out = time::finish(child, "the hook");
        assert!(out.status.success(), "{out:?}");
        let out = String::from_utf8(out.stdout).unwrap();
        if !out.starts_with('{') {
            return out;
        }
        let out: Value = serde_json::from_str(&out).unwrap();
        out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// What Claude Code's session start shows in `folder`, with `vars`.
    fn started(&self, folder: &Path, vars: &[(&str, &str)]) -> String {
        let payload = json!({"session_id":"s","hook_event_name":"SessionStart","source":"startup"});
        self.hook("session-start", "claude", folder, vars, &payload)
    }

    /// What `client`'s subagent hook gives a subagent starting in `folder`,
    /// with `vars` and the payload's `transcript`.
    fn subagent(
        &self,
        client: &str,
        folder: &Path,
        vars: &[(&str, &str)],
        transcript: Option<&Path>,
    ) -> String {
        let mut payload = json!({"session_id":"s","agent_id":"a1","hook_event_name":"SubagentStart","cwd":folder});
        if let Some(path) = transcript {
            payload["transcript_path"] = json!(path);
        }
        self.hook("subagent-start", client, folder, vars, &payload)
    }

    /// Each of `sessions` used each of `memories` (short ids) for `project`:
    /// read when `read`, else shown.
    fn sessions_used(&self, project: &str, sessions: &[&str], memories: &[&str], read: bool) {
        for session in sessions {
            let uses: Vec<Value> = memories
                .iter()
                .map(|id| used(&self.full(id), project, read, false, !read))
                .collect();
            let body = json!({"origin": format!("claude:session/{session}"), "uses": uses});
            let (status, reply) = self.post(SHARED, &body.to_string());
            assert_eq!(status, 200, "{reply}");
        }
    }

    /// The full id of the memory whose short id is `id`.
    fn full(&self, id: &str) -> String {
        self.cli(&["read", id])["memories"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned()
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

/// The 8-character ids an index lists, in its order.
fn listed(index: &str) -> Vec<String> {
    index
        .lines()
        .filter_map(|line| line.strip_prefix("    "))
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect()
}

// Memories a project's sessions read come first among the other memories
// of the project's index, so three past the first page reach its top; a
// read orders before a delivery; rules keep their place; and a project
// with no use, or an index of no project, keeps today's order.
#[test]
fn the_index_lists_what_the_project_s_sessions_used_first() {
    let host = Host::start(&[]);
    let fact = |title: &str| {
        host.cli(&[
            "record",
            "--type",
            "fact",
            "--category",
            "build",
            "--scope",
            "global",
            "--title",
            title,
            "--content",
            title,
        ])["id"]
            .as_str()
            .unwrap()[..8]
            .to_owned()
    };
    // Recorded first, so listed last.
    let used: Vec<String> = [
        "The cache lives in target",
        "Builds need the vendored crates",
        "The linter runs in CI only",
    ]
    .iter()
    .map(|title| fact(title))
    .collect();
    let shown = fact("Release builds strip symbols");
    for title in [
        "Docker images pin their base digest",
        "Fixtures live under tests/fixtures",
        "The changelog is written at release time",
        "Windows paths need forward slashes in config",
        "Postgres runs on port 5433 in development",
        "Translations are synced every Monday",
        "The staging deploy takes ten minutes",
        "Feature flags are read from the flags service",
        "Logs rotate daily and keep a week",
        "Fonts are bundled with the desktop app",
        "Error codes are listed in errors.md",
        "Screenshots are taken at 2x scale",
    ] {
        fact(title);
    }
    let rule = host.cli(&[
        "record",
        "--type",
        "preference",
        "--category",
        "build",
        "--scope",
        "global",
        "--title",
        "Ask before a release build",
        "--content",
        "Ask before a release build",
    ])["id"]
        .as_str()
        .unwrap()[..8]
        .to_owned();
    let page = |project: &[&str]| {
        let mut args = vec!["index", "--page-size", "6"];
        args.extend_from_slice(project);
        listed(&host.text(&args))
    };
    let before = page(&["--project", PROJECT]);
    assert_eq!(before, page(&[]), "no use, no other order");
    for id in used.iter().chain([&shown]) {
        assert!(
            !before[1..].contains(id),
            "{id} on the first page: {before:?}"
        );
    }
    let full = |id: &str| -> String {
        host.cli(&["read", id])["memories"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let mut rows: Vec<Value> = used
        .iter()
        .map(|id| used_row(&full(id), true, false))
        .collect();
    rows.push(used_row(&full(&shown), false, true));
    let body = json!({"origin": "claude:session/one", "uses": rows});
    assert_eq!(host.post(SHARED, &body.to_string()).0, 200);
    let after = page(&["--project", PROJECT]);
    assert_eq!(after[0], rule, "rules first: {after:?}");
    let mut top = after[1..4].to_vec();
    top.sort();
    let mut expected = used.clone();
    expected.sort();
    assert_eq!(top, expected, "{after:?}");
    assert_eq!(after[4], shown, "a delivery after the reads: {after:?}");
    // Another project's index, or one of no project, is as before.
    assert_eq!(page(&[]), before);
    assert_eq!(page(&["--project", "github.com/example/site"]), before);
}

/// A use row of the tests' project, read or shown.
fn used_row(memory: &str, read: bool, shown: bool) -> Value {
    used(memory, PROJECT, read, false, shown)
}

/// The most-used part's first line.
const MOST_USED: &str =
    "Most used in this project's recent sessions (read one for the rest before applying it):";
/// Opens the tests' project's rules.
const RULES_OF_PROJECT: &str =
    "Project rules of github.com/example/app, strongest first; the user stated them, follow them:";
/// Opens the index after the parts before it.
const INDEX_HEADER: &str = "USER MEMORY INDEX";
/// The room the most-used part takes at most, as session start gives it.
const MOST_USED_CHARS: usize = 3_000;
/// Hides the most-used part, as a server from before it (debug builds).
const OLDER_SERVER: (&str, &str) = ("USER_MEMORY_TEST_NO_MOST_USED_PART", "1");

/// The most-used part of a hook's text, from its first line to the index:
/// its lines, or none.
fn most_used(text: &str) -> Vec<&str> {
    let mut lines = text.lines().skip_while(|line| *line != MOST_USED);
    if lines.next().is_none() {
        return Vec::new();
    }
    lines.take_while(|line| line.starts_with("- ")).collect()
}

/// The short ids of a most-used part's lines.
fn ids<'a>(part: &[&'a str]) -> Vec<&'a str> {
    part.iter()
        .map(|line| line.split_whitespace().nth(1).unwrap())
        .collect()
}

/// Distinct one-word titles, which the similarity check never holds alike.
const WORDS: [&str; 20] = [
    "Alpaca", "Bramble", "Cobalt", "Dune", "Ember", "Fjord", "Garnet", "Harbor", "Indigo",
    "Juniper", "Kestrel", "Lantern", "Mosaic", "Nebula", "Orchid", "Pylon", "Quartz", "Rivet",
    "Saffron", "Tundra",
];

/// The sessions s1 to s`n`.
fn sessions(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("s{i}")).collect()
}

// Session start shows, after the project rules and before the index, what
// at least half the project's recent sessions used, the most used first:
// the user's facts and decisions and agents' notes, labelled as pushes
// label them, whole when short and as title and first passage when long;
// never a rule, an inference, an observation about the user, or what
// fewer sessions used. A project with no use, another project, or a
// plain index call shows no part.
#[test]
fn session_start_shows_what_most_of_the_project_s_sessions_used() {
    let host = Host::start(&[]);
    let app = host.repository("app", "git@github.com:example/app.git");
    let site = host.repository("site", "git@github.com:example/site.git");
    let explicit = |title: &str, kind: &str| host.noted(PROJECT, title, title, &["--type", kind]);
    let observed = |title: &str, kind: &str| {
        host.noted(
            PROJECT,
            title,
            title,
            &[
                "--type",
                kind,
                "--source",
                "observed",
                "--evidence",
                "The user ran it so in the session",
            ],
        )
    };
    let often = explicit("Integration tests need the mock server", "fact");
    let half = explicit("The cache stays in SQLite", "decision");
    let once = explicit("The docs build with mdBook", "fact");
    let first = "The release script signs every archive with the project's key, then uploads them in the order the manifest lists, so a mirror that syncs halfway never serves an unsigned file. ".repeat(2);
    let long = host.noted(
        PROJECT,
        "Releases are signed before upload",
        &format!("{}\n\nA second paragraph says how the key is rotated and who holds the spare copy, which a session rarely needs. It runs long on purpose.", first.trim_end()),
        &["--type", "fact"],
    );
    let note = observed("The fixtures regenerate with just fixtures", "fact");
    let wish = observed("The user reruns failing tests alone first", "preference");
    let habit = explicit("The user reviews diffs before committing", "behavior");
    let rule = explicit("Run the linter before every commit", "preference");
    let inferred = host.noted(
        PROJECT,
        "Tests depend on generated fixtures",
        "Tests depend on generated fixtures",
        &[
            "--source",
            "inferred",
            "--evidence-id",
            &host.full(&often),
            "--evidence-id",
            &host.full(&note),
            "--reason",
            "Both say the tests need generated inputs",
        ],
    );
    assert!(most_used(&host.started(&app, &[])).is_empty(), "no use yet");
    let all = sessions(4);
    let all: Vec<&str> = all.iter().map(String::as_str).collect();
    host.sessions_used(
        PROJECT,
        &all,
        &[&often, &habit, &rule, &inferred, &wish],
        true,
    );
    host.sessions_used(PROJECT, &all, &[&note], false);
    host.sessions_used(PROJECT, &all[..3], &[&long], true);
    host.sessions_used(PROJECT, &all[..2], &[&half], true);
    host.sessions_used(PROJECT, &all[..1], &[&once], true);
    let text = host.started(&app, &[]);
    let part = most_used(&text);
    assert_eq!(ids(&part), [&often, &note, &long, &half], "{text}");
    assert_eq!(
        part[0],
        format!("- {often} The user's fact: Integration tests need the mock server")
    );
    assert_eq!(
        part[1],
        format!(
            "- {note} An agent's note from an earlier session (fact): The fixtures regenerate with just fixtures"
        )
    );
    assert_eq!(
        part[2],
        format!(
            "- {long} The user's fact: Releases are signed before upload — {} …",
            first.trim_end()
        )
    );
    assert_eq!(
        part[3],
        format!("- {half} The user's decision: The cache stays in SQLite")
    );
    let at = |line: &str| text.find(line).unwrap_or_else(|| panic!("{line}: {text}"));
    assert!(at(RULES_OF_PROJECT) < at(MOST_USED), "{text}");
    assert!(at(MOST_USED) < at(INDEX_HEADER), "{text}");
    assert!(text[at(RULES_OF_PROJECT)..at(MOST_USED)].contains(&rule));
    assert!(
        most_used(&host.started(&site, &[])).is_empty(),
        "another project"
    );
    // Only the hooks ask for the part: a plain index call is as before.
    for args in [&["index", "--project", PROJECT][..], &["index"]] {
        let index = host.text(args);
        assert!(!index.contains(MOST_USED), "{args:?}: {index}");
    }
}

// The part prints at most fifteen memories, and no more than its room.
#[test]
fn the_most_used_part_keeps_to_fifteen_memories_and_its_room() {
    let host = Host::start(&[]);
    let app = host.repository("app", "git@github.com:example/app.git");
    let site = host.repository("site", "git@github.com:example/site.git");
    let few: Vec<String> = WORDS
        .iter()
        .map(|word| host.noted(PROJECT, word, word, &[]))
        .collect();
    assert_eq!(few.len(), 20);
    let few: Vec<&str> = few.iter().map(String::as_str).collect();
    host.sessions_used(PROJECT, &["s1", "s2"], &few, true);
    assert_eq!(most_used(&host.started(&app, &[])).len(), 15);
    let wide = "github.com/example/site";
    let many: Vec<String> = WORDS[..15]
        .iter()
        .map(|word| {
            // Whole, as it is no longer than 300 characters.
            let content = format!("{word} ").repeat(32);
            assert!(content.len() <= 300, "{}", content.len());
            host.noted(wide, word, content.trim_end(), &[])
        })
        .collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    host.sessions_used(wide, &["s1", "s2"], &many, true);
    let text = host.started(&site, &[]);
    let part = most_used(&text);
    let printed = [MOST_USED]
        .iter()
        .chain(&part)
        .map(|line| line.chars().count() + 1)
        .sum::<usize>();
    assert!(!part.is_empty() && part.len() < 15, "{text}");
    assert!(printed <= MOST_USED_CHARS, "{printed}: {text}");
}

// A subagent gets the most-used part after the project rules, as session
// start shows them, and the part alone in a project with no rules; a
// server from before the part shows none, and a subagent then gets the
// rules alone, or nothing. A forked Codex sub-agent is skipped only when
// its transcript holds both headings.
#[test]
fn a_subagent_gets_the_most_used_part_after_the_rules() {
    let host = Host::start(&[]);
    let app = host.repository("app", "git@github.com:example/app.git");
    let site = host.repository("site", "git@github.com:example/site.git");
    let rule = host.noted(
        PROJECT,
        "Run the linter before every commit",
        "Run the linter before every commit",
        &["--type", "preference"],
    );
    let fact = host.noted(
        PROJECT,
        "Integration tests need the mock server",
        "Integration tests need the mock server",
        &[],
    );
    let site_fact = host.noted(
        "github.com/example/site",
        "Pages build with Zola",
        "Pages build with Zola",
        &[],
    );
    host.sessions_used(PROJECT, &["s1"], &[&fact, &rule], true);
    host.sessions_used("github.com/example/site", &["s1"], &[&site_fact], true);
    let start = host.started(&app, &[]);
    let shown =
        start[start.find(RULES_OF_PROJECT).unwrap()..start.find(INDEX_HEADER).unwrap()].trim_end();
    assert!(
        shown.contains(MOST_USED) && shown.contains(&fact),
        "{start}"
    );
    for client in ["claude", "codex"] {
        assert_eq!(host.subagent(client, &app, &[], None), shown, "{client}");
        let alone = host.subagent(client, &site, &[], None);
        assert!(
            alone.starts_with(MOST_USED) && alone.contains(&site_fact),
            "{client}: {alone}"
        );
        let rules = host.subagent(client, &app, &[OLDER_SERVER], None);
        assert!(
            rules.starts_with(RULES_OF_PROJECT) && !rules.contains(MOST_USED),
            "{client}: {rules}"
        );
        assert_eq!(
            host.subagent(client, &site, &[OLDER_SERVER], None),
            "",
            "{client}"
        );
    }
    assert!(!host.started(&app, &[OLDER_SERVER]).contains(MOST_USED));
    let transcript = |name: &str, text: &str| {
        let line = json!({"timestamp":"2026-11-01T00:00:00.000Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":text}]}});
        let path = host.root.path().join(name);
        fs::write(&path, format!("{line}\n")).unwrap();
        path
    };
    let both = transcript("both.jsonl", &start);
    let rules_only = transcript(
        "rules.jsonl",
        &start.replace(MOST_USED, "Most used elsewhere"),
    );
    assert_eq!(host.subagent("codex", &app, &[], Some(&both)), "");
    assert_eq!(host.subagent("codex", &app, &[], Some(&rules_only)), shown);
}

/// The least room the most-used part gets, out of the rules' when short.
const MOST_USED_FLOOR: usize = 1_500;
/// The most room the project rules take.
const PROJECT_RULE_CHARS: usize = 6_000;

/// The characters a hook's project rules take, line breaks included.
fn rules_chars(text: &str) -> usize {
    text.lines()
        .skip_while(|line| *line != RULES_OF_PROJECT)
        .take_while(|line| *line != MOST_USED && !line.starts_with(INDEX_HEADER))
        .map(|line| line.chars().count() + 1)
        .sum()
}

// Beside the index, a most-used part that prints takes at most its floor
// out of the project rules' room; without one, the rules keep all of it.
#[test]
fn a_most_used_part_takes_at_most_its_floor_from_the_rules() {
    let host = Host::start(&[]);
    let app = host.repository("app", "git@github.com:example/app.git");
    for i in 0..40 {
        // Words of their own, so no rule is held similar to another.
        let words: Vec<String> = (0..30).map(|j| format!("r{i}w{j}")).collect();
        let title = format!("Rule r{i}t{}", i * 37);
        host.noted(PROJECT, &title, &words.join(" "), &["--type", "preference"]);
    }
    let alone = host.started(&app, &[]);
    let whole = rules_chars(&alone);
    assert!(whole > PROJECT_RULE_CHARS - 300, "{whole}: {alone}");
    let facts: Vec<String> = WORDS[..15]
        .iter()
        .map(|word| {
            let content = format!("{word} ").repeat(32);
            host.noted(PROJECT, word, content.trim_end(), &[])
        })
        .collect();
    let facts: Vec<&str> = facts.iter().map(String::as_str).collect();
    host.sessions_used(PROJECT, &["s1", "s2"], &facts, true);
    let text = host.started(&app, &[]);
    let part: usize = [MOST_USED]
        .iter()
        .chain(&most_used(&text))
        .map(|line| line.chars().count() + 1)
        .sum();
    let rules = rules_chars(&text);
    // One rule or one memory's line is the most either can fall short by.
    assert!(part > MOST_USED_FLOOR - 320, "{part}: {text}");
    assert!(
        whole - rules < MOST_USED_FLOOR + 320,
        "{whole} {rules}: {text}"
    );
    assert!(text.contains(INDEX_HEADER), "{text}");
}
