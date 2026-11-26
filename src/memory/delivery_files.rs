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
use anyhow::Context;
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

/// How long a read of a delivery file tries again while Windows refuses to
/// open it ([`crate::platform::windows::refused`]): a name another program
/// deleted without POSIX semantics (a FAT or network volume, or an older
/// user-memory) until its last reader closes, or a file a program opened
/// without sharing, as a scanner does for a moment. Short, as a refusal is,
/// and well inside the tool hook's 100 ms.
#[cfg(windows)]
const REFUSED_READ_WAIT: Duration = Duration::from_millis(20);

/// The pause between two tries of a refused read.
#[cfg(windows)]
const REFUSED_READ_PAUSE: Duration = Duration::from_millis(2);

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

    /// The names of the files the void mark says readers take as absent;
    /// none without a mark or with an empty one. An error when the mark is
    /// there but cannot be read: a removal's mark is never taken as naming
    /// nothing, so a reader then uses none of the files, and a writer
    /// changes nothing.
    pub(crate) fn voided(&self) -> io::Result<Vec<String>> {
        match read(&self.void_path()) {
            Ok(bytes) => Ok(String::from_utf8_lossy(&bytes)
                .lines()
                .map(str::to_owned)
                .collect()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(err) => Err(err),
        }
    }

    /// Makes the void mark name `names`. A mark that comes to name nothing
    /// is written empty, never deleted: on Windows a delete refuses every
    /// open of the name for a moment, and readers open the mark at every
    /// step (#218). With no mark and nothing to name, none is made.
    pub(crate) fn void(&self, lock: &Lock, names: &[String]) -> io::Result<()> {
        if names.is_empty() {
            match read(&self.void_path()) {
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
                Ok(bytes) if bytes.is_empty() => return Ok(()),
                _ => {}
            }
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
            let _ = remove(&partial);
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
        let mut left = self.voided()?;
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
    /// the void mark names, then the names, once none is left; every file
    /// being written, since only the holder can have one under way, so any
    /// other was left by a writer that was killed; and, when `aged` is
    /// given, every base and overlay not written for [`DELIVERY_AGE`]
    /// before it. A file that cannot be deleted stays named in the mark.
    pub(crate) fn tidy(&self, lock: &Lock, aged: Option<SystemTime>) {
        // A mark that cannot be read stays as it is, naming what it names.
        let Ok(voided) = self.voided() else {
            return;
        };
        let mut left = Vec::new();
        for name in voided {
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
        let view = self.view(scope)?;
        Position::kept(view.base.header.position.clone())
            .within(now)
            .then_some(view)
    }

    /// The base of `scope` and the overlay made for it, as a reader takes
    /// them, whatever the base's position: [`Files::open`] without its check
    /// against the store.
    fn view(&self, scope: &[String]) -> Option<View> {
        let voided = self.voided().ok()?;
        let readable = |path: &Path| {
            let name = path.file_name().map(|name| name.to_string_lossy());
            !voided.iter().any(|v| Some(v.as_str()) == name.as_deref())
        };
        let overlay = self.overlay(scope);
        let overlay = readable(&overlay).then(|| load(&overlay).ok()).flatten();
        let base = self.base(scope);
        if !readable(&base) {
            return None;
        }
        let base = load(&base).ok()?;
        if base.header.projects != scope {
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
    let held = load(&files.overlay(&scope)).ok();
    let unchanged = marked
        .as_ref()
        .zip(held.as_ref())
        .is_some_and(|(marked, overlay)| {
            Position::kept(overlay.header.position.clone()).same_change(marked)
        });
    // A base deleted beside its overlay, by hand or by another program,
    // is built again by the prompt hook, so it is looked for too.
    if unchanged && files.voided().is_ok_and(|v| v.is_empty()) && files.base(&scope).exists() {
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

/// Names, in debug builds only, the files that renames and deletes in the
/// delivery folder then fail on, as on Windows while another program holds
/// a file open without sharing delete, so every platform tests it: file
/// names joined by commas, each failing every time, or, written `name#k`,
/// only the `k`th time this process writes it.
#[cfg(debug_assertions)]
const HELD_FILE: &str = "USER_MEMORY_TEST_HELD_FILE";

/// In debug builds, the error [`HELD_FILE`] makes for `path`, which this
/// process is about to write when `writing`, else to delete.
fn held_for_tests(path: &Path, writing: bool) -> io::Result<()> {
    #[cfg(debug_assertions)]
    if let Some(held) = std::env::var(HELD_FILE).ok()
        && let Some(name) = path.file_name().and_then(|name| name.to_str())
    {
        static SEEN: std::sync::Mutex<Vec<(String, usize)>> = std::sync::Mutex::new(Vec::new());
        let mut seen = SEEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let times = match seen.iter_mut().find(|(seen, _)| seen == name) {
            Some((_, times)) => {
                *times += usize::from(writing);
                *times
            }
            None => {
                seen.push((name.to_owned(), usize::from(writing)));
                usize::from(writing)
            }
        };
        let fails = held.split(',').any(|entry| match entry.split_once('#') {
            Some((entry, at)) => writing && entry == name && at.parse() == Ok(times),
            None => entry == name,
        });
        if fails {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
    }
    let _ = (path, writing);
    Ok(())
}

/// Renames `from` over `to`, on Windows trying again for up to
/// [`DELIVERY_REPLACE_WAIT`] while another program holds either file.
///
/// On Windows a reader never finds `to` refused meanwhile, unlike after a
/// delete: `fs::rename` tries `MoveFileExW`, which fails as access denied
/// while a reader has `to` open, and then renames with POSIX semantics,
/// which replaces the old file without leaving its name being deleted. So
/// the files readers open are replaced, never deleted, in the steps of a
/// session, the void mark among them ([`Files::void`]). The test that reads
/// while hooks replace the files (tests/delivery_files.rs) fails if a
/// later standard library drops that.
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    held_for_tests(to, true)?;
    #[cfg(windows)]
    return crate::platform::windows::rename(from, to, DELIVERY_REPLACE_WAIT);
    #[cfg(not(windows))]
    fs::rename(from, to)
}

/// The bytes of the delivery file at `path`, tried again for up to
/// [`REFUSED_READ_WAIT`] while Windows refuses to open it, so a file that
/// is there is not taken as missing. Every read of a delivery file goes
/// through it.
pub(crate) fn read(path: &Path) -> io::Result<Vec<u8>> {
    #[cfg(not(windows))]
    return fs::read(path);
    #[cfg(windows)]
    {
        let deadline = Instant::now() + REFUSED_READ_WAIT;
        loop {
            match fs::read(path) {
                Err(err)
                    if crate::platform::windows::refused(&err) && Instant::now() < deadline =>
                {
                    std::thread::sleep(REFUSED_READ_PAUSE);
                }
                read => return read,
            }
        }
    }
}

/// The delivery index at `path`, read through [`read`].
fn load(path: &Path) -> Result<DeliveryIndex, super::delivery::Error> {
    DeliveryIndex::read(read(path)?)
}

/// Deletes `path`, on Windows trying again as [`replace`] does. A reader
/// that opens the name while it is being deleted is refused for that
/// moment, which [`read`] waits out. Every delete of a delivery file goes
/// through it.
fn remove(path: &Path) -> io::Result<()> {
    held_for_tests(path, false)?;
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
        let left = Left {
            paths: vec![PathBuf::from("a.index")],
        };
        left.note(&mut reply);
        assert_eq!(reply["delivery_left"], serde_json::json!(["a.index"]));
        assert_eq!(reply["next"], format!("Undo with import.\n{LEFT}"));
        let mut reply = serde_json::json!({"updated": true});
        left.note(&mut reply);
        assert_eq!(reply["next"], LEFT);
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

    // Every read and delete of a delivery file goes through `read` and
    // `remove`, which on Windows try a refused open again and delete with
    // POSIX semantics (#218): no other `fs::read` or `fs::remove_file` in
    // this file's code, and none in the remote delivery code.
    #[test]
    fn every_read_and_delete_of_a_delivery_file_goes_through_one_helper() {
        let source = include_str!("delivery_files.rs");
        let start = source.find("#[cfg(test)]\nmod tests {").unwrap();
        let end = start + source[start..].find("\n}\n").unwrap();
        let code = format!("{}{}", &source[..start], &source[end..]);
        for (call, helper) in [
            ("fs::read(", "\npub(crate) fn read(path: &Path)"),
            ("fs::remove_file(", "\nfn remove(path: &Path)"),
        ] {
            let start = code.find(helper).unwrap();
            let body = start..start + code[start..].find("\n}\n").unwrap();
            let outside: Vec<usize> = code
                .match_indices(call)
                .map(|(at, _)| at)
                .filter(|at| !body.contains(at))
                .collect();
            assert!(outside.is_empty(), "{call} outside{helper}: {outside:?}");
        }
        assert!(
            !code.contains("DeliveryIndex::load("),
            "an index read past `load`"
        );
        let remote = include_str!("../remote/delivery.rs");
        for call in ["fs::read(", "fs::remove_file(", "read_to_string("] {
            assert!(
                !remote.contains(call),
                "src/remote/delivery.rs calls {call}"
            );
        }
    }

    // A void mark that is there but cannot be read is never taken as naming
    // nothing: a reader uses none of the files, and a writer changes
    // nothing. Here the mark's name is a folder, which no read opens.
    #[test]
    fn a_void_mark_that_cannot_be_read_withholds_every_file() {
        let (_dir, db, files) = setup();
        let lock = files.try_lock().unwrap().unwrap();
        let built = base(&db);
        files.write(&lock, &files.base(&ledger()), &built).unwrap();
        files
            .write(&lock, &files.overlay(&ledger()), &overlay_for(&built))
            .unwrap();
        assert!(files.view(&ledger()).is_some(), "read with no mark");
        fs::create_dir(files.void_path()).unwrap();
        assert!(files.voided().is_err());
        assert!(files.view(&ledger()).is_none(), "a reader uses nothing");
        assert!(files.readable_base(&ledger()).is_none());
        assert!(files.clear(&lock, &ledger()).is_err(), "a writer stops");
        files.tidy(&lock, None);
        assert!(files.void_path().is_dir(), "the mark is left as it is");
        assert!(files.base(&ledger()).exists() && files.overlay(&ledger()).exists());
    }

    // An empty void mark names nothing: a mark that came to name nothing
    // is kept empty rather than deleted (#218), and every reader, older
    // ones too, reads it as no names.
    #[test]
    fn an_empty_void_mark_voids_nothing() {
        let (_dir, db, files) = setup();
        let lock = files.try_lock().unwrap().unwrap();
        let built = base(&db);
        files.write(&lock, &files.base(&ledger()), &built).unwrap();
        files
            .write(&lock, &files.overlay(&ledger()), &overlay_for(&built))
            .unwrap();
        files.void(&lock, &[]).unwrap();
        assert!(!files.void_path().exists(), "no mark is made for nothing");
        let name = name_of(&files.base(&ledger()));
        files.void(&lock, std::slice::from_ref(&name)).unwrap();
        assert!(files.view(&ledger()).is_none(), "the named base is voided");
        files.void(&lock, &[]).unwrap();
        assert_eq!(fs::read(files.void_path()).unwrap(), b"", "kept, empty");
        assert_eq!(files.voided().unwrap(), Vec::<String>::new());
        assert!(
            files.view(&ledger()).is_some(),
            "an empty mark voids nothing"
        );
        files.tidy(&lock, None);
        assert!(files.view(&ledger()).is_some());
        assert_eq!(fs::read(files.void_path()).unwrap(), b"");
    }

    // On Windows, an open of a delivery file whose name is being deleted is
    // refused: a read tries again, and finds the file gone once the delete
    // ends; while it lasts past the wait, the void mark is taken as voiding
    // everything, never as naming nothing.
    #[cfg(windows)]
    #[test]
    fn a_read_waits_out_a_delete_and_a_mark_it_cannot_read_voids_everything() {
        let (_dir, _db, files) = setup();
        let lock = files.try_lock().unwrap().unwrap();
        files.void(&lock, &["named.index".to_owned()]).unwrap();
        let held = crate::platform::windows::pending_delete(&files.void_path());
        let started = Instant::now();
        let refused = files.voided().unwrap_err();
        assert!(crate::platform::windows::refused(&refused), "{refused}");
        assert!(started.elapsed() >= REFUSED_READ_WAIT, "tried again first");
        drop(held);
        assert_eq!(files.voided().unwrap(), Vec::<String>::new(), "gone");
        // A delete that ends within the wait: the read finds no file.
        files.void(&lock, &["named.index".to_owned()]).unwrap();
        let held = crate::platform::windows::pending_delete(&files.void_path());
        let ends = std::thread::spawn(move || {
            std::thread::sleep(REFUSED_READ_WAIT / 4);
            drop(held);
        });
        assert_eq!(
            read(&files.void_path()).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        ends.join().unwrap();
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
        assert_eq!(fs::read(files.void_path()).unwrap(), b"", "kept, empty");
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

    /// The overlay a server sends a client whose base is `base`, as the
    /// store at `db` stands now.
    fn delta(db: &Path, base: &DeliveryIndex) -> Option<Vec<u8>> {
        let made_for = Position::kept(base.header.position.clone());
        delivery::delta(
            db,
            &base.header.projects,
            made_for.as_str(),
            |id, change| {
                made_for
                    .within(&Position::at(id, change, 0))
                    .then(|| made_for.change())
                    .flatten()
            },
            overlay_memories(),
            |id, change| Position::at(id, change, 0).as_str().to_owned(),
        )
        .unwrap()
    }

    fn change(db: &Path, input: serde_json::Value, forget: bool) {
        let out = if forget {
            crate::memory::forget(db, serde_json::from_value(input).unwrap())
        } else {
            crate::memory::update(db, serde_json::from_value(input).unwrap())
        };
        let out = out.unwrap();
        assert!(out["updated"] == true || out["deleted"] == true, "{out}");
    }

    #[test]
    fn a_folded_base_holds_what_the_store_would_build_and_no_removed_word() {
        let (_dir, db, _files) = setup();
        let old = DeliveryIndex::read(base(&db)).unwrap();
        let ids: Vec<String> = (0..old.len())
            .map(|m| old.id(m).unwrap().to_owned())
            .collect();
        // Added only: the overlay takes nothing out of the base.
        let input = serde_json::json!({
            "type": "fact", "category": "build", "title": "Mooring",
            "content": "Moor the zeppelin to the north mast.", "scope": "global",
            "user_confirmed": true,
        });
        crate::memory::record(&db, serde_json::from_value(input).unwrap()).unwrap();
        let added = DeliveryIndex::read(delta(&db, &old).unwrap()).unwrap();
        assert_eq!(added.len(), 1);
        assert!(!delivery::removes(&old, &added).unwrap(), "an addition");
        // A memory forgotten, then another reworded.
        change(
            &db,
            serde_json::json!({"memory_id": ids[1], "user_confirmed": true}),
            true,
        );
        let forgotten = DeliveryIndex::read(delta(&db, &old).unwrap()).unwrap();
        assert!(delivery::removes(&old, &forgotten).unwrap(), "a forget");
        change(
            &db,
            serde_json::json!({"memory_id": ids[0], "content": "The hangar's quartz door sticks.", "user_confirmed": true}),
            false,
        );
        let overlay = DeliveryIndex::read(delta(&db, &old).unwrap()).unwrap();
        let folded = delivery::fold(&old, &overlay).unwrap();
        let folded = DeliveryIndex::read(folded).unwrap();
        let fresh = DeliveryIndex::read(base(&db)).unwrap();
        assert_eq!(folded.header, fresh.header);
        assert_eq!(folded.contents().unwrap(), fresh.contents().unwrap());
        let bytes = &folded.bytes;
        for gone in ["glacier", "straight quotes", "Prefer"] {
            let hex: String = gone.bytes().map(|b| format!("{b:02x}")).collect();
            assert!(
                !bytes.windows(hex.len()).any(|w| w == hex.as_bytes()),
                "{gone} in hex"
            );
        }
    }

    #[test]
    fn a_reword_takes_words_out_and_a_new_status_does_not() {
        let (_dir, db, _files) = setup();
        let old = DeliveryIndex::read(base(&db)).unwrap();
        let id = old.id(0).unwrap().to_owned();
        // A new status moves the counter and leaves the indexed words.
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("UPDATE memories SET status='tentative' WHERE id=?1", [&id])
            .unwrap();
        // As a write through the store does, so the next one keeps its id.
        crate::memory::changes::publish(&conn, &db);
        drop(conn);
        let overlay = DeliveryIndex::read(delta(&db, &old).unwrap()).unwrap();
        assert_eq!(overlay.header.skip, vec![id.clone()]);
        let same = delivery::removes(&old, &overlay).unwrap();
        change(
            &db,
            serde_json::json!({"memory_id": id, "title": "Hangar gate", "user_confirmed": true}),
            false,
        );
        let overlay = DeliveryIndex::read(delta(&db, &old).unwrap()).unwrap();
        assert!(delivery::removes(&old, &overlay).unwrap(), "a new title");
        assert!(!same, "the indexed words unchanged");
    }

    #[test]
    fn a_server_sends_a_whole_base_for_another_store_a_later_or_pruned_base_or_too_many_changes() {
        let (dir, db, _files) = setup();
        let old = DeliveryIndex::read(base(&db)).unwrap();
        assert!(delta(&db, &old).is_some(), "the same store, nothing since");
        let other = store(&dir.path().join("other"));
        assert!(delta(&other, &old).is_none(), "another store");
        let conn = rusqlite::Connection::open(&db).unwrap();
        let state = crate::memory::changes::state(&conn).unwrap().unwrap();
        let mut later = old.header.clone();
        later.position = Position::at(&state.store_id, state.last_change + 1, 0)
            .as_str()
            .to_owned();
        let later = DeliveryIndex::read(DeliveryIndex::write_with(&later, &[]).unwrap()).unwrap();
        assert!(delta(&db, &later).is_none(), "past the store's change");
        conn.execute(
            "UPDATE store_changes SET pruned_through=last_change+1 WHERE id=1",
            [],
        )
        .unwrap();
        assert!(delta(&db, &old).is_none(), "pruned");
        conn.execute("UPDATE store_changes SET pruned_through=0 WHERE id=1", [])
            .unwrap();
        drop(conn);
        for n in 0..3 {
            let input = serde_json::json!({
                "type": "fact", "category": "build", "title": format!("Mast {n}"),
                "content": format!("Mast {n} faces {}.", ["north", "south", "east"][n]),
                "scope": "global", "user_confirmed": true,
            });
            crate::memory::record(&db, serde_json::from_value(input).unwrap()).unwrap();
        }
        let made_for = Position::kept(old.header.position.clone());
        let over = delivery::delta(
            &db,
            &old.header.projects,
            made_for.as_str(),
            |_, _| made_for.change(),
            2,
            |id, change| Position::at(id, change, 0).as_str().to_owned(),
        )
        .unwrap();
        assert!(over.is_none(), "three changes past a bound of two");
    }

    const SERVER: &str = "https://memory.example.test:8765/mcp";

    /// A store, this machine's files of it as the server's at `SERVER`: a
    /// base of the ledger's scope at the store's position and its empty
    /// overlay; and the folder of the connector's marker files.
    fn served_setup() -> (tempfile::TempDir, PathBuf, Files, PathBuf) {
        let (dir, db, _) = setup();
        let files = Files::in_dir(
            dir.path().join(FOLDER),
            &paths::store_identity(Some(SERVER), Path::new("")),
        );
        let lock = files.try_lock().unwrap().unwrap();
        let built = base(&db);
        files.write(&lock, &files.base(&ledger()), &built).unwrap();
        files
            .write(&lock, &files.overlay(&ledger()), &overlay_for(&built))
            .unwrap();
        let servers = dir.path().join("servers");
        (dir, db, files, servers)
    }

    fn served(files: &Files, servers: &Path) -> bool {
        let marker = crate::remote::marker::testing::served(servers, SERVER);
        files.open_served(marker.as_ref(), &ledger()).is_some()
    }

    /// Run as a process of its own by the test below: holds a connector's
    /// lock on the file `USER_MEMORY_TEST_CONNECTED` names until killed.
    #[test]
    fn hold_a_connector_s_lock() {
        let Some(path) = std::env::var_os("USER_MEMORY_TEST_CONNECTED") else {
            return;
        };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap();
        file.lock_shared().unwrap();
        println!("held");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }

    #[test]
    fn a_base_from_the_server_ends_a_missed_mark_and_an_overlay_for_another_base_is_left() {
        let (_dir, db, files, _servers) = served_setup();
        let lock = files.try_lock().unwrap().unwrap();
        files.write(&lock, &files.missed(&ledger()), b"").unwrap();
        drop(lock);
        let old = files.readable_base(&ledger()).unwrap();
        let input = serde_json::json!({
            "type": "fact", "category": "build", "title": "Mooring",
            "content": "Moor the zeppelin to the north mast.", "scope": "global",
            "user_confirmed": true,
        });
        crate::memory::record(&db, serde_json::from_value(input).unwrap()).unwrap();
        let overlay = delta(&db, &old).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        assert_eq!(
            files.take(&ledger(), overlay.clone(), deadline).unwrap(),
            Taken::Written
        );
        assert!(files.missed(&ledger()).exists(), "no base yet");
        assert_eq!(
            files.take(&ledger(), base(&db), deadline).unwrap(),
            Taken::Written
        );
        assert!(!files.missed(&ledger()).exists(), "a base is here");
        assert_eq!(
            files.take(&ledger(), overlay, deadline).unwrap(),
            Taken::Left,
            "made for the base before"
        );
    }

    #[test]
    fn a_server_s_files_are_read_only_while_a_connector_runs() {
        let (_dir, db, files, servers) = served_setup();
        crate::remote::marker::testing::mark(&servers, SERVER, &now(&db), Duration::ZERO);
        assert!(!served(&files, &servers), "no connector");
        let mut connector = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "memory::delivery_files::tests::hold_a_connector_s_lock",
                "--exact",
                "--nocapture",
            ])
            .env(
                "USER_MEMORY_TEST_CONNECTED",
                crate::remote::marker::testing::connected_path(&servers, SERVER),
            )
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let out = std::io::BufReader::new(connector.stdout.take().unwrap());
        let held = std::io::BufRead::lines(out).any(|line| line.is_ok_and(|line| line == "held"));
        assert!(held);
        assert!(served(&files, &servers), "a connector runs");
        connector.kill().unwrap();
        connector.wait().unwrap();
        assert!(!served(&files, &servers), "the connector was killed");
    }

    #[test]
    fn a_server_s_files_are_unread_once_its_connector_stops_hearing_from_it() {
        let (_dir, db, files, servers) = served_setup();
        let _connector = crate::remote::marker::testing::connect(&servers, SERVER);
        let age = crate::remote::marker::testing::served_age();
        let margin = Duration::from_secs(5);
        crate::remote::marker::testing::mark(&servers, SERVER, &now(&db), age - margin);
        assert!(served(&files, &servers), "a quiet connector's marker");
        crate::remote::marker::testing::mark(&servers, SERVER, &now(&db), age + margin);
        assert!(!served(&files, &servers), "no answer from the server");
    }

    #[test]
    fn a_server_s_files_behind_its_marker_are_unread() {
        let (_dir, db, files, servers) = served_setup();
        let _connector = crate::remote::marker::testing::connect(&servers, SERVER);
        let conn = rusqlite::Connection::open(&db).unwrap();
        let state = crate::memory::changes::state(&conn).unwrap().unwrap();
        let later = Position::at(&state.store_id, state.last_change + 1, 0);
        crate::remote::marker::testing::mark(&servers, SERVER, &later, Duration::ZERO);
        assert!(!served(&files, &servers), "a change the files don't hold");
        // The connector took that change in: an overlay at the marker.
        let base = files.readable_base(&ledger()).unwrap();
        let mut header = base.header.clone();
        header.part = Part::Overlay;
        header.position = later.as_str().to_owned();
        let lock = files.try_lock().unwrap().unwrap();
        files
            .write(
                &lock,
                &files.overlay(&ledger()),
                &DeliveryIndex::write_with(&header, &[]).unwrap(),
            )
            .unwrap();
        assert!(served(&files, &servers), "the files at the marker");
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
        let (done, verb) = self.words();
        format!(
            "Nothing was {done}: {}. Wait a moment and {verb} again; if it still holds it, stop that process first.",
            held_by(holder, wait)
        )
    }
}

impl Removing {
    /// The words of the reply for what this write does: what was not done,
    /// and the command to run again.
    fn words(self) -> (&'static str, &'static str) {
        match self {
            Self::Forget => ("forgotten", "forget"),
            Self::Update => ("updated", "update"),
            Self::Import => ("imported", "import"),
        }
    }

    /// The reply when the void mark at `mark` could not be written before
    /// the commit: without it, a file left after the commit would keep the
    /// removed words in use, so the write is refused.
    fn unmarked(self, mark: &Path, err: &io::Error) -> String {
        let (done, verb) = self.words();
        format!(
            "Nothing was {done}: the mark that keeps this machine's delivery index from using the removed words could not be written to {}: {err}. Make that folder writable, or free space on its disk, then {verb} again.",
            mark.display()
        )
    }
}

/// Who kept a store's delivery lock for all of `wait`: `holder`, the
/// process id the lock file names, or another process when it names none.
fn held_by(holder: Option<u32>, wait: Duration) -> String {
    let who = holder.map_or_else(
        || "another user-memory process".to_owned(),
        |pid| format!("user-memory process {pid}"),
    );
    format!(
        "{who} has held this machine's delivery index for {} s",
        wait.as_secs()
    )
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
    let kept = files.voided().with_context(|| {
        format!(
            "cannot read {}; nothing was changed",
            files.void_path().display()
        )
    })?;
    let mut scopes: Vec<Vec<String>> = Vec::new();
    let mut damaged = Vec::new();
    for (_, path) in files.own() {
        if !path.extension().is_some_and(|e| e == BASE || e == OVERLAY) {
            continue;
        }
        match load(&path) {
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
    if names.len() > removal.kept.len()
        && let Err(err) = removal.files.void(&removal.lock, &names)
    {
        anyhow::bail!(removing.unmarked(&removal.files.void_path(), &err));
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
        // Every file left is named in the mark written before the commit,
        // so a failure here leaves files voided that were built again,
        // until the next writer: recall waits, no word stays in use.
        let _ = self.files.void(&self.lock, &names);
        Left { paths: left }
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
/// text. The void mark, written before the commit, names each of them.
#[derive(Debug, Default)]
pub(crate) struct Left {
    pub(crate) paths: Vec<PathBuf>,
}

/// The reply's `next` line when files still hold removed text.
const LEFT: &str = "The files in delivery_left still hold the removed words because another program has them open. Readers skip them; user-memory deletes them at its next write, and `user-memory doctor` deletes them at once.";

impl Left {
    /// Adds to a removing command's `reply`, when files are left, their
    /// paths as `delivery_left` and a line of `next` saying what happens to
    /// them.
    pub(crate) fn note(&self, reply: &mut serde_json::Value) {
        if self.paths.is_empty() {
            return;
        }
        reply["delivery_left"] = serde_json::json!(self.paths);
        let next = match reply["next"].as_str() {
            Some(next) if !next.is_empty() => format!("{next}\n{LEFT}"),
            _ => LEFT.to_owned(),
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
/// removed text, and those a killed writer left half written. Fails when
/// the folder cannot be read or another process keeps the lock.
pub(crate) fn clean(db: &Path) -> io::Result<Cleaned> {
    match Files::of(&paths::store_identity(None, db)) {
        Some(files) => files.cleaned(None),
        None => Ok(Cleaned::default()),
    }
}

/// For `user-memory doctor` on a memory server's client (#180): what
/// [`clean`] deletes, of this machine's files of the server at `url`, and
/// every base and overlay not written for [`DELIVERY_AGE`]: files no
/// connector kept current, which may hold words the server's forgets took
/// out.
pub(crate) fn clean_served(url: &str) -> io::Result<Cleaned> {
    match served_files(url) {
        Some(files) => files.cleaned(Some(SystemTime::now())),
        None => Ok(Cleaned::default()),
    }
}

impl Files {
    /// [`clean`]'s work on these files, with those not written for
    /// [`DELIVERY_AGE`] before `aged`, when given.
    fn cleaned(&self, aged: Option<SystemTime>) -> io::Result<Cleaned> {
        let files = self;
        match fs::read_dir(&files.dir) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Cleaned::default()),
            Err(err) => return Err(err),
            Ok(_) => {}
        }
        let wait = delivery_lock_wait();
        let Some(lock) = files.lock_until(Instant::now() + wait)? else {
            return Err(io::Error::other(held_by(files.holder(), wait)));
        };
        let partial = format!(".{PARTIAL}");
        let before: Vec<PathBuf> = files
            .voided()?
            .iter()
            .map(|name| files.dir.join(name))
            .chain(
                files
                    .own()
                    .into_iter()
                    .filter(|(name, _)| {
                        let scoped = name.ends_with(&format!(".{BASE}"))
                            || name.ends_with(&format!(".{OVERLAY}"));
                        name.ends_with(&partial) || (aged.is_some() && scoped)
                    })
                    .map(|(_, path)| path),
            )
            .filter(|path| path.exists())
            .collect();
        files.tidy(&lock, aged);
        let held: Vec<PathBuf> = files
            .voided()?
            .iter()
            .map(|name| files.dir.join(name))
            .filter(|path| path.exists())
            .collect();
        Ok(Cleaned {
            deleted: before.iter().filter(|path| !path.exists()).count(),
            held,
        })
    }
}

/// For a memory server's client (#180): the overlay of the memories
/// `projects` can see that changed in the store at `db` since the client's
/// base at `since`, as [`super::delivery::delta`] makes it with this machine's
/// [`OVERLAY_MEMORIES`]; none when the client needs a whole base.
pub(crate) fn served_overlay(
    db: &Path,
    projects: &[String],
    since: &str,
) -> Result<Option<Vec<u8>>, super::delivery::Error> {
    let made_for = Position::kept(since.to_owned());
    super::delivery::delta(
        db,
        &scope(projects),
        since,
        |id, change| {
            made_for
                .within(&Position::at(id, change, 0))
                .then(|| made_for.change())
                .flatten()
        },
        overlay_memories(),
        |id, change| Position::at(id, change, 0).as_str().to_owned(),
    )
}

/// For a memory server's client (#180): the position the store at `db` is
/// at, which names the base its clients share.
pub(crate) fn served_position(db: &Path) -> Option<String> {
    super::changes::position(db).map(|position| position.as_str().to_owned())
}

/// For a memory server's client (#180): a new base of `projects` from the
/// store at `db`; none when the store's keyword index is not current.
pub(crate) fn served_base(db: &Path, projects: &[String]) -> Option<Vec<u8>> {
    base_and_overlay(db, &scope(projects)).map(|(base, _)| base)
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

/// The extension of the mark that a scope's base from a memory server did
/// not arrive within a hook's limit: the prompt hook then leaves it to the
/// next session start, so a slow link costs no prompt a download. A base
/// written ends it.
const MISSED: &str = "missed";

/// The delivery files a client keeps of the store of the server at `url`
/// (#180), named as the session snapshots name that store.
fn served_files(url: &str) -> Option<Files> {
    Files::of(&paths::store_identity(Some(url), Path::new("")))
}

impl Files {
    fn missed(&self, scope: &[String]) -> PathBuf {
        self.file(scope, MISSED)
    }

    /// The base of `scope` unless the void mark names it, whatever its
    /// position.
    fn readable_base(&self, scope: &[String]) -> Option<DeliveryIndex> {
        let path = self.base(scope);
        let name = path.file_name()?.to_string_lossy().into_owned();
        if self.voided().ok()?.contains(&name) {
            return None;
        }
        load(&path).ok()
    }

    /// Writes `base` in place of `scope`'s base, with an empty overlay made
    /// for it, and drops the scope's `.missed` mark: a base is here now. A
    /// base that can't be replaced, as when another program holds it open
    /// on Windows, is named in the void mark with its overlay, so no reader
    /// uses words it should no longer hold.
    fn replace_base(&self, lock: &Lock, scope: &[String], base: &[u8]) -> io::Result<()> {
        let header = DeliveryIndex::read(base.to_vec())
            .map_err(|_| io::ErrorKind::InvalidData)?
            .header;
        let empty =
            super::delivery::empty_overlay(&header).map_err(|_| io::ErrorKind::InvalidData)?;
        if let Err(err) = self.write(lock, &self.base(scope), base) {
            self.clear(lock, scope)?;
            return Err(err);
        }
        self.write(lock, &self.overlay(scope), &empty)?;
        match self.remove(lock, &self.missed(scope)) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        }
    }
}

/// What a client did with a delivery file its server sent.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Taken {
    /// A base, or an overlay that took nothing out of the base, was written.
    Written,
    /// An overlay took words out of the base, so the base was written again
    /// without them: no file holds them now.
    Folded,
    /// Another writer kept the lock past the deadline, or the base moved
    /// since the file was asked for: nothing was written.
    Left,
}

/// Takes `bytes`, the delivery file the server at `url` sent for the scope
/// `projects` (#180), under the lock waited for until `deadline`. A base is
/// written with an empty overlay. An overlay made for the base on disk is
/// written beside it, unless it takes words out of that base (a memory
/// forgotten, reworded or moved out of the scope): the base is then folded
/// with it and written again, so the words leave this machine's disk now
/// rather than at a download.
pub(crate) fn take_served(
    url: &str,
    projects: &[String],
    bytes: Vec<u8>,
    deadline: Instant,
) -> io::Result<Taken> {
    served_files(url)
        .ok_or(io::ErrorKind::NotFound)?
        .take(&scope(projects), bytes, deadline)
}

impl Files {
    /// [`take_served`] for these files.
    fn take(&self, scope: &[String], bytes: Vec<u8>, deadline: Instant) -> io::Result<Taken> {
        let (files, scope) = (self, scope);
        let file = DeliveryIndex::read(bytes).map_err(|_| io::ErrorKind::InvalidData)?;
        if file.header.projects != scope {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let Some(lock) = files.lock_until(deadline)? else {
            return Ok(Taken::Left);
        };
        files.tidy(&lock, None);
        if file.header.part == super::delivery::Part::Base {
            files.replace_base(&lock, scope, &file.bytes)?;
            return Ok(Taken::Written);
        }
        let Some(base) = files.readable_base(scope) else {
            return Ok(Taken::Left);
        };
        if base.header.position != file.header.made_for {
            return Ok(Taken::Left);
        }
        let removes =
            super::delivery::removes(&base, &file).map_err(|_| io::ErrorKind::InvalidData)?;
        if removes {
            let folded =
                super::delivery::fold(&base, &file).map_err(|_| io::ErrorKind::InvalidData)?;
            files.replace_base(&lock, scope, &folded)?;
            return Ok(Taken::Folded);
        }
        files.write(&lock, &files.overlay(scope), &file.bytes)?;
        Ok(Taken::Written)
    }
}

/// The delivery index of the scope `projects` that a hook matches against:
/// for the local store at `db` (`remote` none), its files as they stand at
/// the store's position; for the server at `remote`, this machine's files
/// of its store, only while a connector runs here, its marker is recent
/// ([`crate::remote::marker::served`]), and the newest file is at or past
/// the marker's position, so a removal the connector has heard of is never
/// read.
pub(crate) fn open(remote: Option<&str>, db: &Path, projects: &[String]) -> Option<View> {
    match remote {
        None => Files::of(&paths::store_identity(None, db))?
            .open(&scope(projects), &super::changes::position(db)?),
        Some(url) => served_files(url)?.open_served(
            crate::remote::marker::served(url).as_ref(),
            &scope(projects),
        ),
    }
}

impl Files {
    /// The remote arm of [`open`], with the position `marker` gives: none
    /// without one, or when the newest file is behind it.
    fn open_served(&self, marker: Option<&Position>, scope: &[String]) -> Option<View> {
        let marker = marker?;
        let view = self.view(scope)?;
        let newest = view
            .overlay
            .as_ref()
            .map_or(&view.base.header, |overlay| &overlay.header);
        marker
            .within(&Position::kept(newest.position.clone()))
            .then_some(view)
    }
}

/// Each scope whose base this machine keeps of the server at `url`, with
/// that base's position: what the connector asks the changes since.
pub(crate) fn served_bases(url: &str) -> Vec<(Vec<String>, String)> {
    let Some(files) = served_files(url) else {
        return Vec::new();
    };
    let base = format!(".{BASE}");
    let Ok(voided) = files.voided() else {
        return Vec::new();
    };
    files
        .own()
        .into_iter()
        .filter(|(name, _)| name.ends_with(&base) && !voided.contains(name))
        .filter_map(|(_, path)| load(&path).ok())
        .filter(|index| index.header.part == super::delivery::Part::Base)
        .map(|index| (index.header.projects.clone(), index.header.position.clone()))
        .collect()
}

/// What session start found of a scope's files of a memory server.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ServedStart {
    /// The base is there, with any change folded in.
    Ready,
    /// There is no base: session start asks the server for one.
    Missing,
    /// Another writer kept the lock past the deadline.
    Busy,
}

/// Session start's part for the scope `projects` of the server at `url`
/// (#180), under the lock waited for until `deadline`: clears what
/// [`Files::tidy`] clears with the files a day old, then folds a base's
/// non-empty overlay into it, with no network call. Without a base, the
/// caller downloads one, whatever the `.missed` mark says.
pub(crate) fn served_at_session_start(
    url: &str,
    projects: &[String],
    deadline: Instant,
) -> io::Result<ServedStart> {
    let files = served_files(url).ok_or(io::ErrorKind::NotFound)?;
    let scope = scope(projects);
    let Some(lock) = files.lock_until(deadline)? else {
        return Ok(ServedStart::Busy);
    };
    files.tidy(&lock, Some(SystemTime::now()));
    let Some(base) = files.readable_base(&scope) else {
        return Ok(ServedStart::Missing);
    };
    let overlay = load(&files.overlay(&scope))
        .ok()
        .filter(|overlay| overlay.header.made_for == base.header.position);
    if let Some(overlay) = overlay
        && (overlay.len() > 0 || !overlay.header.skip.is_empty())
    {
        let folded =
            super::delivery::fold(&base, &overlay).map_err(|_| io::ErrorKind::InvalidData)?;
        files.replace_base(&lock, &scope, &folded)?;
    }
    Ok(ServedStart::Ready)
}

/// Whether the prompt hook asks the server at `url` for the base of the
/// scope `projects` (#180): there is none, and its last download did not
/// miss; session start asks either way.
pub(crate) fn served_wanted(url: &str, projects: &[String]) -> bool {
    let Some(files) = served_files(url) else {
        return false;
    };
    let scope = scope(projects);
    files.readable_base(&scope).is_none() && !files.missed(&scope).exists()
}

/// Marks that the base of the scope `projects` of the server at `url` did
/// not arrive in time, so no prompt asks for it again before the next
/// session start.
pub(crate) fn served_missed(url: &str, projects: &[String]) {
    if let Some(files) = served_files(url) {
        let _ = private::create_dir(&files.dir);
        let _ = private::create_file(
            OpenOptions::new().write(true).create(true).truncate(true),
            &files.missed(&scope(projects)),
        );
    }
}

/// Deletes every delivery file of the server at `url`, under its lock
/// waited for until `deadline`: what a client does when the server keeps no
/// delivery index this version reads, so none stays that no one keeps
/// current.
pub(crate) fn drop_served(url: &str, deadline: Instant) {
    let Some(files) = served_files(url) else {
        return;
    };
    let Ok(Some(lock)) = files.lock_until(deadline) else {
        return;
    };
    for (_, path) in files.own() {
        if path != files.lock_path() {
            let _ = files.remove(&lock, &path);
        }
    }
}
