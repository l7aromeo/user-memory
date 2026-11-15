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
