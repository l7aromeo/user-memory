//! Setup, doctor and uninstall on Windows, against the fake clients: a
//! client found through PATHEXT as a `.cmd` shim, the hook forms each
//! client runs (and running them as the clients do), the user PATH entry,
//! and an upgrade while an agent session runs the installed binary.
//!
//! Every run edits a registry key of its own (USER_MEMORY_ENVIRONMENT_KEY),
//! never the user's PATH, and keeps its homes in a temporary folder. The
//! tests run only on Windows; each failure prints what the command said and
//! the files involved, as the first place they run is CI.

#![cfg(windows)]
#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
use serde_json::{Value, json};
use std::{
    env, fs,
    io::Write,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tempfile::{TempDir, tempdir};
use windows_registry::{CURRENT_USER, Type};

#[path = "support/fake.rs"]
mod fake;
#[path = "support/time.rs"]
mod time;

/// The embedding settings a developer's environment may hold, which no
/// test may inherit.
const EMBED_ENV: [&str; 6] = [
    "USER_MEMORY_EMBED_URL",
    "USER_MEMORY_EMBED_MODEL",
    "USER_MEMORY_EMBED_DIMENSIONS",
    "USER_MEMORY_EMBED_QUERY_INSTRUCTION",
    "USER_MEMORY_EMBED_API_KEY_FILE",
    "USER_MEMORY_EMBED_ALLOW_HTTP",
];

/// Starts a process with a console but no window, as an agent's hidden
/// shell does: CONIN$ opens, but no one can type in it.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The registry value setup edits, in the sandbox's key.
const PATH_VALUE: &str = "Path";

/// A session's payload, as Codex hands it to the session-start hook.
const CODEX_PAYLOAD: &str = r#"{"session_id":"thr_1","hook_event_name":"SessionStart"}"#;
/// A session's payload, as Claude Code hands it to the session-start hook.
const CLAUDE_PAYLOAD: &str = r#"{"session_id":"abc1","hook_event_name":"SessionStart"}"#;

/// Homes, clients and a registry key of the test's own.
struct Sandbox {
    root: TempDir,
    /// The key under HKEY_CURRENT_USER that stands in for `Environment`.
    key: String,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempdir().unwrap();
        for folder in ["bin", "real", "home", "home/.codex", "local"] {
            fs::create_dir_all(root.path().join(folder)).unwrap();
        }
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let key = format!(r"Software\user-memory-test-{}-{nanos}", std::process::id());
        Self { root, key }
    }
    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }
    fn install_dir(&self) -> PathBuf {
        self.path("installed tools")
    }
    fn binary(&self) -> PathBuf {
        self.install_dir().join("user-memory.exe")
    }
    /// The store, in a folder setup creates, so it starts private.
    fn db(&self) -> PathBuf {
        self.path("store/memories.sqlite3")
    }
    /// The client as a program in bin, with the fixture the fake registers
    /// for Claude Code; the fake Codex builds its entry from `mcp add`.
    fn client(&self, name: &str) {
        self.fixture(name);
        fake::fake(&self.path("bin"), name);
    }
    /// The client as npm installs it: a `.cmd` shim in bin that runs the
    /// program kept elsewhere, so only PATHEXT finds it.
    fn client_cmd(&self, name: &str) {
        self.fixture(name);
        let real = fake::fake(&self.path("real"), name);
        fs::write(
            self.path(&format!("bin/{name}.cmd")),
            format!("@\"{}\" %*\r\n", real.display()),
        )
        .unwrap();
    }
    fn fixture(&self, name: &str) {
        if name == "claude" {
            fs::write(
                self.path("home/claude.fixture"),
                format!(
                    "Scope: User config (available in all your projects)\nCommand: {}\nArgs: serve\nEnvironment:\n  USER_MEMORY_DB={}\n",
                    self.binary().display(),
                    self.db().display()
                ),
            )
            .unwrap();
        }
    }
    /// The environment of every run: the sandbox's homes and local app data
    /// folder, clients first on PATH, git reading only the sandbox's
    /// configuration, no update check, the fake Codex trusting the hooks,
    /// and the sandbox's registry key.
    fn environment(&self, command: &mut Command) {
        for key in EMBED_ENV {
            command.env_remove(key);
        }
        let system = env::var_os("PATH").unwrap_or_default();
        let path = env::join_paths(
            [self.path("bin")]
                .into_iter()
                .chain(env::split_paths(&system)),
        )
        .unwrap();
        command
            .env("PATH", path)
            .sandboxed(self.path("home"))
            .env("USERPROFILE", self.path("home"))
            .env("LOCALAPPDATA", self.path("local"))
            .env("CLAUDE_CONFIG_DIR", self.path("home"))
            .env("CODEX_HOME", self.path("home/.codex"))
            .env("USER_MEMORY_APP_DIRS", "")
            .env("USER_MEMORY_ENVIRONMENT_KEY", &self.key)
            .env("USER_MEMORY_NO_UPDATE_CHECK", "1")
            .env("FAKE_CODEX_TRUST", "trusted")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_GLOBAL")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("USER_MEMORY_DB")
            .env_remove("USER_MEMORY_URL")
            .env_remove("USER_MEMORY_TOKEN_FILE");
    }
    /// This build's `setup` (or another command taking the same folders).
    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .args(args)
            .arg("--install-dir")
            .arg(self.install_dir())
            .arg("--db")
            .arg(self.db())
            .stdin(Stdio::null());
        self.environment(&mut command);
        command.output().unwrap()
    }
    /// The installed binary, as a client or the user runs it.
    fn installed(&self, args: &[&str]) -> Command {
        let mut command = Command::new(self.binary());
        command.args(args);
        self.environment(&mut command);
        command
    }
    /// The installed binary's `uninstall` for the install folder, keeping
    /// the memories. It runs as an agent's hidden shell runs it, with a
    /// console but no window (CREATE_NO_WINDOW): CONIN$ opens there, but no
    /// one can answer, so it must not ask. Were it to ask, the test fails
    /// at [`time::STUCK`] rather than waiting for ever.
    fn uninstall(&self) -> Output {
        let child = self
            .installed(&["uninstall", "--binary-removed-after", "--install-dir"])
            .arg(self.install_dir())
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        time::finish(child, "uninstall without a console window")
    }
    /// The sandbox key's PATH value and its registry type, if it has one.
    fn user_path(&self) -> Option<(String, Type)> {
        let key = CURRENT_USER.create(&self.key).unwrap();
        let kind = key.get_type(PATH_VALUE).ok()?;
        Some((key.get_string(PATH_VALUE).unwrap(), kind))
    }
    fn set_user_path(&self, value: &str) {
        CURRENT_USER
            .create(&self.key)
            .unwrap()
            .set_expand_string(PATH_VALUE, value)
            .unwrap();
    }
    fn receipt(&self) -> Value {
        read_json(&self.install_dir().join(".user-memory.json"))
    }
    /// Every `user-memory.old-*.exe` an upgrade set aside.
    fn set_aside(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.install_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("user-memory.old-"))
            .collect();
        names.sort();
        names
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = CURRENT_USER.remove_tree(&self.key);
    }
}

/// A command's status and output, for a failure message.
fn said(out: &Output) -> String {
    format!(
        "status {:?}\n--- stdout\n{}--- stderr\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("{} is not JSON ({err}):\n{text}", path.display()))
}

/// Runs `command` with `payload` on stdin, as a client runs a hook.
fn with_payload(command: &mut Command, payload: &str) -> Output {
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
        .write_all(payload.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// npm installs Codex and Claude Code as `.cmd` shims; setup finds one
/// through PATHEXT and registers through it.
#[test]
fn setup_registers_a_client_installed_as_a_cmd_shim() {
    let s = Sandbox::new();
    s.client_cmd("codex");
    let out = s.run(&["setup", "--client", "codex"]);
    assert!(
        out.status.success(),
        "setup through codex.cmd failed\n{}",
        said(&out)
    );
    let entry = read_json(&s.path("home/codex.json"));
    assert_eq!(
        entry["transport"]["command"].as_str(),
        Some(s.binary().to_str().unwrap()),
        "Codex was registered with another binary: {entry}"
    );
    let receipt = s.receipt();
    assert_eq!(receipt["clients"], json!(["codex"]), "receipt: {receipt}");
}

/// Claude Code's Windows hooks need 2.1.139 or later. Setup writes them for
/// an older one too and says to update, as doctor does; an equal or newer
/// one passes, and output naming no version is a note, never a pass.
#[test]
fn an_older_claude_code_is_told_to_update() {
    let action = "Action needed: Claude Code 2.1.138 is older than 2.1.139, which runs user-memory's Windows hooks; run `claude update`";
    let note = "Cannot read Claude Code's version from `claude --version` (it printed: claude, some build); user-memory's Windows hooks need Claude Code 2.1.139 or later: run `claude update` if it is older";
    for (printed, said_action, said_note) in [
        ("2.1.138 (Claude Code)", true, false),
        ("2.1.139 (Claude Code)", false, false),
        ("2.1.140 (Claude Code)", false, false),
        ("claude, some build", false, true),
    ] {
        let s = Sandbox::new();
        s.client("claude");
        fs::write(s.path("home/claude.version"), printed).unwrap();
        let out = s.run(&["setup", "--client", "claude"]);
        assert!(
            out.status.success(),
            "{printed}: setup failed\n{}",
            said(&out)
        );
        let settings = read_json(&s.path("home/settings.json"));
        assert!(
            settings["hooks"]["SessionStart"].is_array(),
            "{printed}: setup wrote no hooks: {settings}"
        );
        let doctor = s.run(&["doctor", "--client", "claude"]);
        assert_eq!(
            doctor.status.code(),
            Some(if said_action { 3 } else { 0 }),
            "{printed}: doctor's exit\n{}",
            said(&doctor)
        );
        for (command, out) in [("setup", &out), ("doctor", &doctor)] {
            let text = stdout(out);
            assert_eq!(
                text.contains(action),
                said_action,
                "{printed}: {command}'s update step\n{}",
                said(out)
            );
            assert_eq!(
                text.contains(note),
                said_note,
                "{printed}: {command}'s note\n{}",
                said(out)
            );
            assert!(
                said_action || said_note || !text.contains("2.1.139"),
                "{printed}: {command} named the minimum\n{}",
                said(out)
            );
        }
    }
}

/// Claude Code's hook names the binary in exec form, which runs without a
/// shell; Codex's is a cmd.exe line. Each runs as its client runs it and
/// prints the index.
#[test]
fn the_hooks_take_each_clients_windows_form_and_run() {
    let s = Sandbox::new();
    s.client("claude");
    s.client("codex");
    let out = s.run(&["setup", "--client", "both"]);
    assert!(out.status.success(), "setup failed\n{}", said(&out));

    let settings = read_json(&s.path("home/settings.json"));
    let claude = &settings["hooks"]["SessionStart"][0]["hooks"][0];
    let forward = s.binary().to_string_lossy().replace('\\', "/");
    assert_eq!(
        claude["command"].as_str(),
        Some(forward.as_str()),
        "Claude Code's hook is not the binary in forward slashes: {settings:#}"
    );
    assert_eq!(
        claude["args"],
        json!(["session-start", "--client", "claude"]),
        "Claude Code's hook arguments: {settings:#}"
    );
    let mut exec = Command::new(claude["command"].as_str().unwrap());
    exec.args(["session-start", "--client", "claude"]);
    s.environment(&mut exec);
    let run = with_payload(&mut exec, CLAUDE_PAYLOAD);
    assert!(
        run.status.success()
            && stdout(&run).starts_with("Conversation origin: claude:session/abc1"),
        "Claude Code's exec-form hook did not run as written\n{}",
        said(&run)
    );

    let hooks = read_json(&s.path("home/.codex/hooks.json"));
    let line = hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap_or_else(|| panic!("no Codex SessionStart hook: {hooks:#}"))
        .to_owned();
    assert_eq!(
        line,
        format!("\"{}\" session-start --client codex", s.binary().display()),
        "Codex's hook line: {hooks:#}"
    );
    // As Codex runs it: %COMSPEC% /C "<line>".
    let comspec = env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
    let mut cmd = Command::new(comspec);
    cmd.raw_arg(format!("/C \"{line}\""));
    s.environment(&mut cmd);
    let run = with_payload(&mut cmd, CODEX_PAYLOAD);
    assert!(
        run.status.success() && stdout(&run).starts_with("Conversation origin: codex:thread/thr_1"),
        "Codex's hook line did not run through cmd.exe: {line}\n{}",
        said(&run)
    );
}

/// Setup appends its folder to the user PATH once, keeping the value's
/// type and the user's own entries; uninstall removes that entry only.
#[test]
fn setup_adds_its_folder_to_the_user_path_and_uninstall_removes_it() {
    let s = Sandbox::new();
    s.client("codex");
    let mine = r"%SystemRoot%\my-tools";
    s.set_user_path(mine);
    let out = s.run(&["setup", "--client", "codex"]);
    assert!(out.status.success(), "setup failed\n{}", said(&out));
    let folder = s.install_dir();
    let expected = format!("{mine};{}", folder.display());
    assert_eq!(
        s.user_path(),
        Some((expected.clone(), Type::ExpandString)),
        "the PATH after setup"
    );
    assert!(
        stdout(&out).contains(&format!(
            "Added {} to your PATH; open a new terminal to use user-memory",
            folder.display()
        )),
        "setup did not say it added the folder\n{}",
        said(&out)
    );
    assert_eq!(s.receipt()["path_added"], json!(true), "{}", s.receipt());

    let again = s.run(&["setup", "--client", "codex"]);
    assert!(
        again.status.success(),
        "second setup failed\n{}",
        said(&again)
    );
    assert_eq!(
        s.user_path(),
        Some((expected, Type::ExpandString)),
        "a second setup changed the PATH"
    );
    assert!(!stdout(&again).contains("to your PATH"), "{}", said(&again));
    assert_eq!(s.receipt()["path_added"], json!(true), "{}", s.receipt());

    let out = s.uninstall();
    assert!(out.status.success(), "uninstall failed\n{}", said(&out));
    assert!(
        stdout(&out).contains(&format!("Removed: the PATH entry {}", folder.display())),
        "uninstall did not report the PATH entry\n{}",
        said(&out)
    );
    assert_eq!(
        s.user_path(),
        Some((mine.to_owned(), Type::ExpandString)),
        "the PATH after uninstall"
    );
}

/// A PATH entry the user made for the folder is theirs: setup records
/// nothing, and uninstall leaves it.
#[test]
fn a_path_entry_the_user_made_stays() {
    let s = Sandbox::new();
    s.client("codex");
    let theirs = s.install_dir().display().to_string();
    s.set_user_path(&theirs);
    let out = s.run(&["setup", "--client", "codex"]);
    assert!(out.status.success(), "setup failed\n{}", said(&out));
    assert!(!stdout(&out).contains("to your PATH"), "{}", said(&out));
    assert!(s.receipt().get("path_added").is_none(), "{}", s.receipt());
    let out = s.uninstall();
    assert!(out.status.success(), "uninstall failed\n{}", said(&out));
    assert_eq!(
        s.user_path(),
        Some((theirs, Type::ExpandString)),
        "uninstall removed the user's own entry\n{}",
        said(&out)
    );
}

/// Windows renames a running program but never replaces it: an upgrade
/// while a session runs `serve` sets the earlier binary aside, and the
/// next setup deletes it once nothing runs it.
#[test]
fn an_upgrade_while_a_session_runs_sets_the_old_binary_aside() {
    let s = Sandbox::new();
    s.client("codex");
    let out = s.run(&["setup", "--client", "codex"]);
    assert!(out.status.success(), "setup failed\n{}", said(&out));
    let mut serve: Child = s
        .installed(&["serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // The image is mapped once the process exists; a moment lets it settle.
    std::thread::sleep(Duration::from_millis(500));
    let upgrade = s.run(&["setup", "--client", "codex"]);
    let aside = s.set_aside();
    let _ = serve.kill();
    let _ = serve.wait();
    assert!(
        upgrade.status.success(),
        "setup over a running binary failed\n{}",
        said(&upgrade)
    );
    assert!(s.binary().is_file(), "no binary in place after the upgrade");
    assert_eq!(
        aside.len(),
        1,
        "expected the running binary set aside once, found {aside:?}\n{}",
        said(&upgrade)
    );
    let later = s.run(&["setup", "--client", "codex"]);
    assert!(later.status.success(), "setup failed\n{}", said(&later));
    assert_eq!(
        s.set_aside(),
        Vec::<String>::new(),
        "the set-aside binary outlived the session that ran it\n{}",
        said(&later)
    );
}

/// After setup on Windows, doctor finds nothing to do: the private store,
/// the skill, the hooks Codex trusts, its registration.
#[test]
fn doctor_finds_nothing_to_do_after_setup() {
    let s = Sandbox::new();
    s.client("codex");
    let out = s.run(&["setup", "--client", "codex"]);
    assert!(out.status.success(), "setup failed\n{}", said(&out));
    let doctor = s.run(&["doctor", "--client", "codex"]);
    assert_eq!(
        doctor.status.code(),
        Some(0),
        "doctor wants something\n{}",
        said(&doctor)
    );
    for line in ["Store privacy: OK\n", "Skill: OK\n"] {
        assert!(
            stdout(&doctor).contains(line),
            "doctor did not say {line:?}\n{}",
            said(&doctor)
        );
    }
}
