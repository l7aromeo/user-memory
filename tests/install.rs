//! The one-line installer published with GitHub Releases, run end to end
//! against a fake releases server and a real loopback memory server, plus the
//! memory server's unauthenticated root and the release script's checks.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]
#![cfg(unix)]

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{Arc, Mutex},
};
#[path = "support/fake.rs"]
mod fake;
#[path = "support/port.rs"]
mod port;
#[path = "support/releases.rs"]
mod releases;
use port::closed_port;
use releases::{Releases, TAG, native_target, sha256};
use tempfile::{TempDir, tempdir};

fn repo_file(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(name)
}

fn python() -> Option<&'static str> {
    [
        "/usr/bin/python3",
        "/usr/local/bin/python3",
        "/opt/homebrew/bin/python3",
    ]
    .into_iter()
    .find(|p| Path::new(p).exists())
}

/// A real memory server on loopback, the address a client installs against.
struct Server {
    root: TempDir,
    child: Child,
    address: String,
}

impl Server {
    fn start() -> Self {
        let root = tempdir().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_user-memory"))
            .args(["host", "--listen", "127.0.0.1:0", "--db"])
            .arg(root.path().join("memories.sqlite3"))
            .arg("--token-file")
            .arg(root.path().join("access-token"))
            .env_remove("USER_MEMORY_DB")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let address = line
            .trim()
            .strip_prefix("Listening: http://")
            .and_then(|l| l.strip_suffix("/mcp"))
            .unwrap_or_else(|| panic!("host did not start: {line}"))
            .to_owned();
        Self {
            root,
            child,
            address,
        }
    }
    fn token(&self) -> String {
        fs::read_to_string(self.root.path().join("access-token"))
            .unwrap()
            .trim()
            .to_owned()
    }
    fn base(&self) -> String {
        format!("http://{}", self.address)
    }
    /// (status, body) of a request with extra curl arguments.
    fn get(&self, path: &str, args: &[&str]) -> (u16, String) {
        let out = Command::new("curl")
            .args(["-s", "-w", "\n%{http_code}"])
            .args(args)
            .arg(format!("{}{path}", self.base()))
            .output()
            .unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        let (body, status) = text.rsplit_once('\n').unwrap();
        (status.parse().unwrap(), body.to_owned())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_server_root_needs_no_token_but_mcp_does_and_nothing_else_is_served() {
    let s = Server::start();
    assert_eq!(
        s.get("/", &[]),
        (200, "user-memory MCP server. Endpoint: /mcp\n".into())
    );
    assert_eq!(s.get("/", &["-X", "POST"]).0, 405);
    for path in [
        "/install",
        "/dist/version",
        "/dist/aarch64-apple-darwin/user-memory",
    ] {
        assert_eq!(s.get(path, &[]).0, 404, "{path}");
    }
    let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    let post = [
        "-X",
        "POST",
        "-H",
        "Content-Type: application/json",
        "-H",
        "Accept: application/json, text/event-stream",
        "--data",
        initialize,
    ];
    assert_eq!(s.get("/mcp", &post).0, 401);
    let bearer = format!("Authorization: Bearer {}", s.token());
    let mut authorized = post.to_vec();
    authorized.extend(["-H", bearer.as_str()]);
    assert_eq!(s.get("/mcp", &authorized).0, 200);
}

/// A port nothing listens on, chosen before the server that binds it starts.
/// A test running beside it must not be able to take it in between: not a
/// connection's own end, nor a listener on port 0, both of which the system
/// gives out of its ephemeral range.
#[test]
fn a_chosen_port_is_one_the_system_never_hands_out_itself() {
    let ephemeral = port::ephemeral_range();
    let ports: Vec<u16> = (0..50).map(|_| closed_port()).collect();
    for port in &ports {
        assert!(!ephemeral.contains(port), "{port} is in {ephemeral:?}");
    }
    let mut unique = ports.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), ports.len(), "a port given twice: {ports:?}");
}

/// A client machine: a temporary HOME with the fake Codex and Docker CLIs
/// (tests/fake/cli.rs says what each does).
struct Client {
    home: TempDir,
}

impl Client {
    fn new(_server: &Server) -> Self {
        Self::bare()
    }
    fn bare() -> Self {
        let home = tempdir().unwrap();
        let cli = home.path().join("cli");
        fs::create_dir(&cli).unwrap();
        for name in ["codex", "docker"] {
            fake::fake(&cli, name);
        }
        Self { home }
    }
    fn docker_log(&self) -> String {
        fs::read_to_string(self.path("docker.log")).unwrap_or_default()
    }
    fn docker_dir(&self) -> PathBuf {
        self.path(".local/share/user-memory/docker")
    }
    fn path(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }
    fn token_file(&self) -> PathBuf {
        self.path(".config/user-memory/client-token")
    }
    /// The one-liner, `curl -fsSL <releases>/latest/download/install.sh | sh
    /// -s -- …`, in a clean environment plus `env`.
    fn command(&self, releases: &Releases, env: &[(&str, &str)], args: &[&str]) -> Command {
        let mut command = self.agentless_command(releases, env, &["--client", "codex"]);
        command.args(args);
        command
    }
    /// The one-liner without the --client every other test passes.
    fn agentless_command(
        &self,
        releases: &Releases,
        env: &[(&str, &str)],
        args: &[&str],
    ) -> Command {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(
                "base=$1; shift; curl -fsSL \"$base/latest/download/install.sh\" | sh -s -- \"$@\"",
            )
            .arg("sh")
            .arg(&releases.base)
            .arg("--install-dir")
            .arg(self.path("bin"))
            .args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("CLAUDE_CONFIG_DIR", self.home.path())
            .env("CODEX_HOME", self.path(".codex"))
            .env("USER_MEMORY_APP_DIRS", "")
            .env("USER_MEMORY_RELEASES", &releases.base)
            // Never the machine's own Ollama: tests opt in with a fake one.
            .env(
                "USER_MEMORY_OLLAMA_URL",
                format!("http://127.0.0.1:{}", closed_port()),
            )
            .env("FAKE_HOST", env!("CARGO_BIN_EXE_user-memory"))
            .env("TMPDIR", std::env::temp_dir())
            // Setup writes the git ignore of this home folder only.
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                    self.path("cli").display()
                ),
            )
            .envs(env.iter().copied())
            .stdin(Stdio::null());
        command
    }
    /// The one-liner without a terminal.
    fn install(&self, releases: &Releases, env: &[(&str, &str)], args: &[&str]) -> Output {
        detached(self.command(releases, env, args))
    }
    /// This checkout's uninstall.sh, run in `dir` without a terminal, in
    /// the same clean environment as the installer.
    fn uninstall(&self, dir: &Path, args: &[&str]) -> Output {
        detached(self.uninstall_command(dir, args))
    }
    fn uninstall_command(&self, dir: &Path, args: &[&str]) -> Command {
        let mut command = Command::new("/bin/sh");
        command
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("uninstall.sh"))
            .arg("--install-dir")
            .arg(self.path("bin"))
            .args(args)
            .current_dir(dir)
            .env_clear()
            .env("HOME", self.home.path())
            .env("CLAUDE_CONFIG_DIR", self.home.path())
            .env("CODEX_HOME", self.path(".codex"))
            .env("USER_MEMORY_APP_DIRS", "")
            .env("FAKE_HOST", env!("CARGO_BIN_EXE_user-memory"))
            .env("TMPDIR", std::env::temp_dir())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                    self.path("cli").display()
                ),
            )
            .stdin(Stdio::null());
        command
    }
}

fn environment(command: &Command) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    command
        .get_envs()
        .filter_map(|(k, v)| v.map(|v| (k.to_owned(), v.to_owned())))
        .collect()
}

/// Runs `command` without a controlling terminal, so a prompt can never wait
/// for input even when the tests run in a terminal.
fn detached(mut command: Command) -> Output {
    let Some(python) = python() else {
        return command.output().unwrap();
    };
    let mut detached = Command::new(python);
    // -B: Apple's python3 would otherwise cache bytecode under the test's
    // HOME, which the uninstall tests compare before and after.
    detached
        .args([
            "-B",
            "-c",
            "import os, sys; os.setsid(); os.execv(sys.argv[1], sys.argv[1:])",
        ])
        .arg(command.get_program())
        .args(command.get_args())
        .env_clear()
        .envs(environment(&command))
        .stdin(Stdio::null());
    if let Some(dir) = command.get_current_dir() {
        detached.current_dir(dir);
    }
    detached.output().unwrap()
}

fn text(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn the_installer_connects_a_verified_client_and_a_rerun_reuses_the_server() {
    let s = Server::start();
    let releases = Releases::start();
    let c = Client::new(&s);
    let token = s.token();
    // A base address gains /mcp; the token comes from the environment.
    let out = c.install(
        &releases,
        &[
            ("USER_MEMORY_URL", &s.base()),
            ("USER_MEMORY_TOKEN", &token),
        ],
        &[],
    );
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains(&format!(
        "Downloading user-memory (latest) for {}",
        native_target()
    )));
    assert!(stdout.contains("Checksum: OK"));
    assert!(
        stdout.contains("Remote MCP connection and tools: OK"),
        "{stdout}"
    );
    assert!(!stdout.contains(&token) && !stderr.contains(&token));
    assert_eq!(
        fs::read(c.path("bin/user-memory")).unwrap(),
        fs::read(env!("CARGO_BIN_EXE_user-memory")).unwrap()
    );
    assert_eq!(
        fs::read_to_string(c.token_file()).unwrap(),
        format!("{token}\n")
    );
    assert_eq!(mode(&c.token_file()), 0o600);
    assert_eq!(mode(c.token_file().parent().unwrap()), 0o700);
    let endpoint = Command::new(c.path("bin/user-memory"))
        .arg("endpoint")
        .env_clear()
        .env("HOME", c.home.path())
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(endpoint.stdout).unwrap(),
        format!("{}/mcp\n{}\n", s.base(), c.token_file().display())
    );

    // Re-running with nothing given reuses the saved server and token.
    let again = c.install(&releases, &[], &[]);
    let (stdout, stderr) = text(&again);
    assert!(again.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains(&format!(
        "Memory server: {}/mcp (from the earlier install)",
        s.base()
    )));
    assert_eq!(
        fs::read_to_string(c.token_file()).unwrap(),
        format!("{token}\n")
    );

    // A pinned version downloads from its tag; a trailing slash is dropped.
    let pinned = c.install(
        &releases,
        &[],
        &["--version", TAG, "--url", &format!("{}/mcp/", s.base())],
    );
    assert!(pinned.status.success(), "{}", text(&pinned).1);
    let asset = format!("/download/{TAG}/user-memory-{}", native_target());
    assert!(releases.requested().iter().any(|p| p.ends_with(&asset)));
    // The doctor run that ends each install has nothing newer to look for.
    assert!(
        !releases
            .requested()
            .iter()
            .any(|p| p.ends_with("/releases/latest")),
        "{:?}",
        releases.requested()
    );
}

#[test]
fn a_download_cut_off_halfway_is_retried() {
    let s = Server::start();
    let releases = Releases::start();
    let c = Client::new(&s);
    let asset = format!("user-memory-{}", native_target());
    releases.drop_once("latest/download", &asset);
    let out = c.install(
        &releases,
        &[
            ("USER_MEMORY_URL", &s.base()),
            ("USER_MEMORY_TOKEN", &s.token()),
        ],
        &[],
    );
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("Checksum: OK"), "{stdout}");
    assert!(
        stderr.contains("Retrying the download (1 of 3)"),
        "{stderr}"
    );
    assert_eq!(
        fs::read(c.path("bin/user-memory")).unwrap(),
        fs::read(env!("CARGO_BIN_EXE_user-memory")).unwrap()
    );
    let path = format!("/latest/download/{asset}");
    let fetched = releases
        .requested()
        .iter()
        .filter(|p| p.ends_with(&path))
        .count();
    assert_eq!(fetched, 2, "one cut-off transfer, then one whole one");
}

#[test]
fn the_installer_refuses_missing_or_insecure_addresses_and_bad_downloads() {
    let s = Server::start();
    let releases = Releases::start();
    let c = Client::new(&s);
    let token = s.token();
    let refused = |env: &[(&str, &str)], args: &[&str], message: &str| {
        let out = c.install(&releases, env, args);
        let (stdout, stderr) = text(&out);
        assert!(!out.status.success(), "{message}: {stdout}");
        assert!(stderr.contains(message), "{message}: {stderr}");
        assert!(!c.path("bin").exists(), "{message}: nothing was installed");
        assert!(!stdout.contains(&token) && !stderr.contains(&token));
    };
    let with_token = [("USER_MEMORY_TOKEN", token.as_str())];
    let base = s.base();
    refused(
        &with_token,
        &[],
        "no server address: pass --url, or --local docker|native",
    );
    refused(
        &with_token,
        &["--url", "http://memory.example.com"],
        "the memory server http://memory.example.com/mcp uses plain HTTP",
    );
    refused(
        &with_token,
        &["--url", "ftp://example.com"],
        "not an address: ftp://example.com",
    );
    refused(
        &with_token,
        &["--url", "https://user@example.com"],
        "not an address",
    );
    refused(
        &with_token,
        &["--url", &base, "--version", "latest"],
        "--version takes a release tag",
    );
    refused(
        &with_token,
        &["--url", &base, "--bogus"],
        "unknown option: --bogus",
    );
    let requests = releases.requested().len();
    refused(
        &[
            ("USER_MEMORY_TOKEN", token.as_str()),
            (
                "USER_MEMORY_RELEASES",
                "http://downloads.example.com/releases",
            ),
        ],
        &["--url", &base],
        "the releases address http://downloads.example.com/releases uses plain HTTP",
    );
    assert_eq!(
        releases.requested().len(),
        requests + 1,
        "only the script itself was fetched"
    );

    let target = native_target();
    let sum = releases.asset("latest/download", &format!("user-memory-{target}.sha256"));
    fs::write(&sum, format!("{}  user-memory-{target}\n", "0".repeat(64))).unwrap();
    refused(
        &with_token,
        &["--url", &base],
        "does not match its checksum",
    );
    fs::write(&sum, "not a checksum\n").unwrap();
    refused(&with_token, &["--url", &base], "checksum file is malformed");
    assert!(!c.token_file().exists());

    let binary = releases.asset("latest/download", &format!("user-memory-{target}"));
    fs::write(&sum, format!("{}  user-memory-{target}\n", sha256(&binary))).unwrap();
    refused(
        &[("USER_MEMORY_TOKEN", "short")],
        &["--url", &base],
        "not an access token",
    );
    refused(
        &[],
        &["--url", &base],
        "no access token: set USER_MEMORY_TOKEN",
    );
}

/// Runs `command` on a new pseudo-terminal, answering each prompt in turn
/// (the prompt is marked with brackets in the returned screen). `None`
/// without python3.
fn on_terminal(command: &Command, answers: &[(&str, &str)]) -> Option<(i32, String)> {
    const DRIVER: &str = r#"
import os, pty, select, signal, sys, time
answers = [(a.encode(), b.encode()) for a, b in zip(sys.argv[1:-1:2], sys.argv[2:-1:2])]
argv = sys.argv[-1].split("\x1f")
pid, fd = pty.fork()
if pid == 0:
    os.execv(argv[0], argv)
out, seen, deadline = b"", 0, time.time() + 120
while time.time() < deadline:
    ready, _, _ = select.select([fd], [], [], 1)
    if not ready:
        continue
    try:
        data = os.read(fd, 4096)
    except OSError:
        break
    if not data:
        break
    out += data
    if seen < len(answers) and answers[seen][0] in out:
        out = out.replace(answers[seen][0], b"[" + answers[seen][0] + b"]", 1)
        os.write(fd, answers[seen][1] + b"\n")
        seen += 1
else:
    # A prompt nobody answers: end the run so the test fails with the
    # screen instead of waiting forever.
    os.kill(pid, signal.SIGKILL)
    out += b"\n[no answer for the prompt above]\n"
_, status = os.waitpid(pid, 0)
sys.stdout.buffer.write(out)
sys.exit(os.waitstatus_to_exitcode(status))
"#;
    let argv: Vec<String> = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let mut driver = Command::new(python()?);
    driver
        .arg("-c")
        .arg(DRIVER)
        .args(
            answers
                .iter()
                .flat_map(|(prompt, answer)| [*prompt, *answer]),
        )
        .arg(argv.join("\x1f"))
        .env_clear()
        .envs(environment(command));
    if let Some(dir) = command.get_current_dir() {
        driver.current_dir(dir);
    }
    let out = driver.output().unwrap();
    Some((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    ))
}

#[test]
fn the_installer_asks_for_the_server_then_the_hidden_token() {
    let s = Server::start();
    let releases = Releases::start();
    let c = Client::new(&s);
    let token = s.token();
    let base = s.base();
    let command = c.command(&releases, &[], &[]);
    let Some((status, screen)) = on_terminal(
        &command,
        &[
            ("Do you already have a user-memory server? [y/N] ", "y"),
            (
                "Memory server address (e.g. https://memory.example.com or http://127.0.0.1:8765): ",
                &base,
            ),
            ("Access token for ", &token),
        ],
    ) else {
        eprintln!("python3 not found; the prompt path is not exercised");
        return;
    };
    assert_eq!(status, 0, "{screen}");
    assert!(screen.contains("[Memory server address"), "{screen}");
    assert!(
        screen.contains(&format!("[Access token for ]{base}/mcp (input hidden)")),
        "{screen}"
    );
    assert!(!screen.contains(&token), "the token is never shown");
    assert_eq!(
        fs::read_to_string(c.token_file()).unwrap(),
        format!("{token}\n")
    );
}

#[test]
fn the_installer_and_release_scripts_parse_and_check_their_options() {
    for (name, option) in [("install.sh", "--url URL"), ("uninstall.sh", "--purge")] {
        let script = repo_file(name);
        for shell in ["/bin/sh", "/bin/dash"] {
            if Path::new(shell).exists() {
                let out = Command::new(shell).arg("-n").arg(&script).output().unwrap();
                assert!(out.status.success(), "{name} {shell}: {}", text(&out).1);
            }
        }
        if let Ok(out) = Command::new("shellcheck")
            .args(["-s", "sh"])
            .arg(&script)
            .output()
        {
            assert!(out.status.success(), "{name}: {}", text(&out).0);
        }
        let help = Command::new("/bin/sh")
            .arg(&script)
            .arg("--help")
            .output()
            .unwrap();
        assert!(text(&help).0.contains(option), "{name}");
    }
    for (args, message) in [
        (&["--yes"][..], "--yes goes with --purge"),
        (&["--no-export"][..], "--no-export goes with --purge"),
        (
            &["--client", "vim"][..],
            "--client takes auto, codex, claude or both",
        ),
        (&["--bogus"][..], "unknown option: --bogus"),
    ] {
        let out = Command::new("/bin/sh")
            .arg(repo_file("uninstall.sh"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(text(&out).1.contains(message), "{args:?}: {}", text(&out).1);
    }

    let release = repo_file("scripts/client-release");
    if !Path::new("/bin/zsh").exists() {
        return;
    }
    let run = |args: &[&str], path: &str| {
        Command::new("/bin/zsh")
            .arg(&release)
            .args(args)
            .env("PATH", path)
            .output()
            .unwrap()
    };
    let system = "/usr/bin:/bin";
    assert!(text(&run(&["--help"], system)).0.contains("--verify"));
    for (args, message) in [
        (&["--upload-now"][..], "unknown option --upload-now"),
        (&["--verify"], "--verify needs a directory"),
        (
            &["--target", "i686-pc-windows-msvc"],
            "unknown target i686-pc-windows-msvc",
        ),
    ] {
        let out = run(args, system);
        assert!(!out.status.success(), "{args:?}");
        assert!(text(&out).1.contains(message), "{args:?}: {}", text(&out).1);
    }
}

#[test]
fn endpoint_names_the_token_file_but_never_the_token_and_refuses_local_installs() {
    let home = tempdir().unwrap();
    let local = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .arg("endpoint")
        .env_clear()
        .env("HOME", home.path())
        .env("USER_MEMORY_DB", home.path().join("memories.sqlite3"))
        .output()
        .unwrap();
    assert!(!local.status.success());
    assert!(text(&local).1.contains("local memory store"));
    let token = "t".repeat(40);
    let token_file = home.path().join("client-token");
    fs::write(&token_file, &token).unwrap();
    let remote = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .arg("endpoint")
        .env_clear()
        .env("HOME", home.path())
        .env("USER_MEMORY_URL", "https://memory.example.com/mcp")
        .env("USER_MEMORY_TOKEN_FILE", &token_file)
        .output()
        .unwrap();
    let (stdout, stderr) = text(&remote);
    assert_eq!(
        stdout,
        format!("https://memory.example.com/mcp\n{}\n", token_file.display())
    );
    assert!(!stdout.contains(&token) && !stderr.contains(&token));
}

/// Ollama on loopback with the calibrated model: `/api/tags` lists it and
/// `/v1/embeddings` answers 1024-dimension vectors. Returns its address.
/// Ollama's HTTP API on loopback, also an OpenAI-compatible embeddings
/// endpoint for any model. It lists `models`, where a name in `chat` makes
/// text rather than embeddings; `/api/pull` adds a model; with `up` set it
/// answers 503 until that file exists, as an Ollama not yet started.
struct FakeOllama {
    address: String,
    models: Arc<Mutex<Vec<String>>>,
    /// Each request line with its body, for checking what was sent.
    requests: Arc<Mutex<Vec<String>>>,
}

/// Chat models a fake Ollama may list; they make no embeddings.
const CHAT_MODELS: [&str; 1] = ["llama3.2:3b"];

impl FakeOllama {
    fn start(models: &[&str], up: Option<PathBuf>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let models = Arc::new(Mutex::new(
            models.iter().map(|m| (*m).to_owned()).collect::<Vec<_>>(),
        ));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (listed, log) = (models.clone(), requests.clone());
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let (listed, log, up) = (listed.clone(), log.clone(), up.clone());
                std::thread::spawn(move || {
                    let mut request = Vec::new();
                    let mut byte = [0; 1];
                    while !request.ends_with(b"\r\n\r\n")
                        && stream.read(&mut byte).unwrap_or(0) == 1
                    {
                        request.push(byte[0]);
                    }
                    let head = String::from_utf8_lossy(&request).to_lowercase();
                    let length: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    let mut body = vec![0; length];
                    let _ = stream.read_exact(&mut body);
                    let body = String::from_utf8_lossy(&body).into_owned();
                    let line = head.lines().next().unwrap_or("").to_owned();
                    log.lock().unwrap().push(format!("{line} {body}"));
                    let model = body
                        .split("\"model\":\"")
                        .nth(1)
                        .and_then(|rest| rest.split('"').next())
                        .unwrap_or("")
                        .to_owned();
                    let (status, reply) = if up.as_ref().is_some_and(|up| !up.exists()) {
                        ("503 Service Unavailable", String::new())
                    } else if line.starts_with("get /api/tags") {
                        let names: Vec<String> = listed
                            .lock()
                            .unwrap()
                            .iter()
                            .map(|m| {
                                format!(
                                    r#"{{"name":"{m}","model":"{m}","details":{{"family":"x"}}}}"#
                                )
                            })
                            .collect();
                        ("200 OK", format!(r#"{{"models":[{}]}}"#, names.join(",")))
                    } else if line.starts_with("post /api/show") {
                        let kind = if CHAT_MODELS.contains(&model.as_str()) {
                            "completion"
                        } else {
                            "embedding"
                        };
                        ("200 OK", format!(r#"{{"capabilities": ["{kind}"]}}"#))
                    } else if line.starts_with("post /api/pull") {
                        listed.lock().unwrap().push(model);
                        ("200 OK", r#"{"status":"success"}"#.to_owned())
                    } else {
                        // One vector for each input, all the same.
                        let inputs = serde_json::from_str::<serde_json::Value>(&body)
                            .ok()
                            .and_then(|request| request["input"].as_array().map(Vec::len))
                            .unwrap_or(1);
                        let vector = vec!["0.03125"; 1024].join(",");
                        let data: Vec<String> = (0..inputs)
                            .map(|index| {
                                format!(r#"{{"object":"embedding","index":{index},"embedding":[{vector}]}}"#)
                            })
                            .collect();
                        (
                            "200 OK",
                            format!(
                                r#"{{"object":"list","model":"{model}","data":[{}]}}"#,
                                data.join(",")
                            ),
                        )
                    };
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                            reply.len()
                        )
                        .as_bytes(),
                    );
                });
            }
        });
        Self {
            address,
            models,
            requests,
        }
    }
    fn has(&self, model: &str) -> bool {
        self.models.lock().unwrap().iter().any(|m| m == model)
    }
    fn requested(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
    fn env(&self) -> [(&str, &str); 1] {
        [("USER_MEMORY_OLLAMA_URL", self.address.as_str())]
    }
}

/// Ollama's install script for Linux: starts the fake Ollama.
const FAKE_OLLAMA_INSTALL: &str = "echo 'fake Ollama install'\ntouch \"$HOME/ollama-up\"\n";

/// Stops the host a fake `docker compose up` started for `client`.
struct FakeServer<'a>(&'a Client);

impl Drop for FakeServer<'_> {
    fn drop(&mut self) {
        if let Ok(pid) = fs::read_to_string(self.0.path("server.pid")) {
            let _ = Command::new("kill")
                .arg(pid.trim())
                .stderr(Stdio::null())
                .status();
        }
    }
}

fn endpoint(c: &Client) -> Output {
    Command::new(c.path("bin/user-memory"))
        .arg("endpoint")
        .env_clear()
        .env("HOME", c.home.path())
        .output()
        .unwrap()
}

const HAVE_SERVER: &str = "Do you already have a user-memory server? [y/N] ";
const SET_UP_HERE: &str = "Set up a memory server on this machine? [Y/n] ";
const IN_DOCKER: &str = "Run it in Docker? [Y/n] ";
const NATIVE_INSTEAD: &str = "Keep the memories in a local store on this machine instead? [Y/n] ";

#[test]
fn declining_both_questions_installs_nothing() {
    let releases = Releases::start();
    let c = Client::bare();
    let Some((status, screen)) = on_terminal(
        &c.command(&releases, &[], &[]),
        &[(HAVE_SERVER, ""), (SET_UP_HERE, "n")],
    ) else {
        return;
    };
    assert_eq!(status, 0, "{screen}");
    assert!(screen.contains("Nothing was installed. Run this installer again with --url URL"));
    assert!(!c.path("bin").exists() && !c.path(".config").exists());
    assert!(
        releases
            .requested()
            .iter()
            .all(|p| p.ends_with("/install.sh")),
        "nothing downloaded"
    );
}

#[test]
fn the_questions_set_up_a_docker_server_and_a_rerun_upgrades_it() {
    let releases = Releases::start();
    let c = Client::bare();
    let _server = FakeServer(&c);
    let port = closed_port();
    let Some((status, screen)) = on_terminal(
        &c.command(&releases, &[], &["--port", &port.to_string()]),
        &[
            (HAVE_SERVER, "n"),
            (SET_UP_HERE, ""),
            (IN_DOCKER, ""),
            (SEMANTIC, "3"),
        ],
    ) else {
        return;
    };
    assert_eq!(status, 0, "{screen}");
    assert!(screen.contains("Semantic search is off (keyword search only). To turn it on later, run this installer again with --embed-model qwen3-embedding:4b --pull-model, or with --embed-url URL --embed-model NAME."));
    assert!(
        screen.contains(&format!(
            "The memory server runs in Docker on 127.0.0.1:{port}"
        )),
        "{screen}"
    );
    let dir = c.docker_dir();
    let compose = fs::read_to_string(dir.join("compose.yaml")).unwrap();
    for line in [
        "name: user-memory",
        "image: user-memory:installed",
        &format!("      - \"127.0.0.1:{port}:8765\""),
        "read_only: true",
        "cap_drop: [ALL]",
        "security_opt: [no-new-privileges:true]",
        "init: true",
        "stop_signal: SIGINT",
        "restart: unless-stopped",
        "    name: user-memory-data",
        "      test: \"USER_MEMORY_URL=http://127.0.0.1:8765/mcp USER_MEMORY_TOKEN_FILE=/data/access-token user-memory index >/dev/null\"",
    ] {
        assert!(compose.contains(line), "{line}\n{compose}");
    }
    assert!(!compose.contains("USER_MEMORY_EMBED"), "{compose}");
    assert_eq!(
        fs::read_to_string(dir.join("embedding.env")).unwrap(),
        "",
        "keyword only"
    );
    let dockerfile = fs::read_to_string(dir.join("Dockerfile")).unwrap();
    for line in [
        "FROM debian:bookworm-slim",
        "useradd --uid 10001 --gid memory --no-create-home memory",
        "ENV USER_MEMORY_DB=/data/memories.sqlite3",
        "ENTRYPOINT [\"/usr/local/bin/user-memory\"]",
    ] {
        assert!(dockerfile.contains(line), "{line}");
    }
    // The server build for the fake Docker's arm64: the release's own asset.
    assert_eq!(
        fs::read(dir.join("user-memory")).unwrap(),
        fs::read(releases.asset("latest/download", "user-memory-aarch64-unknown-linux-gnu"))
            .unwrap()
    );
    let compose_file = dir.join("compose.yaml");
    let log = c.docker_log();
    for command in ["build", "up -d --wait"] {
        assert!(
            log.contains(&format!(
                "compose -f {} {command}\n",
                compose_file.display()
            )),
            "{log}"
        );
    }
    assert!(
        !log.contains("embedding-key"),
        "a new server has no key to remove: {log}"
    );
    assert!(log.contains("cp memory:/data/access-token"));
    let token = fs::read_to_string(c.path("server/access-token")).unwrap();
    assert_eq!(
        fs::read_to_string(c.token_file()).unwrap().trim(),
        token.trim()
    );
    assert_eq!(mode(&c.token_file()), 0o600);
    assert!(!screen.contains(token.trim()));
    assert_eq!(
        String::from_utf8(endpoint(&c).stdout).unwrap(),
        format!(
            "http://127.0.0.1:{port}/mcp\n{}\n",
            c.token_file().display()
        )
    );

    // A re-run upgrades that server: rebuild, stop, migrate, start; the
    // token stays.
    fs::remove_file(c.path("docker.log")).unwrap();
    let again = c.install(&releases, &[], &[]);
    let (stdout, stderr) = text(&again);
    assert!(again.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains(&format!(
        "Upgrading the Docker memory server in {}",
        dir.display()
    )));
    let compose = format!("compose -f {}", compose_file.display());
    let calls: Vec<String> = c
        .docker_log()
        .lines()
        .filter(|l| l.starts_with("compose"))
        .map(|l| l.replace(&compose, "compose"))
        .collect();
    assert_eq!(
        calls,
        [
            "compose build",
            "compose run --rm memory migrate --check",
            "compose stop memory",
            "compose run --rm memory migrate",
            "compose up -d --wait"
        ]
    );

    // A store that needs titles stops the upgrade before the server is
    // stopped; a re-run with the titles map passes it, on stdin, to the check
    // and then to migrate.
    let compose_calls = || -> Vec<String> {
        c.docker_log()
            .lines()
            .filter(|l| l.starts_with("compose"))
            .map(|l| l.replace(&compose, "compose"))
            .collect()
    };
    fs::remove_file(c.path("docker.log")).unwrap();
    fs::write(c.path("migrate-fails"), "").unwrap();
    let refused = c.install(&releases, &[], &[]);
    let (stdout, stderr) = text(&refused);
    assert!(!refused.status.success(), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("needs a title for every memory"),
        "{stderr}"
    );
    assert!(
        stderr.contains("the server keeps running the previous version"),
        "{stderr}"
    );
    assert!(stderr.contains("again with --titles FILE"), "{stderr}");
    assert_eq!(
        compose_calls(),
        ["compose build", "compose run --rm memory migrate --check"]
    );
    fs::remove_file(c.path("migrate-fails")).unwrap();
    fs::remove_file(c.path("docker.log")).unwrap();
    let map = c.path("titles.json");
    fs::write(
        &map,
        r#"{"1a2b3c4d-1111-4aaa-8aaa-000000000001":"Prefer Rust"}"#,
    )
    .unwrap();
    let titled = c.install(&releases, &[], &["--titles", map.to_str().unwrap()]);
    let (stdout, stderr) = text(&titled);
    assert!(titled.status.success(), "{stdout}\n{stderr}");
    assert_eq!(
        compose_calls(),
        [
            "compose build",
            "compose run --rm -T memory migrate --check --titles -",
            "compose stop memory",
            "compose run --rm -T memory migrate --titles -",
            "compose up -d --wait"
        ]
    );
    assert_eq!(
        fs::read(c.path("migrate-stdin")).unwrap(),
        fs::read(&map).unwrap()
    );

    // A memory recorded after the check and before the stop needs a title
    // too: migrate refuses, a second check says why, and the server stays
    // stopped. Any other failure says only that the store was not upgraded.
    for (marker, expected, unexpected) in [
        (
            "changed-at-stop",
            "memories recorded after the check and before the server stopped need titles too",
            "see the message above, then run the installer again",
        ),
        (
            "migrate-breaks",
            "see the message above, then run the installer again",
            "recorded after the check",
        ),
        (
            "broken-at-stop",
            "checking the store again failed too (above), so Docker or the store may be failing",
            "recorded after the check",
        ),
    ] {
        fs::remove_file(c.path("docker.log")).unwrap();
        let _ = fs::remove_file(c.path("migrate-fails"));
        fs::write(c.path(marker), "").unwrap();
        let failed = c.install(&releases, &[], &["--titles", map.to_str().unwrap()]);
        let (stdout, stderr) = text(&failed);
        assert!(!failed.status.success(), "{marker}: {stdout}\n{stderr}");
        assert!(stderr.contains(expected), "{marker}: {stderr}");
        assert!(!stderr.contains(unexpected), "{marker}: {stderr}");
        assert!(
            stderr.contains("the server stays stopped"),
            "{marker}: {stderr}"
        );
        assert_eq!(
            compose_calls()[2..],
            [
                "compose stop memory",
                "compose run --rm -T memory migrate --titles -",
                "compose run --rm -T memory migrate --check --titles -",
            ],
            "{marker}"
        );
        if marker == "broken-at-stop" {
            assert!(
                stderr.contains("Error response from daemon: cannot start container"),
                "{stderr}"
            );
            fs::remove_file(c.path("docker-broken")).unwrap();
        }
        fs::remove_file(c.path(marker)).unwrap();
    }
}

#[test]
fn the_questions_set_up_a_native_store_and_fall_back_to_it_without_docker() {
    let releases = Releases::start();
    let c = Client::bare();
    let Some((status, screen)) = on_terminal(
        &c.command(&releases, &[], &[]),
        &[
            (HAVE_SERVER, "no"),
            (SET_UP_HERE, "yes"),
            (IN_DOCKER, "n"),
            (SEMANTIC, "3"),
        ],
    ) else {
        return;
    };
    assert_eq!(status, 0, "{screen}");
    assert!(
        c.path(".local/share/user-memory/memories.sqlite3")
            .is_file()
    );
    assert!(!c.token_file().exists() && !c.docker_dir().exists());
    let local = endpoint(&c);
    assert!(String::from_utf8_lossy(&local.stderr).contains("local memory store"));
    let again = c.install(&releases, &[], &[]);
    assert!(again.status.success(), "{}", text(&again).1);
    assert!(
        text(&again)
            .0
            .contains("Upgrading the local memory store install")
    );

    let other = Client::bare();
    fs::write(other.path("docker-down"), "").unwrap();
    let Some((status, screen)) = on_terminal(
        &other.command(&releases, &[], &[]),
        &[
            (HAVE_SERVER, ""),
            (SET_UP_HERE, ""),
            (NATIVE_INSTEAD, ""),
            (SEMANTIC, "3"),
        ],
    ) else {
        return;
    };
    assert_eq!(status, 0, "{screen}");
    assert!(
        screen.contains("Docker is not available here (no docker command, or docker info failed).")
    );
    assert!(
        other
            .path(".local/share/user-memory/memories.sqlite3")
            .is_file()
    );
}

#[test]
fn local_modes_run_without_questions_and_refuse_a_busy_port_or_volume() {
    let releases = Releases::start();
    let c = Client::bare();
    let _server = FakeServer(&c);
    let port = closed_port().to_string();
    let out = c.install(&releases, &[], &["--local", "docker", "--port", &port]);
    assert!(out.status.success(), "{}", text(&out).1);
    assert!(text(&out).0.contains("Remote MCP connection and tools: OK"));

    let native = Client::bare();
    let out = native.install(&releases, &[], &["--local", "native"]);
    assert!(out.status.success(), "{}", text(&out).1);
    assert!(
        native
            .path(".local/share/user-memory/memories.sqlite3")
            .is_file()
    );
    // A titles map reaches setup, which says when the store needs none.
    let map = native.path("titles.json");
    fs::write(&map, "{}").unwrap();
    let out = native.install(&releases, &[], &["--titles", map.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out).1);
    assert!(
        text(&out)
            .0
            .contains("No titles map was needed: the store is already at schema version 5."),
        "{}",
        text(&out).0
    );

    let refused = |client: &Client, args: &[&str], message: &str| {
        let out = client.install(&releases, &[], args);
        assert!(!out.status.success(), "{message}");
        assert!(
            text(&out).1.contains(message),
            "{message}: {}",
            text(&out).1
        );
        assert!(
            !client.docker_log().contains("compose"),
            "{message}: nothing started"
        );
        assert!(!client.path("bin").exists() && !client.docker_dir().exists());
    };
    refused(
        &Client::bare(),
        &["--local", "native", "--titles", "/nonexistent/titles.json"],
        "cannot read the titles map /nonexistent/titles.json",
    );
    refused(
        &Client::bare(),
        &[
            "--url",
            "http://127.0.0.1:9/mcp",
            "--titles",
            map.to_str().unwrap(),
        ],
        "--titles upgrades a store on this machine",
    );
    let busy = TcpListener::bind("127.0.0.1:0").unwrap();
    let busy_port = busy.local_addr().unwrap().port().to_string();
    refused(
        &Client::bare(),
        &["--local", "docker", "--port", &busy_port],
        &format!("127.0.0.1:{busy_port} is already in use"),
    );
    let shared = Client::bare();
    fs::write(shared.path("docker-ps"), "checkout-memory-1\n").unwrap();
    refused(
        &shared,
        &["--local", "docker", "--port", &closed_port().to_string()],
        "a container already uses the user-memory-data volume (checkout-memory-1)",
    );
    let down = Client::bare();
    fs::write(down.path("docker-down"), "").unwrap();
    refused(&down, &["--local", "docker"], "Docker is not running");
    for (args, message) in [
        (&["--local", "podman"][..], "--local takes docker or native"),
        (
            &["--local", "docker", "--url", "https://m.example.com"],
            "either --url or --local",
        ),
        (
            &["--local", "docker", "--port", "99999"],
            "--port takes a port number",
        ),
    ] {
        refused(&Client::bare(), args, message);
    }
}

const SEMANTIC: &str = "Semantic search [1]: ";
const INSTALL_OLLAMA: &str = "Ollama isn't installed. Install it now? [y/N] ";
const DOWNLOAD_QWEN: &str = "Download qwen3-embedding:4b (about 2.5 GB)? [Y/n] ";
const PICK_MODEL: &str = "Model [1]: ";
const ADDRESS: &str = "Embeddings address (e.g. http://host:11434 or https://api.openai.com/v1): ";
const MODEL_NAME: &str = "Model name: ";
const API_KEY: &str = "API key (input hidden; Enter for none): ";
const QWEN: &str = "qwen3-embedding:4b";
const QWEN_INSTRUCTION: &str =
    "Instruct: Given a question, retrieve the user's matching memory\\nQuery:";

impl Client {
    /// Puts the fake `name` (tests/fake/cli.rs) on this client's PATH.
    fn fake(&self, name: &str) {
        fake::fake(&self.path("cli"), name);
    }
    /// The embedding settings setup saved for a store on this machine.
    fn saved_embedding(&self) -> serde_json::Value {
        let receipt: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(self.path("bin/.user-memory.json")).unwrap())
                .unwrap();
        receipt["embedding"].clone()
    }
    fn log(&self, name: &str) -> String {
        fs::read_to_string(self.path(name)).unwrap_or_default()
    }
}

/// The settings saved for Ollama on this machine with `model`.
fn ollama_settings(ollama: &FakeOllama, model: &str) -> serde_json::Value {
    let mut settings = serde_json::json!({
        "USER_MEMORY_EMBED_URL": format!("{}/v1/embeddings", ollama.address),
        "USER_MEMORY_EMBED_MODEL": model,
    });
    if model == QWEN {
        settings["USER_MEMORY_EMBED_DIMENSIONS"] = "1024".into();
        settings["USER_MEMORY_EMBED_QUERY_INSTRUCTION"] = QWEN_INSTRUCTION.into();
    }
    settings
}

#[test]
fn the_semantic_search_question_offers_ollama_another_endpoint_or_off() {
    let releases = Releases::start();
    let ollama = FakeOllama::start(&[QWEN], None);
    let c = Client::bare();
    let Some((status, screen)) = on_terminal(
        &c.command(&releases, &ollama.env(), &["--local", "native"]),
        &[(SEMANTIC, "")],
    ) else {
        return;
    };
    assert_eq!(status, 0, "{screen}");
    assert!(
        screen.contains("  1) Ollama on this machine (recommended: private, no API key)"),
        "{screen}"
    );
    assert_eq!(c.saved_embedding(), ollama_settings(&ollama, QWEN));
    assert!(
        screen.contains("Embedding: OK (qwen3-embedding:4b, 1024 dimensions"),
        "{screen}"
    );

    // Another endpoint: a bare host gets the OpenAI-compatible path, and the
    // key is saved privately and never shown.
    let endpoint = FakeOllama::start(&[], None);
    let c = Client::bare();
    let key = "sk-test-0123456789abcdef";
    let Some((status, screen)) = on_terminal(
        &c.command(&releases, &[], &["--local", "native"]),
        &[
            (SEMANTIC, "2"),
            (ADDRESS, &endpoint.address),
            (MODEL_NAME, "text-embedding-3-small"),
            (API_KEY, key),
        ],
    ) else {
        return;
    };
    assert_eq!(status, 0, "{screen}");
    assert!(!screen.contains(key), "{screen}");
    let key_file = c.path(".config/user-memory/embedding-key");
    assert_eq!(fs::read_to_string(&key_file).unwrap(), format!("{key}\n"));
    assert_eq!(mode(&key_file), 0o600);
    assert_eq!(
        c.saved_embedding(),
        serde_json::json!({
            "USER_MEMORY_EMBED_URL": format!("{}/v1/embeddings", endpoint.address),
            "USER_MEMORY_EMBED_MODEL": "text-embedding-3-small",
            "USER_MEMORY_EMBED_API_KEY_FILE": key_file.to_str().unwrap(),
        })
    );
    assert!(
        endpoint
            .requested()
            .iter()
            .any(|r| r.starts_with("post /v1/embeddings")),
        "doctor's probe reached the endpoint"
    );

    let c = Client::bare();
    let Some((status, screen)) = on_terminal(
        &c.command(&releases, &ollama.env(), &["--local", "native"]),
        &[(SEMANTIC, "3")],
    ) else {
        return;
    };
    assert_eq!(status, 0, "{screen}");
    assert!(screen.contains("Semantic search is off (keyword search only)."));
    assert_eq!(c.saved_embedding(), serde_json::Value::Null);
}

#[test]
fn the_model_list_recommends_qwen3_and_offers_its_download() {
    let releases = Releases::start();
    let ollama = FakeOllama::start(&["nomic-embed-text:latest", CHAT_MODELS[0]], None);
    let c = Client::bare();
    let Some((status, screen)) = on_terminal(
        &c.command(&releases, &ollama.env(), &["--local", "native"]),
        &[(SEMANTIC, ""), (PICK_MODEL, "2")],
    ) else {
        return;
    };
    assert_eq!(status, 0, "{screen}");
    assert!(
        screen.contains(
            "  1) download qwen3-embedding:4b (about 2.5 GB, recommended)\r\n  2) nomic-embed-text:latest\r\n"
        ) && !screen.contains(CHAT_MODELS[0]),
        "only embedding models are listed: {screen}"
    );
    assert_eq!(
        c.saved_embedding(),
        ollama_settings(&ollama, "nomic-embed-text:latest")
    );
    assert!(!ollama.has(QWEN), "nothing downloaded");

    // The recommended model, downloaded once asked for.
    let c = Client::bare();
    let Some((status, screen)) = on_terminal(
        &c.command(&releases, &ollama.env(), &["--local", "native"]),
        &[(SEMANTIC, ""), (PICK_MODEL, "")],
    ) else {
        return;
    };
    assert_eq!(status, 0, "{screen}");
    assert!(screen.contains("Downloading qwen3-embedding:4b (about 2.5 GB)"));
    assert!(ollama.has(QWEN));
    assert_eq!(c.saved_embedding(), ollama_settings(&ollama, QWEN));
}

#[test]
fn an_ollama_without_embedding_models_asks_before_downloading_one() {
    let releases = Releases::start();
    for (answer, downloaded) in [("n", false), ("", true)] {
        let ollama = FakeOllama::start(&[CHAT_MODELS[0]], None);
        let c = Client::bare();
        // With the Ollama command, the download shows its progress.
        c.fake("ollama");
        let Some((status, screen)) = on_terminal(
            &c.command(&releases, &ollama.env(), &["--local", "native"]),
            &[(SEMANTIC, ""), (DOWNLOAD_QWEN, answer)],
        ) else {
            return;
        };
        assert_eq!(status, 0, "{screen}");
        assert_eq!(ollama.has(QWEN), downloaded, "{screen}");
        if downloaded {
            assert_eq!(c.log("ollama.log"), format!("pull {QWEN}\n"));
            assert_eq!(c.saved_embedding(), ollama_settings(&ollama, QWEN));
        } else {
            assert_eq!(c.log("ollama.log"), "");
            assert!(screen.contains("Semantic search is off"), "{screen}");
            assert_eq!(c.saved_embedding(), serde_json::Value::Null);
        }
    }
}

#[test]
fn a_missing_ollama_is_installed_only_after_a_yes() {
    let releases = Releases::start();
    let c = Client::bare();
    let up = c.path("ollama-up");
    let ollama = FakeOllama::start(&[QWEN], Some(up.clone()));
    let homebrew = cfg!(target_os = "macos");
    if homebrew {
        c.fake("brew");
    } else {
        fs::write(
            releases.asset("latest/download", "ollama-install.sh"),
            FAKE_OLLAMA_INSTALL,
        )
        .unwrap();
    }
    let script = format!("{}/latest/download/ollama-install.sh", releases.base);
    let env = [
        ollama.env()[0],
        ("USER_MEMORY_OLLAMA_INSTALL_SCRIPT", script.as_str()),
    ];
    let Some((status, screen)) = on_terminal(
        &c.command(&releases, &env, &["--local", "native"]),
        &[(SEMANTIC, ""), (INSTALL_OLLAMA, "")],
    ) else {
        return;
    };
    assert_eq!(status, 0, "{screen}");
    assert!(
        !up.exists() && c.log("brew.log").is_empty(),
        "the default is no"
    );
    assert_eq!(c.saved_embedding(), serde_json::Value::Null);

    let c = Client::bare();
    let up = c.path("ollama-up");
    let ollama = FakeOllama::start(&[QWEN], Some(up.clone()));
    if homebrew {
        c.fake("brew");
    }
    let env = [
        ollama.env()[0],
        ("USER_MEMORY_OLLAMA_INSTALL_SCRIPT", script.as_str()),
    ];
    let Some((status, screen)) = on_terminal(
        &c.command(&releases, &env, &["--local", "native"]),
        &[(SEMANTIC, ""), (INSTALL_OLLAMA, "y")],
    ) else {
        return;
    };
    assert_eq!(status, 0, "{screen}");
    if homebrew {
        assert_eq!(c.log("brew.log"), "install ollama\nservices start ollama\n");
    } else {
        assert!(
            screen.contains("installs it for the whole system and asks for your password (sudo)."),
            "{screen}"
        );
        assert!(screen.contains("fake Ollama install"), "{screen}");
    }
    assert_eq!(c.saved_embedding(), ollama_settings(&ollama, QWEN));
}

/// Without Homebrew on macOS the installer shows the download page and
/// waits for the user instead of installing anything itself.
#[cfg(target_os = "macos")]
#[test]
fn without_homebrew_the_installer_waits_for_ollama_to_be_installed_by_hand() {
    let releases = Releases::start();
    let c = Client::bare();
    let up = c.path("ollama-up");
    let ollama = FakeOllama::start(&[QWEN], Some(up.clone()));
    // The user installs and opens Ollama once the installer has found it
    // missing and shown the link.
    let (opened, asked) = (up.clone(), ollama.requests.clone());
    std::thread::spawn(move || {
        while asked.lock().unwrap().is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        fs::write(opened, "").unwrap();
    });
    let Some((status, screen)) = on_terminal(
        &c.command(&releases, &ollama.env(), &["--local", "native"]),
        &[
            (SEMANTIC, ""),
            (INSTALL_OLLAMA, "yes"),
            ("Press Enter once Ollama is running: ", ""),
        ],
    ) else {
        return;
    };
    assert_eq!(status, 0, "{screen}");
    assert!(
        screen
            .contains("Download Ollama from https://ollama.com/download, install it and open it."),
        "{screen}"
    );
    assert_eq!(c.saved_embedding(), ollama_settings(&ollama, QWEN));

    // With no terminal to wait on, it says what to do instead.
    let c = Client::bare();
    let out = c.install(
        &releases,
        &[("USER_MEMORY_OLLAMA_URL", "http://127.0.0.1:9")],
        &["--local", "native", "--install-ollama"],
    );
    assert!(!out.status.success());
    assert!(
        text(&out).1.contains("Homebrew is not installed, so Ollama cannot be installed here; install it from https://ollama.com/download, then run the installer again"),
        "{}",
        text(&out).1
    );
}

#[test]
fn the_semantic_search_options_need_no_terminal() {
    let releases = Releases::start();
    // The recommended model, downloaded as asked.
    let ollama = FakeOllama::start(&[], None);
    let c = Client::bare();
    let out = c.install(
        &releases,
        &ollama.env(),
        &["--local", "native", "--pull-model"],
    );
    assert!(out.status.success(), "{}", text(&out).1);
    assert!(ollama.has(QWEN));
    assert_eq!(c.saved_embedding(), ollama_settings(&ollama, QWEN));

    // Another model and vector length.
    let ollama = FakeOllama::start(&["nomic-embed-text:latest"], None);
    let c = Client::bare();
    let out = c.install(
        &releases,
        &ollama.env(),
        &[
            "--local",
            "native",
            "--embed-model",
            "nomic-embed-text:latest",
            "--embed-dimensions",
            "512",
        ],
    );
    assert!(out.status.success(), "{}", text(&out).1);
    let mut settings = ollama_settings(&ollama, "nomic-embed-text:latest");
    settings["USER_MEMORY_EMBED_DIMENSIONS"] = "512".into();
    assert_eq!(c.saved_embedding(), settings);

    // A model Ollama lacks, or an Ollama that is missing, without leave to
    // fetch it.
    let out = Client::bare().install(
        &releases,
        &ollama.env(),
        &["--local", "native", "--embed-model", QWEN],
    );
    assert!(!out.status.success());
    assert!(text(&out).1.contains("Ollama has no model qwen3-embedding:4b; pass --pull-model to download it, or run: ollama pull qwen3-embedding:4b"), "{}", text(&out).1);
    let out = Client::bare().install(&releases, &[], &["--local", "native", "--pull-model"]);
    assert!(!out.status.success());
    assert!(text(&out).1.contains("Ollama isn't installed; pass --install-ollama to install it, or install it from https://ollama.com/download"), "{}", text(&out).1);

    // Installed and started with Homebrew, or Ollama's script on Linux.
    let c = Client::bare();
    let ollama = FakeOllama::start(&[QWEN], Some(c.path("ollama-up")));
    c.fake("brew");
    fs::write(
        releases.asset("latest/download", "ollama-install.sh"),
        FAKE_OLLAMA_INSTALL,
    )
    .unwrap();
    let script = format!("{}/latest/download/ollama-install.sh", releases.base);
    let out = c.install(
        &releases,
        &[
            ollama.env()[0],
            ("USER_MEMORY_OLLAMA_INSTALL_SCRIPT", &script),
        ],
        &["--local", "native", "--install-ollama"],
    );
    assert!(out.status.success(), "{}", text(&out).1);
    assert_eq!(c.saved_embedding(), ollama_settings(&ollama, QWEN));

    // Another endpoint with a key file, given as a full URL.
    let endpoint = FakeOllama::start(&[], None);
    let c = Client::bare();
    let key = c.path("embedding key");
    fs::write(&key, "sk-test-0123456789abcdef\n").unwrap();
    let url = format!("{}/v1/embeddings", endpoint.address);
    // A setting left exported in the user's shell is not part of the choice.
    let out = c.install(
        &releases,
        &[("USER_MEMORY_EMBED_DIMENSIONS", "999")],
        &[
            "--local",
            "native",
            "--embed-url",
            &url,
            "--embed-model",
            "text-embedding-3-small",
            "--embed-key-file",
            key.to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "{}", text(&out).1);
    assert_eq!(
        c.saved_embedding(),
        serde_json::json!({
            "USER_MEMORY_EMBED_URL": url,
            "USER_MEMORY_EMBED_MODEL": "text-embedding-3-small",
            "USER_MEMORY_EMBED_API_KEY_FILE": key.to_str().unwrap(),
        })
    );

    // A rerun keeps the choice; --no-embed turns it off.
    let out = c.install(&releases, &[], &[]);
    assert!(out.status.success(), "{}", text(&out).1);
    assert_eq!(
        c.saved_embedding()["USER_MEMORY_EMBED_MODEL"],
        "text-embedding-3-small"
    );
    let out = c.install(&releases, &[], &["--no-embed"]);
    assert!(out.status.success(), "{}", text(&out).1);
    assert_eq!(c.saved_embedding(), serde_json::Value::Null);
}

#[test]
fn the_semantic_search_options_refuse_what_cannot_work() {
    let releases = Releases::start();
    let c = Client::bare();
    let key = c.path("key");
    fs::write(&key, "sk-test\n").unwrap();
    let key = key.to_str().unwrap();
    for (args, message) in [
        (
            &["--no-embed", "--embed-model", "m"][..],
            "--no-embed goes without the other semantic search options",
        ),
        (
            &["--no-embed", "--pull-model"][..],
            "--no-embed goes without the other semantic search options",
        ),
        (
            &["--embed-url", "https://e.example.com/v1"][..],
            "--embed-url needs --embed-model",
        ),
        (
            &["--embed-key-file", key][..],
            "--embed-key-file goes with --embed-url",
        ),
        (
            &[
                "--embed-url",
                "https://e.example.com",
                "--embed-model",
                "m",
                "--pull-model",
            ][..],
            "--install-ollama and --pull-model are for Ollama on this machine, not --embed-url",
        ),
        (
            &["--embed-url", "http://e.example.com", "--embed-model", "m"][..],
            "the embeddings endpoint http://e.example.com uses plain HTTP; use https://",
        ),
        (
            &["--embed-dimensions", "0"][..],
            "--embed-dimensions takes a positive whole number",
        ),
        (
            &["--embed-dimensions", "12a"][..],
            "--embed-dimensions takes a positive whole number",
        ),
        (
            &["--embed-model", "m", "--embed-key-file", "/no/such/key"][..],
            "--embed-key-file goes with --embed-url",
        ),
        (
            &[
                "--embed-url",
                "https://e.example.com",
                "--embed-model",
                "m",
                "--embed-key-file",
                "/no/such/key",
            ][..],
            "cannot read the API key file /no/such/key",
        ),
        (
            &["--url", "https://memory.example.com", "--embed-model", "m"][..],
            "semantic search is set where the memory server runs; pass these options to the installer on that machine",
        ),
    ] {
        let out = c.install(&releases, &[], args);
        assert!(!out.status.success(), "{args:?}");
        assert!(text(&out).1.contains(message), "{args:?}: {}", text(&out).1);
    }
    assert!(!c.path("bin").exists(), "nothing installed");
}

#[test]
fn a_docker_server_gets_its_embedding_settings_as_the_container_sees_them() {
    let releases = Releases::start();
    let ollama = FakeOllama::start(&[QWEN], None);
    let c = Client::bare();
    let _server = FakeServer(&c);
    let port = closed_port().to_string();
    let out = c.install(
        &releases,
        &ollama.env(),
        &["--local", "docker", "--port", &port],
    );
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("Ollama has qwen3-embedding:4b: using it for semantic search."));
    let compose = fs::read_to_string(c.docker_dir().join("compose.yaml")).unwrap();
    for line in [
        "    env_file: embedding.env",
        "      - \"host.docker.internal:host-gateway\"",
    ] {
        assert!(compose.lines().any(|l| l == line), "{line}\n{compose}");
    }
    let ollama_port = ollama.address.rsplit(':').next().unwrap();
    let env = format!(
        "USER_MEMORY_EMBED_URL=http://host.docker.internal:{ollama_port}/v1/embeddings\nUSER_MEMORY_EMBED_MODEL={QWEN}\nUSER_MEMORY_EMBED_DIMENSIONS=1024\nUSER_MEMORY_EMBED_QUERY_INSTRUCTION={QWEN_INSTRUCTION}\nUSER_MEMORY_EMBED_ALLOW_HTTP=1\n"
    );
    let file = c.docker_dir().join("embedding.env");
    assert_eq!(fs::read_to_string(&file).unwrap(), env);

    // An upgrade keeps it; another endpoint's key goes into the volume.
    let out = c.install(&releases, &[], &[]);
    assert!(out.status.success(), "{}", text(&out).1);
    assert_eq!(fs::read_to_string(&file).unwrap(), env);
    let key = c.path("key");
    fs::write(&key, "sk-test-0123456789abcdef\n").unwrap();
    let out = c.install(
        &releases,
        &[],
        &[
            "--embed-url",
            "https://api.example.com/v1",
            "--embed-model",
            "text-embedding-3-small",
            "--embed-key-file",
            key.to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "{}", text(&out).1);
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "USER_MEMORY_EMBED_URL=https://api.example.com/v1/embeddings\nUSER_MEMORY_EMBED_MODEL=text-embedding-3-small\nUSER_MEMORY_EMBED_API_KEY_FILE=/data/embedding-key\n"
    );
    assert_eq!(c.log("server/embedding-key"), "sk-test-0123456789abcdef\n");
    let out = c.install(&releases, &[], &["--no-embed"]);
    assert!(out.status.success(), "{}", text(&out).1);
    assert_eq!(fs::read_to_string(&file).unwrap(), "");
    assert!(
        !c.path("server/embedding-key").exists(),
        "the key is removed with the choice"
    );

    // A key never travels over the container's plain http to this machine.
    let out = c.install(
        &releases,
        &[],
        &[
            "--embed-url",
            &ollama.address,
            "--embed-model",
            "m",
            "--embed-key-file",
            key.to_str().unwrap(),
        ],
    );
    assert!(!out.status.success());
    assert!(
        text(&out)
            .1
            .contains("an API key is never sent over plain http to another host"),
        "{}",
        text(&out).1
    );
}

#[test]
fn an_upgrade_moves_an_older_installers_inline_embedding_settings() {
    let releases = Releases::start();
    let c = Client::bare();
    let _server = FakeServer(&c);
    let port = closed_port().to_string();
    let out = c.install(
        &releases,
        &[],
        &["--local", "docker", "--port", &port, "--no-embed"],
    );
    assert!(out.status.success(), "{}", text(&out).1);
    // The compose.yaml a 0.5 installer wrote, with its settings inline.
    let dir = c.docker_dir();
    let compose = fs::read_to_string(dir.join("compose.yaml")).unwrap().replace(
        "    env_file: embedding.env\n",
        "    environment:\n      USER_MEMORY_EMBED_URL: \"http://host.docker.internal:11434/v1/embeddings\"\n      USER_MEMORY_EMBED_ALLOW_HTTP: \"1\"\n      USER_MEMORY_EMBED_MODEL: \"qwen3-embedding:4b\"\n      USER_MEMORY_EMBED_DIMENSIONS: \"1024\"\n      USER_MEMORY_EMBED_QUERY_INSTRUCTION: 'Instruct: Given a question, retrieve the user''s matching memory\\nQuery:'\n",
    );
    fs::write(dir.join("compose.yaml"), compose).unwrap();
    fs::remove_file(dir.join("embedding.env")).unwrap();
    let out = c.install(&releases, &[], &[]);
    assert!(out.status.success(), "{}", text(&out).1);
    assert_eq!(
        fs::read_to_string(dir.join("embedding.env")).unwrap(),
        format!(
            "USER_MEMORY_EMBED_URL=http://host.docker.internal:11434/v1/embeddings\nUSER_MEMORY_EMBED_ALLOW_HTTP=1\nUSER_MEMORY_EMBED_MODEL={QWEN}\nUSER_MEMORY_EMBED_DIMENSIONS=1024\nUSER_MEMORY_EMBED_QUERY_INSTRUCTION={QWEN_INSTRUCTION}\n"
        )
    );
    let compose = fs::read_to_string(dir.join("compose.yaml")).unwrap();
    assert!(!compose.contains("USER_MEMORY_EMBED"), "{compose}");
}

/// The installer's Docker server run for real under a throwaway project
/// name, with this machine's Docker; removes what it made when dropped,
/// whatever the test got to.
struct RealDocker {
    c: Client,
    releases: Releases,
    name: String,
    /// PATH with the real Docker and the client's fake Codex.
    path: String,
    /// The user's Docker configuration (context, compose plugin), which the
    /// client's temporary HOME would hide.
    config: std::ffi::OsString,
}

impl RealDocker {
    /// `None` unless USER_MEMORY_TEST_DOCKER_BINARY names a Linux build of
    /// this commit for the Docker daemon's architecture. `label` keeps the
    /// project, volume and image of tests running side by side apart.
    fn start(label: &str) -> Option<Self> {
        let linux = std::env::var_os("USER_MEMORY_TEST_DOCKER_BINARY")?;
        let releases = Releases::start();
        let arch = Command::new("docker")
            .args(["info", "--format", "{{.Architecture}}"])
            .output()
            .unwrap();
        let target = match String::from_utf8_lossy(&arch.stdout).trim() {
            "aarch64" | "arm64" => "aarch64-unknown-linux-gnu",
            _ => "x86_64-unknown-linux-gnu",
        };
        let asset = releases.asset("latest/download", &format!("user-memory-{target}"));
        fs::copy(&linux, &asset).unwrap();
        fs::write(
            releases.asset("latest/download", &format!("user-memory-{target}.sha256")),
            format!("{}  user-memory-{target}\n", sha256(&asset)),
        )
        .unwrap();
        let c = Client::bare();
        fs::remove_file(c.path("cli/docker")).unwrap();
        let docker = Command::new("/bin/sh")
            .args(["-c", "command -v docker"])
            .output()
            .unwrap();
        let docker = String::from_utf8(docker.stdout).unwrap();
        let path = format!(
            "{}:{}:/usr/bin:/bin:/usr/sbin:/sbin",
            c.path("cli").display(),
            Path::new(docker.trim()).parent().unwrap().display()
        );
        let config = std::env::var_os("DOCKER_CONFIG").unwrap_or_else(|| {
            Path::new(&std::env::var_os("HOME").unwrap())
                .join(".docker")
                .into_os_string()
        });
        Some(Self {
            c,
            releases,
            name: format!("user-memory-test-{label}-{}", std::process::id()),
            path,
            config,
        })
    }
    fn compose_file(&self) -> PathBuf {
        self.c
            .path(".local/share/user-memory")
            .join(format!("docker-{}/compose.yaml", self.name))
    }
    fn volume(&self) -> String {
        format!("{}-data", self.name)
    }
    fn real(&self, mut command: Command) -> Output {
        command
            .env("USER_MEMORY_DOCKER_NAME", &self.name)
            .env("PATH", &self.path)
            .env("DOCKER_CONFIG", &self.config);
        detached(command)
    }
    fn install(&self, args: &[&str]) -> Output {
        self.real(self.c.command(&self.releases, &[], args))
    }
    fn uninstall(&self, args: &[&str]) -> Output {
        self.real(self.c.uninstall_command(self.c.home.path(), args))
    }
    fn compose(&self, args: &[&str], input: Stdio) -> Output {
        Command::new("docker")
            .args(["compose", "-f"])
            .arg(self.compose_file())
            .args(args)
            .stdin(input)
            .output()
            .unwrap()
    }
    fn volume_exists(&self) -> bool {
        Command::new("docker")
            .args(["volume", "inspect", &self.volume()])
            .output()
            .unwrap()
            .status
            .success()
    }
    /// The HTTP status of an MCP initialize sent with `host` as the Host.
    fn initialize_as(&self, port: &str, host: &str) -> String {
        let token = fs::read_to_string(self.c.token_file()).unwrap();
        let out = Command::new("curl")
            .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-H"])
            .arg(format!("Host: {host}"))
            .arg("-H")
            .arg(format!("Authorization: Bearer {}", token.trim()))
            .args([
                "-H",
                "Accept: application/json, text/event-stream",
                "-H",
                "Content-Type: application/json",
                "-d",
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#,
            ])
            .arg(format!("http://127.0.0.1:{port}/mcp"))
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for RealDocker {
    fn drop(&mut self) {
        if self.compose_file().is_file() {
            let _ = self.compose(&["down", "-v", "--rmi", "local"], Stdio::null());
        }
        let _ = Command::new("docker")
            .args(["volume", "rm", "-f", &self.volume()])
            .output();
        let _ = Command::new("docker")
            .args(["image", "rm", "-f", &format!("{}:installed", self.name)])
            .output();
    }
}

/// With USER_MEMORY_TEST_DOCKER_BINARY naming a Linux build of this commit
/// for the Docker daemon's architecture (`scripts/client-release
/// --build-only --target <triple> --out DIR` makes one, as the docker
/// workflow does), runs the installer's Docker server for real: shared
/// through --allowed-host, upgraded from an older store with its
/// migration, then uninstalled keeping its memories, and again purging them.
#[test]
fn a_real_docker_server_installs_upgrades_and_uninstalls_when_opted_in() {
    let Some(d) = RealDocker::start("upgrade") else {
        return;
    };
    let port = closed_port().to_string();
    let out = d.install(&[
        "--local",
        "docker",
        "--port",
        &port,
        "--allowed-host",
        "memory.example.com",
    ]);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("Remote MCP connection and tools: OK"),
        "{stdout}"
    );
    // The proxy's host name is accepted; any other is refused.
    assert_eq!(d.initialize_as(&port, "memory.example.com"), "200");
    assert_eq!(d.initialize_as(&port, "other.example.com"), "403");

    // An older store in the volume: the rerun migrates it, after a backup.
    let older = tempdir().unwrap();
    let store = older.path().join("memories.sqlite3");
    let snapshots = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/schema");
    let conn = rusqlite::Connection::open(&store).unwrap();
    conn.execute_batch(&fs::read_to_string(snapshots.join("v3.sql")).unwrap())
        .unwrap();
    conn.pragma_update(None, "user_version", 3).unwrap();
    conn.execute_batch(&fs::read_to_string(snapshots.join("v3.seed.sql")).unwrap())
        .unwrap();
    drop(conn);
    assert!(
        d.compose(&["stop", "memory"], Stdio::null())
            .status
            .success()
    );
    let copied = d.compose(
        &[
            "run",
            "--rm",
            "-T",
            "--entrypoint",
            "sh",
            "memory",
            "-c",
            "cat >/data/memories.sqlite3 && rm -f /data/memories.sqlite3-wal /data/memories.sqlite3-shm",
        ],
        fs::File::open(&store).unwrap().into(),
    );
    assert!(copied.status.success(), "{}", text(&copied).1);
    let out = d.install(&[]);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        format!("{stdout}{stderr}").contains("from schema version 3 to"),
        "{stdout}\n{stderr}"
    );
    let index = Command::new(d.c.path("bin/user-memory"))
        .arg("index")
        .env_clear()
        .env("HOME", d.c.home.path())
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&index.stdout).contains("Prefer Rust for new tools"),
        "{}",
        text(&index).0
    );
    // The shared-server choice survived the upgrade.
    assert_eq!(d.initialize_as(&port, "memory.example.com"), "200");

    // Uninstalling keeps the memories in the volume by default.
    let out = d.uninstall(&[]);
    assert!(out.status.success(), "{}", text(&out).1);
    assert!(d.volume_exists(), "{}", text(&out).0);
    assert!(!d.compose_file().exists());

    // A new install finds them again; a purge deletes them.
    let out = d.install(&["--local", "docker", "--port", &port]);
    assert!(out.status.success(), "{}", text(&out).1);
    let out = d.uninstall(&["--purge", "--yes", "--no-export"]);
    assert!(out.status.success(), "{}", text(&out).1);
    assert!(!d.volume_exists(), "{}", text(&out).0);
}

/// The same with a server-only host: the server set up for real, no agent
/// app connected, and doctor checking the store inside the container.
#[test]
fn a_real_docker_server_only_host_when_opted_in() {
    let Some(d) = RealDocker::start("server-only") else {
        return;
    };
    fs::remove_file(d.c.path("cli/codex")).unwrap();
    let port = closed_port().to_string();
    let out = d.real(d.c.agentless_command(&d.releases, &[], &["--server-only", "--port", &port]));
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    for shown in [
        "  Agent apps: none connected (server only)",
        "Remote MCP connection and tools: OK",
        // From the release inside the container.
        "Database: OK",
        "Memory index: OK",
        "Automatic backups: none yet in /data/backups",
    ] {
        assert!(stdout.contains(shown), "{shown}\n{stdout}");
    }
    for file in AGENT_FILES {
        assert!(!d.c.path(file).exists(), "{file}");
    }
}

#[test]
fn the_installer_makes_git_ignore_both_folder_files_once() {
    let releases = Releases::start();
    let lines = |file: &Path| {
        fs::read_to_string(file)
            .unwrap_or_default()
            .lines()
            .filter(|line| matches!(line.trim(), ".user-memory" | ".user-memory-key"))
            .count()
    };
    let install_twice = |c: &Client, ignore: &Path| {
        for run in ["first", "second"] {
            let out = c.install(&releases, &[], &["--local", "native"]);
            assert!(out.status.success(), "{run}: {}", text(&out).1);
            assert_eq!(lines(ignore), 2, "{run}: {}", text(&out).0);
        }
    };

    // core.excludesFile unset: git's own default file.
    let c = Client::bare();
    let default = c.path(".config/git/ignore");
    install_twice(&c, &default);
    assert_eq!(
        fs::read_to_string(&default).unwrap(),
        "# user-memory\n.user-memory\n.user-memory-key\n"
    );

    // core.excludesFile set: that file, after the lines already there.
    let c = Client::bare();
    fs::write(
        c.path(".gitconfig"),
        "[core]\n\texcludesFile = ~/.gitignore_global\n",
    )
    .unwrap();
    let global = c.path(".gitignore_global");
    fs::write(&global, ".DS_Store\n").unwrap();
    install_twice(&c, &global);
    assert_eq!(
        fs::read_to_string(&global).unwrap(),
        ".DS_Store\n# user-memory\n.user-memory\n.user-memory-key\n"
    );
    assert!(!c.path(".config/git/ignore").exists());
}

#[test]
fn a_shared_docker_server_accepts_its_proxy_hosts_and_keeps_them_on_reruns() {
    let releases = Releases::start();
    let c = Client::bare();
    let _server = FakeServer(&c);
    let port = closed_port().to_string();
    let compose = || fs::read_to_string(c.docker_dir().join("compose.yaml")).unwrap();
    let out = c.install(
        &releases,
        &[],
        &[
            "--local",
            "docker",
            "--port",
            &port,
            "--allowed-host",
            "Memory.Example.com",
        ],
    );
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    // This machine's own clients still connect over 127.0.0.1.
    assert!(
        stdout.contains("Remote MCP connection and tools: OK"),
        "{stdout}"
    );
    let file = compose();
    for line in [
        format!("      - \"127.0.0.1:{port}:8765\""),
        "      - --allowed-host\n      - localhost".into(),
        "      - --allowed-host\n      - 127.0.0.1".into(),
        "      - --allowed-host=memory.example.com".into(),
    ] {
        assert!(file.contains(&line), "{line}\n{file}");
    }
    assert_eq!(file.matches(":8765\"").count(), 1, "{file}");
    assert!(stdout.contains("https://memory.example.com"), "{stdout}");
    assert!(
        stdout.contains("--url https://memory.example.com --token-file"),
        "{stdout}"
    );

    // Two hosts and a private address for a proxy on another machine.
    let out = c.install(
        &releases,
        &[],
        &[
            "--local",
            "docker",
            "--allowed-host",
            "memory.example.com",
            "--allowed-host",
            "memory.internal",
            "--bind",
            "10.0.0.5",
        ],
    );
    assert!(out.status.success(), "{}", text(&out).1);
    let file = compose();
    for line in [
        format!("      - \"127.0.0.1:{port}:8765\""),
        format!("      - \"10.0.0.5:{port}:8765\""),
        "      - --allowed-host=memory.example.com".into(),
        "      - --allowed-host=memory.internal".into(),
    ] {
        assert!(file.contains(&line), "{line}\n{file}");
    }

    // A rerun without the options keeps them; giving one again replaces it.
    let out = c.install(&releases, &[], &[]);
    assert!(out.status.success(), "{}", text(&out).1);
    assert_eq!(compose(), file);
    let out = c.install(&releases, &[], &["--allowed-host", "other.example.com"]);
    assert!(out.status.success(), "{}", text(&out).1);
    let file = compose();
    assert!(file.contains("--allowed-host=other.example.com"), "{file}");
    assert!(!file.contains("--allowed-host=memory."), "{file}");
    assert!(
        file.contains(&format!("\"10.0.0.5:{port}:8765\"")),
        "{file}"
    );
    let out = c.install(&releases, &[], &["--bind", "127.0.0.1"]);
    assert!(out.status.success(), "{}", text(&out).1);
    assert_eq!(compose().matches(":8765\"").count(), 1, "{}", compose());
    assert!(compose().contains("--allowed-host=other.example.com"));
}

#[test]
fn the_shared_server_options_refuse_every_network_and_other_modes() {
    let releases = Releases::start();
    for (args, message) in [
        (
            &["--local", "docker", "--bind", "0.0.0.0"][..],
            "HTTPS proxy",
        ),
        (&["--local", "docker", "--bind", "::"][..], "HTTPS proxy"),
        (&["--local", "docker", "--bind", "[::]"][..], "HTTPS proxy"),
        (
            &["--local", "docker", "--bind", "memory.example.com"][..],
            "--bind takes an IPv4 address",
        ),
        (
            &[
                "--local",
                "docker",
                "--allowed-host",
                "https://m.example.com",
            ][..],
            "--allowed-host takes a host name",
        ),
        (
            &["--local", "native", "--allowed-host", "m.example.com"][..],
            "with --local docker",
        ),
        (
            &["--url", "https://m.example.com", "--bind", "10.0.0.5"][..],
            "with --local docker",
        ),
    ] {
        let c = Client::bare();
        let out = c.install(&releases, &[], args);
        assert!(!out.status.success(), "{args:?}");
        assert!(text(&out).1.contains(message), "{args:?}: {}", text(&out).1);
        assert!(
            !c.docker_log().contains("compose"),
            "{args:?}: nothing started"
        );
        assert!(!c.path("bin").exists(), "{args:?}: nothing installed");
    }
}

// Doctor fails while Codex has not approved the session hooks, as on every
// first install with Codex. The installer still says how to run the Docker
// server and how other machines reach it, then fails, naming doctor.
#[test]
fn a_step_left_for_the_user_ends_the_install_and_does_not_fail_it() {
    let releases = Releases::start();
    let c = Client::bare();
    let _server = FakeServer(&c);
    let port = closed_port().to_string();
    let out = c.install(
        &releases,
        &[("FAKE_CODEX_TRUST", "untrusted")],
        &[
            "--local",
            "docker",
            "--port",
            &port,
            "--allowed-host",
            "memory.example.com",
        ],
    );
    let (stdout, stderr) = text(&out);
    // Codex has not approved the hooks yet: nothing is broken, the user
    // has one step left, and the install succeeds.
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(!stderr.contains("doctor found a problem"), "{stderr}");
    for shown in [
        "The memory server runs in Docker on 127.0.0.1:",
        "https://memory.example.com",
        "--url https://memory.example.com --token-file",
    ] {
        assert!(stdout.contains(shown), "{shown}\n{stdout}");
    }
    // The step comes last, after everything else the installer says.
    let step = "Codex session hooks: Codex has not approved them yet, so they never run; open Codex and approve them once";
    let at = stdout.rfind(step).expect("the step is printed");
    assert!(at > stdout.rfind("--token-file").unwrap(), "{stdout}");
    let tail = &stdout[at..];
    assert!(
        tail.trim_end()
            .ends_with("SessionStart and UserPromptSubmit hooks"),
        "{stdout}"
    );
    assert!(
        stdout[..at].contains("\nStill to do before memory works everywhere:\n"),
        "{stdout}"
    );
}

#[test]
fn a_real_problem_still_fails_the_install_after_saying_everything() {
    let releases = Releases::start();
    let c = Client::bare();
    let out = c.install(
        &releases,
        &[("FAKE_CODEX_TRUST", "untrusted"), ("FAKE_CODEX_DRIFT", "1")],
        &["--local", "native"],
    );
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(stderr.contains("Registration does not match"), "{stderr}");
    assert!(
        stderr.contains("doctor found a problem (above)"),
        "{stderr}"
    );
    // The step the user must take is still said, after everything else.
    assert!(
        stdout.contains("Action needed: Codex session hooks:"),
        "{stdout}"
    );
    assert!(
        stdout
            .trim_end()
            .ends_with("SessionStart and UserPromptSubmit hooks"),
        "{stdout}"
    );
}

/// The last non-empty line the installer printed.
fn last_line(stdout: &str) -> &str {
    stdout.trim_end().lines().last().unwrap_or_default()
}

#[test]
fn an_install_beside_someone_elses_skill_finishes_with_the_step() {
    let releases = Releases::start();
    let c = Client::bare();
    let folder = c.path(".agents/skills/user-memory");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("SKILL.md"), "someone else's").unwrap();
    let out = c.install(&releases, &[], &["--local", "native"]);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert_eq!(
        last_line(&stdout),
        format!(
            "  Skill: {} was not installed by user-memory setup; remove it, or run `user-memory setup --no-skill`",
            folder.display()
        ),
        "{stdout}"
    );
    assert_eq!(
        fs::read_to_string(folder.join("SKILL.md")).unwrap(),
        "someone else's"
    );
}

#[test]
fn an_install_where_claude_code_has_an_override_finishes_with_the_step() {
    let releases = Releases::start();
    let c = Client::bare();
    // The fake Claude Code shows claude.override for `mcp get` and writes
    // the user entry to .claude.json on `mcp add`, as Claude Code does.
    c.fake("claude");
    let theirs = "user-memory:\n  Scope: Local config (private to you in this project)\n  Type: stdio\n  Command: /their/user-memory\n  Args: serve\n";
    fs::write(c.path("claude.override"), theirs).unwrap();
    let project = c.path("project");
    fs::create_dir_all(&project).unwrap();
    let mut command = c.command(&releases, &[], &["--local", "native", "--client", "claude"]);
    command.current_dir(&project);
    let out = detached(command);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    let here = fs::canonicalize(&project).unwrap();
    assert_eq!(
        last_line(&stdout),
        format!(
            "  Claude Code: a local-scope user-memory server overrides shared memory in {}; memory works in every other folder. To use it there too, run `claude mcp remove user-memory --scope local` in that folder",
            here.display()
        ),
        "{stdout}"
    );
    assert_eq!(
        fs::read_to_string(c.path("claude.override")).unwrap(),
        theirs
    );
    let user: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(c.path(".claude.json")).unwrap()).unwrap();
    assert_eq!(
        user["mcpServers"]["user-memory"]["command"],
        c.path("bin/user-memory").to_str().unwrap()
    );
}

// ─── uninstall.sh ────────────────────────────────────────────────────────────

/// Every file under `dir` with its contents (a link's target), so a run can
/// be compared with the state before it.
fn files_under(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_symlink() {
                found.push((
                    path.clone(),
                    fs::read_link(&path)
                        .unwrap()
                        .into_os_string()
                        .into_encoded_bytes(),
                ));
            } else if meta.is_dir() {
                stack.push(path);
            } else {
                found.push((path.clone(), fs::read(&path).unwrap()));
            }
        }
    }
    found.sort();
    found
}

/// The user's own Codex instructions, written before the install.
const OWN_AGENTS: &str = "# My rules\n\nBe brief.\n";

/// A client installed natively with one memory, over the user's own
/// AGENTS.md and git ignore.
fn native_install(releases: &Releases) -> Client {
    let c = Client::bare();
    fs::create_dir_all(c.path(".codex")).unwrap();
    fs::write(c.path(".codex/AGENTS.md"), OWN_AGENTS).unwrap();
    fs::create_dir_all(c.path(".config/git")).unwrap();
    fs::write(c.path(".config/git/ignore"), "*.log\n").unwrap();
    let out = c.install(releases, &[], &["--local", "native"]);
    assert!(out.status.success(), "{}", text(&out).1);
    let record = Command::new(c.path("bin/user-memory"))
        .args([
            "record",
            "--type",
            "preference",
            "--category",
            "workflow",
            "--title",
            "Keep commits small",
            "--content",
            "Keep commits small",
            "--scope",
            "global",
        ])
        .env_clear()
        .env("HOME", c.home.path())
        .output()
        .unwrap();
    assert!(record.status.success(), "{}", text(&record).1);
    c
}

/// What install left that uninstall must remove, relative to the home.
const INSTALLED: [&str; 6] = [
    "bin/user-memory",
    "bin/.user-memory.json",
    ".agents/skills/user-memory",
    "codex.json",
    ".codex/hooks.json",
    ".local/state/user-memory",
];

fn assert_gone(c: &Client, paths: &[&str], said: &str) {
    for path in paths {
        assert!(
            fs::symlink_metadata(c.path(path)).is_err(),
            "{path} is left\n{said}"
        );
    }
}

#[test]
fn uninstall_sh_removes_a_native_install_and_keeps_the_memories() {
    let releases = Releases::start();
    let c = native_install(&releases);
    fs::create_dir_all(c.path(".local/state/user-memory/sessions")).unwrap();
    let out = c.uninstall(c.home.path(), &[]);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert_gone(&c, &INSTALLED, &stdout);
    assert_eq!(
        fs::read_to_string(c.path(".codex/AGENTS.md")).unwrap(),
        OWN_AGENTS
    );
    assert_eq!(
        fs::read_to_string(c.path(".config/git/ignore")).unwrap(),
        "*.log\n"
    );
    let store = c.path(".local/share/user-memory/memories.sqlite3");
    assert!(store.is_file());
    assert_eq!(automatic_backups(&c).len(), 1, "kept with the store");
    assert!(
        stdout.contains(&format!("Your memories were kept: {}", store.display())),
        "{stdout}"
    );
    // The script removes the binary itself, so nothing tells the user to.
    assert!(!stdout.contains("binary is still at"), "{stdout}");
    // Again: nothing is left to do.
    let again = c.uninstall(c.home.path(), &[]);
    assert!(again.status.success(), "{}", text(&again).1);
    assert!(
        text(&again).0.contains("Nothing to remove."),
        "{}",
        text(&again).0
    );
}

/// The automatic backups beside the native store, by name.
fn automatic_backups(c: &Client) -> Vec<String> {
    let Ok(entries) = fs::read_dir(c.path(".local/share/user-memory/backups")) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.strip_prefix("memories-")
                .and_then(|rest| rest.strip_suffix(".sqlite3"))
                .is_some_and(|date| date.len() == 10)
        })
        .collect();
    names.sort();
    names
}

#[test]
fn uninstall_sh_purges_a_native_store_after_exporting_it() {
    use std::os::unix::fs::PermissionsExt;
    let releases = Releases::start();
    let c = native_install(&releases);
    let here = c.path("exports");
    fs::create_dir_all(&here).unwrap();
    let out = c.uninstall(&here, &["--purge", "--yes"]);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert_gone(&c, &INSTALLED, &stdout);
    // The automatic backups go with the store, folder and all.
    assert_gone(&c, &[".local/share/user-memory"], &stdout);
    let exports: Vec<_> = fs::read_dir(&here)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(exports.len(), 1, "{stdout}");
    assert_eq!(
        fs::metadata(&exports[0]).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        fs::read_to_string(&exports[0])
            .unwrap()
            .contains("Keep commits small")
    );
}

#[test]
fn uninstall_sh_refuses_a_purge_it_cannot_confirm_and_a_dry_run_changes_nothing() {
    let releases = Releases::start();
    let c = native_install(&releases);
    let before = files_under(c.home.path());
    let out = c.uninstall(c.home.path(), &["--purge"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out).0);
    assert!(
        text(&out).1.contains("also needs --yes"),
        "{}",
        text(&out).1
    );
    assert_eq!(files_under(c.home.path()), before);
    let out = c.uninstall(c.home.path(), &["--dry-run", "--purge", "--yes"]);
    assert!(out.status.success(), "{}", text(&out).1);
    assert!(
        text(&out)
            .0
            .contains("Would remove: the user-memory binary"),
        "{}",
        text(&out).0
    );
    assert_eq!(files_under(c.home.path()), before);
}

#[test]
fn uninstall_sh_removes_the_docker_server_and_keeps_or_purges_its_volume() {
    let releases = Releases::start();
    for purge in [false, true] {
        let c = Client::bare();
        let _server = FakeServer(&c);
        let port = closed_port().to_string();
        let out = c.install(&releases, &[], &["--local", "docker", "--port", &port]);
        assert!(out.status.success(), "{}", text(&out).1);
        let args: &[&str] = if purge { &["--purge", "--yes"] } else { &[] };
        let here = c.path("exports");
        fs::create_dir_all(&here).unwrap();
        let out = c.uninstall(&here, args);
        let (stdout, stderr) = text(&out);
        assert!(out.status.success(), "{stdout}\n{stderr}");
        assert_gone(
            &c,
            &[
                "bin/user-memory",
                "bin/.user-memory.json",
                ".config/user-memory",
            ],
            &stdout,
        );
        assert!(!c.docker_dir().exists(), "{stdout}");
        let down = c
            .docker_log()
            .lines()
            .find(|line| line.contains(" down "))
            .map(str::to_owned)
            .unwrap_or_default();
        assert!(down.contains("--rmi local"), "{down}");
        assert_eq!(down.contains(" -v"), purge, "{down}");
        if purge {
            assert!(!c.path("server").exists(), "the volume is gone");
            assert_eq!(fs::read_dir(&here).unwrap().count(), 1, "an export first");
        } else {
            assert!(
                c.path("server/memories.sqlite3").is_file(),
                "the volume is kept"
            );
            assert!(
                stdout.contains("docker volume rm user-memory-data"),
                "{stdout}"
            );
        }
    }
}

#[test]
fn uninstall_sh_removes_what_it_can_when_the_binary_fails() {
    let releases = Releases::start();
    let c = native_install(&releases);
    // A new file renamed over the binary: Linux refuses to write into a
    // program that is still running, as the install's background sync may be.
    // The fake exits 1 at once as `user-memory`, by the exit file beside it.
    let broken = fake::fake(&c.path("bin"), "user-memory.broken");
    fs::write(c.path("bin/user-memory.exit"), "1").unwrap();
    fs::rename(&broken, c.path("bin/user-memory")).unwrap();
    let out = c.uninstall(c.home.path(), &["--purge", "--yes", "--no-export"]);
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("does not run; removing what is left directly"),
        "{stdout}"
    );
    assert_gone(
        &c,
        &[
            "bin/user-memory",
            "bin/.user-memory.json",
            ".agents/skills/user-memory",
            "codex.json",
        ],
        &stdout,
    );
}

#[test]
fn uninstall_sh_asks_on_a_terminal_and_keeps_the_memories_by_default() {
    let releases = Releases::start();
    let c = native_install(&releases);
    let store = c.path(".local/share/user-memory/memories.sqlite3");
    let Some((status, screen)) = on_terminal(
        &c.uninstall_command(c.home.path(), &[]),
        &[("Keep your memories (", "")],
    ) else {
        return;
    };
    assert_eq!(status, 0, "{screen}");
    assert!(store.is_file(), "{screen}");
    assert!(screen.contains("Your memories were kept"), "{screen}");
}

#[test]
fn uninstall_sh_deletes_on_a_terminal_only_after_export_and_typed_delete() {
    let releases = Releases::start();
    let c = native_install(&releases);
    let store = c.path(".local/share/user-memory/memories.sqlite3");
    let here = c.path("exports");
    fs::create_dir_all(&here).unwrap();
    // Anything but the word delete keeps them.
    let Some((status, screen)) = on_terminal(
        &c.uninstall_command(&here, &[]),
        &[
            ("Keep your memories (", "n"),
            ("Export them to a file", "n"),
            ("Type delete", "yes"),
        ],
    ) else {
        return;
    };
    assert_eq!(status, 0, "{screen}");
    assert!(store.is_file(), "{screen}");

    let c = native_install(&releases);
    let store = c.path(".local/share/user-memory/memories.sqlite3");
    let here = c.path("exports");
    fs::create_dir_all(&here).unwrap();
    let Some((status, screen)) = on_terminal(
        &c.uninstall_command(&here, &[]),
        &[
            ("Keep your memories (", "n"),
            ("Export them to a file", ""),
            ("Type delete", "delete"),
        ],
    ) else {
        return;
    };
    assert_eq!(status, 0, "{screen}");
    assert!(!store.exists(), "{screen}");
    let exports: Vec<_> = fs::read_dir(&here)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(exports.len(), 1, "{screen}");
    assert!(
        fs::read_to_string(&exports[0])
            .unwrap()
            .contains("Keep commits small")
    );
}

/// Without the binary and its saved setup, uninstall.sh looks for the store
/// where the binary keeps it by default, which XDG_DATA_HOME never moves.
#[test]
fn uninstall_sh_finds_the_default_store_whatever_xdg_data_home_says() {
    let releases = Releases::start();
    let c = native_install(&releases);
    let store = c.path(".local/share/user-memory/memories.sqlite3");
    assert!(store.is_file());
    fs::remove_dir_all(c.path("bin")).unwrap();
    let elsewhere = c.path("xdg data");
    fs::create_dir_all(&elsewhere).unwrap();
    let uninstall = |args: &[&str]| {
        let mut command = c.uninstall_command(c.home.path(), args);
        command.env("XDG_DATA_HOME", &elsewhere);
        detached(command)
    };
    let out = uninstall(&[]);
    let (stdout, stderr) = text(&out);
    assert!(
        stdout.contains(&format!("Your memories were kept: {}.", store.display())),
        "{stdout}\n{stderr}"
    );
    assert!(store.is_file());
    let out = uninstall(&["--purge", "--yes", "--no-export"]);
    let (stdout, stderr) = text(&out);
    assert!(
        stdout.contains(&format!("your memories ({})", store.display())),
        "{stdout}\n{stderr}"
    );
    assert!(!store.exists() && !store.with_extension("sqlite3.lock").exists());
    assert!(
        !c.path(".local/share/user-memory").exists(),
        "the emptied folder goes too"
    );
}

/// USER_MEMORY_DB names the store for the binary, and so for uninstall.sh
/// without it.
#[test]
fn uninstall_sh_finds_the_store_user_memory_db_names() {
    let releases = Releases::start();
    let c = native_install(&releases);
    fs::remove_dir_all(c.path("bin")).unwrap();
    let store = c.path("elsewhere/memories.sqlite3");
    fs::create_dir_all(store.parent().unwrap()).unwrap();
    fs::copy(c.path(".local/share/user-memory/memories.sqlite3"), &store).unwrap();
    let mut command = c.uninstall_command(c.home.path(), &[]);
    command.env("USER_MEMORY_DB", &store);
    let out = detached(command);
    let (stdout, stderr) = text(&out);
    assert!(
        stdout.contains(&format!("Your memories were kept: {}.", store.display())),
        "{stdout}\n{stderr}"
    );
}

#[test]
fn uninstall_sh_removes_what_it_can_when_the_binary_is_missing() {
    let releases = Releases::start();
    let c = native_install(&releases);
    fs::remove_file(c.path("bin/user-memory")).unwrap();
    let backups = c.path(".local/share/user-memory/backups");
    let made = automatic_backups(&c);
    assert_eq!(made.len(), 1);
    // Copies cut short and the failure note are user-memory's; anything
    // else in the folder is the user's.
    let partial = backups.join(format!(".{}.partial-123", made[0]));
    let failure = backups.join("last-failure");
    let users = [
        backups.join("notes.txt"),
        backups.join("memories-2026-10-18.sqlite3.copy"),
        backups.join("other-2026-10-18.sqlite3"),
    ];
    for file in users.iter().chain([&partial, &failure]) {
        fs::write(file, "").unwrap();
    }
    let out = c.uninstall(c.home.path(), &["--purge", "--yes", "--no-export"]);
    let (stdout, stderr) = text(&out);
    // The hooks and the AGENTS.md block need the binary to edit safely: named,
    // and the run exits 1 after removing everything else.
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert_gone(
        &c,
        &[
            "bin/.user-memory.json",
            ".agents/skills/user-memory",
            "codex.json",
            ".local/share/user-memory/memories.sqlite3",
            ".local/share/user-memory/memories.sqlite3.lock",
        ],
        &stdout,
    );
    assert!(automatic_backups(&c).is_empty(), "{stdout}");
    assert!(!partial.exists() && !failure.exists());
    assert!(users.iter().all(|file| file.is_file()));
    assert!(
        stdout.contains("Left: the user-memory session hooks in"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Left: the user-memory rules in"),
        "{stdout}"
    );
    assert!(
        fs::read_to_string(c.path(".codex/AGENTS.md"))
            .unwrap()
            .starts_with(OWN_AGENTS)
    );
    assert_eq!(
        fs::read_to_string(c.path(".config/git/ignore")).unwrap(),
        "*.log\n"
    );
}

#[test]
fn uninstall_sh_explains_itself_under_dash_without_a_terminal() {
    // Debian's and Ubuntu's /bin/sh. A failed redirection on `:` would end
    // the script there, silently.
    if !Path::new("/bin/dash").exists() {
        return;
    }
    let releases = Releases::start();
    let c = native_install(&releases);
    let dash = |args: &[&str]| {
        let mut command = c.uninstall_command(c.home.path(), args);
        let script = command.get_args().next().unwrap().to_owned();
        let rest: Vec<_> = command.get_args().skip(1).map(|a| a.to_owned()).collect();
        let envs: Vec<_> = command
            .get_envs()
            .filter_map(|(k, v)| v.map(|v| (k.to_owned(), v.to_owned())))
            .collect();
        command = Command::new("/bin/dash");
        command
            .arg(script)
            .args(rest)
            .current_dir(c.home.path())
            .env_clear()
            .envs(envs)
            .stdin(Stdio::null());
        detached(command)
    };
    let out = dash(&["--purge"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out).1.contains("also needs --yes"),
        "{}",
        text(&out).1
    );
    // Without the binary the script asks about the memories itself; with
    // no terminal it keeps them and finishes.
    fs::remove_file(c.path("bin/user-memory")).unwrap();
    let out = dash(&[]);
    let (stdout, stderr) = text(&out);
    assert!(
        stdout.contains("Your memories were kept"),
        "{stdout}\n{stderr}"
    );
    assert!(
        c.path(".local/share/user-memory/memories.sqlite3")
            .is_file()
    );
}

// Debian and Ubuntu run `curl … | sh` with dash, which ends the script when
// a redirection on a special builtin such as `:` fails, as opening /dev/tty
// does without a terminal. The installer still says what is missing.
#[test]
fn the_installer_explains_itself_under_dash_without_a_terminal() {
    let releases = Releases::start();
    let c = Client::bare();
    std::os::unix::fs::symlink("/bin/dash", c.path("cli/sh")).unwrap();
    let out = c.install(&releases, &[], &[]);
    let (stdout, stderr) = text(&out);
    assert!(!out.status.success(), "{stdout}");
    assert!(
        stderr.contains("no server address: pass --url, or --local docker|native"),
        "{stdout}\n{stderr}"
    );
}

// ─── server-only hosts ──────────────────────────────────────────────────────

/// What setup writes for an agent app, relative to the home folder; a
/// server-only host has none of it.
const AGENT_FILES: [&str; 6] = [
    ".codex/hooks.json",
    ".codex/AGENTS.md",
    ".agents/skills/user-memory",
    "skills/user-memory",
    "rules/user-memory.md",
    ".config/git/ignore",
];

fn receipt(c: &Client) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(c.path("bin/.user-memory.json")).unwrap()).unwrap()
}

#[test]
fn a_server_only_host_runs_the_server_and_connects_no_agent() {
    let releases = Releases::start();
    let c = Client::bare();
    // A host with neither Claude Code nor Codex.
    fs::remove_file(c.path("cli/codex")).unwrap();
    let _server = FakeServer(&c);
    let port = closed_port().to_string();
    let out = detached(c.agentless_command(&releases, &[], &["--server-only", "--port", &port]));
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    for shown in [
        "  Agent apps: none connected (server only)",
        "Setup complete. This machine runs the memory server only; no agent app uses it here.",
        // doctor: the server answers, and the store checks pass where it lives.
        "Remote MCP connection and tools: OK",
        "Database: OK",
        "Memory index: OK",
        "Automatic backups: none yet in ",
        &format!("The memory server runs in Docker on 127.0.0.1:{port}"),
        &format!(
            "token is in {}; every machine that uses the server needs a copy.",
            c.token_file().display()
        ),
        "this installer again with --allowed-host HOST",
    ] {
        assert!(stdout.contains(shown), "{shown}\n{stdout}");
    }
    assert!(
        !stdout.contains("Codex") && !stdout.contains("Still to do"),
        "{stdout}"
    );
    for file in AGENT_FILES {
        assert!(!c.path(file).exists(), "{file}");
    }
    assert_eq!(receipt(&c)["server_only"], true);
    assert_eq!(receipt(&c)["clients"], serde_json::json!([]));

    // A rerun keeps it, even once an agent app appears.
    c.fake("codex");
    let out = detached(c.agentless_command(&releases, &[], &[]));
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("Agent apps: none connected (server only)"),
        "{stdout}"
    );
    for file in AGENT_FILES {
        assert!(!c.path(file).exists(), "{file}");
    }

    // --client makes the host a client too, on the same server.
    let out = c.install(&releases, &[], &[]);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("Codex: connected"), "{stdout}");
    assert!(c.path(".codex/hooks.json").exists());
    assert!(receipt(&c).get("server_only").is_none(), "{}", receipt(&c));
    assert!(
        stdout.contains(&format!(
            "Upgrading the Docker memory server in {}",
            c.docker_dir().display()
        )),
        "{stdout}"
    );
}

#[test]
fn a_server_only_host_applies_its_embedding_choice_in_the_container() {
    let releases = Releases::start();
    let endpoint = FakeOllama::start(&[], None);
    let c = Client::bare();
    fs::remove_file(c.path("cli/codex")).unwrap();
    let _server = FakeServer(&c);
    let port = closed_port().to_string();
    let out = detached(c.agentless_command(
        &releases,
        &[],
        &[
            "--server-only",
            "--port",
            &port,
            "--embed-url",
            &endpoint.address,
            "--embed-model",
            "nomic-embed-text:latest",
        ],
    ));
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    let endpoint_port = endpoint.address.rsplit(':').next().unwrap();
    assert_eq!(
        fs::read_to_string(c.docker_dir().join("embedding.env")).unwrap(),
        format!(
            "USER_MEMORY_EMBED_URL=http://host.docker.internal:{endpoint_port}/v1/embeddings\nUSER_MEMORY_EMBED_MODEL=nomic-embed-text:latest\nUSER_MEMORY_EMBED_ALLOW_HTTP=1\n"
        )
    );
    let log = c.docker_log();
    let recreated = log.rfind(" up -d --wait").expect("recreated");
    assert!(
        log[recreated..].contains("exec -T memory user-memory embedding sync"),
        "synced in the container after the recreate: {log}"
    );
    for file in AGENT_FILES {
        assert!(!c.path(file).exists(), "{file}");
    }
}

#[test]
fn server_only_is_refused_with_a_client_or_another_server() {
    let releases = Releases::start();
    let c = Client::bare();
    for (args, message) in [
        (
            &["--server-only", "--url", "https://memory.example.com"][..],
            "--server-only runs the memory server on this machine, while --url connects to one elsewhere; give one of them",
        ),
        (
            &["--server-only", "--local", "native"][..],
            "--server-only runs the server in Docker; leave out --local native",
        ),
        (
            &["--server-only", "--client", "codex"][..],
            "--server-only connects no agent app; leave out --client",
        ),
    ] {
        let out = detached(c.agentless_command(&releases, &[], args));
        assert!(!out.status.success(), "{args:?}");
        assert!(text(&out).1.contains(message), "{args:?}: {}", text(&out).1);
    }
    assert!(!c.path("bin").exists(), "nothing installed");
}

#[test]
fn uninstall_removes_a_server_only_host() {
    let releases = Releases::start();
    let c = Client::bare();
    fs::remove_file(c.path("cli/codex")).unwrap();
    let _server = FakeServer(&c);
    let port = closed_port().to_string();
    let out = detached(c.agentless_command(&releases, &[], &["--server-only", "--port", &port]));
    assert!(out.status.success(), "{}", text(&out).1);
    let out = c.uninstall(c.home.path(), &[]);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(!c.path("bin/user-memory").exists() && !c.path("bin/.user-memory.json").exists());
    assert!(
        c.docker_log().contains(" down --rmi local"),
        "{}",
        c.docker_log()
    );
    for file in AGENT_FILES {
        assert!(!c.path(file).exists(), "{file}");
    }
}

#[test]
fn a_docker_server_that_cannot_reach_ollama_on_linux_names_the_bridge_fix() {
    let releases = Releases::start();
    releases.native_build_for_every_target();
    let ollama = FakeOllama::start(&[QWEN], None);
    for (system, hint) in [("Linux", true), ("Darwin", false)] {
        let c = Client::bare();
        c.fake("uname");
        let _server = FakeServer(&c);
        fs::write(c.path("sync-fails"), "").unwrap();
        let port = closed_port().to_string();
        let out = c.install(
            &releases,
            &[ollama.env()[0], ("FAKE_UNAME_S", system)],
            &["--local", "docker", "--port", &port, "--embed-model", QWEN],
        );
        let (stdout, stderr) = text(&out);
        assert!(!out.status.success(), "{system}: {stdout}\n{stderr}");
        assert!(
            stderr.contains(
                "semantic search was not changed (above), though everything else is installed; "
            ),
            "{system}: {stderr}"
        );
        let note = format!(
            "on Linux the Docker server reaches Ollama at the Docker bridge address 172.17.0.1, where Ollama does not answer while it listens on 127.0.0.1, its default: set OLLAMA_HOST=172.17.0.1:{} for the Ollama service",
            ollama.address.rsplit(':').next().unwrap()
        );
        assert_eq!(stderr.contains(&note), hint, "{system}: {stderr}");
        assert!(!stderr.contains("0.0.0.0"), "{stderr}");
        // The command to run once Ollama answers there.
        assert!(
            stderr.contains(&format!(
                "once that is fixed, run: '{}' embedding set --url {} --model {QWEN}",
                c.path("bin/user-memory").display(),
                ollama.address
            )),
            "{system}: {stderr}"
        );
    }
}
