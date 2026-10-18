//! `user-memory project move`: re-keys one project's memories and the
//! conversation links to it, after a backup, as one change.

use super::checked_key;
use crate::memory::{
    connect,
    migrate::{backup_path, memory_rows, write_backup},
    problems,
};
use anyhow::{Context, Result, bail};
use rusqlite::{TransactionBehavior, params};
use std::path::{Path, PathBuf};

/// What a move did.
#[derive(Debug)]
pub struct Moved {
    pub old: String,
    pub new: String,
    pub memories: usize,
    pub links: usize,
    /// Project memories the new key held before the move.
    pub already: usize,
    pub backup: PathBuf,
}

impl Moved {
    pub fn describe(&self, db: &Path) -> String {
        let mut text = format!(
            "Backed up {} to {}\nMoved {} and {} from {} to {}.",
            db.display(),
            self.backup.display(),
            counted(self.memories, "memory", "memories"),
            counted(self.links, "conversation link", "conversation links"),
            self.old,
            self.new
        );
        if self.already > 0 {
            text.push_str(&format!(
                "\n{} already had {}; run `user-memory review --project {}` to merge any duplicates.",
                self.new,
                counted(self.already, "memory", "memories"),
                self.new
            ));
        }
        text
    }
}

fn counted(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Re-keys every project memory of `old`, and every conversation link to
/// it, to `new` in one transaction, after a backup of the store beside it,
/// then makes the day's automatic backup when one is due, as every write
/// does. Refuses keys that are local paths, the same project twice, a
/// damaged store, and an `old` nothing uses.
pub fn move_project(db: &Path, old: &str, new: &str) -> Result<Moved> {
    let (old, new) = (checked_key(old)?, checked_key(new)?);
    if old == new {
        bail!("{old} and {new} name the same project; nothing to move");
    }
    if !db.is_file() {
        bail!(
            "No memory store at {}; run setup to create one",
            db.display()
        );
    }
    let mut conn = connect(db)?;
    conn.busy_timeout(crate::memory::BUSY)?;
    // One write lock from the counts to the commit, so no writer slips in
    // between the backup and the move.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let found = problems(&tx)?;
    if !found.is_empty() {
        bail!(
            "{} is damaged, so nothing was moved; run `user-memory repair` first:\n  {}",
            db.display(),
            found.join("\n  ")
        );
    }
    let count = |sql: &str, key: &str| tx.query_row(sql, [key], |r| r.get::<_, usize>(0));
    let memories = count(PROJECT_MEMORIES, &old)?;
    let links = count(
        "SELECT COUNT(*) FROM conversation_projects WHERE project = ?1",
        &old,
    )?;
    if memories == 0 && links == 0 {
        bail!(
            "No memories or conversation links use the project {old}; `user-memory index --scope all` names the projects in use"
        );
    }
    let already = count(PROJECT_MEMORIES, &new)?;
    let stamp: String =
        tx.query_row("SELECT strftime('%Y%m%dT%H%M%SZ','now')", [], |r| r.get(0))?;
    let backup = backup_path(db, "move", &stamp)?;
    write_backup(db, &backup, "moved", memory_rows(&tx)?)?;
    let moved = (|| -> Result<(usize, usize)> {
        // Patterns first: when a supporting memory then arrives under the
        // new key, the patterns citing it are already there, so the store
        // keeps them as they were. A pattern left elsewhere still goes stale.
        let mut memories = 0;
        for which in ["source = 'inferred'", "source != 'inferred'"] {
            memories += tx.execute(
                &format!(
                    "UPDATE memories SET project = ?2, updated_at = CURRENT_TIMESTAMP WHERE scope = 'project' AND project = ?1 AND {which}"
                ),
                params![old, new],
            )?;
        }
        // A conversation linked to both keys keeps one link, to the new one.
        let links = tx.execute(
            "UPDATE OR IGNORE conversation_projects SET project = ?2 WHERE project = ?1",
            params![old, new],
        )?;
        let doubled = tx.execute(
            "DELETE FROM conversation_projects WHERE project = ?1",
            [&old],
        )?;
        Ok((memories, links + doubled))
    })();
    match moved {
        Ok((memories, links)) => {
            tx.commit()?;
            // This transaction is not the store's write turn, whose commit
            // makes the day's backup, so the move makes it here.
            crate::memory::backups::back_up_if_due(db);
            Ok(Moved {
                old,
                new,
                memories,
                links,
                already,
                backup,
            })
        }
        Err(err) => {
            drop(tx);
            Err(err).with_context(|| {
                format!(
                    "Move rolled back: {} is unchanged; a backup is at {}",
                    db.display(),
                    backup.display()
                )
            })
        }
    }
}

const PROJECT_MEMORIES: &str =
    "SELECT COUNT(*) FROM memories WHERE scope = 'project' AND project = ?1";
