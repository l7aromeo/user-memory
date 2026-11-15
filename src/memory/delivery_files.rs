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
    time::{Duration, Instant},
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
