//! SQLite storage: schema, connection and row mapping.

use super::Memory;
use crate::private;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use std::{fs, path::Path, time::Duration};
use uuid::Uuid;

/// Stamped into `PRAGMA user_version`. Bump it whenever `SCHEMA` or the
/// search index SQL in `index.rs` changes, and add the step that upgrades the
/// previous version in `migrate.rs`: `IF NOT EXISTS` keeps an older table or
/// trigger as it was, so connecting refuses a store from another version
/// instead of half upgrading it.
pub(super) const SCHEMA_VERSION: i64 = 5;

/// The tables and the triggers that stale inferred patterns when their
/// support weakens: a supporting memory deleted; one changed in what it
/// says, superseded, made stale or weakened in source or status (a
/// confirmation, observed to explicit or tentative to confirmed with its
/// evidence note dropped, only strengthens it; a reason or title is
/// wording); one moved to a project the pattern is not in, which recording
/// refuses too (made global, or moved with its pattern, it still holds); or
/// a removed conversation leaving the supports fewer than two independent
/// conversations, counting each support without one as its own, as
/// recording an inference requires.
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS memories (
    id TEXT PRIMARY KEY NOT NULL CHECK(length(id)=36 AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND substr(id,15,1)='4' AND substr(id,20,1) IN ('8','9','a','b') AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    memory_type TEXT NOT NULL,
    category TEXT NOT NULL,
    context TEXT NOT NULL DEFAULT '',
    title TEXT NOT NULL CHECK(length(title) BETWEEN 1 AND 120 AND instr(title,char(10))=0 AND instr(title,char(13))=0),
    content TEXT NOT NULL,
    reason TEXT NOT NULL DEFAULT '',
    scope TEXT NOT NULL DEFAULT 'global',
    project TEXT NOT NULL DEFAULT '',
    source TEXT NOT NULL DEFAULT 'explicit',
    status TEXT NOT NULL DEFAULT 'confirmed',
    evidence TEXT NOT NULL DEFAULT '',
    evidence_ids TEXT NOT NULL DEFAULT '[]',
    superseded_by TEXT,
    last_confirmed_at TEXT,
    CHECK(source='inferred' OR evidence_ids='[]'),
    CHECK(status IN ('confirmed','tentative','stale','superseded')),
    CHECK((status='superseded')=(superseded_by IS NOT NULL))
);
CREATE TABLE IF NOT EXISTS memory_origins (
    memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    origin TEXT NOT NULL CHECK(length(origin) BETWEEN 1 AND 240),
    first_seen TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY(memory_id, origin)
);
CREATE TABLE IF NOT EXISTS memory_distinct (
    a TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    b TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    PRIMARY KEY(a, b),
    CHECK(a < b)
);
CREATE TRIGGER IF NOT EXISTS memory_support_deleted AFTER DELETE ON memories BEGIN
    UPDATE memories SET status='stale',updated_at=CURRENT_TIMESTAMP
    WHERE status!='superseded' AND EXISTS (SELECT 1 FROM json_each(evidence_ids) WHERE value=OLD.id);
END;
CREATE TRIGGER IF NOT EXISTS memory_support_changed AFTER UPDATE OF memory_type,category,context,content,source,status,evidence ON memories
WHEN (OLD.memory_type,OLD.category,OLD.context,OLD.content)
    IS NOT (NEW.memory_type,NEW.category,NEW.context,NEW.content)
    OR (OLD.source IS NOT NEW.source AND NOT (OLD.source='observed' AND NEW.source='explicit'))
    OR (OLD.status IS NOT NEW.status AND NOT (OLD.status='tentative' AND NEW.status='confirmed'))
    OR (OLD.evidence IS NOT NEW.evidence AND NOT (OLD.source='observed' AND NEW.source='explicit' AND NEW.evidence='')) BEGIN
    UPDATE memories SET status='stale',updated_at=CURRENT_TIMESTAMP
    WHERE status!='superseded' AND EXISTS (SELECT 1 FROM json_each(evidence_ids) WHERE value=OLD.id);
END;
CREATE TRIGGER IF NOT EXISTS memory_support_moved AFTER UPDATE OF scope,project ON memories
WHEN (OLD.scope,OLD.project) IS NOT (NEW.scope,NEW.project) AND NOT (NEW.scope='global' AND NEW.project='') BEGIN
    UPDATE memories SET status='stale',updated_at=CURRENT_TIMESTAMP
    WHERE status!='superseded' AND EXISTS (SELECT 1 FROM json_each(evidence_ids) WHERE value=OLD.id)
    AND (scope!='project' OR project IS NOT NEW.project);
END;
CREATE TRIGGER IF NOT EXISTS memory_origin_removed AFTER DELETE ON memory_origins BEGIN
    UPDATE memories SET status='stale',updated_at=CURRENT_TIMESTAMP
    WHERE status!='superseded' AND EXISTS (SELECT 1 FROM json_each(evidence_ids) WHERE value=OLD.memory_id)
    AND (SELECT COUNT(DISTINCT o.origin) FROM json_each(memories.evidence_ids) e JOIN memory_origins o ON o.memory_id=e.value)
        + (SELECT COUNT(*) FROM json_each(memories.evidence_ids) e WHERE NOT EXISTS (SELECT 1 FROM memory_origins o WHERE o.memory_id=e.value)) < 2;
END;";

/// Cutoffs computed for an embedding model and vector length that has none
/// built in, from the labelled pairs that ship in the binary. Null cutoffs
/// record that the pairs could not be separated, so duplicate detection
/// stays keyword-only for that model. Kept with the store's vectors, so a
/// copied store keeps its own. Part of the stored schema since version 5.
pub(super) const CUTOFFS: &str = "CREATE TABLE IF NOT EXISTS embedding_cutoffs (
    model TEXT NOT NULL CHECK(length(model) BETWEEN 1 AND 200),
    length INTEGER NOT NULL CHECK(length > 0),
    duplicate REAL,
    related REAL,
    calibrated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY(model, length),
    CHECK((duplicate IS NULL) = (related IS NULL)),
    CHECK(duplicate IS NULL OR (related < duplicate AND duplicate <= 1.0 AND related >= -1.0))
);";

/// The projects each conversation linked, by origin, so its hooks load them
/// with the folder's own project. Keys are canonical project keys, never
/// local paths. Part of the stored schema since version 4.
pub(super) const LINKS: &str = "CREATE TABLE IF NOT EXISTS conversation_projects (
    origin TEXT NOT NULL CHECK(length(origin) BETWEEN 1 AND 240),
    project TEXT NOT NULL CHECK(length(project) BETWEEN 1 AND 240),
    linked_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY(origin, project)
);";

/// An empty in-memory database with the store's tables, for running the
/// store's queries over memories that are not stored, such as an import's.
pub(super) fn scratch() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

pub fn connect(db: &Path) -> Result<Connection> {
    // A missing store folder is created private; the folders above it are
    // the user's own, created as any other.
    if let Some(parent) = db.parent()
        && !parent.exists()
    {
        if let Some(above) = parent.parent() {
            fs::create_dir_all(above)
                .with_context(|| format!("Cannot create {}", above.display()))?;
        }
        private::create_dir(parent)
            .with_context(|| format!("Cannot create {}", parent.display()))?;
    }
    // Create a missing store owner-only, and only then: opening and closing
    // an existing store with std would close a descriptor for it, which on
    // POSIX drops every lock SQLite holds on it in this process, including a
    // transaction under way on another connection, and lets another process
    // write at the same time and corrupt it. Creating holds a lock until the
    // new file is closed again, so another connection in this process cannot
    // open the store and take SQLite's locks before that close drops them.
    static CREATING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let creating = CREATING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match private::create_file(fs::OpenOptions::new().write(true).create_new(true), db) {
        Ok(created) => drop(created),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => {
            return Err(err).with_context(|| format!("Cannot create {}", db.display()));
        }
    }
    drop(creating);
    let conn = Connection::open(db)?;
    conn.busy_timeout(BUSY)?;
    // Per connection and ignored inside a transaction, so set it first.
    conn.pragma_update(None, "foreign_keys", true)?;
    let found = version(&conn)?;
    if found != SCHEMA_VERSION {
        #[cfg(debug_assertions)]
        pause_for_tests();
        // Refuse a store this binary does not own before taking a write
        // turn, which would leave a lock file beside it. Version and tables
        // come from one read snapshot: a store another process is creating
        // meanwhile is then still empty or already current, never tables
        // without their version.
        let snapshot = conn.unchecked_transaction()?;
        let found = version(&snapshot)?;
        if found != SCHEMA_VERSION {
            let kind = inspect(&snapshot, found, super::migrate::BASELINE, SCHEMA_VERSION)?;
            if let Some(refusal) = refusal(&snapshot, db, found, &kind, SCHEMA_VERSION)? {
                bail!(refusal);
            }
        }
        drop(snapshot);
        // Decide again under the write lock so concurrent first connections
        // agree.
        let tx = write(&conn)?;
        let found = version(&tx)?;
        if found != SCHEMA_VERSION {
            // Only an empty database gets the schema; servers never migrate.
            let kind = inspect(&tx, found, super::migrate::BASELINE, SCHEMA_VERSION)?;
            if let Some(refusal) = refusal(&tx, db, found, &kind, SCHEMA_VERSION)? {
                bail!(refusal);
            }
            tx.execute_batch(SCHEMA)?;
            tx.execute_batch(LINKS)?;
            tx.execute_batch(CUTOFFS)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        tx.commit()?;
    }
    private::restrict(db)?;
    // Write-ahead logging lets readers, such as every session start's
    // instruction sync, read while a writer commits, where the rollback
    // journal makes the writer wait for them. The mode is stored in the file,
    // so this converts a store once, and only a store this binary accepts.
    // Switching needs a write lock; while another connection keeps the store
    // busy it keeps its mode until a later open, which tries again, so the
    // open never waits for it. SQLite gives the -wal and -shm files the
    // store's mode, so on Unix this follows the 0600 above.
    conn.busy_timeout(Duration::ZERO)?;
    let switched =
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0));
    conn.busy_timeout(BUSY)?;
    match switched {
        Ok(_) => {}
        Err(err) if err.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy) => {}
        Err(err) => return Err(err).with_context(|| format!("Cannot open {}", db.display())),
    }
    // SQLite would copy the log into the store inside the commit that fills
    // it, while that writer still holds every other writer's turn; on a slow
    // disk that took seconds. [`Write::commit`] does it after the turn.
    conn.pragma_update(None, "wal_autocheckpoint", 0)?;
    conn.execute_batch(SCHEMA)?;
    conn.execute_batch(LINKS)?;
    conn.execute_batch(CUTOFFS)?;
    super::index::install(&conn)?;
    Ok(conn)
}

/// One write transaction at a time, for the writers that share it.
pub(super) struct Gate {
    turn: std::sync::Mutex<()>,
    /// The thread holding the turn, so a write opened inside another on the
    /// same thread never waits for itself.
    holder: std::sync::Mutex<Option<std::thread::ThreadId>>,
}

impl Gate {
    const fn new() -> Self {
        Self {
            turn: std::sync::Mutex::new(()),
            holder: std::sync::Mutex::new(None),
        }
    }

    fn holder(&self) -> std::sync::MutexGuard<'_, Option<std::thread::ThreadId>> {
        self.holder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// This thread's turn, or none when it already holds one.
    fn enter(&'static self) -> Option<Turn> {
        let me = std::thread::current().id();
        if *self.holder() == Some(me) {
            return None;
        }
        let guard = self
            .turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *self.holder() = Some(me);
        Some(Turn {
            gate: self,
            _guard: guard,
        })
    }
}

/// Write transactions in this process.
static WRITERS: Gate = Gate::new();

/// A thread's turn at a [`Gate`]; the turn passes on when it drops.
struct Turn {
    gate: &'static Gate,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl Drop for Turn {
    // Runs before the guard drops, so the next holder records itself after.
    fn drop(&mut self) {
        *self.gate.holder() = None;
    }
}

/// How long a writer waits for the store: for another process's write turn,
/// and as SQLite's busy timeout for SQLite's own locks. Writers queue fairly,
/// so a writer waits out only those ahead of it. Each commit waits until the
/// disk has its data, as full durability needs: a few milliseconds on a
/// working disk, seconds on a slow, busy one, where 5 s was too short for a
/// queue of eight. Past this the writer holding the turn has stalled.
pub(crate) const BUSY: Duration = Duration::from_secs(30);

/// [`BUSY`] for the write turn, which debug builds take in milliseconds
/// from `USER_MEMORY_TURN_WAIT_MS` so tests can see a stalled turn give up.
fn turn_wait() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("USER_MEMORY_TURN_WAIT_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
    {
        return Duration::from_millis(ms);
    }
    BUSY
}

/// Log size past which a commit copies the log into the store once its turn
/// has passed on: SQLite's own default of 1,000 pages of 4 KiB.
const CHECKPOINT_BYTES: u64 = 1000 * 4096;

/// The file beside the store whose lock gives writers in every process their
/// turn: the store with `.lock` appended.
pub fn lock_path(db: &Path) -> std::path::PathBuf {
    let mut path = db.as_os_str().to_owned();
    path.push(".lock");
    path.into()
}

/// This process's turn at the store's lock file, until it drops. The lock is
/// `flock`'s, owned by this open file description: closing another
/// descriptor of the file, as SQLite and std do, never drops it, where a
/// POSIX record lock would be dropped by any close in the process.
struct FileTurn(#[allow(dead_code, reason = "held for its drop")] fs::File);

impl FileTurn {
    /// Waits in the kernel's queue for the lock, at most [`BUSY`]. The wait
    /// runs on a thread of its own: past the bound this gives up, and that
    /// thread drops the lock as soon as it gets it.
    fn take(path: &Path) -> Result<Self> {
        let file = private::create_file(
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false),
            path,
        )
        .with_context(|| format!("Cannot open {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => return Ok(Self(file)),
            Err(fs::TryLockError::WouldBlock) => {}
            Err(fs::TryLockError::Error(err)) => {
                return Err(err).with_context(|| format!("Cannot lock {}", path.display()));
            }
        }
        let (locked, waiting) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = locked.send(file.lock().map(|()| file));
        });
        let wait = turn_wait();
        match waiting.recv_timeout(wait) {
            Ok(Ok(file)) => Ok(Self(file)),
            Ok(Err(err)) => Err(err).with_context(|| format!("Cannot lock {}", path.display())),
            Err(_) => bail!(
                "database is locked: another process kept its write turn on the store for {} s",
                wait.as_secs()
            ),
        }
    }
}

/// A write transaction (`BEGIN IMMEDIATE`) that waits its turn behind the
/// other writers, in this process and then in every process. A server runs
/// many calls at once and every agent session runs a server, and SQLite
/// makes a writer that finds the store locked sleep and try again: under
/// load an unlucky writer sleeps past the busy timeout while the lock is
/// mostly free. Waiting in a queue instead hands the lock on as soon as it
/// is released. A store without a file, such as one in memory, has only
/// the turn in this process.
pub(super) struct Write<'c> {
    // Fields drop in order: the transaction ends before the turns pass on.
    tx: Transaction<'c>,
    file: Option<FileTurn>,
    turn: Option<Turn>,
    conn: &'c Connection,
}

pub(super) fn write(conn: &Connection) -> Result<Write<'_>> {
    write_at(&WRITERS, conn)
}

fn write_at<'c>(gate: &'static Gate, conn: &'c Connection) -> Result<Write<'c>> {
    let turn = gate.enter();
    // A write inside another on this thread already has the file's turn.
    let file = match (&turn, conn.path().filter(|path| !path.is_empty())) {
        (Some(_), Some(db)) => Some(FileTurn::take(&lock_path(Path::new(db)))?),
        _ => None,
    };
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    Ok(Write {
        tx,
        file,
        turn,
        conn,
    })
}

impl<'c> std::ops::Deref for Write<'c> {
    type Target = Transaction<'c>;
    fn deref(&self) -> &Transaction<'c> {
        &self.tx
    }
}

#[cfg(test)]
thread_local! {
    /// Makes [`checkpoint`] fail on this thread, for the tests of a write
    /// whose copy into the store fails.
    static FAIL_CHECKPOINT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Copies the log into the store with SQLite's passive checkpoint.
fn checkpoint(conn: &Connection) -> rusqlite::Result<()> {
    #[cfg(test)]
    if FAIL_CHECKPOINT.get() {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
            Some("checkpoint failed for a test".into()),
        ));
    }
    conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))
}

impl Write<'_> {
    /// Commits, passes the turns on, then copies a long log into the store
    /// and makes the day's backup when one is due. The copy is SQLite's
    /// passive checkpoint: it waits for no one, and a copy left unfinished,
    /// or one that fails, is taken up by the next commit. Only the commit
    /// decides whether the write succeeded; a failed copy is reported on
    /// stderr, and the backup runs either way. A write inside another on
    /// this thread leaves the backup to the outer one, which holds the turn.
    pub(super) fn commit(self) -> Result<()> {
        let Self {
            tx,
            file,
            turn,
            conn,
        } = self;
        tx.commit()?;
        drop(file);
        let outermost = turn.is_some();
        drop(turn);
        if let Some(db) = conn.path().filter(|path| !path.is_empty()) {
            let mut log = std::ffi::OsString::from(db);
            log.push("-wal");
            if fs::metadata(&log).is_ok_and(|log| log.len() > CHECKPOINT_BYTES)
                && let Err(err) = checkpoint(conn)
            {
                // The write is already stored; the next commit tries again.
                eprintln!("user-memory: copying the log into {db} failed: {err}");
            }
            if outermost {
                super::backups::back_up_if_due(Path::new(db));
            }
        }
        Ok(())
    }
    pub(super) fn rollback(self) -> rusqlite::Result<()> {
        self.tx.rollback()
    }
}

/// In debug builds only: while the file `USER_MEMORY_PAUSE_OPEN` names
/// exists, an open of a store that is not yet current waits after reading
/// its version, as one descheduled there would; it first creates that path
/// with `.reached` appended, so a test knows it got there.
#[cfg(debug_assertions)]
fn pause_for_tests() {
    let Some(pause) = std::env::var_os("USER_MEMORY_PAUSE_OPEN") else {
        return;
    };
    let mut reached = pause.clone();
    reached.push(".reached");
    let _ = fs::write(&reached, "");
    while Path::new(&pause).exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub(super) fn version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
}

/// How a database's schema version relates to this binary's.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Found {
    /// A database without tables, ready for the schema.
    Empty,
    Current,
    /// From an earlier version that `migrate` can upgrade.
    Older,
    /// From a later version of user-memory.
    Newer,
    /// Unversioned, from before the migration baseline, or not user-memory's.
    Foreign,
}

pub(super) fn classify(found: i64, has_tables: bool, baseline: i64, current: i64) -> Found {
    match found.cmp(&current) {
        std::cmp::Ordering::Equal => Found::Current,
        std::cmp::Ordering::Greater => Found::Newer,
        std::cmp::Ordering::Less if found == 0 && !has_tables => Found::Empty,
        std::cmp::Ordering::Less if found >= baseline => Found::Older,
        std::cmp::Ordering::Less => Found::Foreign,
    }
}

/// Classifies the database behind `conn`, at schema version `found`.
pub(super) fn inspect(conn: &Connection, found: i64, baseline: i64, current: i64) -> Result<Found> {
    let tables: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table'",
        [],
        |r| r.get(0),
    )?;
    Ok(classify(found, tables > 0, baseline, current))
}

/// Why a database at schema version `found` cannot be used as it is by a
/// binary at `current`, or None when it is current or empty.
pub(super) fn refusal(
    conn: &Connection,
    db: &Path,
    found: i64,
    kind: &Found,
    current: i64,
) -> Result<Option<String>> {
    let db = db.display();
    Ok(match kind {
        Found::Empty | Found::Current => None,
        Found::Older => Some(format!(
            "{db} has schema version {found}; run the installer again (or ./setup, or `user-memory migrate`) to upgrade it to version {current}. A backup is made first; the store was left unchanged."
        )),
        Found::Newer => Some(format!(
            "{db} has schema version {found}, but this user-memory is older than the store (it uses version {current}); update user-memory and restart your client. The store was left unchanged."
        )),
        Found::Foreign => {
            let memories: Option<i64> = conn
                .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
                .ok();
            let holds = match memories {
                Some(count) => format!("holds {count} memories"),
                None => "has no user-memory table".into(),
            };
            let found = if found == 0 {
                "an unversioned schema".to_owned()
            } else {
                format!("schema version {found}")
            };
            Some(format!(
                "{db} has {found} but this user-memory needs schema version {current}; it {holds} and was left unchanged. Move it aside (for example rename it with a .bak suffix) and run setup again to start a fresh store; existing memories are not copied automatically. To keep them, run `user-memory export FILE` with the version that created the store, then move it aside, run setup and `user-memory import FILE --apply`."
            ))
        }
    })
}

pub(super) const SELECT: &str = "SELECT id,created_at,updated_at,memory_type,category,context,content,reason,source,status,scope,project,evidence,evidence_ids,rowid,(SELECT json_group_array(origin) FROM (SELECT origin FROM memory_origins WHERE memory_id=memories.id ORDER BY origin)),superseded_by,last_confirmed_at,title FROM memories";
pub(super) fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Memory> {
    let evidence: String = r.get(13)?;
    let origins: Vec<String> = serde_json::from_str(&r.get::<_, String>(15)?).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(15, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let evidence_ids = serde_json::from_str(&evidence).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(13, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(Memory {
        id: Uuid::parse_str(&r.get::<_, String>(0)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?,
        created_at: r.get(1)?,
        updated_at: r.get(2)?,
        memory_type: r.get(3)?,
        category: r.get(4)?,
        context: r.get(5)?,
        title: r.get(18)?,
        content: r.get(6)?,
        reason: r.get(7)?,
        source: r.get(8)?,
        status: r.get(9)?,
        scope: r.get(10)?,
        project: r.get(11)?,
        evidence: r.get(12)?,
        evidence_ids,
        sequence: r.get(14)?,
        confirmations: origins.len(),
        origins,
        last_confirmed_at: r.get(17)?,
        superseded_by: r
            .get::<_, Option<String>>(16)?
            .map(|id| {
                Uuid::parse_str(&id).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        16,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })
            })
            .transpose()?,
    })
}
pub(super) fn get(conn: &Connection, id: Uuid) -> Result<Option<Memory>> {
    Ok(conn
        .query_row(&format!("{SELECT} WHERE id=?1"), [id.to_string()], row)
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Longest a test waits for a write that should go through; past it the
    /// write is stuck for good, and the test fails instead of hanging.
    const STUCK: Duration = Duration::from_secs(10);

    // A writer waits its turn instead of SQLite's busy timeout: with no
    // timeout at all it still gets the lock once the writer before it
    // commits.
    #[test]
    fn a_writer_waits_its_turn_behind_another_in_this_process() {
        static GATE: Gate = Gate::new();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("m.sqlite3");
        let first = connect(&db).unwrap();
        let held = write_at(&GATE, &first).unwrap();
        let (started, waiting) = mpsc::channel();
        let second = std::thread::spawn({
            let db = db.clone();
            move || {
                let conn = connect(&db).unwrap();
                conn.busy_timeout(Duration::ZERO).unwrap();
                started.send(()).unwrap();
                write_at(&GATE, &conn).map(|tx| tx.commit())
            }
        });
        waiting.recv().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        held.commit().unwrap();
        assert!(matches!(second.join().unwrap(), Ok(Ok(()))));
    }

    // A commit that leaves the log past its limit copies it into the store
    // once the turn has passed, as SQLite's own copy inside the commit is off.
    #[test]
    fn a_commit_past_the_log_limit_copies_the_log_into_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("m.sqlite3");
        let conn = connect(&db).unwrap();
        let size = 2 * CHECKPOINT_BYTES as i64;
        let tx = write(&conn).unwrap();
        tx.execute_batch("CREATE TABLE big(b BLOB)").unwrap();
        tx.execute("INSERT INTO big VALUES (randomblob(?1))", [size])
            .unwrap();
        tx.commit().unwrap();
        assert!(fs::metadata(&db).unwrap().len() > size as u64);
    }

    // The write is stored once the transaction commits: a copy of the log
    // that fails afterwards is reported, never turned into a failed write.
    #[test]
    fn a_committed_write_whose_checkpoint_fails_still_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("m.sqlite3");
        let conn = connect(&db).unwrap();
        let size = 2 * CHECKPOINT_BYTES as i64;
        let tx = write(&conn).unwrap();
        tx.execute_batch("CREATE TABLE big(b BLOB)").unwrap();
        tx.execute("INSERT INTO big VALUES (randomblob(?1))", [size])
            .unwrap();
        FAIL_CHECKPOINT.set(true);
        let committed = tx.commit();
        FAIL_CHECKPOINT.set(false);
        assert!(committed.is_ok(), "{committed:?}");
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM big", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    // A failed copy of the log never costs the day's backup.
    #[test]
    fn a_write_whose_checkpoint_fails_still_makes_the_days_backup() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memories.sqlite3");
        let conn = connect(&db).unwrap();
        let size = 2 * CHECKPOINT_BYTES as i64;
        let tx = write(&conn).unwrap();
        tx.execute_batch("CREATE TABLE big(b BLOB)").unwrap();
        tx.execute("INSERT INTO big VALUES (randomblob(?1))", [size])
            .unwrap();
        tx.execute(
            "INSERT INTO memories (id,memory_type,category,title,content) VALUES ('0bd1ba49-2b63-4aab-8d57-38d9be10a281','fact','tooling','A fact','A fact')",
            [],
        )
        .unwrap();
        FAIL_CHECKPOINT.set(true);
        let committed = tx.commit();
        FAIL_CHECKPOINT.set(false);
        assert!(committed.is_ok(), "{committed:?}");
        assert_eq!(super::super::backups::list(&db).unwrap().len(), 1);
    }

    // One bound serves the wait for a write turn and SQLite's busy timeout.
    #[test]
    fn writers_wait_one_bound_for_their_turn_and_for_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let conn = connect(&dir.path().join("m.sqlite3")).unwrap();
        let busy: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(u128::try_from(busy).unwrap(), BUSY.as_millis());
        assert_eq!(turn_wait(), BUSY);
    }

    // The lock is the open file description's (flock): closing another
    // descriptor of the file, as any code in the process may, keeps it, and
    // another description of it in this same process is refused it.
    #[test]
    fn the_file_turn_outlives_another_descriptor_of_the_file_closing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.sqlite3.lock");
        let turn = FileTurn::take(&path).unwrap();
        drop(fs::File::open(&path).unwrap());
        let other = fs::OpenOptions::new().write(true).open(&path).unwrap();
        assert!(matches!(
            other.try_lock(),
            Err(fs::TryLockError::WouldBlock)
        ));
        drop(turn);
        assert!(other.try_lock().is_ok());
    }

    // A write opened inside another on the same thread, on a second
    // connection, meets SQLite's lock instead of waiting for itself. The
    // writes run on their own thread with their own gate, so a deadlock
    // fails this test and leaves the rest of the tests running.
    #[test]
    fn a_write_inside_another_on_one_thread_never_waits_for_itself() {
        static GATE: Gate = Gate::new();
        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("m.sqlite3");
            let (outer, inner) = (connect(&db).unwrap(), connect(&db).unwrap());
            inner.busy_timeout(Duration::ZERO).unwrap();
            let held = write_at(&GATE, &outer).unwrap();
            let refused = write_at(&GATE, &inner).is_err();
            held.commit().unwrap();
            done.send(refused).unwrap();
        });
        match finished.recv_timeout(STUCK) {
            Ok(refused) => assert!(refused, "the nested write got a second write lock"),
            Err(_) => panic!("nested write deadlocked on the process write lock"),
        }
    }
}
