//! Forward migrations between schema versions. Only setup and
//! `user-memory migrate` run them; servers, the CLI and session hooks refuse
//! a store from an earlier version instead.

use super::{
    repair::problems,
    schema::{self, Found, SCHEMA_VERSION},
    titles::{self, Titles},
};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, backup::Backup};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

/// Pages copied per step of a backup. Our write lock keeps the store from
/// changing meanwhile, so the step size only trades memory for calls; stores
/// are a few hundred pages.
const COPY_PAGES: std::os::raw::c_int = 256;

/// The earliest schema version that can be migrated; earlier stores are refused.
pub(super) const BASELINE: i64 = 2;

/// Upgrades the store by one version inside the migration transaction.
struct Step {
    /// Refuses, before the backup is taken, a store the step cannot migrate
    /// with the titles given, so a refusal leaves no trace.
    check: fn(&Connection, Option<&Titles>) -> Result<()>,
    apply: fn(&Connection, Option<&Titles>) -> Result<()>,
}

/// `STEPS[i]` turns schema version `BASELINE + i` into `BASELINE + i + 1`.
///
/// Bumping `SCHEMA_VERSION` (the build fails until a step is added here):
/// - Add the step, then `tests/fixtures/schema/vN.sql` (the snapshot test
///   writes it under `target/tmp` for you) and `vN.seed.sql` with rows of
///   every kind valid for version N. Never edit an earlier version's files.
/// - Update the tests that pin version-specific wording or columns on
///   purpose: `a_new_store_is_stamped_with_the_schema_version`,
///   `a_store_from_before_versioning_is_refused_and_left_untouched`,
///   `stores_from_another_version_or_program_are_refused` and
///   `database_contains_only_the_generic_memory_schema` in `tests/memory.rs`.
///
/// Writing a step:
/// - It must leave exactly the schema a fresh store gets. `ALTER TABLE …
///   ADD COLUMN` places the column after the last column, before table
///   constraints, so put it at that place in `SCHEMA` too.
/// - Other table changes rebuild the table: create the new table, copy the
///   rows, drop the old table, rename the new one, then recreate its indexes
///   and triggers. Before renaming, drop the triggers in other tables that
///   name it (SQLite re-parses them on rename) and recreate them afterwards.
/// - `run` turns foreign keys off before `BEGIN IMMEDIATE` and checks them
///   with `pragma_foreign_key_check` before commit. Keep it that way: with
///   them on, dropping `memories` cascade-deletes every conversation and
///   distinct pair.
/// - Triggers stay on, so a step that updates memories also changes
///   `updated_at` and stales dependent patterns; the migration test
///   compares those columns.
const STEPS: &[Step] = &[
    Step {
        check: v3_titles,
        apply: to_v3,
    },
    Step {
        check: always,
        apply: to_v4,
    },
    Step {
        check: always,
        apply: to_v5,
    },
];

/// Version 5 adds the table of computed embedding cutoffs, empty until a
/// model without built-in cutoffs is calibrated; no memory changes. The SQL
/// is written out as it was at version 5.
fn to_v5(conn: &Connection, _: Option<&Titles>) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE embedding_cutoffs (
    model TEXT NOT NULL CHECK(length(model) BETWEEN 1 AND 200),
    length INTEGER NOT NULL CHECK(length > 0),
    duplicate REAL,
    related REAL,
    calibrated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY(model, length),
    CHECK((duplicate IS NULL) = (related IS NULL)),
    CHECK(duplicate IS NULL OR (related < duplicate AND duplicate <= 1.0 AND related >= -1.0))
);",
    )?;
    Ok(())
}

/// A step that needs nothing from the user.
fn always(_: &Connection, _: Option<&Titles>) -> Result<()> {
    Ok(())
}

/// Version 4 adds the table of conversations' linked projects; no memory
/// changes.
/// Version 4 adds conversation links, and stales a pattern whose support
/// moves only when the support ends up in a project the pattern is not in:
/// a whole project moving together keeps its patterns.
fn to_v4(conn: &Connection, _: Option<&Titles>) -> Result<()> {
    conn.execute_batch(schema::LINKS)?;
    conn.execute_batch(V4_TRIGGERS)?;
    Ok(())
}

/// The support triggers as they stand at schema version 4.
const V4_TRIGGERS: &str = "DROP TRIGGER memory_support_changed;
CREATE TRIGGER memory_support_changed AFTER UPDATE OF memory_type,category,context,content,source,status,evidence ON memories
WHEN (OLD.memory_type,OLD.category,OLD.context,OLD.content)
    IS NOT (NEW.memory_type,NEW.category,NEW.context,NEW.content)
    OR (OLD.source IS NOT NEW.source AND NOT (OLD.source='observed' AND NEW.source='explicit'))
    OR (OLD.status IS NOT NEW.status AND NOT (OLD.status='tentative' AND NEW.status='confirmed'))
    OR (OLD.evidence IS NOT NEW.evidence AND NOT (OLD.source='observed' AND NEW.source='explicit' AND NEW.evidence='')) BEGIN
    UPDATE memories SET status='stale',updated_at=CURRENT_TIMESTAMP
    WHERE status!='superseded' AND EXISTS (SELECT 1 FROM json_each(evidence_ids) WHERE value=OLD.id);
END;
CREATE TRIGGER memory_support_moved AFTER UPDATE OF scope,project ON memories
WHEN (OLD.scope,OLD.project) IS NOT (NEW.scope,NEW.project) AND NOT (NEW.scope='global' AND NEW.project='') BEGIN
    UPDATE memories SET status='stale',updated_at=CURRENT_TIMESTAMP
    WHERE status!='superseded' AND EXISTS (SELECT 1 FROM json_each(evidence_ids) WHERE value=OLD.id)
    AND (scope!='project' OR project IS NOT NEW.project);
END;";

/// Version 3 gives each memory a title: the map's, for every memory, or
/// a refusal listing what is missing.
fn titled(conn: &Connection, titles: Option<&Titles>) -> Result<BTreeMap<String, String>> {
    let memories = conn
        .prepare("SELECT id, content FROM memories ORDER BY rowid")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<(String, String)>>>()?;
    titles::assign(
        &memories,
        titles,
        "Migrating to schema version 3",
        "`user-memory migrate --titles FILE` (`-` for stdin)",
    )
}

fn v3_titles(conn: &Connection, titles: Option<&Titles>) -> Result<()> {
    titled(conn, titles).map(drop)
}

/// Version 3 adds `memories.title`, a single line of at most 120 characters
/// listed in the index, taken from the titles map. A `CHECK` on a new column
/// needs the table rebuilt, so this step creates the version 3 table, copies
/// every row with its rowid (recall breaks ties by insertion order) and its
/// title, and swaps it in. The search index gains a title column and is
/// rebuilt from the table; stored vectors embed the same text as before and
/// stay. The SQL is written out as it was at version 3 so later schema changes
/// never alter what this step does.
fn to_v3(conn: &Connection, titles: Option<&Titles>) -> Result<()> {
    let titles = titled(conn, titles)?;
    conn.execute_batch(
        "DROP TRIGGER memory_origin_removed;
         CREATE TEMP TABLE step_titles (id TEXT PRIMARY KEY, title TEXT NOT NULL);",
    )?;
    let mut insert = conn.prepare("INSERT INTO temp.step_titles (id, title) VALUES (?1, ?2)")?;
    for (id, title) in &titles {
        insert.execute([id, title])?;
    }
    conn.execute_batch(V3_REBUILD)?;
    Ok(())
}

/// Everything after the titles are in `temp.step_titles`, as it stood at
/// schema version 3.
const V3_REBUILD: &str = "CREATE TABLE memories_v3 (
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
INSERT INTO memories_v3 (rowid,id,created_at,updated_at,memory_type,category,context,title,content,reason,scope,project,source,status,evidence,evidence_ids,superseded_by,last_confirmed_at)
    SELECT m.rowid,m.id,m.created_at,m.updated_at,m.memory_type,m.category,m.context,d.title,m.content,m.reason,m.scope,m.project,m.source,m.status,m.evidence,m.evidence_ids,m.superseded_by,m.last_confirmed_at
    FROM memories m JOIN temp.step_titles d ON d.id=m.id ORDER BY m.rowid;
DROP TABLE temp.step_titles;
DROP TABLE memories;
ALTER TABLE memories_v3 RENAME TO memories;
CREATE TRIGGER memory_support_deleted AFTER DELETE ON memories BEGIN
    UPDATE memories SET status='stale',updated_at=CURRENT_TIMESTAMP
    WHERE status!='superseded' AND EXISTS (SELECT 1 FROM json_each(evidence_ids) WHERE value=OLD.id);
END;
CREATE TRIGGER memory_support_changed AFTER UPDATE OF memory_type,category,context,content,scope,project,source,status,evidence ON memories
WHEN (OLD.memory_type,OLD.category,OLD.context,OLD.content,OLD.scope,OLD.project)
    IS NOT (NEW.memory_type,NEW.category,NEW.context,NEW.content,NEW.scope,NEW.project)
    OR (OLD.source IS NOT NEW.source AND NOT (OLD.source='observed' AND NEW.source='explicit'))
    OR (OLD.status IS NOT NEW.status AND NOT (OLD.status='tentative' AND NEW.status='confirmed'))
    OR (OLD.evidence IS NOT NEW.evidence AND NOT (OLD.source='observed' AND NEW.source='explicit' AND NEW.evidence='')) BEGIN
    UPDATE memories SET status='stale',updated_at=CURRENT_TIMESTAMP
    WHERE status!='superseded' AND EXISTS (SELECT 1 FROM json_each(evidence_ids) WHERE value=OLD.id);
END;
CREATE TRIGGER memory_origin_removed AFTER DELETE ON memory_origins BEGIN
    UPDATE memories SET status='stale',updated_at=CURRENT_TIMESTAMP
    WHERE status!='superseded' AND EXISTS (SELECT 1 FROM json_each(evidence_ids) WHERE value=OLD.memory_id)
    AND (SELECT COUNT(DISTINCT o.origin) FROM json_each(memories.evidence_ids) e JOIN memory_origins o ON o.memory_id=e.value)
        + (SELECT COUNT(*) FROM json_each(memories.evidence_ids) e WHERE NOT EXISTS (SELECT 1 FROM memory_origins o WHERE o.memory_id=e.value)) < 2;
END;
DROP TABLE memory_search;
CREATE VIRTUAL TABLE memory_search USING fts5(id UNINDEXED,memory_type,category,title,content,context,reason,evidence,tokenize='unicode61 remove_diacritics 0');
INSERT INTO memory_search(id,memory_type,category,title,content,context,reason,evidence) SELECT id,memory_type,category,title,content,context,reason,evidence FROM memories;
CREATE TRIGGER memory_search_inserted AFTER INSERT ON memories BEGIN
    INSERT INTO memory_search(id,memory_type,category,title,content,context,reason,evidence) VALUES (NEW.id,NEW.memory_type,NEW.category,NEW.title,NEW.content,NEW.context,NEW.reason,NEW.evidence);
END;
CREATE TRIGGER memory_search_changed AFTER UPDATE OF memory_type,category,title,content,context,reason,evidence ON memories BEGIN
    UPDATE memory_search SET memory_type=NEW.memory_type,category=NEW.category,title=NEW.title,content=NEW.content,context=NEW.context,reason=NEW.reason,evidence=NEW.evidence WHERE id=OLD.id;
END;
CREATE TRIGGER memory_search_deleted AFTER DELETE ON memories BEGIN
    DELETE FROM memory_search WHERE id=OLD.id;
    DELETE FROM memory_vectors WHERE id=OLD.id;
END;";

const _: () = assert!(
    BASELINE + STEPS.len() as i64 == SCHEMA_VERSION,
    "every schema version after BASELINE needs a migration step"
);

/// What `migrate` found or did.
#[derive(Debug, PartialEq, Eq)]
pub enum Migration {
    /// The database has no tables yet; connecting creates the schema.
    Empty,
    /// Already at this version's schema.
    Current(i64),
    Migrated {
        from: i64,
        to: i64,
        backup: PathBuf,
    },
    /// A check found the store ready to migrate, and changed nothing.
    Ready {
        from: i64,
        to: i64,
    },
}

/// Whether a run migrates the store or only checks that it could.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Apply,
    Check,
}

impl Migration {
    /// What happened to the store at `db`, for the user.
    pub fn describe(&self, db: &Path) -> String {
        let db = db.display();
        match self {
            Self::Empty => format!("{db} holds no memory store yet; setup creates it."),
            Self::Current(version) => format!("{db} is up to date (schema version {version})."),
            Self::Migrated { from, to, backup } => format!(
                "Backed up {db} to {}\nMigrated {db} from schema version {from} to {to}.",
                backup.display()
            ),
            Self::Ready { from, to } => format!(
                "{db} can be migrated from schema version {from} to {to}; the check changed nothing."
            ),
        }
    }

    /// Why a titles map the user gave was not used, so it is never ignored
    /// silently; `None` when a migration step read it.
    pub fn unused_titles(&self) -> Option<String> {
        match self {
            Self::Empty => Some("No titles map was needed: there is no store to migrate.".into()),
            Self::Current(version) => Some(format!(
                "No titles map was needed: the store is already at schema version {version}."
            )),
            Self::Migrated { from, .. } | Self::Ready { from, .. } if *from >= TITLED_VERSION => {
                Some(format!(
                    "No titles map was needed: the store was already at schema version {from}."
                ))
            }
            Self::Migrated { .. } | Self::Ready { .. } => None,
        }
    }
}

/// The first schema version whose memories carry titles.
const TITLED_VERSION: i64 = 3;

/// Upgrades the store at `db` to this version's schema, after writing a
/// backup beside it. The step to version 3 takes each memory's title from
/// `titles`.
pub fn migrate(db: &Path, titles: Option<&Titles>) -> Result<Migration> {
    run(db, BASELINE, STEPS, titles, Mode::Apply)
}

/// Checks that the store at `db` could be migrated with `titles`, refusing
/// as `migrate` would, without changing or backing up anything: an upgrade
/// can learn what it needs before it stops a running server.
pub fn check_migration(db: &Path, titles: Option<&Titles>) -> Result<Migration> {
    run(db, BASELINE, STEPS, titles, Mode::Check)
}

fn run(
    db: &Path,
    baseline: i64,
    steps: &[Step],
    titles: Option<&Titles>,
    mode: Mode,
) -> Result<Migration> {
    let current = baseline + steps.len() as i64;
    if !db.is_file() {
        bail!(
            "No memory store at {}; run setup to create one",
            db.display()
        );
    }
    let flags = match mode {
        Mode::Apply => OpenFlags::SQLITE_OPEN_READ_WRITE,
        Mode::Check => OpenFlags::SQLITE_OPEN_READ_ONLY,
    };
    let mut conn = Connection::open_with_flags(db, flags)
        .with_context(|| format!("Cannot open {}", db.display()))?;
    conn.busy_timeout(super::schema::BUSY)?;
    // Off for the whole run, as SQLite prescribes for schema changes: with
    // them on, rebuilding a table would cascade-delete every conversation
    // and distinct pair that refers to it. The check before commit restores
    // the guarantee.
    conn.pragma_update(None, "foreign_keys", false)?;
    // Decide and migrate under one write lock: no other writer can slip in
    // between the backup and the migration, or between steps. A check only
    // reads, so a running server keeps writing.
    let behavior = match mode {
        Mode::Apply => TransactionBehavior::Immediate,
        Mode::Check => TransactionBehavior::Deferred,
    };
    let tx = conn.transaction_with_behavior(behavior)?;
    let from = schema::version(&tx)?;
    let kind = schema::inspect(&tx, from, baseline, current)?;
    match kind {
        Found::Empty => return Ok(Migration::Empty),
        Found::Current => return Ok(Migration::Current(from)),
        Found::Older => {}
        Found::Newer | Found::Foreign => {
            bail!(schema::refusal(&tx, db, from, &kind, current)?.unwrap_or_default())
        }
    }
    // A damaged store is never migrated, nor backed up for it: the steps
    // copy its rows, and a damaged index can hide some of them.
    let found = problems(&tx)?;
    if !found.is_empty() {
        bail!(
            "{} is damaged, so it was not migrated and nothing changed; run `user-memory repair` first, then migrate again:\n  {}",
            db.display(),
            found.join("\n  ")
        );
    }
    for version in from..current {
        (steps[(version - baseline) as usize].check)(&tx, titles)?;
    }
    if mode == Mode::Check {
        return Ok(Migration::Ready { from, to: current });
    }
    let stamp: String =
        tx.query_row("SELECT strftime('%Y%m%dT%H%M%SZ','now')", [], |r| r.get(0))?;
    let backup = backup_path(db, &format!("v{from}"), &stamp)?;
    write_backup(db, &backup, "migrated", memory_rows(&tx)?)?;
    let migrated = (|| -> Result<()> {
        for version in from..current {
            (steps[(version - baseline) as usize].apply)(&tx, titles).with_context(|| {
                format!("Migrating schema version {version} to {}", version + 1)
            })?;
            tx.pragma_update(None, "user_version", version + 1)?;
        }
        let violations: i64 =
            tx.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })?;
        if violations > 0 {
            bail!("Migration left {violations} rows referring to missing memories");
        }
        Ok(())
    })();
    match migrated {
        Ok(()) => tx.commit()?,
        Err(err) => {
            drop(tx);
            return Err(err.context(format!(
                "Migration rolled back: {} is unchanged; a backup is at {}",
                db.display(),
                backup.display()
            )));
        }
    }
    Ok(Migration::Migrated {
        from,
        to: current,
        backup,
    })
}

/// `<store>.<LABEL>-<UTC time>.bak` beside the store: `v<FROM>` for a
/// migration, `repair` for a repair, `move` for a project move. Never
/// rotated, unlike the automatic backups in `backups/`.
pub(crate) fn backup_path(db: &Path, label: &str, stamp: &str) -> Result<PathBuf> {
    let name = db.file_name().context("Invalid memory store path")?;
    Ok(db.with_file_name(format!("{}.{label}-{stamp}.bak", name.to_string_lossy())))
}

/// The memories in the table itself, counted without any index: a damaged
/// index can list more or fewer.
pub(crate) fn memory_rows(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM memories NOT INDEXED", [], |r| {
        r.get(0)
    })
}

/// Copies the store page by page into a new file at `backup`, mode 0600,
/// never replacing an existing file, and accepts the copy only if it holds
/// all `memories`. A second connection takes the copy, so it sees exactly
/// what the caller's open write transaction is about to change. `task`
/// names that change, as in "nothing was migrated".
pub(crate) fn write_backup(db: &Path, backup: &Path, task: &str, memories: i64) -> Result<()> {
    create_private(backup).with_context(|| {
        format!(
            "Cannot create the backup {}; nothing was {task}",
            backup.display()
        )
    })?;
    let copied = copy_into(db, backup).and_then(|copied| {
        if copied != memories {
            bail!("the backup holds {copied} of the store's {memories} memories");
        }
        Ok(())
    });
    if let Err(err) = copied {
        let _ = fs::remove_file(backup);
        return Err(err.context(format!(
            "Cannot back up {}; nothing was {task}",
            db.display()
        )));
    }
    Ok(())
}

/// Creates the empty file `path`, private, never replacing one.
pub(crate) fn create_private(path: &Path) -> std::io::Result<fs::File> {
    crate::private::create_file(fs::OpenOptions::new().write(true).create_new(true), path)
}

/// Copies the store page by page into the empty file `backup`, as one
/// file on disk, and returns the memories the copy holds. Page by page,
/// unlike `VACUUM INTO`, which copies rows and may read them through a
/// damaged index: the copy is the store exactly as it is, damage included,
/// and holds what its write-ahead log holds too.
pub(crate) fn copy_into(db: &Path, backup: &Path) -> Result<i64> {
    let source = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    source.busy_timeout(super::schema::BUSY)?;
    let mut copy = Connection::open(backup)?;
    Backup::new(&source, &mut copy)?.run_to_completion(COPY_PAGES, Duration::ZERO, None)?;
    // The copy takes the store's write-ahead log mode. The rollback
    // journal keeps the backup one whole file, with no -wal or -shm
    // beside it, so copying it back restores everything.
    copy.pragma_update_and_check(None, "journal_mode", "DELETE", |r| r.get::<_, String>(0))?;
    drop(copy);
    drop(source);
    // Opened for writing: Windows syncs only a handle that may write
    // (FlushFileBuffers needs write access) and refuses a read-only one.
    fs::OpenOptions::new()
        .write(true)
        .open(backup)?
        .sync_all()?;
    Ok(memory_rows(&Connection::open_with_flags(
        backup,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// A store holding one memory, stamped as version 2 of the fake chains
    /// below whatever the real schema version is.
    fn store(dir: &Path) -> PathBuf {
        let db = dir.join("memories.sqlite3");
        let conn = schema::connect(&db).unwrap();
        conn.execute(
            "INSERT INTO memories (id,memory_type,category,title,content) VALUES ('0bd1ba49-2b63-4aab-8d57-38d9be10a281','preference','programming','Prefer Rust','Prefer Rust')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
        db
    }

    fn ready(_: &Connection, _: Option<&Titles>) -> Result<()> {
        Ok(())
    }

    /// A step that is always ready to run.
    fn step(apply: fn(&Connection, Option<&Titles>) -> Result<()>) -> Step {
        Step {
            check: ready,
            apply,
        }
    }

    fn refusing(_: &Connection, _: Option<&Titles>) -> Result<()> {
        bail!("needs titles")
    }

    #[test]
    fn a_check_runs_the_step_checks_and_changes_nothing() {
        let tmp = tempdir().unwrap();
        let db = store(tmp.path());
        let before = fs::read(&db).unwrap();
        let ready = run(&db, 2, &[step(first), step(second)], None, Mode::Check).unwrap();
        assert_eq!(ready, Migration::Ready { from: 2, to: 4 });
        let steps = [
            step(first),
            Step {
                check: refusing,
                apply: second,
            },
        ];
        let err = run(&db, 2, &steps, None, Mode::Check).unwrap_err();
        assert_eq!(err.to_string(), "needs titles");
        assert_eq!(fs::read(&db).unwrap(), before);
        assert!(backups(tmp.path()).is_empty());
        assert_eq!(
            ready.describe(Path::new("m.sqlite3")),
            "m.sqlite3 can be migrated from schema version 2 to 4; the check changed nothing."
        );
        assert_eq!(ready.unused_titles(), None);
        assert!(
            Migration::Ready { from: 3, to: 4 }
                .unused_titles()
                .is_some()
        );
    }

    #[test]
    fn a_titles_map_is_reported_unless_the_step_to_version_3_read_it() {
        let migrated = |from| Migration::Migrated {
            from,
            to: 4,
            backup: PathBuf::from("m.bak"),
        };
        assert_eq!(migrated(2).unused_titles(), None);
        assert_eq!(
            migrated(3).unused_titles().as_deref(),
            Some("No titles map was needed: the store was already at schema version 3.")
        );
        assert_eq!(
            Migration::Current(3).unused_titles().as_deref(),
            Some("No titles map was needed: the store is already at schema version 3.")
        );
        assert_eq!(
            Migration::Empty.unused_titles().as_deref(),
            Some("No titles map was needed: there is no store to migrate.")
        );
    }

    #[test]
    fn a_step_that_refuses_its_inputs_leaves_no_backup_and_no_change() {
        let tmp = tempdir().unwrap();
        let db = store(tmp.path());
        let before = fs::read(&db).unwrap();
        let steps = [
            step(first),
            Step {
                check: refusing,
                apply: second,
            },
        ];
        let err = run(&db, 2, &steps, None, Mode::Apply)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "needs titles");
        assert_eq!(fs::read(&db).unwrap(), before);
        assert!(backups(tmp.path()).is_empty());
    }

    fn backups(dir: &Path) -> Vec<PathBuf> {
        let mut found: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "bak"))
            .collect();
        found.sort();
        found
    }

    /// Notes which step ran, the version it saw, and whether a backup
    /// already existed beside the store when it ran.
    fn log(conn: &Connection, step: i64) -> Result<()> {
        let path: String = conn.query_row(
            "SELECT file FROM pragma_database_list WHERE name='main'",
            [],
            |r| r.get(0),
        )?;
        let dir = Path::new(&path).parent().unwrap();
        let backed_up = !backups(dir).is_empty();
        conn.execute_batch("CREATE TABLE IF NOT EXISTS steps (step, seen, backed_up)")?;
        conn.execute(
            "INSERT INTO steps VALUES (?1, (SELECT user_version FROM pragma_user_version), ?2)",
            rusqlite::params![step, backed_up],
        )?;
        Ok(())
    }

    fn first(conn: &Connection, _: Option<&Titles>) -> Result<()> {
        log(conn, 1)?;
        conn.execute_batch("ALTER TABLE memories ADD COLUMN mood TEXT NOT NULL DEFAULT 'calm'")?;
        Ok(())
    }

    fn second(conn: &Connection, _: Option<&Titles>) -> Result<()> {
        log(conn, 2)
    }

    fn failing(conn: &Connection, _: Option<&Titles>) -> Result<()> {
        log(conn, 2)?;
        bail!("step two broke")
    }

    fn orphan(conn: &Connection, _: Option<&Titles>) -> Result<()> {
        conn.execute(
            "INSERT INTO memory_origins (memory_id, origin) VALUES ('6f1c3a52-7d0e-4c1b-9a2e-3b4c5d6e7f80','chat/x')",
            [],
        )?;
        Ok(())
    }

    /// Rebuilds the memories table the way SQLite documents for changes
    /// ALTER TABLE cannot make: triggers elsewhere that name the table are
    /// dropped first (a real step recreates them afterwards).
    fn rebuild(conn: &Connection, _: Option<&Titles>) -> Result<()> {
        conn.execute_batch(
            "DROP TRIGGER memory_origin_removed;
             CREATE TABLE memories_new (id TEXT PRIMARY KEY NOT NULL, created_at, updated_at, memory_type, category, context, content, reason, scope, project, source, status, evidence, evidence_ids, superseded_by, last_confirmed_at);
             INSERT INTO memories_new SELECT id,created_at,updated_at,memory_type,category,context,content,reason,scope,project,source,status,evidence,evidence_ids,superseded_by,last_confirmed_at FROM memories;
             DROP TABLE memories;
             ALTER TABLE memories_new RENAME TO memories;",
        )?;
        Ok(())
    }

    #[test]
    fn rebuilding_a_table_keeps_the_rows_that_refer_to_it() {
        let tmp = tempdir().unwrap();
        let db = store(tmp.path());
        Connection::open(&db)
            .unwrap()
            .execute(
                "INSERT INTO memory_origins (memory_id, origin) VALUES ('0bd1ba49-2b63-4aab-8d57-38d9be10a281','chat/x')",
                [],
            )
            .unwrap();
        run(&db, 2, &[step(rebuild)], None, Mode::Apply).unwrap();
        let origins: i64 = Connection::open(&db)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM memory_origins", [], |r| r.get(0))
            .unwrap();
        assert_eq!(origins, 1, "the rebuild cascaded into conversations");
    }

    #[test]
    fn steps_run_in_order_after_a_backup_and_stamp_each_version() {
        let tmp = tempdir().unwrap();
        let db = store(tmp.path());
        let out = run(&db, 2, &[step(first), step(second)], None, Mode::Apply).unwrap();
        let backup = backups(tmp.path()).pop().expect("a backup");
        assert_eq!(
            out,
            Migration::Migrated {
                from: 2,
                to: 4,
                backup: backup.clone()
            }
        );
        let name = backup.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("memories.sqlite3.v2-"), "{name}");
        assert_eq!(
            name.len(),
            "memories.sqlite3.v2-20261015T120000Z.bak".len(),
            "{name}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let conn = Connection::open(&db).unwrap();
        assert_eq!(schema::version(&conn).unwrap(), 4);
        let steps: Vec<(i64, i64, bool)> = conn
            .prepare("SELECT step, seen, backed_up FROM steps ORDER BY rowid")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(steps, [(1, 2, true), (2, 3, true)]);
        let mood: String = conn
            .query_row("SELECT mood FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mood, "calm");
        // The backup is the store as it was, still at version 2.
        let old = Connection::open(&backup).unwrap();
        assert_eq!(schema::version(&old).unwrap(), 2);
        let content: String = old
            .query_row("SELECT content FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(content, "Prefer Rust");
        assert_eq!(
            run(&db, 2, &[step(first), step(second)], None, Mode::Apply).unwrap(),
            Migration::Current(4)
        );
        assert_eq!(
            backups(tmp.path()).len(),
            1,
            "nothing to migrate, no backup"
        );
    }

    #[test]
    fn a_failing_step_rolls_back_the_whole_run_and_keeps_the_backup() {
        for steps in [[step(first), step(failing)], [step(first), step(orphan)]] {
            let tmp = tempdir().unwrap();
            let db = store(tmp.path());
            let before = fs::read(&db).unwrap();
            let err = format!("{:#}", run(&db, 2, &steps, None, Mode::Apply).unwrap_err());
            assert!(err.contains("Migration rolled back"), "{err}");
            assert!(err.contains("is unchanged; a backup is at"), "{err}");
            assert_eq!(fs::read(&db).unwrap(), before, "{err}");
            assert_eq!(backups(tmp.path()).len(), 1);
        }
    }

    #[test]
    fn newer_foreign_and_missing_stores_are_refused_without_a_backup() {
        let tmp = tempdir().unwrap();
        let db = store(tmp.path());
        let conn = Connection::open(&db).unwrap();
        conn.pragma_update(None, "user_version", 5).unwrap();
        let err = run(&db, 2, &[step(first), step(second)], None, Mode::Apply)
            .unwrap_err()
            .to_string();
        assert!(err.contains("older than the store"), "{err}");
        conn.pragma_update(None, "user_version", 1).unwrap();
        let err = run(&db, 2, &[step(first), step(second)], None, Mode::Apply)
            .unwrap_err()
            .to_string();
        assert!(err.contains("has schema version 1 but"), "{err}");
        assert!(backups(tmp.path()).is_empty());
        let missing = tmp.path().join("missing.sqlite3");
        let err = run(&missing, 2, &[step(first)], None, Mode::Apply)
            .unwrap_err()
            .to_string();
        assert!(err.contains("run setup to create one"), "{err}");
        assert!(!missing.exists());
        let empty = tmp.path().join("empty.sqlite3");
        fs::write(&empty, "").unwrap();
        assert_eq!(
            run(&empty, 2, &[step(first)], None, Mode::Apply).unwrap(),
            Migration::Empty
        );
        assert_eq!(fs::read(&empty).unwrap(), b"");
    }

    #[test]
    fn a_damaged_store_is_repaired_before_it_is_migrated() {
        let tmp = tempdir().unwrap();
        let db = store(tmp.path());
        // Part of the keyword index is lost; a backup copies it as it is.
        let conn = Connection::open(&db).unwrap();
        conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, false)
            .unwrap();
        conn.execute_batch("DELETE FROM memory_search_data WHERE id > 1")
            .unwrap();
        drop(conn);
        let before = fs::read(&db).unwrap();
        let steps = [step(first), step(second)];
        for mode in [Mode::Check, Mode::Apply] {
            let err = format!("{:#}", run(&db, 2, &steps, None, mode).unwrap_err());
            assert!(
                err.contains("is damaged, so it was not migrated and nothing changed")
                    && err.contains("run `user-memory repair` first")
                    && err.contains("memory_search"),
                "{err}"
            );
            assert_eq!(fs::read(&db).unwrap(), before);
            assert!(backups(tmp.path()).is_empty());
        }
        assert!(matches!(
            super::super::repair(&db).unwrap(),
            super::super::repair::Repair::Repaired { .. }
        ));
        assert!(matches!(
            run(&db, 2, &steps, None, Mode::Apply).unwrap(),
            Migration::Migrated { from: 2, to: 4, .. }
        ));
    }

    #[test]
    fn a_backup_never_replaces_an_existing_file() {
        let tmp = tempdir().unwrap();
        let db = store(tmp.path());
        Connection::open(&db)
            .unwrap()
            .execute(
                "INSERT INTO memories (id,memory_type,category,title,content) VALUES ('7c1f5d2e-3b4a-4c5d-9e6f-0a1b2c3d4e5f','preference','writing','Answer briefly','Answer briefly')",
                [],
            )
            .unwrap();
        let taken = tmp.path().join("taken.bak");
        fs::write(&taken, "keep me").unwrap();
        let err = format!(
            "{:#}",
            write_backup(&db, &taken, "migrated", 2).unwrap_err()
        );
        assert!(err.contains("nothing was migrated"), "{err}");
        assert_eq!(fs::read(&taken).unwrap(), b"keep me");
        let fresh = tmp.path().join("fresh.bak");
        write_backup(&db, &fresh, "migrated", 2).unwrap();
        assert_eq!(memory_rows(&Connection::open(&fresh).unwrap()).unwrap(), 2);
        // A copy short of the store's memories is removed.
        let refused = tmp.path().join("refused.bak");
        let err = format!(
            "{:#}",
            write_backup(&db, &refused, "repaired", 3).unwrap_err()
        );
        assert!(
            err.contains("the backup holds 2 of the store's 3 memories")
                && err.contains("nothing was repaired"),
            "{err}"
        );
        assert!(!refused.exists());
        assert_eq!(
            backup_path(Path::new("/d/m.sqlite3"), "v2", "20261015T120000Z").unwrap(),
            Path::new("/d/m.sqlite3.v2-20261015T120000Z.bak")
        );
    }

    #[test]
    fn versions_are_classified_against_the_baseline_and_this_binary() {
        use schema::{Found, classify};
        assert_eq!(classify(4, true, 2, 4), Found::Current);
        assert_eq!(classify(0, false, 2, 4), Found::Empty);
        assert_eq!(classify(0, true, 2, 4), Found::Foreign);
        assert_eq!(classify(1, true, 2, 4), Found::Foreign);
        assert_eq!(classify(2, true, 2, 4), Found::Older);
        assert_eq!(classify(3, true, 2, 4), Found::Older);
        assert_eq!(classify(5, true, 2, 4), Found::Newer);
        let tmp = tempdir().unwrap();
        let db = store(tmp.path());
        let conn = Connection::open(&db).unwrap();
        let message = |found| {
            let kind = schema::inspect(&conn, found, 2, 4).unwrap();
            schema::refusal(&conn, &db, found, &kind, 4).unwrap()
        };
        let older = message(3).unwrap();
        assert!(
            older.ends_with("has schema version 3; run the installer again (or ./setup, or `user-memory migrate`) to upgrade it to version 4. A backup is made first; the store was left unchanged."),
            "{older}"
        );
        let newer = message(5).unwrap();
        assert!(
            newer.contains("has schema version 5, but this user-memory is older than the store (it uses version 4); update user-memory and restart your client."),
            "{newer}"
        );
        assert_eq!(message(4), None);
    }
}
