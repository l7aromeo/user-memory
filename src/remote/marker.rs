//! The marker a memory server's connector keeps for the hooks on this
//! machine: the server's position as its last poll found it. A prompt whose
//! snapshot holds that position asks the server nothing. The connector keeps
//! no copy of the server's memories; which ones a session sees is decided by
//! the server alone.

use super::Connection;
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

/// Polls a quiet machine makes in [`REMOTE_MARKER_POLL`]'s stead: once a
/// minute, so a machine left open asks the server little.
const QUIET_POLLS: u32 = 12;
/// Polls' worth of time after the last hook before the connector slows down:
/// two minutes, longer than the pause between two prompts of a session.
const QUIET_AFTER_POLLS: u32 = 24;
/// Polls a marker may miss before it no longer counts: its connector is
/// down or cut off, or slowed for a quiet spell, so a hook asks the server
/// itself and no change is late.
const STALE_AFTER_POLLS: u32 = 3;

/// Times a connector looks at the marker's age in each poll: a due poll
/// starts at most a tenth of a poll late, and the look is two file stats.
const TICKS_PER_POLL: u32 = 10;

/// Hex characters of the server key in the marker's file name: 64 bits of
/// the SHA-256 of the server's address, so two servers never share a
/// marker, without the address in the name.
const KEY_CHARS: usize = 16;

/// The marker's extension.
const MARKER: &str = "changed";
/// The extension of the file whose time says when a hook last ran.
const WANTED: &str = "wanted";
/// The extension of the file a connector touches as it starts a poll, so
/// another connector's tick in the meantime does not poll too.
const CLAIM: &str = "polling";

/// [`REMOTE_MARKER_POLL`], which debug builds take in milliseconds from
/// `USER_MEMORY_MARKER_POLL_MS` so tests can see the quiet interval and a
/// stale marker without waiting minutes.
fn poll_every() -> Duration {
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
    fn due(&self, now: SystemTime) -> bool {
        let every = poll_every();
        let quiet = age(&self.wanted, now).is_none_or(|age| age > every * QUIET_AFTER_POLLS);
        let interval = if quiet { every * QUIET_POLLS } else { every };
        let old = |path: &Path| age(path, now).is_none_or(|age| age >= interval);
        old(&self.marker) && old(&self.claim)
    }
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
pub async fn poll(connection: Connection) {
    let Some(files) = Files::of(&connection.url) else {
        return;
    };
    // The version of a server that answered with no position.
    let mut without_positions: Option<serde_json::Value> = None;
    let mut last_asked: Option<tokio::time::Instant> = None;
    loop {
        let woken = tokio::select! {
            () = tokio::time::sleep(poll_every() / TICKS_PER_POLL) => false,
            () = WROTE.notified() => true,
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
        last_asked = Some(tokio::time::Instant::now());
        let _ = touch(&files.claim);
        if let Some(old) = &without_positions {
            if version(&connection).await.is_none_or(|now| &now == old) {
                continue;
            }
            without_positions = None;
        }
        let input = serde_json::json!({
            "stamps": true,
            "stamps_since": files.position().unwrap_or_default(),
        });
        let Some(reply) = connection
            .call("index", input)
            .await
            .ok()
            .and_then(|text| parse_stamps_reply(&text))
        else {
            // Unreachable for now: the marker ages out and the hooks ask
            // the server themselves.
            continue;
        };
        match reply.since {
            Some(position) => {
                let _ = files.write(&position);
            }
            None => {
                let _ = fs::remove_file(&files.marker);
                without_positions = Some(version(&connection).await.unwrap_or_default());
            }
        }
    }
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
}
