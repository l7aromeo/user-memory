//! The delivery index's files on this machine (issue #175), in the state
//! folder's `delivery` folder:
//!
//! - for each store and scope, `<store>-<scope>.index`, the base, and
//!   `<store>-<scope>.overlay` (see [`super::delivery`]), where `<store>` is
//!   the store's key ([`paths::store_key`]) and `<scope>` [`scope_key`] of
//!   the scope's projects;
//! - for each store, `<store>.lock`, the lock every writer of the store's
//!   files holds from reading the old files to its last rename, never
//!   deleted, and at times `<store>.void`, the names of files readers take
//!   as absent.
//!
//! Readers take no lock: they read the void mark, then the overlay, then the
//! base ([`Files::open`]). Writers rename a base before its overlay, so a
//! reader never pairs a base with an overlay made for another.
//!
//! A store write that removes or replaces memory text takes the lock before
//! its commit, under the store's write turn, voids the files holding that
//! text and builds them again after ([`before_removal`]). Everything else
//! that holds the lock only reads the store, so nothing under the lock ever
//! waits for the write turn.
//!
//! Every file here is private ([`private`]) and read whole, never mapped,
//! so a reader holds a file only while it reads it.

use super::{
    Position,
    delivery::{DeliveryIndex, View},
};
use crate::{paths, private};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, Seek, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

/// The folder in the state folder.
const FOLDER: &str = "delivery";

/// Hex characters of a scope key: 64 bits of the SHA-256 of the scope's
/// project keys, so two scopes of one store never share files, without a
/// project's name in a file's name.
const SCOPE_KEY_CHARS: usize = 16;

/// How often a writer that waits for the lock tries it again: the standard
/// library's lock has no time limit, and a live holder lets go within
/// milliseconds.
const LOCK_POLL: Duration = Duration::from_millis(5);

/// How long a rename or a delete is tried again on Windows while another
/// program, such as a virus scanner or a backup tool, has the file open
/// without sharing delete: a scanner holds a 3 MB file well under a
/// second, and a removal's reply waits for it. Elsewhere an open file
/// never stops either.
#[cfg(windows)]
pub(crate) const DELIVERY_REPLACE_WAIT: Duration = Duration::from_secs(2);

/// How long a store's delivery file may go unwritten before session start
/// deletes it: a scope no session used for a day keeps no copy of its
/// memories' terms on this machine, and building it again costs about 48 ms
/// at 10,000 memories.
pub(crate) const DELIVERY_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// The most memories an overlay holds the rows of. Overlays of 100, 500 and
/// 2,000 memories measured 61, 238 and 746 KiB at 10,000 memories; a lookup
/// in one of 1,000 costs a few milliseconds more than in an empty one.
/// Memories past it are hidden from the base and wait for session start's
/// new base: late, never wrong.
pub(crate) const OVERLAY_MEMORIES: usize = 1_000;

/// [`OVERLAY_MEMORIES`], which debug builds take from
/// `USER_MEMORY_TEST_OVERLAY_MEMORIES`, so a test reaches the bound with a
/// few memories.
fn overlay_memories() -> usize {
    #[cfg(debug_assertions)]
    if let Some(bound) = std::env::var("USER_MEMORY_TEST_OVERLAY_MEMORIES")
        .ok()
        .and_then(|bound| bound.parse().ok())
    {
        return bound;
    }
    OVERLAY_MEMORIES
}

/// How long a write that removes memory text waits for its store's
/// delivery lock before it refuses. Every live holder lets go within its own
/// limit: a tool hook within its 100 ms, the prompt hook within its limit,
/// session start within 5 s, another removal once it built its bases again
/// (under half a second). Twice the longest, so only a stopped or hung
/// holder makes a removal give up.
pub(crate) const DELIVERY_LOCK_WAIT: Duration = Duration::from_secs(10);

/// [`DELIVERY_LOCK_WAIT`], which debug builds take in seconds from
/// `USER_MEMORY_TEST_DELIVERY_LOCK_WAIT`, so a test of a refusal is short.
fn delivery_lock_wait() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(secs) = std::env::var("USER_MEMORY_TEST_DELIVERY_LOCK_WAIT")
        .ok()
        .and_then(|secs| secs.parse().ok())
    {
        return Duration::from_secs(secs);
    }
    DELIVERY_LOCK_WAIT
}

/// Names, in debug builds only, a switch that makes a removing write leave
/// the delivery files as they are, as a binary that knows nothing of them
/// does, for the tests of the hooks' removal check.
#[cfg(debug_assertions)]
const NO_DELIVERY: &str = "USER_MEMORY_TEST_NO_DELIVERY";

/// Names, in debug builds only, the file that holds a removing write after
/// its commit and before it publishes the store's marker, so a test can
/// kill it there.
#[cfg(debug_assertions)]
pub(crate) const HOLD_REMOVAL: &str = "USER_MEMORY_HOLD_REMOVAL";

#[cfg(debug_assertions)]
thread_local! {
    /// How many delivery locks this thread holds, so debug builds can check
    /// that no store write turn is taken under one.
    static LOCKS_HELD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Whether this thread holds a delivery lock. The store's write turn always
/// comes first: a write waiting for the turn under the lock could wait for
/// a removal that waits for the lock.
#[cfg(debug_assertions)]
pub(crate) fn holds_lock() -> bool {
    LOCKS_HELD.get() > 0
}

/// The extensions of the files.
const BASE: &str = "index";
const OVERLAY: &str = "overlay";
const LOCK: &str = "lock";
const VOID: &str = "void";
/// The extension of a file being written, after the writer's process id.
const PARTIAL: &str = "partial";

/// A scope as the files name it: the session's projects, sorted, each once.
pub(crate) fn scope(projects: &[String]) -> Vec<String> {
    let mut scope = projects.to_vec();
    scope.sort();
    scope.dedup();
    scope
}

/// The key that names a scope's files: see [`SCOPE_KEY_CHARS`].
pub(crate) fn scope_key(scope: &[String]) -> String {
    crate::setup::sha256(scope.join("\n").as_bytes())[..SCOPE_KEY_CHARS].to_owned()
}

/// The delivery files of one store.
pub(crate) struct Files {
    dir: PathBuf,
    key: String,
}

/// A store's lock, held until dropped: the operating system lets it go when
/// its holder exits or is killed too.
pub(crate) struct Lock {
    _file: File,
}

#[cfg(debug_assertions)]
impl Drop for Lock {
    fn drop(&mut self) {
        LOCKS_HELD.set(LOCKS_HELD.get() - 1);
    }
}

impl Files {
    /// The files of the store named `identity` ([`paths::store_identity`]);
    /// none without a state folder.
    pub(crate) fn of(identity: &str) -> Option<Self> {
        Some(Self::in_dir(paths::state_dir()?.join(FOLDER), identity))
    }

    fn in_dir(dir: PathBuf, identity: &str) -> Self {
        Self {
            dir,
            key: paths::store_key(identity),
        }
    }

    fn file(&self, scope: &[String], extension: &str) -> PathBuf {
        self.dir
            .join(format!("{}-{}.{extension}", self.key, scope_key(scope)))
    }

    /// The base of `scope` (see [`scope`]).
    pub(crate) fn base(&self, scope: &[String]) -> PathBuf {
        self.file(scope, BASE)
    }

    /// The overlay of `scope`.
    pub(crate) fn overlay(&self, scope: &[String]) -> PathBuf {
        self.file(scope, OVERLAY)
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.join(format!("{}.{LOCK}", self.key))
    }

    fn void_path(&self) -> PathBuf {
        self.dir.join(format!("{}.{VOID}", self.key))
    }

    /// The lock file, open and private, created with its folder.
    fn lock_file(&self) -> io::Result<File> {
        private::create_dir(&self.dir)?;
        private::create_file(
            OpenOptions::new().read(true).write(true).create(true),
            &self.lock_path(),
        )
    }

    /// The lock, taken now or not at all: none while another holds it.
    pub(crate) fn try_lock(&self) -> io::Result<Option<Lock>> {
        let file = self.lock_file()?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self::held(file))),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(err)) => Err(err),
        }
    }

    /// The lock, waited for until `deadline`: none if another still holds
    /// it then.
    pub(crate) fn lock_until(&self, deadline: Instant) -> io::Result<Option<Lock>> {
        let file = self.lock_file()?;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Some(Self::held(file))),
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(err)) => return Err(err),
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            std::thread::sleep(LOCK_POLL.min(left));
        }
    }

    /// Writes this process's id into the lock file it now holds, so a
    /// writer that gives up waiting can name the holder. Only informative:
    /// a lock with no id in it is held all the same.
    fn held(mut file: File) -> Lock {
        let _ = file
            .set_len(0)
            .and_then(|()| file.rewind())
            .and_then(|()| write!(file, "{}", std::process::id()));
        #[cfg(debug_assertions)]
        LOCKS_HELD.set(LOCKS_HELD.get() + 1);
        Lock { _file: file }
    }

    /// The process id the lock's holder wrote; none when the file holds
    /// none, or cannot be read while held, as on Windows.
    pub(crate) fn holder(&self) -> Option<u32> {
        fs::read_to_string(self.lock_path())
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// The names of the files the void mark says readers take as absent.
    pub(crate) fn voided(&self) -> Vec<String> {
        fs::read_to_string(self.void_path())
            .map(|text| text.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    /// Makes the void mark name `names`, and removes it when there are
    /// none.
    pub(crate) fn void(&self, lock: &Lock, names: &[String]) -> io::Result<()> {
        if names.is_empty() {
            return match self.remove(lock, &self.void_path()) {
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
                removed => removed,
            };
        }
        self.write(lock, &self.void_path(), names.join("\n").as_bytes())
    }

    /// Replaces the file at `path` with `bytes`, whole: written to a file of
    /// its own beside it, then renamed over it.
    pub(crate) fn write(&self, _lock: &Lock, path: &Path, bytes: &[u8]) -> io::Result<()> {
        private::create_dir(&self.dir)?;
        let name = path
            .file_name()
            .ok_or(io::ErrorKind::InvalidInput)?
            .to_string_lossy();
        let partial = path.with_file_name(format!("{name}.{}.{PARTIAL}", std::process::id()));
        let written = private::create_file(
            OpenOptions::new().write(true).create(true).truncate(true),
            &partial,
        )
        .and_then(|mut file| file.write_all(bytes))
        .and_then(|()| replace(&partial, path));
        if written.is_err() {
            let _ = fs::remove_file(&partial);
        }
        written
    }

    /// Deletes the file at `path`.
    pub(crate) fn remove(&self, _lock: &Lock, path: &Path) -> io::Result<()> {
        remove(path)
    }

    /// Deletes `scope`'s base, then its overlay; a file that cannot be
    /// deleted is named in the void mark, so no reader uses it.
    pub(crate) fn clear(&self, lock: &Lock, scope: &[String]) -> io::Result<()> {
        let mut left = self.voided();
        for path in [self.base(scope), self.overlay(scope)] {
            match self.remove(lock, &path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(_) => {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    if !left.iter().any(|v| *v == name) {
                        left.push(name.into_owned());
                    }
                }
            }
        }
        self.void(lock, &left)
    }

    /// This store's files in the folder, by name.
    fn own(&self) -> Vec<(String, PathBuf)> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let (scoped, single) = (format!("{}-", self.key), format!("{}.", self.key));
        entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                (name.starts_with(&scoped) || name.starts_with(&single))
                    .then(|| (name, entry.path()))
            })
            .collect()
    }

    /// What a writer holding the lock clears before its own work: the files
    /// the void mark names, then the mark once none is left; every file
    /// being written, since only the holder can have one under way, so any
    /// other was left by a writer that was killed; and, when `aged` is
    /// given, every base and overlay not written for [`DELIVERY_AGE`]
    /// before it. A file that cannot be deleted stays named in the mark.
    pub(crate) fn tidy(&self, lock: &Lock, aged: Option<SystemTime>) {
        let mut left = Vec::new();
        for name in self.voided() {
            match self.remove(lock, &self.dir.join(&name)) {
                Err(err) if err.kind() != io::ErrorKind::NotFound => left.push(name),
                _ => {}
            }
        }
        let _ = self.void(lock, &left);
        for (name, path) in self.own() {
            let partial = name.ends_with(&format!(".{PARTIAL}"));
            let old = aged.is_some_and(|now| {
                let scoped = path.extension().is_some_and(|e| e == BASE || e == OVERLAY);
                let written = fs::metadata(&path).and_then(|m| m.modified());
                scoped
                    && written.is_ok_and(|written| {
                        now.duration_since(written).unwrap_or_default() > DELIVERY_AGE
                    })
            });
            if partial || old {
                let _ = self.remove(lock, &path);
            }
        }
    }

    /// The base of `scope` and the overlay made for it, as a reader takes
    /// them, the store being at `now`: none when the base is missing,
    /// voided, damaged, of another scope, or of another store or a position
    /// past `now` (a store put back by hand). The overlay is read before the
    /// base, so one renamed between the two reads is left out (see
    /// [`View::new`]).
    pub(crate) fn open(&self, scope: &[String], now: &Position) -> Option<View> {
        let voided = self.voided();
        let readable = |path: &Path| {
            let name = path.file_name().map(|name| name.to_string_lossy());
            !voided.iter().any(|v| Some(v.as_str()) == name.as_deref())
        };
        let overlay = self.overlay(scope);
        let overlay = readable(&overlay)
            .then(|| DeliveryIndex::load(&overlay).ok())
            .flatten();
        let base = self.base(scope);
        if !readable(&base) {
            return None;
        }
        let base = DeliveryIndex::load(&base).ok()?;
        let held = Position::kept(base.header.position.clone()).within(now);
        if !held || base.header.projects != scope {
            return None;
        }
        View::new(base, overlay)
    }
}

/// What session start or the prompt hook did with its scope's delivery
/// index.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Started {
    /// The base was there for the store's position, with no overlay to fold.
    Current,
    /// A new base was built and renamed into place.
    Built,
    /// Another writer kept the lock past the deadline, the store's keyword
    /// index was not current, or the build ended past the deadline: nothing
    /// was renamed, and the next prompt or session start builds it.
    Left,
}

/// Session start's part (#175), for the local store at `db` and the
/// session's `projects`: under the store's lock, waited for until
/// `deadline`, clears what [`Files::tidy`] clears with the files a day old,
/// then builds the scope's base again when it is missing, unusable, or has
/// an overlay that holds a change.
pub(crate) fn at_session_start(
    db: &Path,
    projects: &[String],
    deadline: Instant,
) -> io::Result<Started> {
    build_base(
        db,
        projects,
        deadline,
        Some(SystemTime::now()),
        Keep::Folded,
    )
}

/// The prompt hook's part (#175), for the local store at `db` and the
/// session's `projects`: refreshes the scope's overlay, waiting for the lock
/// until `deadline`, and builds the scope's base when there is none it can
/// use: a new scope after `link` or `unlink`, files deleted by a project
/// move, `migrate` or a removal, a store put back by hand, or a position the
/// store no longer counts the changes since.
pub(crate) fn at_prompt(db: &Path, projects: &[String], deadline: Instant) -> io::Result<Started> {
    match refresh(db, projects, deadline, true)? {
        Refreshed::NoBase | Refreshed::Behind | Refreshed::Cleared => {
            build_base(db, projects, deadline, None, Keep::Current)
        }
        Refreshed::Busy | Refreshed::Late => Ok(Started::Left),
        Refreshed::Unchanged | Refreshed::Done => Ok(Started::Current),
    }
}

/// When a scope's usable base and its overlay are kept as they are.
#[derive(Clone, Copy)]
enum Keep {
    /// At session start: while the overlay holds no change, so each session
    /// starts on a base with every change folded in.
    Folded,
    /// At the prompt: while the overlay holds the store's latest change. A
    /// base whose overlay the store can no longer bring up to date is built
    /// again; one another hook built meanwhile is kept.
    Current,
}

/// Under the store's lock, waited for until `deadline`: clears what
/// [`Files::tidy`] clears (with the files older than [`DELIVERY_AGE`]
/// before `aged`, when given), then builds the scope's base unless there is
/// a usable one to `keep`, and puts an empty overlay made for it beside it.
/// The build reads the store; it is never renamed into place past
/// `deadline`.
fn build_base(
    db: &Path,
    projects: &[String],
    deadline: Instant,
    aged: Option<SystemTime>,
    keep: Keep,
) -> io::Result<Started> {
    let Some(files) = Files::of(&paths::store_identity(None, db)) else {
        return Ok(Started::Left);
    };
    let scope = scope(projects);
    let Some(lock) = files.lock_until(deadline)? else {
        return Ok(Started::Left);
    };
    files.tidy(&lock, aged);
    let Some(now) = super::changes::position(db) else {
        return Ok(Started::Left);
    };
    let current = files.open(&scope, &now).is_some_and(|view| match keep {
        Keep::Folded => view
            .overlay
            .as_ref()
            .is_none_or(|overlay| overlay.len() == 0 && overlay.header.skip.is_empty()),
        Keep::Current => view.overlay.as_ref().is_some_and(|overlay| {
            Position::kept(overlay.header.position.clone()).same_change(&now)
        }),
    });
    if current {
        return Ok(Started::Current);
    }
    let Some((built, empty)) = base_and_overlay(db, &scope) else {
        return Ok(Started::Left);
    };
    if Instant::now() > deadline {
        return Ok(Started::Left);
    }
    files.write(&lock, &files.base(&scope), &built)?;
    files.write(&lock, &files.overlay(&scope), &empty)?;
    Ok(Started::Built)
}

/// What a hook's refresh of its scope's overlay did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refreshed {
    /// The overlay already holds the store's latest change.
    Unchanged,
    /// A new overlay was renamed into place.
    Done,
    /// Another writer holds the lock: nothing was written, and the files
    /// are used as they are.
    Busy,
    /// There is no usable base: the next prompt or session start builds
    /// one.
    NoBase,
    /// The store no longer says what changed since the base, or its keyword
    /// index is not current: nothing was written, and the next prompt or
    /// session start builds the base again.
    Behind,
    /// The deadline passed before the rename: nothing was renamed.
    Late,
    /// The base held words a removal took out, so it and its overlay were
    /// deleted.
    Cleared,
}

/// A hook's refresh of its scope's overlay (#175), for the local store at
/// `db` and the scope `projects`: when the store's marker names a change
/// the overlay does not hold, takes the store's lock (once, unless `wait`,
/// then until `deadline`), reads the old files again under it, and renames
/// a new overlay of the memories changed since the base's position into
/// place, unless `deadline` has passed. A base holding words a removal took
/// out is deleted with its overlay instead. Never builds a base.
pub(crate) fn refresh(
    db: &Path,
    projects: &[String],
    deadline: Instant,
    wait: bool,
) -> io::Result<Refreshed> {
    let Some(files) = Files::of(&paths::store_identity(None, db)) else {
        return Ok(Refreshed::NoBase);
    };
    let scope = scope(projects);
    // Before the lock: one small read says whether there is anything to do.
    let marked = super::changes::marked(db);
    let held = DeliveryIndex::load(&files.overlay(&scope)).ok();
    let unchanged = marked
        .as_ref()
        .zip(held.as_ref())
        .is_some_and(|(marked, overlay)| {
            Position::kept(overlay.header.position.clone()).same_change(marked)
        });
    // A base deleted beside its overlay, by hand or by another program,
    // is built again by the prompt hook, so it is looked for too.
    if unchanged && files.voided().is_empty() && files.base(&scope).exists() {
        return Ok(Refreshed::Unchanged);
    }
    let lock = if wait {
        files.lock_until(deadline)?
    } else {
        files.try_lock()?
    };
    let Some(lock) = lock else {
        return Ok(Refreshed::Busy);
    };
    files.tidy(&lock, None);
    let Some(now) = super::changes::position(db) else {
        return Ok(Refreshed::NoBase);
    };
    let Some(view) = files.open(&scope, &now) else {
        return Ok(Refreshed::NoBase);
    };
    if view
        .overlay
        .as_ref()
        .is_some_and(|overlay| Position::kept(overlay.header.position.clone()).same_change(&now))
    {
        return Ok(Refreshed::Unchanged);
    }
    let since = Position::kept(view.base.header.position.clone())
        .change()
        .unwrap_or_default();
    let overlay =
        match super::delivery::overlay(db, &view.base, since, overlay_memories(), |id, change| {
            Position::at(id, change, 0).as_str().to_owned()
        }) {
            Ok(overlay) => overlay,
            Err(super::delivery::Error::Pruned | super::delivery::Error::NotCurrent) => {
                return Ok(Refreshed::Behind);
            }
            Err(super::delivery::Error::Store(_) | super::delivery::Error::File(_)) => {
                return Ok(Refreshed::Busy);
            }
            Err(super::delivery::Error::Format) => return Ok(Refreshed::NoBase),
        };
    #[cfg(debug_assertions)]
    super::hold_for_tests(HOLD_REFRESH);
    if overlay.stale {
        files.clear(&lock, &scope)?;
        return Ok(Refreshed::Cleared);
    }
    if Instant::now() > deadline {
        return Ok(Refreshed::Late);
    }
    files.write(&lock, &files.overlay(&scope), &overlay.bytes)?;
    Ok(Refreshed::Done)
}

/// Names, in debug builds only, the file that holds a hook's refresh after
/// it took the lock and built its overlay from the store, before it renames
/// it, so a test can run a removal against a hook holding the lock.
#[cfg(debug_assertions)]
pub(crate) const HOLD_REFRESH: &str = "USER_MEMORY_HOLD_REFRESH";

/// Names, in debug builds only, a file name that renames and deletes in the
/// delivery folder then fail on, as on Windows while another program holds
/// the file open without sharing delete, so every platform tests it.
#[cfg(debug_assertions)]
const HELD_FILE: &str = "USER_MEMORY_TEST_HELD_FILE";

/// In debug builds, the error [`HELD_FILE`] makes for `path`.
fn held_for_tests(path: &Path) -> io::Result<()> {
    #[cfg(debug_assertions)]
    if let Some(held) = std::env::var_os(HELD_FILE)
        && path.file_name() == Some(held.as_os_str())
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let _ = path;
    Ok(())
}

/// Renames `from` over `to`, on Windows trying again for up to
/// [`DELIVERY_REPLACE_WAIT`] while another program holds either file.
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    held_for_tests(to)?;
    #[cfg(windows)]
    return crate::platform::windows::rename(from, to, DELIVERY_REPLACE_WAIT);
    #[cfg(not(windows))]
    fs::rename(from, to)
}

/// Deletes `path`, on Windows trying again as [`replace`] does.
fn remove(path: &Path) -> io::Result<()> {
    held_for_tests(path)?;
    #[cfg(windows)]
    return crate::platform::windows::remove_file(path, DELIVERY_REPLACE_WAIT);
    #[cfg(not(windows))]
    fs::remove_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::delivery::{self, Part};

    #[test]
    fn files_left_add_their_paths_and_a_line_to_the_reply() {
        let mut reply = serde_json::json!({"deleted": true, "next": "Undo with import."});
        Left::default().note(&mut reply);
        assert_eq!(
            reply,
            serde_json::json!({"deleted": true, "next": "Undo with import."})
        );
        let left = |marked| Left {
            paths: vec![PathBuf::from("a.index")],
            marked,
        };
        left(true).note(&mut reply);
        assert_eq!(reply["delivery_left"], serde_json::json!(["a.index"]));
        assert_eq!(reply["next"], format!("Undo with import.\n{LEFT_MARKED}"));
        let mut reply = serde_json::json!({"updated": true});
        left(false).note(&mut reply);
        assert_eq!(reply["next"], LEFT_UNMARKED);
    }

    #[test]
    #[should_panic(expected = "a store write turn is never taken under a delivery lock")]
    fn a_store_write_under_a_delivery_lock_is_a_broken_invariant() {
        let dir = tempfile::tempdir().unwrap();
        let files = Files::in_dir(dir.path().join(FOLDER), "a store");
        let _lock = files.try_lock().unwrap().unwrap();
        let conn = rusqlite::Connection::open(dir.path().join("memories.sqlite3")).unwrap();
        let _ = crate::memory::schema::write(&conn);
    }

    /// A store at `dir` holding a stated memory of the ledger project and a
    /// global one.
    fn store(dir: &Path) -> PathBuf {
        let db = dir.join("memories.sqlite3");
        for (title, content, scope) in [
            (
                "Hangar door",
                "The zeppelin hangar's glacier door sticks.",
                "project",
            ),
            (
                "Quotes",
                "Prefer straight quotes in every document.",
                "global",
            ),
        ] {
            let input = serde_json::json!({
                "type": "fact", "category": "build", "title": title, "content": content,
                "scope": scope, "project": if scope == "project" { LEDGER } else { "" },
                "user_confirmed": true,
            });
            let out = crate::memory::record(&db, serde_json::from_value(input).unwrap()).unwrap();
            assert_eq!(out["stored"], true, "{out}");
        }
        db
    }

    const LEDGER: &str = "github.com/example/ledger";

    fn ledger() -> Vec<String> {
        scope(&[LEDGER.to_owned()])
    }

    /// The base file of the ledger's scope in the store at `db`, as built
    /// now.
    fn base(db: &Path) -> Vec<u8> {
        delivery::build(db, &ledger(), |id, change| {
            Position::at(id, change, 0).as_str().to_owned()
        })
        .unwrap()
    }

    /// An overlay of no memories made for `base`.
    fn overlay_for(base: &[u8]) -> Vec<u8> {
        let base = DeliveryIndex::read(base.to_vec()).unwrap();
        let mut header = base.header.clone();
        header.part = Part::Overlay;
        header.made_for = base.header.position.clone();
        DeliveryIndex::write_with(&header, &[]).unwrap()
    }

    fn setup() -> (tempfile::TempDir, PathBuf, Files) {
        let dir = tempfile::tempdir().unwrap();
        let db = store(dir.path());
        let files = Files::in_dir(dir.path().join(FOLDER), &paths::store_identity(None, &db));
        (dir, db, files)
    }

    fn now(db: &Path) -> Position {
        crate::memory::store_position(db).unwrap()
    }

    #[test]
    fn a_base_is_read_with_the_overlay_made_for_it_and_no_other() {
        let (_dir, db, files) = setup();
        let lock = files.try_lock().unwrap().unwrap();
        let first = base(&db);
        files.write(&lock, &files.base(&ledger()), &first).unwrap();
        files
            .write(&lock, &files.overlay(&ledger()), &overlay_for(&first))
            .unwrap();
        let view = files.open(&ledger(), &now(&db)).unwrap();
        assert_eq!(view.base.len(), 2);
        assert!(view.overlay.is_some());
        // A writer renames a new base, and a reader comes before its
        // overlay: the old overlay was made for another base.
        let input = serde_json::json!({
            "type": "fact", "category": "build", "title": "Mooring",
            "content": "Moor the zeppelin to the north mast.", "scope": "global",
            "user_confirmed": true,
        });
        // The store is written with no delivery lock held.
        drop(lock);
        crate::memory::record(&db, serde_json::from_value(input).unwrap()).unwrap();
        let lock = files.try_lock().unwrap().unwrap();
        files
            .write(&lock, &files.base(&ledger()), &base(&db))
            .unwrap();
        let view = files.open(&ledger(), &now(&db)).unwrap();
        assert_eq!(view.base.len(), 3);
        assert!(view.overlay.is_none(), "an overlay of the older base");
    }

    #[test]
    fn a_voided_file_is_absent() {
        let (_dir, db, files) = setup();
        let lock = files.try_lock().unwrap().unwrap();
        let built = base(&db);
        files.write(&lock, &files.base(&ledger()), &built).unwrap();
        files
            .write(&lock, &files.overlay(&ledger()), &overlay_for(&built))
            .unwrap();
        let name = |path: PathBuf| path.file_name().unwrap().to_string_lossy().into_owned();
        files
            .void(&lock, &[name(files.overlay(&ledger()))])
            .unwrap();
        let view = files.open(&ledger(), &now(&db)).unwrap();
        assert!(view.overlay.is_none());
        files.void(&lock, &[name(files.base(&ledger()))]).unwrap();
        assert!(files.open(&ledger(), &now(&db)).is_none());
        files.void(&lock, &[]).unwrap();
        assert!(files.open(&ledger(), &now(&db)).is_some());
        assert!(!files.void_path().exists());
    }

    #[test]
    fn a_base_of_another_store_or_a_later_change_is_absent() {
        let (dir, db, files) = setup();
        let lock = files.try_lock().unwrap().unwrap();
        files
            .write(&lock, &files.base(&ledger()), &base(&db))
            .unwrap();
        drop(lock);
        let at = now(&db);
        assert!(files.open(&ledger(), &at).is_some());
        // The store as a backup made before its last change held it.
        let conn = rusqlite::Connection::open(&db).unwrap();
        let state = crate::memory::changes::state(&conn).unwrap().unwrap();
        let earlier = Position::at(&state.store_id, state.last_change - 1, 0);
        assert!(files.open(&ledger(), &earlier).is_none());
        // Another store at the same path.
        let other = store(&dir.path().join("other"));
        assert!(files.open(&ledger(), &now(&other)).is_none());
        // Another scope's files are other files.
        assert!(files.open(&scope(&[]), &at).is_none());
    }

    #[test]
    fn the_lock_is_one_holder_s_at_a_time_and_names_it() {
        let (_dir, _db, files) = setup();
        let held = files.try_lock().unwrap().unwrap();
        assert!(files.try_lock().unwrap().is_none());
        let started = Instant::now();
        let waited = files
            .lock_until(started + Duration::from_millis(50))
            .unwrap();
        assert!(waited.is_none());
        assert!(started.elapsed() >= Duration::from_millis(50));
        #[cfg(not(windows))]
        assert_eq!(files.holder(), Some(std::process::id()));
        drop(held);
        assert!(files.try_lock().unwrap().is_some());
    }
}

/// After a committed write that changed what every scope of the local store
/// at `db` holds, a migration or a project move (#175): deletes the store's
/// bases and overlays, its void mark and its files being written, under its
/// lock, waited for up to [`DELIVERY_LOCK_WAIT`], so the next prompt or
/// session start builds each scope again from the store. A holder that
/// keeps the lock longer keeps the files: a migration gave the store a new
/// id, so no reader uses them, and after a move the overlays say which
/// memories changed.
pub(crate) fn drop_store(db: &Path) {
    let Some(files) = Files::of(&paths::store_identity(None, db)) else {
        return;
    };
    let Ok(Some(lock)) = files.lock_until(Instant::now() + delivery_lock_wait()) else {
        return;
    };
    for (_, path) in files.own() {
        if path != files.lock_path() {
            let _ = files.remove(&lock, &path);
        }
    }
}

/// What a write that removes memory text does, as its refusal names it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Removing {
    Forget,
    Update,
    Import,
}

impl Removing {
    /// The reply when `holder` (a process id, when the lock file names one)
    /// kept the lock for all of `wait`.
    fn refusal(self, holder: Option<u32>, wait: Duration) -> String {
        let (done, verb) = match self {
            Self::Forget => ("forgotten", "forget"),
            Self::Update => ("updated", "update"),
            Self::Import => ("imported", "import"),
        };
        let who = holder.map_or_else(
            || "another user-memory process".to_owned(),
            |pid| format!("user-memory process {pid}"),
        );
        format!(
            "Nothing was {done}: {who} has held this machine's delivery index for {} s. Wait a moment and {verb} again; if it still holds it, stop that process first.",
            wait.as_secs()
        )
    }
}

/// A removing write's hold on its store's delivery files, from before its
/// commit until it built them again: the lock, and the scopes it voided.
pub(crate) struct Removal {
    files: Files,
    lock: Lock,
    /// The scopes whose base or overlay holds a memory the write removed or
    /// rewrote, voided until they are built again.
    scopes: Vec<Vec<String>>,
    /// Files of no scope it could read, voided until they are deleted.
    damaged: Vec<PathBuf>,
    /// What the void mark named before this write.
    kept: Vec<String>,
}

/// Before the commit of a write to the local store at `db` that removes or
/// replaces the text of the memories `touched` (#175): when there are any,
/// takes the store's delivery lock, waiting [`DELIVERY_LOCK_WAIT`], or
/// fails with the reply for `removing` that names the holder, so the write
/// rolls back. Holding it, names in the void mark every file that holds one
/// of them, which readers then take as absent, so neither a hook that took
/// the lock first nor a write killed after its commit leaves their words in
/// use. [`Removal::rebuild`] after the commit builds those files again;
/// [`Removal::abandon`] when the commit fails puts the mark back.
pub(crate) fn before_removal(
    db: &Path,
    touched: &HashSet<String>,
    removing: Removing,
) -> anyhow::Result<Option<Removal>> {
    if touched.is_empty() {
        return Ok(None);
    }
    #[cfg(debug_assertions)]
    if std::env::var_os(NO_DELIVERY).is_some() {
        return Ok(None);
    }
    let Some(files) = Files::of(&paths::store_identity(None, db)) else {
        return Ok(None);
    };
    let wait = delivery_lock_wait();
    let Some(lock) = files.lock_until(Instant::now() + wait)? else {
        anyhow::bail!(removing.refusal(files.holder(), wait));
    };
    files.tidy(&lock, None);
    let kept = files.voided();
    let mut scopes: Vec<Vec<String>> = Vec::new();
    let mut damaged = Vec::new();
    for (_, path) in files.own() {
        if !path.extension().is_some_and(|e| e == BASE || e == OVERLAY) {
            continue;
        }
        match DeliveryIndex::load(&path) {
            Ok(index) => {
                let holds =
                    (0..index.len()).any(|m| index.id(m).is_ok_and(|id| touched.contains(id)));
                if holds && !scopes.contains(&index.header.projects) {
                    scopes.push(index.header.projects);
                }
            }
            Err(_) => damaged.push(path),
        }
    }
    let removal = Removal {
        files,
        lock,
        scopes,
        damaged,
        kept,
    };
    let mut names = removal.kept.clone();
    names.extend(removal.voided().iter().map(|path| name_of(path)));
    if names.len() > removal.kept.len() {
        removal.files.void(&removal.lock, &names)?;
    }
    Ok(Some(removal))
}

/// The file name of `path`, as the void mark keeps it.
fn name_of(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

impl Removal {
    /// The files this removal voided.
    fn voided(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self
            .scopes
            .iter()
            .flat_map(|scope| [self.files.base(scope), self.files.overlay(scope)])
            .collect();
        paths.extend(self.damaged.iter().cloned());
        paths
    }

    /// After the commit, still holding the lock: builds each voided scope's
    /// base from the store at `db` again, each on its own thread, with an
    /// empty overlay made for it; deletes a voided file it could not build
    /// again; then names in the void mark only what is still on disk and
    /// voided, which it returns, and lets the lock go.
    pub(crate) fn rebuild(self, db: &Path) -> Left {
        let built: Vec<Option<(Vec<u8>, Vec<u8>)>> = std::thread::scope(|threads| {
            let builds: Vec<_> = self
                .scopes
                .iter()
                .map(|scope| threads.spawn(move || base_and_overlay(db, scope)))
                .collect();
            builds
                .into_iter()
                .map(|build| build.join().ok().flatten())
                .collect()
        });
        let mut done = HashSet::new();
        for (scope, built) in self.scopes.iter().zip(built) {
            let (base, overlay) = (self.files.base(scope), self.files.overlay(scope));
            if let Some((base_bytes, overlay_bytes)) = built
                && self.files.write(&self.lock, &base, &base_bytes).is_ok()
                && self
                    .files
                    .write(&self.lock, &overlay, &overlay_bytes)
                    .is_ok()
            {
                done.extend([name_of(&base), name_of(&overlay)]);
            }
        }
        let mut left = Vec::new();
        for path in self.voided() {
            if done.contains(&name_of(&path)) {
                continue;
            }
            match self.files.remove(&self.lock, &path) {
                Err(err) if err.kind() != io::ErrorKind::NotFound => left.push(path),
                _ => {}
            }
        }
        let mut names = self.kept.clone();
        names.extend(left.iter().map(|path| name_of(path)));
        let marked = self.files.void(&self.lock, &names).is_ok();
        Left {
            paths: left,
            marked,
        }
    }

    /// When the write failed after its files were voided: the void mark
    /// names again only what it named before, then the lock goes, so the
    /// files are used as they were.
    pub(crate) fn abandon(self) {
        let _ = self.files.void(&self.lock, &self.kept);
    }
}

/// The delivery files a removing write could neither build again nor
/// delete, as another program had them open: they still hold the removed
/// text.
#[derive(Debug, Default)]
pub(crate) struct Left {
    pub(crate) paths: Vec<PathBuf>,
    /// Whether the void mark names them, so readers skip them.
    pub(crate) marked: bool,
}

/// The reply's `next` line when files still hold removed text and the void
/// mark names them.
const LEFT_MARKED: &str = "The files in delivery_left still hold the removed words because another program has them open. Readers skip them; user-memory deletes them at its next write, and `user-memory doctor` deletes them at once.";

/// The reply's `next` line when files still hold removed text and even the
/// void mark could not be written.
const LEFT_UNMARKED: &str = "The files in delivery_left still hold the removed words because another program has them open, and the mark that makes readers skip them could not be written. Close that program, then run `user-memory doctor` to delete them.";

impl Left {
    /// Adds to a removing command's `reply`, when files are left, their
    /// paths as `delivery_left` and a line of `next` saying what happens to
    /// them.
    pub(crate) fn note(&self, reply: &mut serde_json::Value) {
        if self.paths.is_empty() {
            return;
        }
        reply["delivery_left"] = serde_json::json!(self.paths);
        let line = if self.marked {
            LEFT_MARKED
        } else {
            LEFT_UNMARKED
        };
        let next = match reply["next"].as_str() {
            Some(next) if !next.is_empty() => format!("{next}\n{line}"),
            _ => line.to_owned(),
        };
        reply["next"] = serde_json::json!(next);
    }
}

/// What doctor's clean-up of a store's delivery files did.
#[derive(Debug, Default)]
pub(crate) struct Cleaned {
    /// How many voided or half-written files it deleted.
    pub(crate) deleted: usize,
    /// The voided files another program still holds open.
    pub(crate) held: Vec<PathBuf>,
}

/// Doctor's part (#175), for the local store at `db`: under the store's
/// lock, waited for up to [`DELIVERY_LOCK_WAIT`], deletes what
/// [`Files::tidy`] deletes: the files the void mark names, which may hold
/// removed text, and those a killed writer left half written. None when
/// the lock could not be had.
pub(crate) fn clean(db: &Path) -> Option<Cleaned> {
    let files = Files::of(&paths::store_identity(None, db))?;
    if !files.dir.exists() {
        return Some(Cleaned::default());
    }
    let lock = files
        .lock_until(Instant::now() + delivery_lock_wait())
        .ok()
        .flatten()?;
    let partial = format!(".{PARTIAL}");
    let before: Vec<PathBuf> = files
        .voided()
        .iter()
        .map(|name| files.dir.join(name))
        .chain(
            files
                .own()
                .into_iter()
                .filter(|(name, _)| name.ends_with(&partial))
                .map(|(_, path)| path),
        )
        .filter(|path| path.exists())
        .collect();
    files.tidy(&lock, None);
    let held: Vec<PathBuf> = files
        .voided()
        .iter()
        .map(|name| files.dir.join(name))
        .filter(|path| path.exists())
        .collect();
    Some(Cleaned {
        deleted: before.iter().filter(|path| !path.exists()).count(),
        held,
    })
}

/// A new base of `scope` from the store at `db`, and an empty overlay made
/// for it; none when the store's keyword index is not current.
fn base_and_overlay(db: &Path, scope: &[String]) -> Option<(Vec<u8>, Vec<u8>)> {
    let base = super::delivery::build(db, scope, |id, change| {
        Position::at(id, change, 0).as_str().to_owned()
    })
    .ok()?;
    let header = DeliveryIndex::read(base.clone()).ok()?.header;
    let overlay = super::delivery::empty_overlay(&header).ok()?;
    Some((base, overlay))
}
