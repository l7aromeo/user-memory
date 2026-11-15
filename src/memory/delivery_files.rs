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
//! Every file here is private ([`private`]) and read whole, never mapped,
//! so a reader holds a file only while it reads it.

use super::{
    Position,
    delivery::{DeliveryIndex, View},
};
use crate::{paths, private};
use std::{
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
        Lock { _file: file }
    }

    #[cfg(test)]
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
    let Ok(built) = super::delivery::build(db, &scope, |id, change| {
        Position::at(id, change, 0).as_str().to_owned()
    }) else {
        return Ok(Started::Left);
    };
    if Instant::now() > deadline {
        return Ok(Started::Left);
    }
    let header = DeliveryIndex::read(built.clone())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?
        .header;
    let empty = super::delivery::empty_overlay(&header)
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
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
    if unchanged && files.voided().is_empty() {
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
    #[cfg(debug_assertions)]
    super::hold_for_tests(HOLD_REFRESH);
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
/// it took the lock and before it reads the old files again, so a test can
/// run a removal against a hook holding the lock.
#[cfg(debug_assertions)]
pub(crate) const HOLD_REFRESH: &str = "USER_MEMORY_HOLD_REFRESH";

/// Renames `from` over `to`, on Windows trying again for up to
/// [`DELIVERY_REPLACE_WAIT`] while another program holds either file.
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(windows)]
    return crate::platform::windows::rename(from, to, DELIVERY_REPLACE_WAIT);
    #[cfg(not(windows))]
    fs::rename(from, to)
}

/// Deletes `path`, on Windows trying again as [`replace`] does.
fn remove(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    return crate::platform::windows::remove_file(path, DELIVERY_REPLACE_WAIT);
    #[cfg(not(windows))]
    fs::remove_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::delivery::{self, Part};

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
        crate::memory::record(&db, serde_json::from_value(input).unwrap()).unwrap();
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
