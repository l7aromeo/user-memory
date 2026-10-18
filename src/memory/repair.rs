//! The store's integrity check, and the repair of a store whose indexes no
//! longer match its tables: the damage left when two processes wrote to it
//! at once. Rows are never changed; every index is rebuilt from them.

use super::{
    migrate::{BASELINE, backup_path, memory_rows, write_backup},
    schema::{self, Found, SCHEMA_VERSION},
};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use std::path::{Path, PathBuf};

/// Every problem SQLite's integrity check finds in the database behind
/// `conn`, or none. The full check, not `quick_check`: only it compares each
/// index with its table, and it checks the search index too.
pub fn problems(conn: &Connection) -> Result<Vec<String>> {
    let found = conn
        .prepare("PRAGMA integrity_check")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(if found == ["ok"] { Vec::new() } else { found })
}

/// What `repair` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Repair {
    /// The store passed the integrity check; nothing was changed.
    Clean,
    Repaired {
        /// The problems the integrity check found before the repair.
        found: Vec<String>,
        backup: PathBuf,
    },
}

impl Repair {
    pub fn describe(&self, db: &Path) -> String {
        let db = db.display();
        match self {
            Self::Clean => format!("{db} passed the integrity check; nothing to repair."),
            Self::Repaired { found, backup } => format!(
                "Backed up {db} to {}\nFound:\n  {}\nRebuilt every index of {db} from its memories; it now passes the integrity check.",
                backup.display(),
                found.join("\n  ")
            ),
        }
    }
}

/// Checks the store at `db` and, if it is damaged, backs it up beside
/// itself and rebuilds every index from the tables, then the whole file if
/// that is not enough. Fails, keeping the backup, if a memory went missing
/// or the store is still damaged. A store from an earlier schema version is
/// repaired too, so it can be migrated afterwards.
pub fn repair(db: &Path) -> Result<Repair> {
    if !db.is_file() {
        bail!(
            "No memory store at {}; run setup to create one",
            db.display()
        );
    }
    // Opened without `connect`, which refuses an earlier schema version.
    let mut conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .with_context(|| format!("Cannot open {}", db.display()))?;
    conn.busy_timeout(super::schema::BUSY)?;
    // Check, back up and repair under one write lock, so no other writer
    // changes the store in between.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version = schema::version(&tx)?;
    let kind = schema::inspect(&tx, version, BASELINE, SCHEMA_VERSION)?;
    if matches!(kind, Found::Newer | Found::Foreign) {
        bail!(schema::refusal(&tx, db, version, &kind, SCHEMA_VERSION)?.unwrap_or_default());
    }
    let found = problems(&tx)?;
    if found.is_empty() {
        return Ok(Repair::Clean);
    }
    let before = memory_rows(&tx)?;
    let stamp: String =
        tx.query_row("SELECT strftime('%Y%m%dT%H%M%SZ','now')", [], |r| r.get(0))?;
    let backup = backup_path(db, "repair", &stamp)?;
    write_backup(db, &backup, "repaired", before)?;
    let kept = || {
        format!(
            "the backup {} holds the store as it was before the repair",
            backup.display()
        )
    };
    // Every store user-memory creates or migrates has the keyword index.
    tx.execute_batch("REINDEX;\nINSERT INTO memory_search(memory_search) VALUES('rebuild');")
        .with_context(|| {
            format!(
                "Rebuilding the indexes of {} failed, so it is unchanged; {}",
                db.display(),
                kept()
            )
        })?;
    tx.commit()?;
    if !problems(&conn)?.is_empty() {
        // Damage beyond the indexes, such as a miscounted free-page list:
        // only rewriting the whole file mends it, and the indexes it copies
        // the rows through are sound by now.
        conn.execute_batch("VACUUM")
            .with_context(|| format!("Rewriting {} failed; {}", db.display(), kept()))?;
    }
    // Writers may have recorded memories since the commit released the
    // lock; fewer than before means the rebuild lost some.
    let after = memory_rows(&conn)?;
    if after < before {
        bail!(
            "Repairing {} kept {after} of its {before} memories; {}",
            db.display(),
            kept()
        );
    }
    let left = problems(&conn)?;
    if !left.is_empty() {
        bail!(
            "{} is still damaged after rebuilding its indexes and the whole file, so its tables themselves are damaged; {}. Restore a backup or an export you trust. Still found:\n  {}",
            db.display(),
            kept(),
            left.join("\n  ")
        );
    }
    Ok(Repair::Repaired { found, backup })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    /// A store at `dir` holding three memories.
    fn store(dir: &Path) -> PathBuf {
        let db = dir.join("m.sqlite3");
        for content in ["Prefer Rust", "Answer briefly", "Ask before pushing"] {
            let input = serde_json::from_value(json!({
                "type":"preference","category":"workflow","title":content,
                "content":content,"scope":"global","user_confirmed":true
            }))
            .unwrap();
            let out = crate::memory::record(&db, input).unwrap();
            assert_eq!(out["stored"], true, "{out}");
        }
        db
    }

    fn backups(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.to_string_lossy().contains(".repair-"))
            .collect()
    }

    #[test]
    fn a_healthy_store_is_reported_clean_and_left_alone() {
        let tmp = tempdir().unwrap();
        let db = store(tmp.path());
        let before = fs::read(&db).unwrap();
        let repaired = repair(&db).unwrap();
        assert_eq!(repaired, Repair::Clean);
        assert!(
            repaired
                .describe(&db)
                .ends_with("passed the integrity check; nothing to repair."),
        );
        assert_eq!(fs::read(&db).unwrap(), before);
        assert!(backups(tmp.path()).is_empty());
    }

    #[test]
    fn a_store_that_is_missing_or_not_user_memorys_is_refused() {
        let tmp = tempdir().unwrap();
        let missing = tmp.path().join("missing.sqlite3");
        let err = repair(&missing).unwrap_err().to_string();
        assert!(err.contains("No memory store"), "{err}");
        assert!(!missing.exists());
        let foreign = tmp.path().join("foreign.sqlite3");
        Connection::open(&foreign)
            .unwrap()
            .execute_batch("CREATE TABLE other(x)")
            .unwrap();
        let err = repair(&foreign).unwrap_err().to_string();
        assert!(err.contains("was left unchanged"), "{err}");
    }
}
