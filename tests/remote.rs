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
};
use tempfile::{TempDir, tempdir};

#[path = "support/fake.rs"]
mod fake;
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
        let mut child = Command::new(env!("CARGO_BIN_EXE_user-memory"))
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
    let line = lines.recv_timeout(STUCK).expect("Connector sent no reply");
    serde_json::from_str(&line).unwrap()
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
    let errors = child.0.stderr.take().unwrap();
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
    input.flush().unwrap();
    host.child.kill().unwrap();
    host.child.wait().unwrap();
    writeln!(input, "{}", call(2, "index", json!({}))).unwrap();
    input.flush().unwrap();
    let value = reply(&output);
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
    let unavailable = reply(&output);
    assert_eq!(unavailable["id"], 3);
    assert!(unavailable.get("error").is_some());
    host.restart();
    writeln!(input, "{}", call(4, "index", json!({}))).unwrap();
    input.flush().unwrap();
    let recovered = reply(&output);
    assert_eq!(recovered["id"], 4);
    assert_ne!(recovered["result"]["isError"], true);
    host.child.kill().unwrap();
    host.child.wait().unwrap();
    fs::write(host.root.path().join("token"), "f".repeat(64)).unwrap();
    host.restart();
    writeln!(input, "{}", call(5, "index", json!({}))).unwrap();
    input.flush().unwrap();
    let unauthorized = reply(&output);
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
    ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_user-memory"))
            .arg("connect")
            .arg("--url")
            .arg(format!("http://{}/mcp", host.address))
            .arg("--token-file")
            .arg(token)
            .sandboxed(home)
            .env_remove("USER_MEMORY_DB")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    )
}

/// Reads a finished connector's whole stderr and fails if the token is in it.
fn assert_no_token(mut errors: impl Read) {
    let mut said = String::new();
    errors.read_to_string(&mut said).unwrap();
    assert!(
        !said.contains(TOKEN),
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
    let errors = child.0.stderr.take().unwrap();
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
    assert_eq!(before["id"], 2);
    assert_ne!(before["result"]["isError"], true, "{before}");
    host.child.kill().unwrap();
    host.child.wait().unwrap();
    host.restart();
    writeln!(input, "{}", call(3, "index", json!({}))).unwrap();
    input.flush().unwrap();
    let after = reply(&output);
    assert_eq!(after["id"], 3);
    assert_ne!(after["result"]["isError"], true, "{after}");
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
    // This client refuses reindex, so the advice names where it runs.
    let next = applied["next"].as_str().unwrap();
    assert!(
        next.contains("optionally run `user-memory reindex` on the machine that serves http://"),
        "{next}"
    );
    assert!(preview.get("next").is_none(), "{preview}");
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
        if cfg!(windows) {
            fs::rename(&exe, bin.join("old.exe")).unwrap();
        }
        fs::rename(&staged, &exe).unwrap();

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
    let said = said.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let said = said.clone();
            std::thread::spawn(move || answer_fake(stream, &said));
        }
    });
    format!("http://{address}/mcp")
}

/// Answers one request to the fake server of [`versioned_endpoint`].
fn answer_fake(mut stream: std::net::TcpStream, said: &Shared) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();
    let mut length = 0;
    if reader.read_line(&mut request).is_err() {
        return;
    }
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let message: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let mut said = said.lock().unwrap();
    let slow = said.slow_tools && message["method"] == "tools/list";
    let (status, reply) = if !request.starts_with("POST") {
        (405, Value::Null)
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
            Some("initialize") => {
                let mut capabilities = json!({"tools":{}});
                if let Some(min) = &said.min_client {
                    capabilities["experimental"] = json!({"user-memory": {"min_client": min}});
                }
                json!({"result":{"protocolVersion":message["params"]["protocolVersion"],"capabilities":capabilities,"serverInfo":{"name":"user-memory","version":said.version}}})
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
    let _ = write!(
        stream,
        "HTTP/1.1 {status} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
            if cfg!(windows) {
                fs::rename(&exe, bin.join("old.exe")).unwrap();
            }
            fs::rename(&staged, &exe).unwrap();
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
