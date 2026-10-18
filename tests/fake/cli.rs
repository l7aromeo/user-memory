//! One fake for every command-line program the tests stand in for, on every
//! platform. A test copies this binary in under the program's name, such as
//! `cli/codex` or `cli\codex.exe`, and the file stem picks what it does.
//! Each fake keeps its state in files under `$HOME`, as the shell scripts it
//! replaces did, so it works the same whether a test runs it directly or
//! through `install.sh` with a cleared environment.
//!
//! A file `<stem>.exit` beside the copy makes it exit at once with the code
//! the file holds: a program that is there but fails.

#![allow(
    clippy::unwrap_used,
    reason = "a test fake: a failed unwrap fails the test that ran it"
)]

use std::{
    env, fs,
    io::{self, BufRead, Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    thread,
    time::Duration,
};

fn main() -> ExitCode {
    let exe = env::current_exe().unwrap();
    let stem = exe
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Ok(code) = fs::read_to_string(exe.with_file_name(format!("{stem}.exit"))) {
        return ExitCode::from(code.trim().parse::<u8>().unwrap());
    }
    let args: Vec<String> = env::args().skip(1).collect();
    let code = match stem.as_str() {
        "claude" | "codex" => agent(&stem, &args),
        "docker" => docker(&args),
        "brew" => brew(&args),
        "ollama" => ollama(&args),
        "uname" => uname(&args),
        "gh" => gh(&exe.with_file_name("gh.calls"), &args),
        "cargo" => cargo(&args),
        "cargo-about" => 0,
        "rustc" => say("host: aarch64-apple-darwin"),
        "rustup" => say("aarch64-apple-darwin"),
        other => {
            eprintln!("user-memory-fake: no fake for {other:?}");
            1
        }
    };
    ExitCode::from(code)
}

/// The fake's state folder: the test's temporary HOME.
fn home() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap()
}

fn at(name: &str) -> PathBuf {
    home().join(name)
}

fn say(line: &str) -> u8 {
    println!("{line}");
    0
}

/// `$*`: the arguments joined by single spaces.
fn joined(args: &[String]) -> String {
    args.join(" ")
}

/// Appends `$*` as one line to `log`.
fn log(log: &Path, args: &[String]) {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .unwrap();
    writeln!(file, "{}", joined(args)).unwrap();
}

/// `printf '%s\n' "$@"`: each argument on its own line.
fn lines(args: &[String]) -> String {
    args.iter().map(|arg| format!("{arg}\n")).collect()
}

/// The arguments' `--env K=V` value and the binary after `--`, as `mcp add`
/// gives them.
fn env_and_binary(args: &[String]) -> (String, String) {
    let (mut env, mut binary) = (String::new(), String::new());
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--env" => env = rest.next().cloned().unwrap_or_default(),
            "--" => {
                binary = rest.next().cloned().unwrap_or_default();
                break;
            }
            _ => {}
        }
    }
    (env, binary)
}

/// Writes the user entry `mcp add --scope user` makes, to `$HOME/.claude.json`,
/// as Claude Code does.
fn write_user_entry(args: &[String]) {
    let (env, binary) = env_and_binary(args);
    let (key, value) = env.split_once('=').unwrap_or((env.as_str(), env.as_str()));
    let entry = serde_json::json!({"mcpServers":{"user-memory":{
        "type":"stdio","command":binary,"args":["serve"],"env":{key:value}}}});
    fs::write(at(".claude.json"), format!("{entry}\n")).unwrap();
}

/// `claude` and `codex`. `--version` prints `<name>.version` as written,
/// else a version above what setup requires. `codex app-server` answers the
/// hooks list with `FAKE_CODEX_TRUST` or beside `codex.fixture`, and fails
/// otherwise. `mcp` commands follow one of the tests' arrangements, by the
/// files under `$HOME`:
/// - `<name>.override`: a project- or local-scope entry that overrides the
///   shared server: `get` shows it, `add` writes the user entry to
///   `.claude.json`, `remove` deletes it;
/// - `<name>.fixture`: `add` records its arguments in `<name>.args` and
///   installs the fixture as `<name>.config` (refusing a second add, or
///   failing when `<name>.refuse` exists), `get` shows it (Codex only with
///   `--json`, Claude only without), `remove` records `<name>.removed`;
/// - `fixture.json`: `add` installs it as `config.json`, `get` shows it;
/// - otherwise Codex builds `codex.json` from what `add` gives, `get` shows
///   it (another binary once the hooks were listed with `FAKE_CODEX_DRIFT`).
fn agent(name: &str, args: &[String]) -> u8 {
    if args.first().is_some_and(|arg| arg == "--version") {
        let version = fs::read(at(&format!("{name}.version"))).unwrap_or_else(|_| {
            let default = if name == "claude" {
                "2.1.200 (Claude Code)"
            } else {
                "codex-cli 0.130.0"
            };
            default.into()
        });
        let mut out = io::stdout();
        out.write_all(&version).unwrap();
        out.write_all(b"\n").unwrap();
        return 0;
    }
    if name == "codex" && args.first().is_some_and(|arg| arg == "app-server") {
        // Only the arrangements whose Codex has an app server answer.
        let trusting = env::var("FAKE_CODEX_TRUST").is_ok_and(|trust| !trust.is_empty());
        if at("codex.fixture").is_file() || trusting {
            return app_server();
        }
        return 1;
    }
    let command = args.get(1).map_or("", String::as_str);
    if at(&format!("{name}.override")).is_file() {
        match command {
            "get" => {
                print!(
                    "{}",
                    fs::read_to_string(at(&format!("{name}.override"))).unwrap()
                );
                return 0;
            }
            "add" => {
                fs::write(at(&format!("{name}.args")), lines(args)).unwrap();
                write_user_entry(args);
                return 0;
            }
            "remove" => {
                fs::write(at(&format!("{name}.removed")), lines(args)).unwrap();
                let _ = fs::remove_file(at(".claude.json"));
                return 0;
            }
            _ => {}
        }
    }
    let config = at(&format!("{name}.config"));
    if at(&format!("{name}.fixture")).is_file() {
        return match command {
            "remove" => {
                fs::write(at(&format!("{name}.removed")), lines(args)).unwrap();
                let _ = fs::remove_file(&config);
                0
            }
            "add" => {
                if config.is_file() {
                    eprintln!("already exists");
                    return 7;
                }
                fs::write(at(&format!("{name}.args")), lines(args)).unwrap();
                if at(&format!("{name}.refuse")).is_file() {
                    eprintln!("registration refused");
                    return 9;
                }
                fs::copy(at(&format!("{name}.fixture")), &config).unwrap();
                0
            }
            "get" => {
                // Codex reports configuration as JSON only with --json;
                // Claude has no such flag.
                let flag = args.get(3).map_or("", String::as_str);
                let flag_ok = if name == "codex" {
                    flag == "--json"
                } else {
                    flag.is_empty()
                };
                if !flag_ok {
                    return 2;
                }
                show(&config)
            }
            _ => 1,
        };
    }
    if at("fixture.json").is_file() {
        return match command {
            "add" => {
                fs::copy(at("fixture.json"), at("config.json")).unwrap();
                0
            }
            "get" => show(&at("config.json")),
            _ => 1,
        };
    }
    if name != "codex" {
        return 1;
    }
    let saved = at("codex.json");
    match command {
        "add" => {
            // mcp add user-memory --env K=V … -- BINARY serve
            let mut env = serde_json::Map::new();
            let mut rest = args.iter().skip(3);
            let mut binary = String::new();
            while let Some(arg) = rest.next() {
                if arg == "--" {
                    binary = rest.next().cloned().unwrap_or_default();
                    break;
                }
                let pair = rest.next().cloned().unwrap_or_default();
                let (key, value) = pair.split_once('=').unwrap_or((pair.as_str(), ""));
                env.insert(key.to_owned(), value.into());
            }
            let entry = serde_json::json!({"enabled":true,"transport":{
                "type":"stdio","command":binary,"args":["serve"],"env":env}});
            fs::write(&saved, format!("{entry}\n")).unwrap();
            0
        }
        "remove" => {
            if !saved.is_file() {
                return 1;
            }
            fs::remove_file(&saved).unwrap();
            0
        }
        "get" => {
            if !saved.is_file() {
                return 1;
            }
            let mut entry: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&saved).unwrap()).unwrap();
            if at("codex-drifted").is_file() {
                entry["transport"]["command"] = "/elsewhere/user-memory".into();
            }
            println!("{entry}");
            0
        }
        _ => 1,
    }
}

/// Prints `file`, or fails as `get` does for a server not registered.
fn show(file: &Path) -> u8 {
    match fs::read_to_string(file) {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err(_) => 1,
    }
}

/// `codex app-server`: answers the hooks list on stdin. With
/// `FAKE_CODEX_TRUST` it lists the hooks in `$CODEX_HOME/hooks.json` with
/// that trust status, as Codex does before the user approves them, and with
/// `FAKE_CODEX_DRIFT` too it marks `codex-drifted`, so a later `mcp get`
/// reports another binary. Otherwise it answers with `$HOME/codex-hooks.json`,
/// or "method not found" as an older Codex does. While `$HOME/codex-hang`
/// exists it answers nothing.
fn app_server() -> u8 {
    while at("codex-hang").exists() {
        thread::sleep(Duration::from_millis(100));
    }
    let trust = env::var("FAKE_CODEX_TRUST").ok().filter(|t| !t.is_empty());
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let out = io::stdout();
        let mut out = out.lock();
        if let Some(trust) = &trust {
            if line.contains("\"id\":1") {
                if env::var("FAKE_CODEX_DRIFT").is_ok_and(|d| !d.is_empty()) {
                    fs::write(at("codex-drifted"), "").unwrap();
                }
                let file = PathBuf::from(env::var_os("CODEX_HOME").unwrap()).join("hooks.json");
                let mut hooks = Vec::new();
                let listed: serde_json::Value =
                    serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
                commands(&listed, &mut hooks);
                let hooks: Vec<_> = hooks
                    .into_iter()
                    .map(|command| {
                        serde_json::json!({"command":command,
                        "sourcePath":file,"trustStatus":trust})
                    })
                    .collect();
                let reply = serde_json::json!({"id":1,"result":{"data":[{"hooks":hooks}]}});
                writeln!(out, "{reply}").unwrap();
                return 0;
            }
            continue;
        }
        if line.contains("\"id\":0") {
            writeln!(out, "{{\"id\":0,\"result\":{{}}}}").unwrap();
        } else if line.contains("\"hooks/list\"") {
            match fs::read_to_string(at("codex-hooks.json")) {
                Ok(reply) => write!(out, "{reply}").unwrap(),
                Err(_) => writeln!(
                    out,
                    "{{\"id\":1,\"error\":{{\"code\":-32601,\"message\":\"Method not found\"}}}}"
                )
                .unwrap(),
            }
        }
        out.flush().unwrap();
    }
    0
}

/// Every `command` string in `value`, in document order.
fn commands(value: &serde_json::Value, found: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                match (key.as_str(), value) {
                    ("command", serde_json::Value::String(command)) => found.push(command.clone()),
                    _ => commands(value, found),
                }
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|item| commands(item, found)),
        _ => {}
    }
}

/// A Docker CLI that records every call in `$HOME/docker.log`. `compose up`
/// starts a real `user-memory host` (`$FAKE_HOST`) on the compose file's
/// port, `compose stop` stops it, `compose cp` copies its token out, and a
/// one-off container's shell runs with the store folder `$HOME/server` as
/// `/data`. `$HOME/docker-down` makes `docker info` fail; `$HOME/docker-ps`
/// is what `docker ps` prints.
fn docker(args: &[String]) -> u8 {
    log(&at("docker.log"), args);
    match args.first().map_or("", String::as_str) {
        "info" => {
            if at("docker-down").exists() {
                return 1;
            }
            if joined(args).contains("Architecture") {
                println!("aarch64");
            }
            0
        }
        "ps" => {
            if let Ok(listed) = fs::read_to_string(at("docker-ps")) {
                print!("{listed}");
            }
            0
        }
        // The default bridge's gateway, as `docker network inspect bridge` shows it.
        "network" => say("172.17.0.1"),
        "compose" => compose(args),
        _ => 0,
    }
}

fn compose(args: &[String]) -> u8 {
    let file = args.get(2).cloned().unwrap_or_default();
    let all = format!(" {} ", joined(args));
    let last = args.last().cloned().unwrap_or_default();
    let server = at("server");
    let host = || PathBuf::from(env::var_os("FAKE_HOST").unwrap());
    if all.contains(" embedding sync ") {
        // The container cannot reach the endpoint.
        if at("sync-fails").exists() {
            eprintln!("The embedding endpoint did not answer the probe");
            return 1;
        }
    } else if all.contains(" --entrypoint sh memory -c ") {
        // A one-off container's shell, with the store folder as /data. The
        // script's own exit status is ignored, as the shell fake's was.
        fs::create_dir_all(&server).unwrap();
        // sh reads `\` as an escape; Git for Windows' sh takes `C:/…` paths.
        let data = server.display().to_string().replace('\\', "/");
        let script = last.replace("/data/", &format!("{data}/"));
        let Some(sh) = posix_shell() else {
            eprintln!(
                "user-memory-fake: no POSIX sh for the container's shell; on Windows install Git for Windows"
            );
            return 1;
        };
        let _ = Command::new(sh).arg("-c").arg(script).status();
    } else if all.contains(" up ") {
        // Recreated: the running server stops first.
        if stop_server() {
            thread::sleep(Duration::from_millis(300));
        }
        let Some(port) = compose_port(&fs::read_to_string(&file).unwrap()) else {
            return 1;
        };
        fs::create_dir_all(&server).unwrap();
        let output = fs::File::create(at("server.log")).unwrap();
        // Left running, as the shell's `&` did: it is the server until the
        // test stops it with `compose stop` or `down`.
        #[allow(
            clippy::zombie_processes,
            reason = "the server outlives this fake on purpose"
        )]
        let child = Command::new(host())
            .args(["host", "--listen", &format!("127.0.0.1:{port}")])
            .arg("--db")
            .arg(server.join("memories.sqlite3"))
            .arg("--token-file")
            .arg(server.join("access-token"))
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap();
        fs::write(at("server.pid"), format!("{}\n", child.id())).unwrap();
        if !answers(port) {
            return 1;
        }
    } else if all.contains(" stop ") {
        stop_server();
        // A memory recorded between the check and the stop.
        if at("changed-at-stop").exists() {
            fs::write(at("migrate-fails"), "").unwrap();
        }
        // Docker failing once the server is stopped.
        if at("broken-at-stop").exists() {
            fs::write(at("docker-broken"), "").unwrap();
        }
        thread::sleep(Duration::from_millis(300));
    } else if all.contains(" migrate ") {
        if at("docker-broken").exists() {
            eprintln!("Error response from daemon: cannot start container");
            return 1;
        }
        if all.contains(" --titles - ") {
            let mut titles = Vec::new();
            io::stdin().read_to_end(&mut titles).unwrap();
            fs::write(at("migrate-stdin"), titles).unwrap();
        }
        if !all.contains(" --check ") && at("migrate-breaks").exists() {
            eprintln!("disk full");
            return 1;
        }
        if at("migrate-fails").exists() {
            eprintln!("Migrating to schema version 3 needs a title for every memory");
            return 1;
        }
    } else if all.contains(" cp memory:/data/access-token ") {
        fs::copy(server.join("access-token"), &last).unwrap();
    } else if all.contains(" down ") {
        stop_server();
        // The volume is the fake server's store folder.
        if all.contains(" -v ") {
            let _ = fs::remove_dir_all(&server);
        }
    } else if all
        .find(" exec ")
        .is_some_and(|at| all[at..].contains(" doctor --server-only"))
    {
        // The store's checks, as the release inside the container runs them.
        let status = Command::new(host())
            .args(["doctor", "--server-only"])
            .env("USER_MEMORY_DB", server.join("memories.sqlite3"))
            .status()
            .unwrap();
        return status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .unwrap_or(1);
    } else if all.contains(" export - ") {
        let _ = Command::new(host())
            .args(["export", "-"])
            .env("USER_MEMORY_DB", server.join("memories.sqlite3"))
            .status();
    }
    0
}

/// The POSIX shell that stands in for the container's: `sh` on PATH, else on
/// Windows the one Git for Windows installs (`%ProgramFiles%\Git\bin\sh.exe`),
/// which GitHub's Windows images have.
fn posix_shell() -> Option<PathBuf> {
    let name = format!("sh{}", env::consts::EXE_SUFFIX);
    let on_path = env::var_os("PATH")
        .into_iter()
        .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .map(|dir| dir.join(&name))
        .find(|candidate| candidate.is_file());
    on_path.or_else(|| {
        let git = PathBuf::from(env::var_os("ProgramFiles")?).join(r"Git\bin\sh.exe");
        (cfg!(windows) && git.is_file()).then_some(git)
    })
}

/// The host port of the compose file's `- "127.0.0.1:<port>:8765"` line.
fn compose_port(compose: &str) -> Option<u16> {
    compose.lines().find_map(|line| {
        line.trim_start()
            .strip_prefix("- \"127.0.0.1:")?
            .strip_suffix(":8765\"")?
            .parse()
            .ok()
    })
}

/// Stops the server `compose up` started; whether one was running.
fn stop_server() -> bool {
    let Ok(pid) = fs::read_to_string(at("server.pid")) else {
        return false;
    };
    let pid = pid.trim();
    let mut kill = if cfg!(windows) {
        let mut kill = Command::new("taskkill");
        kill.args(["/PID", pid, "/F"]);
        kill
    } else {
        let mut kill = Command::new("kill");
        kill.arg(pid);
        kill
    };
    kill.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Whether a server answers HTTP on `port` within 10 seconds, as
/// `curl -s -o /dev/null http://127.0.0.1:<port>/` would find.
fn answers(port: u16) -> bool {
    for _ in 0..100 {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            let asked =
                stream.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
            let mut first = [0; 5];
            if asked.is_ok() && stream.read_exact(&mut first).is_ok() && &first == b"HTTP/" {
                return true;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Homebrew: logs each command; `brew services start ollama` starts the
/// fake Ollama by creating `$HOME/ollama-up`.
fn brew(args: &[String]) -> u8 {
    log(&at("brew.log"), args);
    if joined(args) == "services start ollama" {
        fs::write(at("ollama-up"), "").unwrap();
    }
    0
}

/// The Ollama command: logs each command; `ollama pull NAME` asks the API at
/// `OLLAMA_HOST`, as the real one does, and fails as `curl -f` does when the
/// API refuses.
fn ollama(args: &[String]) -> u8 {
    log(&at("ollama.log"), args);
    if args.first().is_none_or(|arg| arg != "pull") {
        return 0;
    }
    let body = serde_json::json!({"model": args.get(1).cloned().unwrap_or_default()}).to_string();
    let base = env::var("OLLAMA_HOST").unwrap_or_default();
    match post(&base, "/api/pull", &body) {
        Ok(status) if status < 400 => 0,
        Ok(status) => {
            eprintln!("curl: (22) The requested URL returned error: {status}");
            22
        }
        Err(err) => {
            eprintln!("curl: (7) Failed to connect: {err}");
            7
        }
    }
}

/// POSTs `body` to `path` on the `http://host:port` server `base` names;
/// its status code.
fn post(base: &str, path: &str, body: &str) -> io::Result<u16> {
    let address = base
        .strip_prefix("http://")
        .unwrap_or(base)
        .trim_end_matches('/');
    let mut stream = TcpStream::connect(address)?;
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    reply
        .split_whitespace()
        .nth(1)
        .and_then(|status| status.parse().ok())
        .ok_or_else(|| io::Error::other(format!("no HTTP status in {reply:?}")))
}

/// `uname` that names `$FAKE_UNAME_S` as the system and asks the real one
/// for everything else.
fn uname(args: &[String]) -> u8 {
    if args.first().is_some_and(|arg| arg == "-s") {
        return say(&env::var("FAKE_UNAME_S").unwrap_or_default());
    }
    let status = Command::new("/usr/bin/uname").args(args).status().unwrap();
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1)
}

/// GitHub's CLI, which scripts/client-release never calls: every call is
/// logged to `gh.calls` beside this copy and fails, so a test sees any.
fn gh(calls: &Path, args: &[String]) -> u8 {
    log(calls, args);
    1
}

/// The Rust toolchain for scripts/client-release: `cargo build` writes a
/// binary holding `$FAKE_BINARY_TEXT`, `cargo test` passes (or prints
/// `$FAKE_TEST_FAILURE` and fails), and `cargo about` writes the notices.
fn cargo(args: &[String]) -> u8 {
    let after = |flag: &str| {
        args.iter()
            .position(|arg| arg == flag)
            .and_then(|at| args.get(at + 1))
    };
    match args.first().map_or("", String::as_str) {
        "test" => match env::var("FAKE_TEST_FAILURE").ok().filter(|f| !f.is_empty()) {
            Some(failure) => {
                println!("{failure}");
                101
            }
            None => 0,
        },
        "about" => {
            if let Some(out) = after("-o") {
                fs::write(out, "notices\n").unwrap();
            }
            0
        }
        "build" => {
            let target = after("--target").cloned().unwrap_or_default();
            let dir = PathBuf::from(env::var_os("CARGO_TARGET_DIR").unwrap())
                .join(target)
                .join("release");
            fs::create_dir_all(&dir).unwrap();
            let text = env::var("FAKE_BINARY_TEXT")
                .ok()
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| "binary".to_owned());
            fs::write(dir.join("user-memory"), text).unwrap();
            0
        }
        _ => 2,
    }
}
