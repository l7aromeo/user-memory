//! `serve` hands itself over to a replaced binary in place, so open sessions
//! run the new code after an upgrade without a restart.
//!
//! The app holds `serve`'s stdin and stdout for the whole session and never
//! reconnects a stdio server that exits, so `serve` reads its input itself,
//! one message per line, in front of the server (this binary's own, or the
//! bridge to a memory server), and before each message checks whether the
//! binary it runs from was replaced. When it was and no request is in
//! flight, it starts the new binary in its place with the session: on Unix
//! by `exec`, which keeps the process, its stdin and its stdout; on Windows,
//! which has no `exec`, as a child it relays to and restarts on the next
//! update, so there is never a relay of a relay. The new binary replays the
//! app's handshake to its server under a private id, keeps that reply from
//! the app, and goes on with the bytes the old one had read but not handled.
//! The front answers a 2026-07-28 app's `subscriptions/listen` stream for
//! tool list changes itself, so the stream outlives the server process and
//! carries the change a hand-over brings.

use crate::setup::versions::{self, Version};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    env, fs, io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// Carries the session to a new binary: the path of its state file, or
/// [`PROBE`]. Only `serve` reads it.
pub const ENV: &str = "USER_MEMORY_HANDOVER";
/// [`ENV`]'s value that asks a binary which sessions it can take over.
const PROBE: &str = "probe";
/// The state file's format; a binary's probe lists the ones it reads.
const FORMAT: u64 = 1;
/// How long a replacement has to answer `--version` or the probe. Either
/// answers in milliseconds; the rest covers macOS checking a new executable
/// on its first start on a busy machine.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a relay waits for its child to end once its input is closed,
/// before it kills it: a child with no request in flight ends at once.
const CHILD_STOP_WAIT: Duration = Duration::from_secs(5);
/// The ids of the requests the front sends its server itself; their replies
/// never reach the app.
const REPLAY_ID: &str = "user-memory-handover-handshake";
const TOOLS_ID: &str = "user-memory-handover-tools";
/// The in-memory pipe between the front and a server in this process: one
/// stdio read's worth, as the pipe from the app holds.
pub(crate) const PIPE_BUFFER: usize = 64 * 1024;
/// How much the reader takes from stdin in one read.
const READ_CHUNK: usize = 64 * 1024;
/// In debug builds, set to 1, it makes `serve` hand over by relaying on
/// Unix too, so the Windows path runs in every test run.
#[cfg(debug_assertions)]
const TEST_RELAY: &str = "USER_MEMORY_TEST_RELAY";
/// In debug builds, set to 1, it makes the `exec` or the relay's start
/// fail after a replacement passed its checks, for the tests of a failed
/// hand-over.
#[cfg(debug_assertions)]
const TEST_EXEC_FAIL: &str = "USER_MEMORY_TEST_EXEC_FAIL";
/// In debug builds, set, it makes `serve` write to stderr, at each
/// hand-over decision, the build serving, the file on disk and the
/// requests in flight, so a test that sees no hand-over says why.
#[cfg(debug_assertions)]
const TEST_TRACE: &str = "USER_MEMORY_TEST_TRACE";
/// How many times a replacement whose check was ended by a signal is
/// checked again, at later messages, before it is refused. macOS can kill a
/// new executable while it assesses it on a loaded machine
/// (docs/dev/development.md); the next start runs. After an `exec` there is
/// no old process to fall back to, so only a check that ran counts.
const KILLED_CHECKS: u32 = 3;
/// How a hand-over state file's name starts and ends:
/// `handover-<pid>-<nanoseconds>.json`.
const STATE_PREFIX: &str = "handover-";
const STATE_SUFFIX: &str = ".json";
/// How old a hand-over state file is when a starting `serve` removes it as
/// left behind. The new process reads its file as it starts, within
/// seconds even on a machine checking a new executable under load, so ten
/// minutes never catches a hand-over in progress.
const STALE_STATE: Duration = Duration::from_secs(10 * 60);
/// The `_meta` key that names a 2026-07-28 request's protocol version
/// (specification 2026-07-28, basic/index, "`_meta`"; server/discover).
const META_PROTOCOL: &str = "io.modelcontextprotocol/protocolVersion";
/// The `_meta` key that ties a notification to the `subscriptions/listen`
/// request whose stream it is on; its value is that request's id
/// (specification 2026-07-28, basic/patterns/subscriptions, "Receiving
/// Notifications").
const META_SUBSCRIPTION: &str = "io.modelcontextprotocol/subscriptionId";
/// The notification kinds a 2026-07-28 `subscriptions/listen` filter
/// defines (specification 2026-07-28, basic/patterns/subscriptions,
/// "Notification Filter").
const LISTEN_KINDS: [&str; 4] = [
    "toolsListChanged",
    "promptsListChanged",
    "resourcesListChanged",
    "resourceSubscriptions",
];

/// The server behind the front.
#[derive(Clone)]
pub enum Inner {
    /// This binary's own server, on the store at this path.
    Local(PathBuf),
    /// The bridge to a memory server.
    Bridge(crate::remote::Connection),
}

/// What a new binary needs to continue the session without a new handshake.
#[derive(Clone, Serialize, Deserialize)]
struct Session {
    format: u64,
    /// The binary the session runs from, which an upgrade replaces.
    binary: PathBuf,
    /// The version that handed the session over.
    from: String,
    /// The app's `initialize` (or `server/discover`) request.
    handshake: Option<Value>,
    /// The app's `notifications/initialized`.
    initialized: Option<Value>,
    /// The protocol version the server agreed with the app.
    protocol: Option<String>,
    /// The tools the app was last given, to tell whether they changed.
    tools: Option<Value>,
    /// Started by a relay, which hands over itself: this process never does.
    relayed: bool,
    /// For a bridge, what its server's handshake said about versions, so a
    /// re-handshake that says something new is told once.
    #[serde(default)]
    server_line: Option<String>,
    /// The parameters of the app's last `tools/list`. A 2026-07-28 request
    /// must carry its protocol version and client in `_meta`, so the new
    /// server is asked for its tools with the app's own.
    #[serde(default)]
    tools_params: Option<Value>,
    /// The ids of the app's open `subscriptions/listen` streams for tool
    /// list changes. The front answers them itself, so they outlive the
    /// server process across a hand-over.
    #[serde(default)]
    listens: Vec<Value>,
    /// The identity of the binary the old process checked and started, so
    /// the new code's baseline is that build even when another upgrade
    /// replaced the file before the new code first looked at it.
    #[serde(default)]
    identity: Option<Identity>,
}

/// What a binary's probe answers: its version, the state formats it reads
/// and the protocol versions its server speaks.
pub fn probe_answer() -> Value {
    json!({"version": versions::running(), "formats": [FORMAT], "protocols": protocols()})
}

/// The protocol versions this binary's server speaks.
fn protocols() -> Vec<String> {
    let server = crate::UserMemoryServer {
        db: PathBuf::new(),
        in_client_folder: true,
    };
    rmcp::ServerHandler::supported_protocol_versions(&server)
        .iter()
        .map(ToString::to_string)
        .collect()
}

/// Whether this process was asked for its probe answer only.
pub fn probing() -> bool {
    env::var(ENV).is_ok_and(|value| value == PROBE)
}

/// A file's identity: its file id (device and inode on Unix, volume and
/// file index on Windows) and size. An upgrade replaces the binary by
/// rename, which changes the id whatever the clock says, so the modified
/// time is never compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    file: (u64, u64),
    size: u64,
}

fn identity(path: &Path) -> io::Result<Identity> {
    let meta = fs::metadata(path)?;
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    };
    #[cfg(windows)]
    let file = crate::platform::windows::file_id(path)?;
    Ok(Identity {
        file,
        size: meta.len(),
    })
}

/// Reads stdin on a thread of its own, one read at a time behind `gate`.
/// On Unix the thread waits with `poll` while it holds nothing, and reads
/// only once input is there, so whoever holds the gate knows no read is
/// under way: every byte read is in `chunks`, and an `exec` loses none.
struct Reader {
    /// What each read took; an empty chunk is the end of the input.
    chunks: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    #[cfg_attr(not(unix), allow(dead_code, reason = "only exec takes the gate"))]
    gate: Arc<Mutex<()>>,
}

impl Reader {
    fn start() -> Self {
        let (sender, chunks) = tokio::sync::mpsc::unbounded_channel();
        let gate = Arc::new(Mutex::new(()));
        let held = gate.clone();
        std::thread::spawn(move || read_stdin(&held, &sender));
        Self { chunks, gate }
    }
}

fn read_stdin(gate: &Mutex<()>, sender: &tokio::sync::mpsc::UnboundedSender<Vec<u8>>) {
    let stdin = io::stdin();
    let mut buffer = vec![0; READ_CHUNK];
    loop {
        #[cfg(unix)]
        if wait_readable(&stdin).is_err() {
            let _ = sender.send(Vec::new());
            return;
        }
        let guard = gate.lock().unwrap_or_else(PoisonError::into_inner);
        let chunk = match read_once(&stdin, &mut buffer) {
            Ok(read) => buffer[..read].to_vec(),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => Vec::new(),
        };
        let end = chunk.is_empty();
        // Sent before the gate opens: its holder finds every byte read.
        if sender.send(chunk).is_err() || end {
            return;
        }
        drop(guard);
    }
}

/// Waits until stdin has input or has ended, reading nothing.
#[cfg(unix)]
fn wait_readable(stdin: &io::Stdin) -> io::Result<()> {
    use std::os::fd::AsFd;
    let fd = stdin.as_fd();
    loop {
        let mut fds = [rustix::event::PollFd::new(
            &fd,
            rustix::event::PollFlags::IN,
        )];
        match rustix::event::poll(&mut fds, None) {
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => {}
            Err(err) => return Err(err.into()),
        }
    }
}

/// One read of stdin, straight from the file: std's own stdin buffers
/// ahead, and what it buffered would be lost at `exec`.
#[cfg(unix)]
fn read_once(stdin: &io::Stdin, buffer: &mut [u8]) -> io::Result<usize> {
    use std::os::fd::AsFd;
    Ok(rustix::io::read(stdin.as_fd(), buffer)?)
}

#[cfg(not(unix))]
fn read_once(stdin: &io::Stdin, buffer: &mut [u8]) -> io::Result<usize> {
    use std::io::Read;
    stdin.lock().read(buffer)
}

type Output = tokio::io::Lines<BufReader<Box<dyn AsyncRead + Send + Unpin>>>;

/// The server the front passes messages to.
struct Backend {
    /// Messages for the server, written by [`feed`]'s task so the front
    /// never waits on a full pipe while the server waits on its output;
    /// `None` once closed.
    input: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    writer: tokio::task::JoinHandle<()>,
    output: Output,
    running: Running,
}

/// Writes what the front sends to `input` in order, and closes it when the
/// front closes the channel.
fn feed(
    mut input: impl AsyncWrite + Send + Unpin + 'static,
) -> (
    tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    tokio::task::JoinHandle<()>,
) {
    let (sender, mut messages) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let writer = tokio::spawn(async move {
        while let Some(message) = messages.recv().await {
            if input.write_all(&message).await.is_err() || input.flush().await.is_err() {
                // The server stopped reading: it has ended, and the front
                // sees its output end.
                return;
            }
        }
        let _ = input.shutdown().await;
    });
    (sender, writer)
}

enum Running {
    /// The server runs in this process.
    Here(tokio::task::JoinHandle<Result<()>>),
    /// A relay's child: the replaced binary serving the session. Boxed:
    /// a child holds its process and pipe handles, 272 bytes on Windows.
    Child(Box<tokio::process::Child>),
}

fn start_here(inner: &Inner) -> Backend {
    let (front, server) = tokio::io::duplex(PIPE_BUFFER);
    let (server_read, server_write) = tokio::io::split(server);
    let task = match inner.clone() {
        Inner::Local(db) => tokio::spawn(crate::serve_here(db, server_read, server_write)),
        Inner::Bridge(connection) => tokio::spawn(crate::remote::connect_over(
            connection,
            server_read,
            server_write,
            true,
        )),
    };
    let (read, write) = tokio::io::split(front);
    let read: Box<dyn AsyncRead + Send + Unpin> = Box::new(read);
    let (input, writer) = feed(write);
    Backend {
        input: Some(input),
        writer,
        output: BufReader::new(read).lines(),
        running: Running::Here(task),
    }
}

fn start_child(binary: &Path, state: &Path) -> io::Result<Backend> {
    let mut child = tokio::process::Command::new(binary)
        .arg("serve")
        .env(ENV, state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let input = child
        .stdin
        .take()
        .context("no child input")
        .map_err(io::Error::other)?;
    let output = child
        .stdout
        .take()
        .context("no child output")
        .map_err(io::Error::other)?;
    let output: Box<dyn AsyncRead + Send + Unpin> = Box::new(output);
    let (input, writer) = feed(input);
    Ok(Backend {
        input: Some(input),
        writer,
        output: BufReader::new(output).lines(),
        running: Running::Child(Box::new(child)),
    })
}

/// The key of a JSON-RPC id, which may be a number or a string.
fn key(id: &Value) -> String {
    id.to_string()
}

/// Why a replaced binary can't take over the session, as the next tool
/// reply says once.
fn refusal(why: &Refused) -> String {
    let running = versions::running();
    match why {
        Refused::Older(new) => format!(
            "user-memory {new} replaced the binary this session runs ({running}), but it is older, so this session keeps {running}; restart your client to run {new}."
        ),
        Refused::Protocol(new, protocol) => format!(
            "user-memory {new} replaced the binary this session runs ({running}), but it can't continue this session's protocol ({protocol}), so this session keeps {running}; restart your client to run {new}."
        ),
        Refused::Broken(err) | Refused::Killed(err) => format!(
            "user-memory was replaced on disk, but the new binary can't take over this session ({err}), so this session keeps {running}; run user-memory doctor, then restart your client."
        ),
    }
}

/// Why a replacement was refused.
#[derive(Debug)]
enum Refused {
    Older(Version),
    Protocol(Version, String),
    Broken(String),
    /// Its check was ended by a signal: perhaps the system refusing a new
    /// executable once, so it is checked again before it is refused.
    #[cfg_attr(not(unix), allow(dead_code, reason = "only Unix has signals"))]
    Killed(String),
}

/// What `binary` prints for `args`, run with `handover` as [`ENV`] or
/// without it.
async fn ask(binary: &Path, args: &[&str], handover: Option<&str>) -> Result<String, Refused> {
    let mut command = tokio::process::Command::new(binary);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    match handover {
        Some(value) => command.env(ENV, value),
        None => command.env_remove(ENV),
    };
    let output = tokio::time::timeout(PROBE_TIMEOUT, command.output())
        .await
        .map_err(|_| Refused::Broken("it did not answer".to_owned()))?
        .map_err(|err| Refused::Broken(format!("it did not start: {err}")))?;
    if !output.status.success() {
        let said = format!("it ended with {}", output.status);
        #[cfg(unix)]
        if std::os::unix::process::ExitStatusExt::signal(&output.status).is_some() {
            return Err(Refused::Killed(said));
        }
        return Err(Refused::Broken(said));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| Refused::Broken("its answer isn't text".to_owned()))
}

/// Checks that `binary` can take over a session agreed on `protocol`: it
/// runs, is not older than this one, reads this state format and speaks
/// the protocol. The same version in another file, such as a rebuild,
/// takes over; only a lower version is refused.
async fn check(binary: &Path, protocol: Option<&str>) -> Result<(), Refused> {
    let printed = ask(binary, &["--version"], None).await?;
    let new = versions::of_binary(&printed)
        .ok_or_else(|| Refused::Broken("it doesn't say its version".into()))?;
    if Version::parse(versions::running()).is_some_and(|running| new < running) {
        return Err(Refused::Older(new));
    }
    let answer = ask(binary, &["serve"], Some(PROBE)).await?;
    judge(new, &answer, protocol)
}

/// Whether a binary at version `new`, whose probe printed `answer`, can
/// take over a session agreed on `protocol`.
fn judge(new: Version, answer: &str, protocol: Option<&str>) -> Result<(), Refused> {
    let answer: Value = serde_json::from_str(answer.trim())
        .map_err(|_| Refused::Broken("it can't take over a session".into()))?;
    let lists = |field: &str, wanted: &Value| {
        answer[field]
            .as_array()
            .is_some_and(|list| list.contains(wanted))
    };
    if !lists("formats", &json!(FORMAT)) {
        return Err(Refused::Broken("it can't take over a session".into()));
    }
    if let Some(protocol) = protocol
        && !lists("protocols", &json!(protocol))
    {
        return Err(Refused::Protocol(new, protocol.to_owned()));
    }
    Ok(())
}

/// Writes `session` and the input not yet handled to a new private state
/// file, for the binary taking over.
fn write_state(session: &Session, pending: &[u8]) -> Result<PathBuf> {
    use std::io::Write;
    let dir = crate::paths::state_dir().context("No folder for user-memory's state")?;
    fs::create_dir_all(&dir).with_context(|| format!("Cannot create {}", dir.display()))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let path = dir.join(format!(
        "{STATE_PREFIX}{}-{nanos}{STATE_SUFFIX}",
        std::process::id()
    ));
    let mut file =
        crate::private::create_file(fs::OpenOptions::new().write(true).create_new(true), &path)
            .with_context(|| format!("Cannot create {}", path.display()))?;
    let written = (|| -> io::Result<()> {
        serde_json::to_writer(&mut file, session)?;
        file.write_all(b"\n")?;
        file.write_all(pending)?;
        file.sync_all()
    })();
    if let Err(err) = written {
        let _ = fs::remove_file(&path);
        return Err(err).with_context(|| format!("Cannot write {}", path.display()));
    }
    Ok(path)
}

/// The session handed to this process and the input it is to handle first,
/// read from the state file [`ENV`] names, which is deleted. `None` for a
/// new session.
fn take_session() -> Option<(Session, Vec<u8>)> {
    let path = PathBuf::from(env::var_os(ENV)?);
    if !is_state_file(&path) {
        // Never read or delete a file the hand-over didn't write.
        eprintln!(
            "user-memory: {} is not a hand-over file in user-memory's state folder; serving this session as a new one",
            path.display()
        );
        return None;
    }
    let read = fs::read(&path);
    let _ = fs::remove_file(&path);
    let parsed = read.map_err(|err| err.to_string()).and_then(|bytes| {
        let end = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or("it has no session line")?;
        let session: Session =
            serde_json::from_slice(&bytes[..end]).map_err(|err| err.to_string())?;
        if session.format != FORMAT {
            return Err(format!("format {} is not {FORMAT}", session.format));
        }
        Ok((session, bytes[end + 1..].to_vec()))
    });
    match parsed {
        Ok(taken) => Some(taken),
        Err(err) => {
            eprintln!(
                "user-memory: the session handed over in {} can't be read ({err}); serving it as a new one",
                Path::new(&path).display()
            );
            None
        }
    }
}

/// Whether `path` is a hand-over state file: a plain file in this user's
/// state folder, named as [`write_state`] names one.
fn is_state_file(path: &Path) -> bool {
    let named = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(STATE_PREFIX)?.strip_suffix(STATE_SUFFIX))
        .is_some_and(|ids| {
            let mut parts = ids.split('-');
            let number = |part: Option<&str>| {
                part.is_some_and(|part| {
                    !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())
                })
            };
            number(parts.next()) && number(parts.next()) && parts.next().is_none()
        });
    named
        && crate::paths::state_dir().is_some_and(|dir| path.parent() == Some(dir.as_path()))
        && fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file())
}

/// Removes the hand-over state files a process left behind when it died
/// between the hand-over and reading its file: each can hold input the
/// app sent, such as a memory being recorded.
fn remove_stale_state() {
    let Some(entries) = crate::paths::state_dir().and_then(|dir| fs::read_dir(dir).ok()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|written| written.elapsed().is_ok_and(|age| age > STALE_STATE));
        if stale && is_state_file(&path) {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Whether this platform hands over by relaying: Windows, which has no
/// `exec`, and a debug build told to by [`TEST_RELAY`].
fn relays() -> bool {
    #[cfg(debug_assertions)]
    if env::var(TEST_RELAY).is_ok_and(|value| value == "1") {
        return true;
    }
    cfg!(windows)
}

/// The binary to start in this one's place: `binary`, or in a debug build
/// told to by [`TEST_EXEC_FAIL`], one that isn't there.
fn target(binary: &Path) -> PathBuf {
    #[cfg(debug_assertions)]
    if env::var(TEST_EXEC_FAIL).is_ok_and(|value| value == "1") {
        return binary.with_file_name("missing-user-memory");
    }
    binary.to_path_buf()
}

/// Whether the front answers `message`, a `subscriptions/listen`, itself:
/// one on the 2026-07-28 protocol, with only the parameters and the
/// notification kinds that version defines, asking for tool list changes.
/// Anything else goes to the server, which answers as it always has
/// (method not found), so what the front does not fully know costs only
/// the reload: new tools then appear at the next session.
fn takes_listen(message: &Value) -> bool {
    let Some(params) = message.get("params").and_then(Value::as_object) else {
        return false;
    };
    let protocol = params
        .get("_meta")
        .and_then(|meta| meta.get(META_PROTOCOL))
        .and_then(Value::as_str);
    let Some(kinds) = params.get("notifications").and_then(Value::as_object) else {
        return false;
    };
    message.get("id").is_some()
        && protocol == Some(rmcp::model::ProtocolVersion::V_2026_07_28.as_str())
        && params
            .keys()
            .all(|key| key == "_meta" || key == "notifications")
        && kinds
            .keys()
            .all(|kind| LISTEN_KINDS.contains(&kind.as_str()))
        && kinds.get("toolsListChanged") == Some(&Value::Bool(true))
}

/// What the front does with a message from the app.
enum Step {
    /// Passed to the server.
    Passed,
    /// The binary was replaced: the message goes to the new one, with what
    /// follows it.
    HandOver(Vec<u8>),
}

/// `serve`: the server behind a front that hands the session over when the
/// binary is replaced.
pub async fn serve(inner: Inner) -> Result<()> {
    let taken = take_session();
    remove_stale_state();
    let handed = taken.is_some();
    let (session, pending) = taken.unwrap_or_else(|| {
        let binary = env::current_exe().unwrap_or_default();
        (
            Session {
                format: FORMAT,
                binary,
                from: versions::running().to_owned(),
                handshake: None,
                initialized: None,
                protocol: None,
                tools: None,
                relayed: false,
                server_line: None,
                tools_params: None,
                listens: Vec::new(),
                identity: None,
            },
            Vec::new(),
        )
    });
    if handed && matches!(inner, Inner::Local(_)) {
        // Setup replaces the binary before it migrates the store.
        crate::memory::expect_migration();
    }
    // A handed-over session names the build that was checked and started.
    // A plain start can only look at the file: a replacement between the
    // app starting `serve` and this look, a few milliseconds, goes unseen
    // until the next one.
    let own = session.identity.or_else(|| identity(&session.binary).ok());
    let mut front = Front {
        identity: own,
        own,
        session,
        refused: None,
        wanted: None,
        killed: None,
        in_flight: HashMap::new(),
        notices: Vec::new(),
        buffer: Vec::new(),
        backend: start_here(&inner),
        inner,
        stdout: tokio::io::stdout(),
        eof: false,
    };
    if handed {
        eprintln!(
            "user-memory {}: took over this session from {}",
            versions::running(),
            front.session.from
        );
        front.replay().await?;
    }
    let mut reader = Reader::start();
    front.buffer = pending;
    front.handle_input(&mut reader).await?;
    let stopped = crate::remote::stopped();
    tokio::pin!(stopped);
    let mut signalled = false;
    loop {
        tokio::select! {
            chunk = reader.chunks.recv(), if !front.eof => match chunk {
                Some(chunk) if !chunk.is_empty() => {
                    front.buffer.extend_from_slice(&chunk);
                    front.handle_input(&mut reader).await?;
                }
                _ => front.end_input(),
            },
            line = front.backend.output.next_line() => match line {
                Ok(Some(line)) => {
                    front.on_output(line).await?;
                    if front.wanted.is_some() && !front.app_in_flight() && !front.eof {
                        let pending = std::mem::take(&mut front.buffer);
                        front.hand_over(&mut reader, pending).await?;
                        front.handle_input(&mut reader).await?;
                    }
                }
                _ if front.eof => break,
                _ => front.restart().await?,
            },
            () = &mut stopped, if !signalled => {
                signalled = true;
                front.close_listens().await?;
                front.end_input();
            }
        }
    }
    let failed = front.finish().await;
    if signalled {
        // The reader thread may be blocked on input that never comes.
        std::process::exit(0);
    }
    failed.map_or(Ok(()), Err)
}

struct Front {
    session: Session,
    /// The identity of the build whose code serves the session now.
    identity: Option<Identity>,
    /// The identity of this process's own build, which serves again when
    /// the server is restarted here.
    own: Option<Identity>,
    /// A replacement already refused, so it is checked and reported once.
    refused: Option<Identity>,
    /// A checked replacement waiting for the requests in flight.
    wanted: Option<Identity>,
    /// A replacement whose checks a signal ended, and how many.
    killed: Option<(Identity, u32)>,
    /// The app's requests the server has not answered, by id, with their
    /// method.
    in_flight: HashMap<String, String>,
    /// Lines for the next tool reply.
    notices: Vec<String>,
    /// Input read but not yet handled: at most a part of one message.
    buffer: Vec<u8>,
    backend: Backend,
    inner: Inner,
    stdout: tokio::io::Stdout,
    /// The app closed its input.
    eof: bool,
}

impl Front {
    /// Handles every whole message in the buffer.
    async fn handle_input(&mut self, reader: &mut Reader) -> Result<()> {
        while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=end).collect();
            if let Step::HandOver(line) = self.on_input(line).await? {
                let mut pending = line;
                pending.append(&mut self.buffer);
                self.hand_over(reader, pending).await?;
            }
        }
        if self.eof {
            self.end_input();
        }
        Ok(())
    }

    async fn on_input(&mut self, line: Vec<u8>) -> Result<Step> {
        let replaced = !self.session.relayed && self.replaced().await;
        #[cfg(debug_assertions)]
        if env::var_os(TEST_TRACE).is_some() {
            eprintln!(
                "user-memory trace: serving {:?}, on disk {:?}, replaced {replaced}, in flight {:?}",
                self.identity,
                identity(&self.session.binary).ok(),
                self.in_flight.keys().collect::<Vec<_>>()
            );
        }
        if replaced && !self.app_in_flight() {
            return Ok(Step::HandOver(line));
        }
        if let Ok(message) = serde_json::from_slice::<Value>(&line) {
            if !self.legacy()
                && let Some(protocol) = message
                    .pointer("/params/_meta")
                    .and_then(|meta| meta.get(META_PROTOCOL))
                    .and_then(Value::as_str)
                && protocols().iter().any(|known| known == protocol)
            {
                // The 2026-07-28 protocol has no handshake: every request
                // names its version, and one the server refuses is not the
                // session's.
                self.session.protocol = Some(protocol.to_owned());
            }
            match message["method"].as_str() {
                Some("initialize" | "server/discover") => {
                    self.session.handshake = Some(message.clone());
                }
                Some("notifications/initialized") => {
                    self.session.initialized = Some(message.clone());
                }
                Some("tools/list") => {
                    self.session.tools_params = message.get("params").cloned();
                }
                Some("subscriptions/listen") if takes_listen(&message) => {
                    self.listen(message["id"].clone()).await?;
                    return Ok(Step::Passed);
                }
                Some("notifications/cancelled") => {
                    let cancelled = message.pointer("/params/requestId");
                    // A cancelled request gets no answer (specification
                    // 2026-07-28, basic/patterns/cancellation), so it no
                    // longer holds a hand-over back; a late answer still
                    // reaches the app.
                    if let Some(id) = cancelled {
                        self.in_flight.remove(&key(id));
                    }
                    let before = self.session.listens.len();
                    self.session.listens.retain(|id| Some(id) != cancelled);
                    if self.session.listens.len() != before {
                        // A stream the server never saw.
                        return Ok(Step::Passed);
                    }
                }
                _ => {}
            }
            if let (Some(id), Some(method)) = (message.get("id"), message["method"].as_str()) {
                self.in_flight.insert(key(id), method.to_owned());
            }
        }
        self.send(&line);
        Ok(Step::Passed)
    }

    /// Whether a request of the app's is in flight. The front's own replayed
    /// handshake and tool list don't hold a hand-over back: stopping the
    /// server answers them first, and the new code asks again.
    fn app_in_flight(&self) -> bool {
        let own = [key(&json!(REPLAY_ID)), key(&json!(TOOLS_ID))];
        self.in_flight.keys().any(|id| !own.contains(id))
    }

    /// Whether the app opened the session with an `initialize` handshake,
    /// as protocols before 2026-07-28 do.
    fn legacy(&self) -> bool {
        self.session
            .handshake
            .as_ref()
            .is_some_and(|handshake| handshake["method"] == "initialize")
    }

    /// Opens the app's `subscriptions/listen` stream `id` for tool list
    /// changes: acknowledges it with the one kind the front honours, as the
    /// first message carrying its id (specification 2026-07-28,
    /// basic/patterns/subscriptions, "Acknowledgment": the server MUST
    /// acknowledge first, and omits the kinds it does not support).
    async fn listen(&mut self, id: Value) -> Result<()> {
        let acknowledged = json!({
            "jsonrpc": "2.0",
            "method": "notifications/subscriptions/acknowledged",
            "params": {
                "_meta": {META_SUBSCRIPTION: id},
                "notifications": {"toolsListChanged": true},
            },
        });
        self.write_app(&acknowledged.to_string()).await?;
        self.session.listens.push(id);
        Ok(())
    }

    /// Ends the open listen streams as a server that stops on its own does:
    /// with a completion result for each (specification 2026-07-28,
    /// basic/patterns/subscriptions, "Graceful Closure").
    async fn close_listens(&mut self) -> Result<()> {
        for id in std::mem::take(&mut self.session.listens) {
            let complete = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"resultType": "complete", "_meta": {META_SUBSCRIPTION: id}},
            });
            self.write_app(&complete.to_string()).await?;
        }
        Ok(())
    }

    /// How many checks of the replacement `now` a signal ended, this one
    /// included.
    fn killed_checks(&mut self, now: Identity) -> u32 {
        match &mut self.killed {
            Some((identity, count)) if *identity == now => {
                *count += 1;
                *count
            }
            _ => {
                self.killed = Some((now, 1));
                1
            }
        }
    }

    /// Whether the binary was replaced by one that can take over: checked
    /// once per replacement, a refusal said in the next tool reply.
    async fn replaced(&mut self) -> bool {
        let Ok(now) = identity(&self.session.binary) else {
            // Mid-replacement, or gone: the next message looks again.
            return false;
        };
        if self.wanted == Some(now) {
            return true;
        }
        if Some(now) == self.identity || Some(now) == self.refused {
            return false;
        }
        match check(&self.session.binary, self.session.protocol.as_deref()).await {
            Ok(()) => {
                self.wanted = Some(now);
                true
            }
            Err(Refused::Killed(why)) if self.killed_checks(now) < KILLED_CHECKS => {
                // Checked again at a later message, saying nothing to the
                // app yet; the app's log of serve's stderr says why.
                eprintln!(
                    "user-memory: a check of the replaced binary failed ({why}); trying again at the next message"
                );
                false
            }
            Err(why) => {
                self.refused = Some(now);
                self.wanted = None;
                self.notices.push(refusal(&why));
                false
            }
        }
    }

    fn send(&mut self, line: &[u8]) {
        let mut message = line.to_vec();
        if !message.ends_with(b"\n") {
            message.push(b'\n');
        }
        if let Some(input) = &self.backend.input {
            // A server that stopped reading has ended; its output ends
            // too, and the loop starts it again.
            let _ = input.send(message);
        }
    }

    fn send_value(&mut self, message: &Value) {
        self.send(message.to_string().as_bytes());
    }

    async fn write_app(&mut self, line: &str) -> Result<()> {
        self.stdout.write_all(line.as_bytes()).await?;
        self.stdout.write_all(b"\n").await?;
        self.stdout.flush().await?;
        Ok(())
    }

    async fn on_output(&mut self, line: String) -> Result<()> {
        let Ok(mut message) = serde_json::from_str::<Value>(&line) else {
            return self.write_app(&line).await;
        };
        if message["method"] == crate::remote::SERVER_LINE_NOTICE {
            // The bridge's own, for the front.
            self.session.server_line = message
                .pointer("/params/line")
                .and_then(Value::as_str)
                .map(str::to_owned);
            return Ok(());
        }
        if message["method"] == "notifications/tools/list_changed"
            && message.pointer("/params/_meta").is_none()
            && !self.legacy()
        {
            // The bridge's, after a reconnect: a 2026-07-28 app hears it
            // only on the streams it opened for it.
            return self.tell_listens().await;
        }
        let response = message.get("method").is_none()
            && (message.get("result").is_some() || message.get("error").is_some());
        let Some(id) = message.get("id").filter(|_| response).cloned() else {
            return self.write_app(&line).await;
        };
        let Some(method) = self.in_flight.remove(&key(&id)) else {
            return self.write_app(&line).await;
        };
        match id.as_str() {
            Some(REPLAY_ID) => {
                let line = self.server_line(&message);
                if line.is_some() && line != self.session.server_line {
                    self.notices.extend(line.clone());
                }
                self.session.server_line = line;
                return Ok(());
            }
            Some(TOOLS_ID) => return self.compare_tools(&message).await,
            _ => {}
        }
        match method.as_str() {
            "initialize" | "server/discover" => {
                if let Some(protocol) = message.pointer("/result/protocolVersion") {
                    self.session.protocol = protocol.as_str().map(str::to_owned);
                }
                self.session.server_line = self.server_line(&message);
            }
            "tools/list" => {
                if let Some(tools) = message.pointer("/result/tools") {
                    self.session.tools = Some(tools.clone());
                }
            }
            "tools/call" if !self.notices.is_empty() => {
                if let Some(content) = message
                    .pointer_mut("/result/content")
                    .and_then(Value::as_array_mut)
                {
                    for notice in self.notices.drain(..) {
                        content.push(json!({"type": "text", "text": notice}));
                    }
                    return self.write_app(&message.to_string()).await;
                }
            }
            _ => {}
        }
        self.write_app(&line).await
    }

    /// What a bridge's server said about versions in the handshake reply
    /// `message`, as session start says it; nothing for this binary's own
    /// server.
    fn server_line(&self, message: &Value) -> Option<String> {
        let Inner::Bridge(connection) = &self.inner else {
            return None;
        };
        versions::server_line(versions::running(), &connection.url, message.get("result")?)
    }

    /// After a hand-over: tells the app when the tools it was given changed.
    async fn compare_tools(&mut self, message: &Value) -> Result<()> {
        let Some(tools) = message.pointer("/result/tools") else {
            return Ok(());
        };
        let changed = self
            .session
            .tools
            .as_ref()
            .is_some_and(|given| given != tools);
        if !changed {
            return Ok(());
        }
        self.session.tools = Some(tools.clone());
        if self.legacy() {
            return self
                .write_app(
                    &json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
                        .to_string(),
                )
                .await;
        }
        self.tell_listens().await
    }

    /// Tells a 2026-07-28 app its tools changed. From 2026-07-28 a change is
    /// said only on a stream the app opened for it, tagged with the
    /// stream's id (specification 2026-07-28, server/tools, "List Changed
    /// Notification"; basic/index, "`_meta`"); with none open, nobody is
    /// told.
    async fn tell_listens(&mut self) -> Result<()> {
        for id in self.session.listens.clone() {
            let changed = json!({
                "jsonrpc": "2.0",
                "method": "notifications/tools/list_changed",
                "params": {"_meta": {META_SUBSCRIPTION: id}},
            });
            self.write_app(&changed.to_string()).await?;
        }
        Ok(())
    }

    /// Replays the app's handshake to a server that has not seen it, and
    /// asks for its tools, keeping both replies from the app.
    async fn replay(&mut self) -> Result<()> {
        // A 2026-07-28 app may send no handshake at all.
        if let Some(mut handshake) = self.session.handshake.clone() {
            let method = handshake["method"].as_str().unwrap_or_default().to_owned();
            handshake["id"] = json!(REPLAY_ID);
            self.in_flight.insert(key(&json!(REPLAY_ID)), method);
            self.send_value(&handshake);
        }
        if let Some(initialized) = self.session.initialized.clone() {
            self.send_value(&initialized);
        }
        if self.session.tools.is_some() {
            let mut params = self
                .session
                .tools_params
                .clone()
                .unwrap_or_else(|| json!({}));
            // The reply is the front's own, so no progress is reported.
            if let Some(meta) = params.get_mut("_meta").and_then(Value::as_object_mut) {
                meta.remove("progressToken");
            }
            self.in_flight
                .insert(key(&json!(TOOLS_ID)), "tools/list".into());
            self.send_value(
                &json!({"jsonrpc": "2.0", "id": TOOLS_ID, "method": "tools/list", "params": params}),
            );
        }
        Ok(())
    }

    /// Closes the server's input once the app closed its own; the server
    /// answers what is in flight, then ends.
    fn end_input(&mut self) {
        self.eof = true;
        self.backend.input = None;
    }

    /// Stops the server: closes its input, passes on what it still says,
    /// and waits for it to end.
    async fn stop(&mut self) {
        self.backend.input = None;
        while let Ok(Some(line)) = self.backend.output.next_line().await {
            let _ = self.on_output(line).await;
        }
        if let Some(err) = self.finish().await {
            eprintln!("user-memory: {err:#}");
        }
    }

    /// Waits for the server, whose output has ended, to end; why it
    /// failed, if it did.
    async fn finish(&mut self) -> Option<anyhow::Error> {
        let _ = (&mut self.backend.writer).await;
        match &mut self.backend.running {
            Running::Here(task) => match task.await {
                Ok(Err(err)) => return Some(err),
                Err(err) => return Some(err.into()),
                Ok(Ok(())) => {}
            },
            Running::Child(child) => {
                if tokio::time::timeout(CHILD_STOP_WAIT, child.wait())
                    .await
                    .is_err()
                {
                    let _ = child.kill().await;
                }
            }
        }
        None
    }

    /// The server ended while the app is still connected: answers what it
    /// left in flight and starts it again with the session.
    async fn restart(&mut self) -> Result<()> {
        // Its input goes with it, so the writer feeding it ends and the
        // wait for the server returns.
        self.backend.input = None;
        // A server of this binary that failed fails `serve`, as it did
        // before the front: its cause, such as a broken setup, stays.
        if let Some(err) = self.finish().await {
            return Err(err);
        }
        let failed: Vec<(String, String)> = self.in_flight.drain().collect();
        for (id, method) in failed {
            let Ok(id) = serde_json::from_str::<Value>(&id) else {
                continue;
            };
            if id == json!(REPLAY_ID) || id == json!(TOOLS_ID) {
                continue;
            }
            let text = "user-memory's server stopped while handling this call, which may not have completed; it was started again.";
            let reply = if method == "tools/call" {
                json!({"jsonrpc":"2.0","id":id,"result":{"isError":true,"content":[{"type":"text","text":text}]}})
            } else {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":rmcp::model::ErrorCode::INTERNAL_ERROR.0,"message":text}})
            };
            self.write_app(&reply.to_string()).await?;
        }
        self.backend = start_here(&self.inner);
        self.identity = self.own;
        self.replay().await
    }

    /// Starts the replaced binary in this one's place with the session and
    /// `pending`, the input it is to handle first. Returns only when it
    /// relays to it, or when this binary goes on, with `pending` back in
    /// the buffer for the caller to handle.
    async fn hand_over(&mut self, reader: &mut Reader, mut pending: Vec<u8>) -> Result<()> {
        let mut wanted = self.wanted.take();
        // Replaced again since the check: check that one.
        if identity(&self.session.binary).ok() != wanted {
            if !self.replaced().await {
                self.buffer = pending;
                return Ok(());
            }
            wanted = self.wanted.take();
        }
        self.stdout.flush().await?;
        self.stop().await;
        self.session.identity = wanted;
        self.session.from = versions::running().to_owned();
        if relays() {
            let session = Session {
                relayed: true,
                ..self.session.clone()
            };
            let started = write_state(&session, &pending).and_then(|state| {
                start_child(&target(&self.session.binary), &state).map_err(|err| {
                    let _ = fs::remove_file(&state);
                    anyhow::Error::from(err)
                })
            });
            match started {
                Ok(backend) => {
                    self.backend = backend;
                    self.identity = wanted;
                    Ok(())
                }
                Err(err) => self.resume(wanted, &format!("{err:#}"), pending).await,
            }
        } else {
            let error = self.exec(reader, &mut pending);
            self.resume(wanted, &error, pending).await
        }
    }

    /// Replaces this process with the new binary, keeping its stdin and
    /// stdout; returns only why that failed. The gate is held from the last
    /// drain of the reader to the `exec`, so no byte read is left behind.
    #[cfg(unix)]
    fn exec(&mut self, reader: &mut Reader, pending: &mut Vec<u8>) -> String {
        use std::os::unix::process::CommandExt;
        let gate = reader.gate.clone();
        let _held = gate.lock().unwrap_or_else(PoisonError::into_inner);
        while let Ok(chunk) = reader.chunks.try_recv() {
            if chunk.is_empty() {
                self.eof = true;
            }
            pending.extend_from_slice(&chunk);
        }
        let state = match write_state(&self.session, pending) {
            Ok(state) => state,
            Err(err) => return format!("{err:#}"),
        };
        let err = std::process::Command::new(target(&self.session.binary))
            .arg("serve")
            .env(ENV, &state)
            .exec();
        let _ = fs::remove_file(&state);
        err.to_string()
    }

    #[cfg(not(unix))]
    fn exec(&mut self, _reader: &mut Reader, _pending: &mut Vec<u8>) -> String {
        "this system can't replace a running program".into()
    }

    /// Goes on with this binary after a failed hand-over to the one with
    /// identity `wanted`, saying so in the next tool reply.
    async fn resume(
        &mut self,
        wanted: Option<Identity>,
        error: &str,
        pending: Vec<u8>,
    ) -> Result<()> {
        self.refused = wanted;
        self.notices
            .push(refusal(&Refused::Broken(error.to_owned())));
        self.backend = start_here(&self.inner);
        self.replay().await?;
        // The caller handles it, starting with the message that asked.
        self.buffer = pending;
        Ok(())
    }
}

/// The test-only tool of a debug build told to report a test version: the
/// version answering, which tells the tests which code serves the session,
/// and a tool list that differs from the real build's.
#[cfg(debug_assertions)]
#[rmcp::tool_router(router = debug_router, vis = "pub(crate)")]
impl crate::UserMemoryServer {
    #[rmcp::tool(
        description = "Debug builds only: the user-memory version and process serving this session."
    )]
    async fn debug_version(&self) -> String {
        json!({"version": versions::running(), "pid": std::process::id()}).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    /// This build's own probe answer takes over a session on any protocol
    /// it speaks; another format, a protocol it lacks or no answer at all
    /// is refused, each saying why.
    #[test]
    fn a_replacement_takes_over_only_a_session_it_can_continue() {
        let own = probe_answer().to_string();
        let new = version("9.9.9");
        for protocol in ["2024-11-05", "2025-06-18", "2025-11-25"] {
            assert!(judge(new, &own, Some(protocol)).is_ok(), "{protocol}");
        }
        assert!(judge(new, &own, None).is_ok());
        let later = r#"{"version":"9.9.9","formats":[2],"protocols":["2025-06-18"]}"#;
        let narrower = r#"{"version":"9.9.9","formats":[1],"protocols":["2026-07-28"]}"#;
        for (answer, said) in [
            (
                later,
                "user-memory was replaced on disk, but the new binary can't take over this session (it can't take over a session)",
            ),
            (
                "",
                "user-memory was replaced on disk, but the new binary can't take over this session (it can't take over a session)",
            ),
            (
                narrower,
                "user-memory 9.9.9 replaced the binary this session runs",
            ),
        ] {
            let refused = judge(new, answer, Some("2025-06-18")).unwrap_err();
            assert!(
                refusal(&refused).starts_with(said),
                "{answer}: {}",
                refusal(&refused)
            );
        }
        assert!(
            refusal(&judge(new, narrower, Some("2025-06-18")).unwrap_err())
                .contains("it can't continue this session's protocol (2025-06-18)")
        );
    }

    /// The probe answer names this build's version and the state format.
    #[test]
    fn the_probe_answer_names_the_version_and_format() {
        let answer = probe_answer();
        assert_eq!(answer["version"], versions::running());
        assert_eq!(answer["formats"], json!([FORMAT]));
        assert!(
            answer["protocols"]
                .as_array()
                .is_some_and(|p| !p.is_empty())
        );
    }
}
