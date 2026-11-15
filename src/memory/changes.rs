//! The store's count of its own changes, so a session hook learns what
//! changed since it last looked without reading every memory. Stored
//! triggers count each change in the commit that makes it, so no write path,
//! whichever process or program makes it, is left out.
//!
//! - `store_changes`, one row: the store's id, a random value that a store
//!   restored, edited by hand or migrated gets anew, so a session that saw
//!   another id reads everything again; the number of its latest change; and
//!   the number up to which removals were pruned.
//! - `memory_changes`, one row per memory changed since: its id and the
//!   number of its latest change, and for a forgotten memory the time it was
//!   removed. It holds no text.
//!
//! The counter is not a column of `memories`: a trigger could set one only
//! with a second update of the row, which the history would record as a
//! change nobody made and the keyword index would index again.
//!
//! Beside the store, `<store>.changed` repeats the store's id and latest
//! change, so a hook learns with one stat whether anything changed. Every
//! writer rewrites it after its commit while holding the write turn.

use crate::private;
use rusqlite::{Connection, OptionalExtension};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

/// Whether a change to a column of `memories` moves the store's counter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Counts {
    /// What an agent acts on: a change to it is news to a running session.
    Yes,
    /// Not news to a session, for the reason given.
    No(&'static str),
}

/// Every column of `memories`, in the table's order, and whether a change to
/// it moves the counter. The triggers are made from this list, and a test
/// fails when the table has a column it lacks, so a new column is decided
/// here.
pub(super) const MEMORY_COLUMNS: [(&str, Counts); 17] = [
    ("id", Counts::No("the row's identity, never updated")),
    (
        "created_at",
        Counts::No("set once, when the memory is recorded"),
    ),
    (
        "updated_at",
        Counts::No(
            "moves on a confirmation and on an update that changes nothing, which change nothing an agent acts on",
        ),
    ),
    ("memory_type", Counts::Yes),
    ("category", Counts::Yes),
    ("context", Counts::Yes),
    ("title", Counts::Yes),
    ("content", Counts::Yes),
    ("reason", Counts::Yes),
    ("scope", Counts::Yes),
    ("project", Counts::Yes),
    ("source", Counts::Yes),
    ("status", Counts::Yes),
    ("evidence", Counts::Yes),
    ("evidence_ids", Counts::Yes),
    ("superseded_by", Counts::Yes),
    (
        "last_confirmed_at",
        Counts::No("a confirmation changes nothing an agent acts on"),
    ),
];

/// The columns whose change moves the counter.
pub(super) fn counted() -> impl Iterator<Item = &'static str> {
    MEMORY_COLUMNS
        .iter()
        .filter(|(_, counts)| *counts == Counts::Yes)
        .map(|(name, _)| *name)
}

/// The tables, index and triggers of the counter, safe to run on every open:
/// each is created only where missing. The row of `store_changes` is not
/// here, since an insert takes a write lock even when it adds nothing; a
/// new or migrated store gets it from [`START`].
pub(super) fn schema() -> String {
    let columns: Vec<&str> = counted().collect();
    let old: Vec<String> = columns.iter().map(|c| format!("OLD.{c}")).collect();
    let new: Vec<String> = columns.iter().map(|c| format!("NEW.{c}")).collect();
    // One row per memory: a memory forgotten and then recorded again under
    // the same id, as an import can, is live again, so its row is replaced
    // and loses its removal time.
    let count = |id: &str, removed: &str| {
        format!(
            "    UPDATE store_changes SET last_change=last_change+1 WHERE id=1;
    INSERT OR REPLACE INTO memory_changes(memory_id,change,removed_at) SELECT {id},last_change,{removed} FROM store_changes WHERE id=1;"
        )
    };
    let links = "    UPDATE store_changes SET last_change=last_change+1 WHERE id=1;";
    format!(
        "CREATE TABLE IF NOT EXISTS store_changes (
    id INTEGER PRIMARY KEY CHECK(id=1),
    store_id TEXT NOT NULL CHECK(length(store_id)=32),
    last_change INTEGER NOT NULL DEFAULT 0 CHECK(last_change>=0),
    pruned_through INTEGER NOT NULL DEFAULT 0 CHECK(pruned_through>=0)
);
CREATE TABLE IF NOT EXISTS memory_changes (
    memory_id TEXT PRIMARY KEY NOT NULL,
    change INTEGER NOT NULL,
    removed_at TEXT
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS memory_changes_by_change ON memory_changes(change);
CREATE TRIGGER IF NOT EXISTS memory_change_recorded AFTER INSERT ON memories BEGIN
{recorded}
END;
CREATE TRIGGER IF NOT EXISTS memory_change_updated AFTER UPDATE OF {list} ON memories
WHEN ({old}) IS NOT ({new}) BEGIN
{updated}
END;
CREATE TRIGGER IF NOT EXISTS memory_change_forgotten AFTER DELETE ON memories BEGIN
{forgotten}
END;
CREATE TRIGGER IF NOT EXISTS link_change_added AFTER INSERT ON conversation_projects BEGIN
{links}
END;
CREATE TRIGGER IF NOT EXISTS link_change_removed AFTER DELETE ON conversation_projects BEGIN
{links}
END;
CREATE TRIGGER IF NOT EXISTS link_change_moved AFTER UPDATE OF origin,project ON conversation_projects
WHEN (OLD.origin,OLD.project) IS NOT (NEW.origin,NEW.project) BEGIN
{links}
END;",
        recorded = count("NEW.id", "NULL"),
        list = columns.join(","),
        old = old.join(","),
        new = new.join(","),
        updated = count("NEW.id", "NULL"),
        forgotten = count("OLD.id", "CURRENT_TIMESTAMP"),
    )
}

/// Gives a new or migrated store its row of `store_changes`: a new id and no
/// changes yet.
pub(super) const START: &str =
    "INSERT INTO store_changes(id,store_id) VALUES (1,lower(hex(randomblob(16))));";

/// Gives the store a new id, so every session reads it whole once.
const RENEW: &str = "UPDATE store_changes SET store_id=lower(hex(randomblob(16))) WHERE id=1";

/// What the store says of its changes, and what its marker repeats: its id
/// and the number of its latest change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub store_id: String,
    pub last_change: i64,
}

/// The store's [`State`], or none for a store without the counter: one from
/// before schema version 10, which a migration is upgrading.
pub(super) fn state(conn: &Connection) -> rusqlite::Result<Option<State>> {
    let read = conn
        .query_row(
            "SELECT store_id,last_change FROM store_changes WHERE id=1",
            [],
            |r| {
                Ok(State {
                    store_id: r.get(0)?,
                    last_change: r.get(1)?,
                })
            },
        )
        .optional();
    match read {
        Err(rusqlite::Error::SqliteFailure(_, Some(message)))
            if message.starts_with("no such table") =>
        {
            Ok(None)
        }
        read => read,
    }
}

/// The marker beside the store at `db`: the store with `.changed` appended.
/// A session hook reads it with one stat and one small read, where opening
/// the store would cost far more, and the store's own files move on writes
/// that change no memory.
pub fn marker_path(db: &Path) -> PathBuf {
    let mut path = db.as_os_str().to_owned();
    path.push(".changed");
    path.into()
}

/// The [`State`] the marker beside the store at `db` holds, or none when it
/// is missing or unreadable.
pub fn read_marker(db: &Path) -> Option<State> {
    let text = fs::read_to_string(marker_path(db)).ok()?;
    let (store_id, last_change) = text.trim_end().split_once(' ')?;
    Some(State {
        store_id: store_id.to_owned(),
        last_change: last_change.parse().ok()?,
    })
}

/// Replaces the marker beside the store at `db` with `state`, through a
/// temporary file and a rename, so a reader never sees half of it.
fn write_marker(db: &Path, state: &State) -> io::Result<()> {
    let marker = marker_path(db);
    let mut temporary = marker.as_os_str().to_owned();
    temporary.push(format!(".{}", std::process::id()));
    let temporary = PathBuf::from(temporary);
    let written = (|| {
        let mut file = private::create_file(
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true),
            &temporary,
        )?;
        file.write_all(format!("{} {}\n", state.store_id, state.last_change).as_bytes())?;
        fs::rename(&temporary, &marker)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}

/// At the start of a write to the store at `db`, inside its transaction and
/// holding the store's write turn. A marker that does not match the store
/// (missing, unreadable, behind it, ahead of it or of another id) means the
/// store changed without it: a crash between a commit and its marker, a
/// restored backup, an edit by hand, or another program. The store then
/// takes a new id, so every session reads it whole once rather than trust a
/// count that may not hold.
pub(super) fn begin(conn: &Connection, db: &Path) -> rusqlite::Result<()> {
    let Some(state) = state(conn)? else {
        return Ok(());
    };
    if read_marker(db).as_ref() != Some(&state) {
        conn.execute(RENEW, [])?;
    }
    Ok(())
}

/// After a write to the store at `db` committed, still holding the write
/// turn, so no other writer's marker can come between: rewrites the marker
/// when the store's state differs from it. The write already succeeded, so
/// a marker that cannot be written is only reported; the next write finds
/// it behind and renews the store's id.
pub(super) fn publish(conn: &Connection, db: &Path) {
    let Ok(Some(state)) = state(conn) else {
        return;
    };
    if read_marker(db).as_ref() != Some(&state)
        && let Err(err) = write_marker(db, &state)
    {
        eprintln!(
            "user-memory: writing {} failed: {err}",
            marker_path(db).display()
        );
    }
}

/// How long a forgotten memory's row stays, in SQLite's date terms: as long
/// as a session's snapshot unused (`SNAPSHOT_AGE` in `session/live.rs`), so a
/// session still running learns the memory is gone, and one idle longer has
/// lost its snapshot and reads everything anyway.
const REMOVED_KEPT_FOR: &str = "-30 days";

/// Drops the rows of memories forgotten over [`REMOVED_KEPT_FOR`] ago, and
/// records the latest change dropped, below which a session must read the
/// store whole. Returns how many went.
pub(super) fn prune(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE store_changes SET pruned_through=max(pruned_through,coalesce((SELECT max(change) FROM memory_changes WHERE removed_at < datetime('now',?1)),0)) WHERE id=1",
        [REMOVED_KEPT_FOR],
    )?;
    conn.execute(
        "DELETE FROM memory_changes WHERE removed_at < datetime('now',?1)",
        [REMOVED_KEPT_FOR],
    )
}

/// Gives the store a new id inside a migration, so every session reads it
/// whole once after an upgrade.
pub(super) fn renew(conn: &Connection) -> rusqlite::Result<()> {
    if state(conn)?.is_some() {
        conn.execute(RENEW, [])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// A store with its schema, as a fresh one is made, and one memory.
    fn store() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = super::super::schema::connect(&dir.path().join("m.sqlite3")).unwrap();
        conn.execute(
            "INSERT INTO memories (id,memory_type,category,title,content) VALUES ('0bd1ba49-2b63-4aab-9c3a-111111111111','fact','tooling','A title','Some content')",
            [],
        )
        .unwrap();
        (dir, conn)
    }

    const ID: &str = "0bd1ba49-2b63-4aab-9c3a-111111111111";

    /// The store's last change and the memory's row: its change and
    /// removal time.
    fn counted_state(conn: &Connection) -> (i64, Option<(i64, Option<String>)>) {
        let last = conn
            .query_row("SELECT last_change FROM store_changes", [], |r| r.get(0))
            .unwrap();
        let row = conn
            .query_row(
                "SELECT change,removed_at FROM memory_changes WHERE memory_id=?1",
                [ID],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        (last, row)
    }

    #[test]
    fn every_column_of_memories_is_decided() {
        let (_dir, conn) = store();
        let mut columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('memories') ORDER BY cid")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let listed: Vec<String> = MEMORY_COLUMNS
            .iter()
            .map(|(c, _)| (*c).to_owned())
            .collect();
        assert_eq!(listed, columns, "the list names every column, in order");
        // What counts is what the history records as a change of the
        // memory's values: the columns its trigger compares last.
        let history: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='memory_event_changed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let compared = history
            .rsplit("WHERE (")
            .next()
            .and_then(|rest| rest.split(')').next())
            .unwrap();
        let mut valued: Vec<&str> = compared
            .split(',')
            .filter_map(|part| part.trim().strip_prefix("OLD."))
            .collect();
        valued.sort_unstable();
        let mut counted: Vec<&str> = counted().collect();
        counted.sort_unstable();
        assert_eq!(counted, valued);
        columns.sort();
        columns.dedup();
        assert_eq!(columns.len(), MEMORY_COLUMNS.len());
    }

    #[test]
    fn a_change_an_agent_acts_on_moves_the_counter_and_nothing_else_does() {
        let (_dir, conn) = store();
        let (recorded, row) = counted_state(&conn);
        assert_eq!(row, Some((recorded, None)), "a record counts");
        let mut last = recorded;
        for (column, counts) in MEMORY_COLUMNS {
            if column == "id" {
                continue;
            }
            // A value of the column's kind that differs from the current
            // one, valid with the columns set before it.
            let set = match column {
                "status" => "status='tentative'".to_owned(),
                "scope" => "scope='project'".to_owned(),
                "source" => "source='inferred'".to_owned(),
                "evidence_ids" => {
                    "evidence_ids='[\"11111111-1111-4111-8111-111111111111\"]'".to_owned()
                }
                // Only a superseded memory names what replaced it.
                "superseded_by" => {
                    "status='superseded',superseded_by='11111111-1111-4111-8111-111111111111'"
                        .to_owned()
                }
                "created_at" | "updated_at" | "last_confirmed_at" => {
                    format!("{column}='2030-01-01 00:00:00'")
                }
                _ => format!("{column}='changed'"),
            };
            conn.execute(&format!("UPDATE memories SET {set} WHERE id=?1"), [ID])
                .unwrap();
            let (now, row) = counted_state(&conn);
            match counts {
                Counts::Yes => {
                    assert_eq!(now, last + 1, "{column} counts");
                    assert_eq!(row, Some((now, None)), "{column}");
                }
                Counts::No(_) => assert_eq!(now, last, "{column} does not count"),
            }
            last = now;
            // The same value again changes nothing.
            conn.execute(&format!("UPDATE memories SET {set} WHERE id=?1"), [ID])
                .unwrap();
            assert_eq!(counted_state(&conn).0, last, "{column} set to itself");
        }
    }

    #[test]
    fn a_forgotten_memory_keeps_its_id_and_time_and_comes_back_when_recorded_again() {
        let (_dir, conn) = store();
        conn.execute("DELETE FROM memories WHERE id=?1", [ID])
            .unwrap();
        let (forgotten, row) = counted_state(&conn);
        let (change, removed) = row.unwrap();
        assert_eq!(change, forgotten);
        assert!(removed.is_some(), "the time it was removed");
        conn.execute(
            "INSERT INTO memories (id,memory_type,category,title,content) VALUES (?1,'fact','tooling','A title','Some content')",
            [ID],
        )
        .unwrap();
        let (again, row) = counted_state(&conn);
        assert_eq!(row, Some((again, None)), "live again, one row");
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_changes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn a_forgotten_memory_goes_from_the_count_after_its_days_and_marks_how_far() {
        let (_dir, conn) = store();
        let pruned = || -> i64 {
            conn.query_row("SELECT pruned_through FROM store_changes", [], |r| r.get(0))
                .unwrap()
        };
        conn.execute("DELETE FROM memories WHERE id=?1", [ID])
            .unwrap();
        let (forgotten, _) = counted_state(&conn);
        assert_eq!(prune(&conn).unwrap(), 0, "a fresh removal stays");
        assert_eq!(pruned(), 0);
        // Just inside the days, then just past them.
        for (age, kept) in [("-29 days", true), ("-31 days", false)] {
            conn.execute(
                "UPDATE memory_changes SET removed_at=datetime('now',?1)",
                [age],
            )
            .unwrap();
            assert_eq!(prune(&conn).unwrap(), usize::from(!kept), "{age}");
            assert_eq!(counted_state(&conn).1.is_some(), kept, "{age}");
            assert_eq!(pruned(), if kept { 0 } else { forgotten }, "{age}");
        }
        // A later prune with nothing to drop keeps the mark.
        assert_eq!(prune(&conn).unwrap(), 0);
        assert_eq!(pruned(), forgotten);
    }

    #[test]
    fn a_project_link_moves_the_counter_without_a_memory() {
        let (_dir, conn) = store();
        let before = counted_state(&conn);
        let last = || -> i64 {
            conn.query_row("SELECT last_change FROM store_changes", [], |r| r.get(0))
                .unwrap()
        };
        let mut seen = before.0;
        for sql in [
            "INSERT INTO conversation_projects (origin,project) VALUES ('claude:session/a','github.com/x/y')",
            "UPDATE conversation_projects SET project='github.com/x/z'",
            "DELETE FROM conversation_projects",
        ] {
            conn.execute(sql, []).unwrap();
            assert_eq!(last(), seen + 1, "{sql}");
            seen += 1;
        }
        conn.execute(
            "INSERT INTO conversation_projects (origin,project) VALUES ('claude:session/a','github.com/x/y')",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE conversation_projects SET linked_at='2030-01-01'",
            [],
        )
        .unwrap();
        assert_eq!(last(), seen + 1, "the link's time is not a change");
        assert_eq!(counted_state(&conn).1, before.1, "no memory's row moved");
    }
}
