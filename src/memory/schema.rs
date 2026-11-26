//! SQLite storage: schema, connection and row mapping.

use super::Memory;
use crate::private;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use std::{fs, path::Path, time::Duration};
use uuid::Uuid;

/// Stamped into `PRAGMA user_version`. Bump it whenever `SCHEMA`, the
/// search index SQL in `index.rs` or the change counter's in `changes.rs`
/// changes, and add the step that upgrades the
/// previous version in `migrate.rs`: `IF NOT EXISTS` keeps an older table or
/// trigger as it was, so connecting refuses a store from another version
/// instead of half upgrading it.
pub(super) const SCHEMA_VERSION: i64 = 13;

/// The tables and the triggers that stale inferred patterns when their
/// support weakens: a supporting memory deleted; one changed in what it
/// says, superseded, made stale or weakened in source or status (a
/// confirmation, observed to explicit or tentative to confirmed with its
/// evidence note dropped, only strengthens it; a reason or title is
/// wording); one moved to a project the pattern is not in, which recording
/// refuses too (made global, or moved with its pattern, it still holds); or
/// a removed conversation leaving the supports fewer than two independent
/// conversations, counting each support without one as its own, as
/// recording an inference requires; or a supporting memory ended.
///
/// A memory may hold until a condition (#121): one kind per memory, a UTC
/// instant in one fixed format (so comparing the text compares the times),
/// a tag with the project whose tags count, or free text. `memory_ended`
/// records the end of a tag or text condition, once; a date ends when it
/// passes, with no write. `live_memories` is what is in force: confirmed or
/// tentative, its date not passed, not ended. Every reader that shows
/// memories reads it, so no channel can miss an end; it selects `rowid`,
/// which the store reads.
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
    until_kind TEXT NOT NULL DEFAULT '' CHECK(until_kind IN ('','date','tag','text')),
    until_value TEXT NOT NULL DEFAULT '',
    until_project TEXT NOT NULL DEFAULT '' CHECK((until_kind='' AND until_value='' AND until_project='') OR (until_kind='date' AND until_value GLOB '[0-9][0-9][0-9][0-9]-[01][0-9]-[0-3][0-9]T[0-2][0-9]:[0-5][0-9]:[0-5][0-9]Z' AND until_project='') OR (until_kind='tag' AND length(until_value) BETWEEN 1 AND 255 AND length(until_project) BETWEEN 1 AND 240) OR (until_kind='text' AND length(until_value) BETWEEN 1 AND 120 AND until_project='')),
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
END;
CREATE TABLE IF NOT EXISTS memory_ended (
    memory_id TEXT PRIMARY KEY NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    ended_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    how TEXT NOT NULL CHECK(how IN ('tag','evidence')),
    evidence TEXT NOT NULL CHECK(length(evidence) BETWEEN 1 AND 2000),
    origin TEXT NOT NULL DEFAULT ''
);
CREATE TRIGGER IF NOT EXISTS memory_support_ended AFTER INSERT ON memory_ended BEGIN
    UPDATE memories SET status='stale',updated_at=CURRENT_TIMESTAMP
    WHERE status!='superseded' AND EXISTS (SELECT 1 FROM json_each(evidence_ids) WHERE value=NEW.memory_id);
END;
CREATE VIEW IF NOT EXISTS live_memories AS SELECT rowid AS rowid, * FROM memories
WHERE status IN ('confirmed','tentative')
    AND NOT (until_kind='date' AND until_value<=strftime('%Y-%m-%dT%H:%M:%SZ','now'))
    AND NOT EXISTS (SELECT 1 FROM memory_ended WHERE memory_id=memories.id);";

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
    conn.busy_timeout(super::budget::wait(BUSY))?;
    // Per connection and ignored inside a transaction, so set it first.
    conn.pragma_update(None, "foreign_keys", true)?;
    // Deleted and replaced content is overwritten with zeros rather than
    // left in free space, where a forgotten memory's text would stay in
    // the file and in every page-by-page backup of it.
    conn.pragma_update(None, "secure_delete", true)?;
    let current = schema_version();
    let found = wait_for_migration(&conn, version(&conn)?, current)?;
    if found != current {
        #[cfg(debug_assertions)]
        pause_for_tests();
        // Refuse a store this binary does not own before taking a write
        // turn, which would leave a lock file beside it. Version and tables
        // come from one read snapshot: a store another process is creating
        // meanwhile is then still empty or already current, never tables
        // without their version.
        let snapshot = conn.unchecked_transaction()?;
        let found = version(&snapshot)?;
        if found != current {
            let kind = inspect(&snapshot, found, super::migrate::BASELINE, current)?;
            if let Some(refusal) = refusal(&snapshot, db, found, &kind, current)? {
                bail!(refusal);
            }
        }
        drop(snapshot);
        // Decide again under the write lock so concurrent first connections
        // agree.
        let tx = write(&conn)?;
        let found = version(&tx)?;
        if found != current {
            // Only an empty database gets the schema; servers never migrate.
            let kind = inspect(&tx, found, super::migrate::BASELINE, current)?;
            if let Some(refusal) = refusal(&tx, db, found, &kind, current)? {
                bail!(refusal);
            }
            tx.execute_batch(SCHEMA)?;
            tx.execute_batch(LINKS)?;
            tx.execute_batch(CUTOFFS)?;
            tx.execute_batch(super::history::EVENTS)?;
            tx.execute_batch(super::usage::TABLE)?;
            super::index::create(&tx)?;
            super::index::start(&tx)?;
            tx.execute_batch(&super::changes::schema())?;
            tx.execute_batch(super::changes::START)?;
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
    conn.busy_timeout(super::budget::wait(BUSY))?;
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
    conn.execute_batch(super::history::EVENTS)?;
    conn.execute_batch(super::usage::TABLE)?;
    super::index::create(&conn)?;
    conn.execute_batch(&super::changes::schema())?;
    Ok(conn)
}

/// How long a `serve` that took over a session from a replaced binary
/// waits for a store older than its code to be migrated. `user-memory
/// upgrade` replaces the binary first, then setup backs the store up and
/// migrates it, which a call meeting the store then waits for instead of
/// being refused. 50 s covers the 45 s budget for v0.10.0's whole upgrade
/// of 100,000 memories (evals/scale/upgrade.py), which measured 17.6 s
/// (median of three) from schema 6 to 9 on 100,000 memories with
/// 2,560-dimension vectors (1.3 GB), release build on an Apple M4 Pro.
/// Codex gives an MCP tool call 300 s by default (`DEFAULT_TOOL_TIMEOUT`
/// in its codex-rs/codex-mcp/src/rmcp_client.rs), so the waiting call is
/// answered rather than timed out there. Past the wait the call is refused
/// as before: the store is not being migrated.
const MIGRATION_WAIT: Duration = Duration::from_secs(50);
/// How often the waiting `serve` reads the store's version again.
const MIGRATION_POLL: Duration = Duration::from_millis(100);
/// Until when this process waits for a migration: set by a `serve` that
/// took over a session.
static MIGRATION_DEADLINE: std::sync::Mutex<Option<std::time::Instant>> =
    std::sync::Mutex::new(None);

/// Makes this process wait, up to [`MIGRATION_WAIT`] from now, for a store
/// older than its code to be migrated, instead of refusing it at once.
pub fn expect_migration() {
    *MIGRATION_DEADLINE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(std::time::Instant::now() + MIGRATION_WAIT);
}

/// The schema version of the store behind `conn`, found at `found`: once a
/// migration expected by [`expect_migration`] made it `current`, or the
/// wait ran out. Only an older store with tables waits; an empty, current,
/// newer or foreign one never does.
fn wait_for_migration(conn: &Connection, mut found: i64, current: i64) -> Result<i64> {
    let deadline = *MIGRATION_DEADLINE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(deadline) = deadline else {
        return Ok(found);
    };
    let deadline = deadline.min(std::time::Instant::now() + super::budget::wait(MIGRATION_WAIT));
    while found < current
        && std::time::Instant::now() < deadline
        && inspect(conn, found, super::migrate::BASELINE, current)? == Found::Older
    {
        std::thread::sleep(MIGRATION_POLL);
        found = version(conn)?;
    }
    Ok(found)
}

/// Names, in debug builds only, the schema version `connect` compares a
/// store against in place of [`SCHEMA_VERSION`]: the tests' newer build
/// meets a store migrated past this one. A new store still gets
/// [`SCHEMA_VERSION`], and no migration reads it.
#[cfg(debug_assertions)]
const TEST_SCHEMA_VERSION: &str = "USER_MEMORY_TEST_SCHEMA_VERSION";

/// How long [`servable`] waits for a lock that keeps readers out, such as a
/// commit to a store not yet on the write-ahead log, so a healthy store
/// busy for a moment is not reported as broken. A commit takes seconds at
/// most on a slow, busy disk; this stays well under the 20 seconds the
/// Docker healthcheck waits for its answer.
const SERVABLE_WAIT: Duration = Duration::from_secs(10);

/// Whether the store at `db` can be served, checked without writing to it:
/// it exists, reads as a database, and has this binary's schema version.
/// For the server's unauthenticated health check, which must never create,
/// migrate or convert a store as [`connect`] does.
pub fn servable(db: &Path) -> Result<()> {
    let conn = Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("Cannot open {}", db.display()))?;
    conn.busy_timeout(SERVABLE_WAIT)?;
    let found = version(&conn).with_context(|| format!("Cannot read {}", db.display()))?;
    let current = schema_version();
    if found != current {
        bail!(
            "{} has schema version {found}; this server serves version {current}",
            db.display()
        );
    }
    Ok(())
}

/// The schema version `connect` compares a store against: [`SCHEMA_VERSION`],
/// or in a debug build the one [`TEST_SCHEMA_VERSION`] names.
fn schema_version() -> i64 {
    #[cfg(debug_assertions)]
    if let Some(version) = std::env::var(TEST_SCHEMA_VERSION)
        .ok()
        .and_then(|version| version.parse().ok())
    {
        return version;
    }
    SCHEMA_VERSION
}

/// A connection that keeps the store at `db` open while a server runs, or
/// none when there is no store yet or it cannot be opened, which each call
/// then reports. SQLite removes a WAL store's log and shared-memory files
/// when its last connection closes and makes them again at the next open;
/// a server opens a connection per call, so without this every call paid
/// that, about 10 ms and at times far more, a third of a search on a large
/// store.
pub fn hold(db: &Path) -> Option<Connection> {
    db.is_file().then(|| connect(db).ok()).flatten()
}

/// One write transaction at a time, for the writers that share it.
pub(super) struct Gate {
    /// The thread holding the turn, or none while it is free; a write opened
    /// inside another on the same thread never waits for itself.
    holder: std::sync::Mutex<Option<std::thread::ThreadId>>,
    /// Signalled each time the turn is freed.
    freed: std::sync::Condvar,
}

impl Gate {
    const fn new() -> Self {
        Self {
            holder: std::sync::Mutex::new(None),
            freed: std::sync::Condvar::new(),
        }
    }

    fn holder(&self) -> std::sync::MutexGuard<'_, Option<std::thread::ThreadId>> {
        self.holder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// This thread's turn, waiting at most `wait` for it, or none when it
    /// already holds one. Past `wait` it refuses as a writer kept waiting by
    /// another process does: a queue behind a stalled write must not wait
    /// without end.
    fn enter(&'static self, wait: Duration) -> Result<Option<Turn>> {
        let me = std::thread::current().id();
        let holder = self.holder();
        if *holder == Some(me) {
            return Ok(None);
        }
        let (mut holder, _) = self
            .freed
            .wait_timeout_while(holder, wait, |holder| holder.is_some())
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if holder.is_some() {
            bail!(
                "database is locked: another write in this process kept its write turn on the store for {} s{}",
                wait.as_secs(),
                super::budget::ran_out()
            );
        }
        *holder = Some(me);
        Ok(Some(Turn { gate: self }))
    }
}

/// Write transactions in this process.
static WRITERS: Gate = Gate::new();

/// A thread's turn at a [`Gate`]; the turn passes on when it drops.
struct Turn {
    gate: &'static Gate,
}

impl Drop for Turn {
    fn drop(&mut self) {
        *self.gate.holder() = None;
        self.gate.freed.notify_one();
    }
}

/// How long a writer waits at each stage of taking the store: its process's
/// turn, another process's write turn, and, as SQLite's busy timeout,
/// SQLite's own locks. Writers take turns in no promised order, and the
/// bound holds per stage, not for the whole write. Each commit waits until
/// the disk has its data, as full durability needs: a few milliseconds on a
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
    /// Waits in the kernel's queue for the lock, at most `wait`. The wait
    /// runs on a thread of its own: past the bound this gives up, and that
    /// thread drops the lock as soon as it gets it.
    fn take(path: &Path, wait: Duration) -> Result<Self> {
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
        match waiting.recv_timeout(wait) {
            Ok(Ok(file)) => Ok(Self(file)),
            Ok(Err(err)) => Err(err).with_context(|| format!("Cannot lock {}", path.display())),
            Err(_) => bail!(
                "database is locked: another process kept its write turn on the store for {} s{}",
                wait.as_secs(),
                super::budget::ran_out()
            ),
        }
    }
}

/// The store's write turn, in this process and then in every process, for a
/// writer that opens its own transaction, as a migration and a project move
/// do: it calls [`StoreTurn::begin`] inside its transaction and
/// [`StoreTurn::publish`] after its commit, as every [`Write`] does, so the
/// store's marker ([`super::changes`]) stays right.
pub struct StoreTurn {
    // Fields drop in order: the file's turn passes on before this process's.
    file: Option<FileTurn>,
    #[allow(dead_code, reason = "held for its drop")]
    turn: Option<Turn>,
    db: std::path::PathBuf,
}

/// Takes the write turn on the store at `db`, waiting as a write does.
pub fn store_turn(db: &Path) -> Result<StoreTurn> {
    let turn = WRITERS.enter(super::budget::wait(turn_wait()))?;
    let file = match turn {
        Some(_) => Some(FileTurn::take(
            &lock_path(db),
            super::budget::wait(turn_wait()),
        )?),
        None => None,
    };
    Ok(StoreTurn {
        file,
        turn,
        db: db.to_owned(),
    })
}

impl StoreTurn {
    /// Inside the writer's transaction: renews the store's id when its
    /// marker does not match it.
    pub fn begin(&self, tx: &Connection) -> Result<()> {
        if self.file.is_some() {
            super::changes::begin(tx, &self.db)?;
        }
        Ok(())
    }

    /// After the writer's commit: rewrites the marker.
    pub fn publish(&self, conn: &Connection) {
        if self.file.is_some() {
            super::changes::publish(conn, &self.db);
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
    /// Whether this write named its client for the history's events, a row
    /// it removes before committing.
    named: bool,
    file: Option<FileTurn>,
    turn: Option<Turn>,
    conn: &'c Connection,
    /// The store's latest change when this write began, for a file-backed
    /// store that counts its changes.
    since: Option<i64>,
}

pub(super) fn write(conn: &Connection) -> Result<Write<'_>> {
    write_at(&WRITERS, conn, turn_wait())
}

/// [`write`] at `gate`, waiting at most `wait` for each turn and SQLite's
/// lock, or what is left of the call's budget ([`super::budget`]).
fn write_at<'c>(gate: &'static Gate, conn: &'c Connection, wait: Duration) -> Result<Write<'c>> {
    #[cfg(debug_assertions)]
    assert!(
        !super::delivery_files::holds_lock(),
        "a store write turn is never taken under a delivery lock"
    );
    let turn = gate.enter(super::budget::wait(wait))?;
    // A write inside another on this thread already has the file's turn.
    let file = match (&turn, conn.path().filter(|path| !path.is_empty())) {
        (Some(_), Some(db)) => Some(FileTurn::take(
            &lock_path(Path::new(db)),
            super::budget::wait(wait),
        )?),
        _ => None,
    };
    // Inside a call, what is left of its budget: the connection's timeout
    // was set when it opened, and the budget has run on since. Outside one
    // the connection keeps its own.
    if super::budget::deadline().is_some() {
        conn.busy_timeout(super::budget::wait(BUSY))?;
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let named = super::history::begin(&tx)?;
    if file.is_some()
        && let Some(db) = conn.path()
    {
        super::changes::begin(&tx, Path::new(db))?;
    }
    let since = match file {
        Some(_) => super::changes::state(&tx)?.map(|state| state.last_change),
        None => None,
    };
    Ok(Write {
        tx,
        named,
        file,
        turn,
        conn,
        since,
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
pub(super) fn checkpoint(conn: &Connection) -> rusqlite::Result<()> {
    #[cfg(test)]
    if FAIL_CHECKPOINT.get() {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
            Some("checkpoint failed for a test".into()),
        ));
    }
    conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))
}

/// How long clearing the log after a write that removed text waits for
/// readers to finish: they hold the log for milliseconds, and a forget's
/// reply must not wait as long as a writer may ([`BUSY`]).
const CLEAR_LOG_WAIT: Duration = Duration::from_secs(1);

/// Copies the whole log into the store and empties it, so no earlier
/// version of a page, such as one holding a forgotten memory's text, stays
/// in the log file. Waits at most [`CLEAR_LOG_WAIT`] for readers; a log a
/// reader still holds is left, and noted on stderr.
fn clear_log(conn: &Connection, db: &str) {
    let cleared = conn.busy_timeout(CLEAR_LOG_WAIT).and_then(|()| {
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
            r.get::<_, i64>(0)
        })
    });
    let _ = conn.busy_timeout(super::budget::wait(BUSY));
    match cleared {
        Ok(0) => {}
        Ok(_) => eprintln!(
            "user-memory: the log of {db} still holds earlier versions of what was removed: another connection was reading it; the next write that removes text clears it"
        ),
        Err(err) => eprintln!("user-memory: clearing the log of {db} failed: {err}"),
    }
}

/// Rewrites the store at `db` without its free space (SQLite's `VACUUM`)
/// when it holds memories, then clears the log, and says whether it did.
/// A store written before deleted content was overwritten keeps forgotten
/// text in free space inside its pages, which no count shows, so setup and
/// migrate rewrite every store with memories. Waits for other writers as
/// long as a write does; past that it fails with SQLite's busy error and
/// the store is left as it was.
pub fn compact(db: &Path) -> Result<bool> {
    compact_waiting(db, turn_wait())
}

/// [`compact`], waiting at most `wait` for other writers.
fn compact_waiting(db: &Path, wait: Duration) -> Result<bool> {
    let conn = connect(db)?;
    let memories: i64 = conn.query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))?;
    if memories == 0 {
        return Ok(false);
    }
    conn.busy_timeout(wait)?;
    conn.execute_batch("VACUUM")?;
    conn.busy_timeout(BUSY)?;
    clear_log(&conn, &db.to_string_lossy());
    Ok(true)
}

/// What [`compact`] did to the store at `db` when an upgrade ran it.
pub fn compacted(db: &Path) -> String {
    format!("Rewrote {} without its free space.", db.display())
}

/// The note when [`compact`] could not rewrite the store at `db`: nothing is
/// lost, and the next setup or migrate tries again.
pub fn compact_skipped(db: &Path, err: &anyhow::Error) -> String {
    format!(
        "user-memory: rewriting {} without its free space was skipped ({err:#}); text forgotten before this version may stay in it until the next setup or migrate",
        db.display()
    )
}

impl Write<'_> {
    /// Says what this write does, for the history events its changes add:
    /// `op` (`update`, `confirm`, `merge` or `import`, else none), the
    /// memory it is about, and the conversation it comes from.
    pub(super) fn note(
        &mut self,
        op: Option<&str>,
        subject: Option<Uuid>,
        origin: &str,
    ) -> Result<()> {
        super::history::note(&self.tx, op, subject, origin)?;
        self.named = true;
        Ok(())
    }

    /// Indexes the memories this write changed, commits, passes the turns
    /// on, then copies a long log into the store and makes the day's backup
    /// when one is due. The copy is SQLite's
    /// passive checkpoint: it waits for no one, and a copy left unfinished,
    /// or one that fails, is taken up by the next commit. Only the commit
    /// decides whether the write succeeded; a failed copy is reported on
    /// stderr, and the backup runs either way. A write inside another on
    /// this thread leaves the backup to the outer one, which holds the turn.
    pub(super) fn commit(self) -> Result<()> {
        self.finish(false, None).map(drop)
    }

    /// [`commit`](Self::commit) for a write that removed or replaced
    /// memory text, which it does as `removing`: after the commit the log
    /// is cleared, so neither the store's files nor a backup made after hold
    /// the old text. The pages themselves lose it as it is deleted
    /// (`secure_delete`, and the keyword index's `secure-delete`). This
    /// machine's delivery index loses it too: under its lock, taken before
    /// the commit, the files holding a memory whose text went are built
    /// again ([`delivery_files::before_removal`]).
    ///
    /// [`delivery_files::before_removal`]: super::delivery_files::before_removal
    ///
    /// Returns the delivery files that still hold removed text, for the
    /// reply ([`Left::note`](super::delivery_files::Left::note)).
    pub(super) fn commit_removing(
        self,
        removing: super::delivery_files::Removing,
    ) -> Result<super::delivery_files::Left> {
        self.finish(true, Some(removing))
    }

    /// [`commit_removing`](Self::commit_removing) for a write that removed
    /// no memory's text, only its history.
    pub(super) fn commit_removing_history(self) -> Result<()> {
        self.finish(true, None).map(drop)
    }

    /// The memories whose text this write removes or replaces: those it
    /// deleted, and those whose words wait to be indexed again.
    fn touched(&self) -> Result<std::collections::HashSet<String>> {
        let mut touched = std::collections::HashSet::new();
        let mut pending = self
            .tx
            .prepare("SELECT k.id FROM search_pending p JOIN search_keys k ON k.key=p.key")?;
        for id in pending.query_map([], |r| r.get::<_, String>(0))? {
            touched.insert(id?);
        }
        if let Some(since) = self.since {
            let mut removed = self.tx.prepare(
                "SELECT memory_id FROM memory_changes WHERE change>?1 AND removed_at IS NOT NULL",
            )?;
            for id in removed.query_map([since], |r| r.get::<_, String>(0))? {
                touched.insert(id?);
            }
        }
        Ok(touched)
    }

    fn finish(
        self,
        removed: bool,
        removing: Option<super::delivery_files::Removing>,
    ) -> Result<super::delivery_files::Left> {
        let local = conn_file(self.conn).filter(|_| self.file.is_some());
        let removal = match (removing, &local) {
            (Some(removing), Some(db)) => {
                super::delivery_files::before_removal(db, &self.touched()?, removing)?
            }
            _ => None,
        };
        let Self {
            tx,
            named,
            file,
            turn,
            conn,
            since: _,
        } = self;
        let committed = (|| -> Result<()> {
            if named {
                super::history::end(&tx)?;
            }
            super::index::settle(&tx)?;
            #[cfg(debug_assertions)]
            if removal.is_some() && std::env::var_os(FAIL_REMOVAL).is_some() {
                anyhow::bail!("the commit failed for a test");
            }
            Ok(tx.commit()?)
        })();
        if let Err(err) = committed {
            if let Some(removal) = removal {
                removal.abandon();
            }
            return Err(err);
        }
        #[cfg(debug_assertions)]
        if removal.is_some() {
            super::hold_for_tests(super::delivery_files::HOLD_REMOVAL);
        }
        // Before the turns pass on, so no other writer's marker comes
        // between this commit and its own.
        if file.is_some()
            && let Some(db) = conn.path()
        {
            super::changes::publish(conn, Path::new(db));
        }
        drop(file);
        let outermost = turn.is_some();
        drop(turn);
        let left = match (removal, &local) {
            (Some(removal), Some(db)) => removal.rebuild(db),
            _ => super::delivery_files::Left::default(),
        };
        if let Some(db) = conn.path().filter(|path| !path.is_empty()) {
            let mut log = std::ffi::OsString::from(db);
            log.push("-wal");
            if removed {
                clear_log(conn, db);
            } else if fs::metadata(&log).is_ok_and(|log| log.len() > CHECKPOINT_BYTES)
                && let Err(err) = checkpoint(conn)
            {
                // The write is already stored; the next commit tries again.
                eprintln!("user-memory: copying the log into {db} failed: {err}");
            }
            if outermost {
                super::backups::back_up_if_due(Path::new(db));
            }
        }
        Ok(left)
    }
    pub(super) fn rollback(self) -> rusqlite::Result<()> {
        self.tx.rollback()
    }
}

/// Names, in debug builds only, a switch that fails a removing write's
/// commit after its delivery files were voided, for the test of what it
/// leaves.
#[cfg(debug_assertions)]
const FAIL_REMOVAL: &str = "USER_MEMORY_TEST_FAIL_REMOVAL";

/// The file of the store behind `conn`; none for one in memory.
fn conn_file(conn: &Connection) -> Option<std::path::PathBuf> {
    conn.path()
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
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
        let held = write_at(&GATE, &first, STUCK).unwrap();
        let (started, waiting) = mpsc::channel();
        let second = std::thread::spawn({
            let db = db.clone();
            move || {
                let conn = connect(&db).unwrap();
                conn.busy_timeout(Duration::ZERO).unwrap();
                started.send(()).unwrap();
                write_at(&GATE, &conn, STUCK).map(|tx| tx.commit())
            }
        });
        waiting.recv().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        held.commit().unwrap();
        assert!(matches!(second.join().unwrap(), Ok(Ok(()))));
    }

    // A writer queued behind another in this process waits at most its
    // bound, then gets the store's "database is locked" refusal, as one
    // waiting for another process's turn does; once the turn is free, the
    // next writer gets it. A writer already holding it never waits.
    #[test]
    fn a_writer_waits_at_most_its_bound_behind_another_in_this_process() {
        static GATE: Gate = Gate::new();
        let held = GATE.enter(Duration::ZERO).unwrap();
        assert!(held.is_some());
        assert!(
            GATE.enter(Duration::ZERO).unwrap().is_none(),
            "a turn within a turn"
        );
        let queued = |wait: Duration| {
            let (sent, got) = mpsc::channel();
            std::thread::spawn(move || {
                let turn = GATE.enter(wait).map(|turn| turn.is_some());
                let _ = sent.send(turn.map_err(|err| err.to_string()));
            });
            got
        };
        let locked = "database is locked: another write in this process kept its write turn on the store for 1 s";
        let refused = queued(Duration::from_secs(1))
            .recv_timeout(STUCK)
            .expect("the queue's wait has no bound");
        assert_eq!(refused, Err(locked.to_owned()));
        // A store write waits for the gate with the wait it is given.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("m.sqlite3");
        let (sent, got) = mpsc::channel();
        std::thread::spawn(move || {
            let conn = connect(&db).unwrap();
            let refused = write_at(&GATE, &conn, Duration::from_secs(1)).err();
            let _ = sent.send(refused.map(|err| err.to_string()));
        });
        let refused = got
            .recv_timeout(STUCK)
            .expect("the write's wait has no bound");
        assert_eq!(refused.as_deref(), Some(locked));
        // Woken by the freed turn, long before its own bound; the pause lets
        // it start waiting first.
        let next = queued(2 * STUCK);
        std::thread::sleep(Duration::from_millis(200));
        drop(held);
        assert_eq!(next.recv_timeout(STUCK), Ok(Ok(true)));
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
            "INSERT INTO memories (id,memory_type,category,title,content) VALUES ('e0000001-0000-4000-8000-000000000001','fact','tooling','A fact','A fact')",
            [],
        )
        .unwrap();
        FAIL_CHECKPOINT.set(true);
        let committed = tx.commit();
        FAIL_CHECKPOINT.set(false);
        assert!(committed.is_ok(), "{committed:?}");
        assert_eq!(super::super::backups::list(&db).unwrap().len(), 1);
    }

    // Inside a tool call a connection waits for SQLite's lock no longer than
    // what is left of the call's budget: set when it opens, after the open
    // tries the log mode, and after a removal clears the log.
    #[test]
    fn a_calls_connection_waits_for_sqlite_no_longer_than_its_budget() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("m.sqlite3");
        let left = Duration::from_secs(2);
        let busy = |conn: &Connection| -> u128 {
            let ms: i64 = conn
                .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
                .unwrap();
            u128::try_from(ms).unwrap()
        };
        super::super::budget::until(Some(std::time::Instant::now() + left), || {
            let conn = connect(&db).unwrap();
            assert!(busy(&conn) <= left.as_millis(), "{}", busy(&conn));
            conn.busy_timeout(BUSY).unwrap();
            clear_log(&conn, &db.to_string_lossy());
            assert!(busy(&conn) <= left.as_millis(), "{}", busy(&conn));
        });
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
        let turn = FileTurn::take(&path, STUCK).unwrap();
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
            let held = write_at(&GATE, &outer, STUCK).unwrap();
            let refused = write_at(&GATE, &inner, STUCK).is_err();
            held.commit().unwrap();
            done.send(refused).unwrap();
        });
        match finished.recv_timeout(STUCK) {
            Ok(refused) => assert!(refused, "the nested write got a second write lock"),
            Err(_) => panic!("nested write deadlocked on the process write lock"),
        }
    }

    // While another connection holds the write lock past the wait, compact
    // fails with SQLite's busy error and leaves the store as it was; setup
    // and migrate print compact_skipped and go on.
    #[test]
    fn compact_gives_up_on_a_store_another_writer_holds() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("m.sqlite3");
        connect(&db)
            .unwrap()
            .execute(
                "INSERT INTO memories (id,memory_type,category,title,content) VALUES ('e0000001-0000-4000-8000-000000000001','fact','tooling','A fact','A fact')",
                [],
            )
            .unwrap();
        let writer = Connection::open(&db).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let before = fs::read(&db).unwrap();
        let err = compact_waiting(&db, Duration::from_millis(100)).unwrap_err();
        assert_eq!(
            err.downcast_ref::<rusqlite::Error>()
                .and_then(rusqlite::Error::sqlite_error_code),
            Some(rusqlite::ErrorCode::DatabaseBusy),
            "{err:#}"
        );
        assert_eq!(fs::read(&db).unwrap(), before);
        // The reason in parentheses is SQLite's own text for the busy store,
        // which its releases word as they like; the rest is ours.
        assert_eq!(
            compact_skipped(&db, &err),
            format!(
                "user-memory: rewriting {} without its free space was skipped ({err:#}); text forgotten before this version may stay in it until the next setup or migrate",
                db.display()
            )
        );
        assert_eq!(
            err.to_string(),
            err.downcast_ref::<rusqlite::Error>().unwrap().to_string()
        );
        writer.execute_batch("ROLLBACK").unwrap();
        assert!(compact_waiting(&db, Duration::from_millis(100)).unwrap());
    }
}
