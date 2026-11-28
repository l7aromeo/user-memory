//! The marker a memory server's connector keeps for the hooks on this
//! machine: the server's position as its last poll found it. A prompt whose
//! snapshot holds that position asks the server nothing. The connector keeps
//! no copy of the server's memories; which ones a session sees is decided by
//! the server alone.

use super::{Connection, Session};
use crate::{
    memory::{Position, parse_stamps_reply},
    paths, private,
};
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// How often a connector asks its server where it stands while sessions on
/// this machine are active: a change reaches the marker within this, and
/// one poll is one small request.
pub const REMOTE_MARKER_POLL: Duration = Duration::from_secs(5);

/// How long one poll waits for the server's answer. A poll left unanswered
/// is asked again at the next one, and a marker that misses polls stops
/// counting, so the hooks ask the server themselves; waiting longer would
/// only hold the poller back.
const POLL_CALL_WAIT: Duration = Duration::from_secs(10);

/// Polls a quiet machine makes in [`REMOTE_MARKER_POLL`]'s stead: once a
/// minute, so a machine left open asks the server little.
pub(crate) const QUIET_POLLS: u32 = 12;
/// Polls' worth of time after the last hook before the connector slows down:
/// two minutes, longer than the pause between two prompts of a session.
const QUIET_AFTER_POLLS: u32 = 24;
/// Polls a marker may miss before it no longer counts: its connector is
/// down or cut off, or slowed for a quiet spell, so a hook asks the server
/// itself and no change is late.
const STALE_AFTER_POLLS: u32 = 3;

/// Times a connector looks at the marker's age in each poll: a due poll
/// starts at most a tenth of a poll late, and the look is two file stats.
pub(crate) const TICKS_PER_POLL: u32 = 10;

/// Hex characters of the server key in the marker's file name: 64 bits of
/// the SHA-256 of the server's address, so two servers never share a
/// marker, without the address in the name.
const KEY_CHARS: usize = 16;

/// The marker's extension.
const MARKER: &str = "changed";
/// The extension of the file whose time says when a hook last ran.
const WANTED: &str = "wanted";
/// The extension of the file a connector writes as it starts a poll, and
/// renews while the poll waits for its answer, so another connector's tick
/// in the meantime does not poll too.
const CLAIM: &str = "polling";
/// The extension of the file a connector holds an exclusive lock on while
/// it decides whether to take the claim, so only one can take it.
const CLAIM_LOCK: &str = "polling.lock";
/// The extension of the file every running connector holds a shared lock
/// on, which the system lets go when it exits or is killed: the hooks read
/// a server's delivery files only while one holds it (#180).
const CONNECTED: &str = "connected";
/// The extension of the file the connector that answers the hooks' title
/// lookups names its loopback port and key in (#292).
const TITLES: &str = "titles";
/// The extension of the file that connector holds an exclusive lock on
/// while it answers them, which the system lets go when it exits or is
/// killed, so another connector takes over.
const TITLES_LOCK: &str = "titles.lock";

/// [`REMOTE_MARKER_POLL`], which debug builds take in milliseconds from
/// `USER_MEMORY_MARKER_POLL_MS` so tests can see the quiet interval and a
/// stale marker without waiting minutes.
pub(crate) fn poll_every() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("USER_MEMORY_MARKER_POLL_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
    {
        return Duration::from_millis(ms);
    }
    REMOTE_MARKER_POLL
}

/// The marker of the server at `url` and the file the hooks touch.
struct Files {
    marker: PathBuf,
    wanted: PathBuf,
    claim: PathBuf,
    claim_lock: PathBuf,
    connected: PathBuf,
    titles: PathBuf,
    titles_lock: PathBuf,
}

impl Files {
    fn of(url: &str) -> Option<Self> {
        Some(Self::in_dir(&paths::state_dir()?.join("servers"), url))
    }

    fn in_dir(dir: &Path, url: &str) -> Self {
        let key = &crate::setup::sha256(url.as_bytes())[..KEY_CHARS];
        Self {
            marker: dir.join(format!("{key}.{MARKER}")),
            wanted: dir.join(format!("{key}.{WANTED}")),
            claim: dir.join(format!("{key}.{CLAIM}")),
            claim_lock: dir.join(format!("{key}.{CLAIM_LOCK}")),
            connected: dir.join(format!("{key}.{CONNECTED}")),
            titles: dir.join(format!("{key}.{TITLES}")),
            titles_lock: dir.join(format!("{key}.{TITLES_LOCK}")),
        }
    }

    /// The position in the marker, whatever its age.
    fn position(&self) -> Option<Position> {
        let text = fs::read_to_string(&self.marker).ok()?;
        let text = text.trim_end();
        (!text.is_empty()).then(|| Position::kept(text.to_owned()))
    }

    /// Replaces the marker with `position`, whole: a hook never reads half.
    fn write(&self, position: &Position) -> io::Result<()> {
        let dir = self.marker.parent().ok_or(io::ErrorKind::InvalidInput)?;
        private::create_dir(dir)?;
        let temporary = self
            .marker
            .with_extension(format!("{MARKER}.{}", std::process::id()));
        let written = (|| {
            let mut file = private::create_file(
                fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true),
                &temporary,
            )?;
            io::Write::write_all(&mut file, format!("{}\n", position.as_str()).as_bytes())?;
            drop(file);
            fs::rename(&temporary, &self.marker)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        written
    }

    /// Whether a poll is due: the marker and the last claim are older than
    /// the interval, which is [`QUIET_POLLS`] polls once no hook ran for
    /// [`QUIET_AFTER_POLLS`]. Another connector's poll makes them young, so
    /// a machine polls once an interval however many sessions it runs.
    ///
    /// A claim newer than the marker is a poll still waiting for its answer:
    /// its connector renews the claim while it waits ([`holding`]), so no
    /// other poll starts until it is answered, however slow the server, or
    /// the claim goes unrenewed for [`STALE_AFTER_POLLS`] polls, when its
    /// connector counts as gone. A poll that fails stops renewing its claim
    /// and leaves it, so a server that is down is asked once in that time;
    /// the marker is stale by then, and the hooks ask the server themselves.
    fn due(&self, now: SystemTime) -> bool {
        let every = poll_every();
        let quiet = age(&self.wanted, now).is_none_or(|age| age > every * QUIET_AFTER_POLLS);
        let interval = if quiet { every * QUIET_POLLS } else { every };
        let old = |path: &Path| age(path, now).is_none_or(|age| age >= interval);
        let claimed = age(&self.claim, now);
        let waiting = claimed.is_some_and(|claimed| {
            claimed < every * STALE_AFTER_POLLS
                && age(&self.marker, now).is_none_or(|marker| claimed < marker)
        });
        old(&self.marker) && old(&self.claim) && !waiting
    }

    /// Takes the claim for a poll that [`Files::due`] found due; none when
    /// another connector took it first. Several connectors find a poll due
    /// at the same instant, as connectors started together tick together,
    /// so each decides under an exclusive lock on [`CLAIM_LOCK`]'s file: a
    /// poll still due there is this connector's, and the claim it writes
    /// makes the poll not due for the next one. The lock is held for two
    /// file stats and a write, and the system lets go of it if the process
    /// dies. A lock file that can't be had takes no claim: the marker ages
    /// and the hooks ask the server themselves.
    fn take(&self) -> Option<Claim> {
        let dir = self.claim.parent()?;
        private::create_dir(dir).ok()?;
        let lock = private::create_file(
            fs::OpenOptions::new().read(true).write(true).create(true),
            &self.claim_lock,
        )
        .ok()?;
        lock.lock().ok()?;
        if !self.due(SystemTime::now()) {
            return None;
        }
        let claim = Claim::new();
        let mut file = private::create_file(
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true),
            &self.claim,
        )
        .ok()?;
        io::Write::write_all(&mut file, claim.0.as_bytes()).ok()?;
        Some(claim)
    }

    /// Removes the `<key>.polling.<pid>` files that connectors of 0.9.1 to
    /// 0.9.3 left when they stopped between renaming the claim away and
    /// removing it: nothing reads them, and this version never makes one.
    /// One younger than the stale age may be an older connector's still in
    /// that step, during an upgrade, so it stays.
    fn remove_old_takes(&self) {
        let (Some(dir), Some(name)) = (self.claim.parent(), self.claim.file_name()) else {
            return;
        };
        let prefix = format!("{}.", name.to_string_lossy());
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let file = entry.file_name();
            let Some(pid) = file
                .to_string_lossy()
                .strip_prefix(&prefix)
                .map(str::to_owned)
            else {
                continue;
            };
            let old =
                age(&entry.path(), now).is_some_and(|age| age >= poll_every() * STALE_AFTER_POLLS);
            if !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()) && old {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// Whether the claim is still `claim`: no other connector took it over
    /// after it counted as gone. A claim removed after a write this machine
    /// relayed ([`moved`]) counts as still this poll's.
    fn owns(&self, claim: &Claim) -> bool {
        match fs::read_to_string(&self.claim) {
            Ok(text) => text == claim.0,
            Err(err) => err.kind() == io::ErrorKind::NotFound,
        }
    }

    /// Makes `claim` young again while its poll waits for its answer, so it
    /// counts as gone only once its connector stops renewing it, however
    /// slow the server. A claim another connector took over is left alone.
    fn renew(&self, claim: &Claim) {
        let Ok(mut file) = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.claim)
        else {
            return;
        };
        let mut text = String::new();
        if io::Read::read_to_string(&mut file, &mut text).is_ok() && text == claim.0 {
            let _ = file.set_modified(SystemTime::now());
        }
    }

    /// Writes the `position` the poll of `claim` was answered with, unless
    /// another connector took the claim over meanwhile: that poll answers
    /// with a position at least as new, and a marker written now would make
    /// its claim look answered, so a third poll would start beside it.
    /// Whether the marker was written.
    fn answer(&self, claim: &Claim, position: &Position) -> io::Result<bool> {
        if !self.owns(claim) {
            return Ok(false);
        }
        self.write(position)?;
        Ok(true)
    }
}

/// A claim one poll took: what it wrote in the claim file, this process's
/// id, a nanosecond stamp and a count, so the poll can tell its own claim
/// from one another connector took after it.
#[derive(Debug, PartialEq)]
struct Claim(String);

impl Claim {
    fn new() -> Self {
        static TAKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let count = TAKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(format!("{}.{nanos}.{count}", std::process::id()))
    }
}

/// Runs `work`, a poll that holds `claim`, renewing the claim at each tick
/// until it is done; a renewal is one small file write, and a poll answered
/// within a tick makes none.
async fn holding<T>(files: &Files, claim: &Claim, work: impl std::future::Future<Output = T>) -> T {
    let mut tick = tokio::time::interval(poll_every() / TICKS_PER_POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick is now, when the claim is new.
    tick.tick().await;
    let mut work = std::pin::pin!(work);
    loop {
        tokio::select! {
            done = &mut work => return done,
            _ = tick.tick() => files.renew(claim),
        }
    }
}

impl Files {
    /// Takes the shared lock that says a connector runs; none when the file
    /// can't be had, and the hooks then read no delivery file of the server.
    fn connect(&self) -> Option<fs::File> {
        let dir = self.connected.parent()?;
        private::create_dir(dir).ok()?;
        let file = private::create_file(
            fs::OpenOptions::new().read(true).write(true).create(true),
            &self.connected,
        )
        .ok()?;
        file.lock_shared().ok()?;
        Some(file)
    }

    /// Whether a connector runs: the lock it shares is held. The exclusive
    /// lock tried without waiting, and let go at once when had.
    fn connected(&self) -> bool {
        let Ok(file) = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.connected)
        else {
            return false;
        };
        matches!(file.try_lock(), Err(fs::TryLockError::WouldBlock))
    }
}

/// The title lookups' files of the server at `url` (#292): the endpoint
/// file the hooks read and the lock its writer holds.
pub(crate) struct TitlesFiles {
    pub(crate) endpoint: PathBuf,
    pub(crate) lock: PathBuf,
}

/// [`TitlesFiles`] of the server at `url` in this user's state folder.
pub(crate) fn titles_files(url: &str) -> Option<TitlesFiles> {
    Files::of(url).map(|files| TitlesFiles {
        endpoint: files.titles,
        lock: files.titles_lock,
    })
}

/// The server's position in the marker, whatever its age: what tells the
/// title lookups' connector that a poll found a change since it last asked
/// for titles.
pub(crate) fn polled(url: &str) -> Option<Position> {
    Files::of(url)?.position()
}

/// How long ago `path` was last written, none when it is missing.
fn age(path: &Path, now: SystemTime) -> Option<Duration> {
    let modified = fs::metadata(path).and_then(|m| m.modified()).ok()?;
    Some(now.duration_since(modified).unwrap_or_default())
}

/// For a hook: the position the server at `url` was at when its connector
/// last polled it, none when no connector polled it in the last
/// [`STALE_AFTER_POLLS`] polls. Also says a hook ran, so a connector that
/// slowed for a quiet spell polls at [`REMOTE_MARKER_POLL`] again.
pub fn position(url: &str) -> Option<Position> {
    let files = Files::of(url)?;
    let _ = touch(&files.wanted);
    let fresh = age(&files.marker, SystemTime::now())
        .is_some_and(|age| age <= poll_every() * STALE_AFTER_POLLS);
    fresh.then(|| files.position()).flatten()
}

/// For a hook reading the delivery files of the server at `url` (#180): the
/// position its connector last found it at, when a connector runs on this
/// machine and found it in the last [`SERVED_AFTER_POLLS`] quiet polls;
/// else none, and the hook reads none of its files, since no one would
/// take a removal on the server out of them. Says a hook ran, as
/// [`position`] does.
pub(crate) fn served(url: &str) -> Option<Position> {
    Files::of(url)?.served()
}

impl Files {
    /// [`served`] for these files.
    fn served(&self) -> Option<Position> {
        let _ = touch(&self.wanted);
        let recent = age(&self.marker, SystemTime::now())
            .is_some_and(|age| age <= poll_every() * QUIET_POLLS * SERVED_AFTER_POLLS);
        (recent && self.connected())
            .then(|| self.position())
            .flatten()
    }
}

/// A connector's marker files in a folder of a test's own, for tests of
/// what the hooks read (#180).
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// Writes `position` in the marker of the server at `url` in `dir`,
    /// as a poll does, its age `ago`.
    pub(crate) fn mark(dir: &Path, url: &str, position: &Position, ago: Duration) {
        let files = Files::in_dir(dir, url);
        files.write(position).unwrap();
        let marker = fs::OpenOptions::new()
            .write(true)
            .open(&files.marker)
            .unwrap();
        marker.set_modified(SystemTime::now() - ago).unwrap();
    }

    /// Holds the lock a connector holds while it runs, until dropped.
    pub(crate) fn connect(dir: &Path, url: &str) -> fs::File {
        Files::in_dir(dir, url).connect().unwrap()
    }

    /// The lock file a connector holds, for a process of a test's own.
    pub(crate) fn connected_path(dir: &Path, url: &str) -> PathBuf {
        Files::in_dir(dir, url).connected
    }

    /// [`served`] for the marker files in `dir`.
    pub(crate) fn served(dir: &Path, url: &str) -> Option<Position> {
        Files::in_dir(dir, url).served()
    }

    /// The age past which a running connector's marker no longer counts.
    pub(crate) fn served_age() -> Duration {
        poll_every() * QUIET_POLLS * SERVED_AFTER_POLLS
    }
}

/// Quiet polls a connector that runs may miss before the hooks no longer
/// read its server's delivery files: one whose server can't be reached
/// learns of no removal there. A quiet connector polls once in
/// [`QUIET_POLLS`] polls, so it never misses this many while it reaches
/// the server.
const SERVED_AFTER_POLLS: u32 = STALE_AFTER_POLLS;

/// Wakes this process's poller after a write the relay passed on.
static WROTE: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// For the relay, after the server answered a write it passed on: the
/// marker no longer says where the server at `url` stands. It is made old
/// rather than removed, so the hooks of every session on this machine ask
/// the server and hear the write, while the poll, woken at once, still asks
/// from the position it holds and gets only the changes since. Another
/// connector's poll that was asked before the write and answers after this
/// one can put the older position back; the write then reaches the other
/// sessions with the next poll, as a write from elsewhere does.
pub fn moved(url: &str) {
    if let Some(files) = Files::of(url) {
        if let Ok(marker) = fs::OpenOptions::new().write(true).open(&files.marker) {
            let _ = marker.set_modified(SystemTime::UNIX_EPOCH);
        }
        let _ = fs::remove_file(&files.claim);
    }
    WROTE.notify_one();
}

/// Sets the time of the private file `path`, creating it.
fn touch(path: &Path) -> io::Result<()> {
    private::create_dir(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
    let file = private::create_file(fs::OpenOptions::new().write(true).create(true), path)?;
    file.set_modified(SystemTime::now())
}

/// Polls the server `connection` names for as long as the connector runs:
/// at each tick a due poll asks `index` for the stamps of the global
/// memories changed since the marker's position, which is a small reply,
/// and keeps the position the server answers. A server that knows no
/// positions (an older one sends every global stamp, about 800 KB at 10,000
/// memories, and none) gets no marker, so the hooks ask it as before; it is
/// asked again only at the quiet interval, and then only for its version
/// in a handshake, until that version changes.
///
/// A poll answered with a position keeps its session for the next poll, so
/// a poll is one request rather than a handshake and a request; a session
/// the server dropped is made again by the transport within the poll
/// (`Connection::transport`). Any other outcome ends the session, so the
/// next poll's handshake names the server's version afresh: a server that
/// gave no positions and was then upgraded is met as it is now.
///
/// The poller stops once `stop` turns true, ending its kept session within
/// [`POLLER_STOP_WAIT`].
pub async fn poll(connection: Connection, mut stop: tokio::sync::watch::Receiver<bool>) {
    let Some(files) = Files::of(&connection.url) else {
        return;
    };
    // Held until the connector exits, by this process's open file.
    let _connected = files.connect();
    files.remove_old_takes();
    // Whether a delivery file may be behind the server: at this connector's
    // first poll, even when the marker already names the server's position,
    // and after an upkeep that left one behind.
    let mut behind = true;
    // The version of a server that answered with no position.
    let mut without_positions: Option<serde_json::Value> = None;
    let mut last_asked: Option<tokio::time::Instant> = None;
    // The session of the last poll answered with a position.
    let mut kept: Option<Session> = None;
    loop {
        let woken = tokio::select! {
            () = tokio::time::sleep(poll_every() / TICKS_PER_POLL) => false,
            () = WROTE.notified() => true,
            _ = stop.wait_for(|stopped| *stopped) => break,
        };
        // Never sooner than a poll after its own last ask, answered or not,
        // so an unreachable server is not asked at every tick; a write this
        // process relayed is the exception.
        let wait = if without_positions.is_none() {
            poll_every()
        } else {
            poll_every() * QUIET_POLLS
        };
        let waiting = !woken && last_asked.is_some_and(|at| at.elapsed() < wait);
        if waiting || !files.due(SystemTime::now()) {
            continue;
        }
        let Some(claim) = files.take() else {
            continue;
        };
        last_asked = Some(tokio::time::Instant::now());
        let session = kept.take().filter(|session| !session.is_closed());
        let work = holding(&files, &claim, async {
            if let Some(old) = &without_positions {
                if version(&connection).await.is_none_or(|now| &now == old) {
                    return Polled::Unchanged;
                }
                without_positions = None;
            }
            let session = match session {
                Some(session) => session,
                None => match connection.open().await {
                    Ok(session) => session,
                    // Unreachable for now: the marker ages out and the
                    // hooks ask the server themselves.
                    Err(_) => return Polled::Unanswered,
                },
            };
            let reply = connection
                .call_in(
                    &session,
                    "index",
                    poll_input(files.position()),
                    POLL_CALL_WAIT,
                )
                .await
                .ok()
                .and_then(|text| parse_stamps_reply(&text));
            let Some(reply) = reply else {
                end(session).await;
                return Polled::Unanswered;
            };
            match reply.since {
                Some(position) => {
                    // The delivery files first: a hook reads them only once
                    // they are at or past the marker.
                    let moved = files
                        .position()
                        .is_none_or(|marked| !marked.same_change(&position));
                    if moved || behind {
                        behind = !super::delivery::upkeep(&connection, &position).await;
                    }
                    Polled::At(position, session)
                }
                None => {
                    end(session).await;
                    Polled::WithoutPositions(version(&connection).await.unwrap_or_default())
                }
            }
        });
        // A poll under way when the connector stops is left: its claim goes
        // unrenewed, and another connector polls once it counts as gone.
        let asked = tokio::select! {
            asked = work => asked,
            _ = stop.wait_for(|stopped| *stopped) => break,
        };
        match asked {
            Polled::At(position, session) => {
                let _ = files.answer(&claim, &position);
                kept = Some(session);
            }
            Polled::WithoutPositions(version) => {
                let _ = fs::remove_file(&files.marker);
                without_positions = Some(version);
            }
            Polled::Unchanged | Polled::Unanswered => {}
        }
    }
    if let Some(session) = kept {
        end(session).await;
    }
}

/// How long ending a session may take: the one request that tells the
/// server it can let the session go, which a server that answers at all
/// answers well within half a second. Nothing waits on it at a stop, so it
/// bounds only how long a stopped poller lingers; a session not ended in
/// time is let go by the server once idle, as one cut off is.
pub(super) const POLLER_STOP_WAIT: Duration = Duration::from_millis(500);

/// How long a stopped poller has to end on its own before it is stopped
/// outright. Longer than [`POLLER_STOP_WAIT`], which it must hold whole:
/// the poller first sees the stop at its next tick or as a poll's await
/// gives way, then spends up to that wait ending its session; twice it
/// leaves that room on a loaded machine.
pub(super) const POLLER_END_WAIT: Duration = Duration::from_millis(1000);

/// Ends `session`, telling the server, for at most [`POLLER_STOP_WAIT`].
async fn end(session: Session) {
    let _ = tokio::time::timeout(POLLER_STOP_WAIT, session.cancel()).await;
}

/// The poll's `index` input: the stamps changed since `position`. No
/// server version refuses it, since none of its fields has a floor, so the
/// version a kept session's first handshake named, stale after the server
/// is upgraded or rolled back, decides nothing for it. A server rolled back
/// to a release without positions ignores `stamps_since` and answers
/// without a position; the poll then ends the session, and the next
/// handshake reads the version afresh.
fn poll_input(position: Option<Position>) -> serde_json::Value {
    serde_json::json!({
        "stamps": true,
        "stamps_since": position.unwrap_or_default(),
    })
}

/// What one poll found.
enum Polled {
    /// The server's position, with the delivery files brought up to it, and
    /// the session it came in, kept for the next poll.
    At(Position, Session),
    /// A server that keeps no positions, at this version.
    WithoutPositions(serde_json::Value),
    /// A server without positions whose version has not changed.
    Unchanged,
    /// No answer.
    Unanswered,
}

/// The version the server names in its handshake.
async fn version(connection: &Connection) -> Option<serde_json::Value> {
    let handshake = connection.handshake().await.ok()?;
    crate::setup::versions::server_version(&handshake).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://memory.example.test:8765/mcp";

    /// A kept session keeps the version its first handshake named, so the
    /// poll is safe only while no server version would refuse its input.
    #[test]
    fn no_server_version_refuses_the_polls_input() {
        let input = poll_input(Some(Position::kept("1.0123456789abcdef.7".into())));
        let refused =
            crate::setup::versions::refused_option(URL, "0.0.0", "index", &input).map(|r| r.0);
        assert_eq!(refused, None);
    }

    #[test]
    fn a_marker_is_private_and_named_by_a_hash_alone() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("servers");
        let files = Files::in_dir(&dir, URL);
        let position = Position::kept("1.0123456789abcdef.7".into());
        files.write(&position).unwrap();
        touch(&files.wanted).unwrap();
        assert_eq!(files.position(), Some(position));
        assert!(private::is_private(&dir).unwrap());
        touch(&files.claim).unwrap();
        for path in [&files.marker, &files.wanted, &files.claim] {
            assert!(private::is_private(path).unwrap(), "{path:?}");
            let stem = path.file_stem().unwrap().to_str().unwrap();
            assert_eq!(stem.len(), KEY_CHARS, "{path:?}");
            assert!(stem.chars().all(|c| c.is_ascii_hexdigit()), "{path:?}");
        }
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            3,
            "no temporary file left"
        );
    }

    #[test]
    fn a_poll_is_due_by_the_markers_age_against_the_interval() {
        let root = tempfile::tempdir().unwrap();
        let files = Files::in_dir(root.path(), URL);
        let poll = REMOTE_MARKER_POLL;
        let now = SystemTime::now();
        assert!(files.due(now), "no marker yet");
        files
            .write(&Position::kept("1.0123456789abcdef.7".into()))
            .unwrap();
        let now = SystemTime::now();
        assert!(!files.due(now), "another connector just polled");
        // A hook ran lately: one poll's interval.
        touch(&files.wanted).unwrap();
        assert!(files.due(now + poll * 2));
        // Another connector started a poll just now.
        fs::remove_file(&files.marker).unwrap();
        touch(&files.claim).unwrap();
        assert!(!files.due(SystemTime::now()));
        files
            .write(&Position::kept("1.0123456789abcdef.7".into()))
            .unwrap();
        // No hook for a while: the quiet interval.
        fs::remove_file(&files.wanted).unwrap();
        assert!(!files.due(now + poll * 2));
        assert!(files.due(now + poll * (QUIET_POLLS + 1)));
    }

    /// Sets the time of `path` to `ago` before now.
    fn aged(path: &Path, ago: Duration) {
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - ago)
            .unwrap();
    }

    #[test]
    fn a_poll_waits_for_one_not_yet_answered_up_to_the_stale_age() {
        let root = tempfile::tempdir().unwrap();
        let files = Files::in_dir(root.path(), URL);
        let poll = REMOTE_MARKER_POLL;
        touch(&files.wanted).unwrap();
        files
            .write(&Position::kept("1.0123456789abcdef.7".into()))
            .unwrap();
        aged(&files.marker, poll * 2);
        // Claimed after the marker was written: a poll still waiting for
        // its answer, past the interval.
        touch(&files.claim).unwrap();
        aged(&files.claim, poll + poll / 2);
        let now = SystemTime::now();
        assert!(!files.due(now), "a poll under way");
        assert!(
            files.due(now + poll * STALE_AFTER_POLLS),
            "its connector is gone"
        );
        // Answered: the marker is newer than the claim.
        aged(&files.claim, poll * 3);
        assert!(files.due(SystemTime::now()));
        // A server without positions, or one that is down, leaves no newer
        // marker: one poll in that time.
        fs::remove_file(&files.marker).unwrap();
        aged(&files.claim, poll * 2);
        let now = SystemTime::now();
        assert!(!files.due(now));
        assert!(files.due(now + poll * STALE_AFTER_POLLS));
    }

    /// A marker answered `ago`, and a claim taken after it whose connector
    /// looks gone: unrenewed for the stale age.
    fn gone_claim(files: &Files, ago: Duration) -> Claim {
        touch(&files.wanted).unwrap();
        files
            .write(&Position::kept("1.0123456789abcdef.7".into()))
            .unwrap();
        aged(&files.marker, ago);
        let claim = files.take().unwrap();
        aged(&files.claim, REMOTE_MARKER_POLL * STALE_AFTER_POLLS);
        claim
    }

    #[test]
    fn a_poll_under_way_renews_its_claim_past_the_stale_age() {
        let root = tempfile::tempdir().unwrap();
        let files = Files::in_dir(root.path(), URL);
        let claim = gone_claim(&files, REMOTE_MARKER_POLL * STALE_AFTER_POLLS * 2);
        assert!(files.due(SystemTime::now()), "unrenewed: counts as gone");
        let tick = REMOTE_MARKER_POLL / TICKS_PER_POLL;
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                holding(&files, &claim, tokio::time::sleep(tick + tick / 2)).await;
            });
        assert!(!files.due(SystemTime::now()), "renewed: still under way");
    }

    #[test]
    fn a_renewal_leaves_a_claim_another_poll_took_over_alone() {
        let root = tempfile::tempdir().unwrap();
        let files = Files::in_dir(root.path(), URL);
        let first = gone_claim(&files, REMOTE_MARKER_POLL * STALE_AFTER_POLLS * 2);
        let second = files
            .take()
            .expect("a gone connector's claim is taken over");
        aged(&files.claim, REMOTE_MARKER_POLL * 2);
        let before = fs::metadata(&files.claim).unwrap().modified().unwrap();
        files.renew(&first);
        assert_eq!(
            fs::metadata(&files.claim).unwrap().modified().unwrap(),
            before
        );
        files.renew(&second);
        assert!(fs::metadata(&files.claim).unwrap().modified().unwrap() > before);
    }

    #[test]
    fn a_poll_whose_claim_was_taken_over_leaves_the_marker_to_the_other() {
        let root = tempfile::tempdir().unwrap();
        let files = Files::in_dir(root.path(), URL);
        let first = gone_claim(&files, REMOTE_MARKER_POLL * STALE_AFTER_POLLS * 2);
        let second = files
            .take()
            .expect("a gone connector's claim is taken over");
        let late = Position::kept("1.0123456789abcdef.8".into());
        assert!(!files.answer(&first, &late).unwrap());
        assert_ne!(files.position(), Some(late));
        let newer = Position::kept("1.0123456789abcdef.9".into());
        assert!(files.answer(&second, &newer).unwrap());
        assert_eq!(files.position(), Some(newer));
        // A write relayed here removes the claim; the poll under way still
        // writes its answer.
        fs::remove_file(&files.claim).unwrap();
        let after = Position::kept("1.0123456789abcdef.10".into());
        assert!(files.answer(&second, &after).unwrap());
        assert_eq!(files.position(), Some(after));
    }

    #[test]
    fn a_claim_taken_a_moment_ago_is_not_taken_again() {
        let root = tempfile::tempdir().unwrap();
        let files = Files::in_dir(root.path(), URL);
        // A hook ran lately: one poll's interval.
        touch(&files.wanted).unwrap();
        // No claim yet.
        assert!(files.take().is_some());
        assert!(files.take().is_none(), "taken a moment ago");
        // A claim whose connector is gone.
        aged(&files.claim, REMOTE_MARKER_POLL * STALE_AFTER_POLLS);
        assert!(files.take().is_some());
        assert!(files.take().is_none(), "taken a moment ago");
        let mut left: Vec<_> = fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                files.claim.clone(),
                files.claim_lock.clone(),
                files.wanted.clone()
            ],
            "one claim, its lock and the hook's file"
        );
    }

    /// Connectors that take a due claim at once: as many as start together.
    const TAKERS: usize = 8;
    /// Rounds of [`TAKERS`] taking at once, enough to meet a race between
    /// the look and the write of a claim taken without the lock.
    const TAKE_ROUNDS: usize = 200;

    #[test]
    fn one_of_many_connectors_taking_a_due_claim_at_once_has_it() {
        for round in 0..TAKE_ROUNDS {
            let root = tempfile::tempdir().unwrap();
            let files = Files::in_dir(root.path(), URL);
            touch(&files.wanted).unwrap();
            // Due with no claim yet, and with one whose connector is gone.
            if round % 2 == 1 {
                files.take().unwrap();
                aged(&files.claim, REMOTE_MARKER_POLL * STALE_AFTER_POLLS);
            }
            let start = std::sync::Barrier::new(TAKERS);
            let won = std::thread::scope(|scope| {
                let takers: Vec<_> = (0..TAKERS)
                    .map(|_| {
                        scope.spawn(|| {
                            start.wait();
                            files.take().is_some()
                        })
                    })
                    .collect();
                takers
                    .into_iter()
                    .map(|taker| taker.join().is_ok_and(|won| won))
                    .filter(|&won| won)
                    .count()
            });
            assert_eq!(won, 1, "round {round}");
        }
    }

    #[test]
    fn an_old_take_left_by_an_earlier_version_is_removed_and_a_young_one_kept() {
        let root = tempfile::tempdir().unwrap();
        let files = Files::in_dir(root.path(), URL);
        files.take().unwrap();
        let with = |name: &str| files.claim.with_extension(format!("{CLAIM}.{name}"));
        for name in ["4242", "4343", "notapid"] {
            touch(&with(name)).unwrap();
        }
        aged(&with("4242"), REMOTE_MARKER_POLL * STALE_AFTER_POLLS);
        aged(&with("notapid"), REMOTE_MARKER_POLL * STALE_AFTER_POLLS);
        files.remove_old_takes();
        assert!(!with("4242").exists(), "an old leftover");
        assert!(
            with("4343").exists(),
            "maybe an older connector's, mid-take"
        );
        assert!(with("notapid").exists(), "not one of these files");
        assert!(files.claim.exists() && files.claim_lock.exists());
    }
}
