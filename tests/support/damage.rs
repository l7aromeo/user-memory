//! Stores damaged the way two processes writing at once without SQLite's
//! locks can leave them, for the tests of `doctor`, `repair` and `migrate`.
//! Each damage leaves every memory's row in place.

use rusqlite::Connection;
use std::{fs, path::Path};

/// What `PRAGMA <pragma>` (`integrity_check` or `quick_check`) finds in `db`.
pub fn check(db: &Path, pragma: &str) -> Vec<String> {
    Connection::open(db)
        .unwrap()
        .prepare(&format!("PRAGMA {pragma}"))
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// Memories in the table itself, read without any index.
pub fn rows(db: &Path) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row("SELECT count(*) FROM memories NOT INDEXED", [], |r| {
            r.get(0)
        })
        .unwrap()
}

/// Replaces the id index with one built from `rows` (rowid, id): a stand-in
/// table is filled with them, the two indexes swap root pages, and the
/// stand-in is dropped with the old index's pages, so no page is lost.
fn swap_id_index(db: &Path, rows: &str) {
    let conn = Connection::open(db).unwrap();
    conn.execute_batch(&format!(
        "CREATE TABLE stand_in(id TEXT PRIMARY KEY NOT NULL);
         INSERT INTO stand_in(rowid, id) {rows};"
    ))
    .unwrap();
    let root = |name: &str| -> i64 {
        conn.query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name = ?1",
            [name],
            |r| r.get(0),
        )
        .unwrap()
    };
    let (real, stand_in) = (
        root("sqlite_autoindex_memories_1"),
        root("sqlite_autoindex_stand_in_1"),
    );
    conn.execute_batch("PRAGMA writable_schema = ON").unwrap();
    for (name, page) in [
        ("sqlite_autoindex_memories_1", stand_in),
        ("sqlite_autoindex_stand_in_1", real),
    ] {
        conn.execute(
            "UPDATE sqlite_schema SET rootpage = ?1 WHERE name = ?2",
            rusqlite::params![page, name],
        )
        .unwrap();
    }
    conn.execute_batch("PRAGMA writable_schema = OFF").unwrap();
    drop(conn);
    Connection::open(db)
        .unwrap()
        .execute_batch("DROP TABLE stand_in")
        .unwrap();
    assert_ne!(check(db, "integrity_check"), ["ok"], "the store is damaged");
}

/// The newest memory's row stays in the table but is gone from the id
/// index, so a scan lists it and a lookup by id misses it.
pub fn lose_newest_from_id_index(db: &Path) {
    swap_id_index(
        db,
        "SELECT rowid, id FROM memories WHERE rowid < (SELECT max(rowid) FROM memories)",
    );
}

/// The id index keeps an entry for a row the table does not have.
pub fn keep_a_stale_id_index_entry(db: &Path) {
    swap_id_index(
        db,
        "SELECT rowid, id FROM memories UNION ALL SELECT (SELECT max(rowid) FROM memories) + 1000, '1a2b3c4d-0000-4000-8000-00000000dead'",
    );
}

/// The id index points the two newest ids at each other's rows: as many
/// entries as rows, so `quick_check`, which only counts them, passes it.
pub fn cross_two_id_index_entries(db: &Path) {
    swap_id_index(
        db,
        "SELECT rowid, CASE rowid
             WHEN (SELECT max(rowid) FROM memories)
                 THEN (SELECT id FROM memories WHERE rowid = (SELECT max(rowid) FROM memories) - 1)
             WHEN (SELECT max(rowid) FROM memories) - 1
                 THEN (SELECT id FROM memories WHERE rowid = (SELECT max(rowid) FROM memories))
             ELSE id END
         FROM memories",
    );
    assert_eq!(check(db, "quick_check"), ["ok"], "quick_check misses it");
}

/// The keyword index loses the size record of one memory's text, which
/// FTS5 reports as a malformed inverted index.
pub fn break_the_keyword_index(db: &Path) {
    Connection::open(db)
        .unwrap()
        .execute_batch(
            "DELETE FROM memory_search_docsize WHERE id = (SELECT max(id) FROM memory_search_docsize)",
        )
        .unwrap();
    assert_ne!(check(db, "integrity_check"), ["ok"], "the store is damaged");
}

/// The file header counts three free pages the store does not have: damage
/// beyond the indexes that only rewriting the whole file mends. Written
/// while no connection is open.
pub fn miscount_free_pages(db: &Path) {
    let mut bytes = fs::read(db).unwrap();
    let count = u32::from_be_bytes(bytes[36..40].try_into().unwrap());
    bytes[36..40].copy_from_slice(&(count + 3).to_be_bytes());
    fs::write(db, bytes).unwrap();
    assert_ne!(check(db, "integrity_check"), ["ok"], "the store is damaged");
}

/// A kind of damage, by name, and what makes it.
pub type Damage = (&'static str, fn(&Path));

/// Every kind of damage above.
pub const ALL: [Damage; 5] = [
    ("lose_newest_from_id_index", lose_newest_from_id_index),
    ("keep_a_stale_id_index_entry", keep_a_stale_id_index_entry),
    ("cross_two_id_index_entries", cross_two_id_index_entries),
    ("break_the_keyword_index", break_the_keyword_index),
    ("miscount_free_pages", miscount_free_pages),
];
