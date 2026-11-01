//! Search indexes derived from stored memories: a keyword index of each
//! memory's stemmed words, kept in step with the memories in the transaction
//! that changes them, and embedding vectors filled lazily at recall time.

use super::{
    Memory,
    schema::{SELECT, connect, row, write},
    selection::{Filter, condition},
    similar::{Draft, DraftVector, cosine, document, from_blob, save_vector},
};
use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path};
use uuid::Uuid;

pub(super) mod embed;
pub(super) mod stems;

use stems::{Field, Stems, Text, fields, stamp, token};

/// The keyword index and the vectors, created with every store and by the
/// migration to version 6. Part of the stored schema: bump
/// `schema::SCHEMA_VERSION` whenever this SQL changes.
///
/// - `search_keys` gives each memory a key, which VACUUM keeps (memories
///   are keyed by their text id, and VACUUM may renumber a rowid), in the
///   order the memories were stored. It copies the columns search filters
///   on under their `memories` names, so one filter serves both tables.
/// - `search_words` holds each memory's stems under its key, each stem in
///   the column of the strongest field holding it, as hex tokens (see
///   `stems::token`). It stores no text and no positions: search asks only
///   which memories hold a stem, and in which field.
/// - `search_pending` lists the memories whose words changed since they
///   were indexed. Stemming needs Rust, so triggers queue the memory and
///   every write drains the queue before it commits.
/// - `search_state` holds the stamp of the code that made the stored stems.
/// - `search_changes` logs the id of every memory or vector changed, in
///   order, so a server holding vectors in memory reloads only what changed
///   since it last looked. Writes keep the newest [`CHANGES_KEPT`].
///
/// The triggers keep the keys, the queue and the log in step with any
/// write, whichever program makes it. A memory replaced through
/// `INSERT OR REPLACE` loses its old key on insert, as SQLite fires no
/// delete trigger for it.
const SEARCH: &str = "CREATE TABLE IF NOT EXISTS memory_vectors (
    id TEXT PRIMARY KEY NOT NULL,
    model TEXT NOT NULL,
    dimensions INTEGER NOT NULL,
    source TEXT NOT NULL,
    vector BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS search_keys (
    key INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    memory_type TEXT NOT NULL,
    category TEXT NOT NULL,
    scope TEXT NOT NULL,
    project TEXT NOT NULL,
    source TEXT NOT NULL,
    status TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS search_words USING fts5(category,title,type,body,content='',contentless_delete=1,detail=column,tokenize='ascii');
CREATE TABLE IF NOT EXISTS search_pending (key INTEGER PRIMARY KEY NOT NULL);
CREATE TABLE IF NOT EXISTS search_state (
    id INTEGER PRIMARY KEY CHECK(id=1),
    stemmed_by TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS search_changes (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL
);
CREATE TRIGGER IF NOT EXISTS search_inserted AFTER INSERT ON memories BEGIN
    DELETE FROM search_words WHERE rowid IN (SELECT key FROM search_keys WHERE id=NEW.id);
    DELETE FROM search_pending WHERE key IN (SELECT key FROM search_keys WHERE id=NEW.id);
    DELETE FROM search_keys WHERE id=NEW.id;
    INSERT INTO search_keys(id,memory_type,category,scope,project,source,status,updated_at) VALUES (NEW.id,NEW.memory_type,NEW.category,NEW.scope,NEW.project,NEW.source,NEW.status,NEW.updated_at);
    INSERT INTO search_pending(key) SELECT key FROM search_keys WHERE id=NEW.id;
    INSERT INTO search_changes(id) VALUES (NEW.id);
END;
CREATE TRIGGER IF NOT EXISTS search_changed AFTER UPDATE ON memories BEGIN
    UPDATE search_keys SET id=NEW.id,memory_type=NEW.memory_type,category=NEW.category,scope=NEW.scope,project=NEW.project,source=NEW.source,status=NEW.status,updated_at=NEW.updated_at WHERE id=OLD.id;
    INSERT OR IGNORE INTO search_pending(key) SELECT key FROM search_keys WHERE id=NEW.id AND (NEW.memory_type IS NOT OLD.memory_type OR NEW.category IS NOT OLD.category OR NEW.title IS NOT OLD.title OR NEW.content IS NOT OLD.content OR NEW.context IS NOT OLD.context OR NEW.reason IS NOT OLD.reason OR NEW.evidence IS NOT OLD.evidence);
    INSERT INTO search_changes(id) VALUES (NEW.id);
END;
CREATE TRIGGER IF NOT EXISTS search_deleted AFTER DELETE ON memories BEGIN
    DELETE FROM search_words WHERE rowid IN (SELECT key FROM search_keys WHERE id=OLD.id);
    DELETE FROM search_pending WHERE key IN (SELECT key FROM search_keys WHERE id=OLD.id);
    DELETE FROM search_keys WHERE id=OLD.id;
    DELETE FROM memory_vectors WHERE id=OLD.id;
    INSERT INTO search_changes(id) VALUES (OLD.id);
END;
CREATE TRIGGER IF NOT EXISTS search_vector_inserted AFTER INSERT ON memory_vectors BEGIN
    INSERT INTO search_changes(id) VALUES (NEW.id);
END;
CREATE TRIGGER IF NOT EXISTS search_vector_changed AFTER UPDATE ON memory_vectors BEGIN
    INSERT INTO search_changes(id) VALUES (NEW.id);
END;
CREATE TRIGGER IF NOT EXISTS search_vector_deleted AFTER DELETE ON memory_vectors BEGIN
    INSERT INTO search_changes(id) VALUES (OLD.id);
END;";

/// Creates the search tables and triggers where missing, as every open of a
/// current store does; a store created now gets an empty index stamped as
/// this binary's.
pub(super) fn create(conn: &Connection) -> Result<()> {
    conn.execute_batch(SEARCH)?;
    Ok(())
}

/// Stamps a store just created, whose index is empty and so current.
pub(super) fn stamp_new(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO search_state(id,stemmed_by) VALUES (1,?1)",
        [stamp()],
    )?;
    Ok(())
}

/// The longest token the keyword index stores whole: FTS5 cuts longer
/// tokens to this many bytes, which would make two long stems one. A stem
/// is at most a word of one field, and recording refuses a field long
/// enough to hold such a word (4,000 characters of at most 4 bytes, 32,000
/// hex digits); a memory written past that limit by another program is
/// left unindexed, and search then reads every memory.
const TOKEN_LIMIT: usize = 32_768;

/// Whether the stored stems were made by this binary's code.
fn current(conn: &Connection) -> Result<bool> {
    let stamped: Option<String> = conn
        .query_row("SELECT stemmed_by FROM search_state WHERE id=1", [], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(stamped.is_some_and(|stamped| stamped == stamp()))
}

fn pending(conn: &Connection) -> Result<bool> {
    Ok(
        conn.query_row("SELECT EXISTS(SELECT 1 FROM search_pending)", [], |r| {
            r.get(0)
        })?,
    )
}

/// Changes a server can fall behind by and still catch up on row by row:
/// far more than the writes between two searches of one session. A server
/// further behind reloads every vector.
const CHANGES_KEPT: i64 = 10_000;

/// What every write does before it commits: [`drain`] the queue, and keep
/// only the newest [`CHANGES_KEPT`] changes.
pub(super) fn settle(conn: &Connection) -> Result<()> {
    conn.execute(
        "DELETE FROM search_changes WHERE seq <= (SELECT max(seq) FROM search_changes) - ?1",
        [CHANGES_KEPT],
    )?;
    drain(conn)
}

/// Indexes the memories queued in `search_pending`, inside the caller's
/// write transaction. Every write runs it before committing, so the index
/// matches the memories in each committed state. An index stamped by other
/// code is left as it is: only a rebuild makes it current again.
pub(super) fn drain(conn: &Connection) -> Result<()> {
    if !pending(conn)? || !current(conn)? {
        return Ok(());
    }
    let queued = conn
        .prepare(
            "SELECT k.key,m.memory_type,m.category,m.title,m.content,m.context,m.reason,m.evidence FROM search_pending p JOIN search_keys k ON k.key=p.key JOIN memories m ON m.id=k.id ORDER BY k.key",
        )?
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                [
                    r.get::<_, String>(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ],
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut stems = Stems::new();
    let mut forget = conn.prepare_cached("DELETE FROM search_words WHERE rowid=?1")?;
    let mut insert = conn.prepare_cached(
        "INSERT INTO search_words(rowid,category,title,type,body) VALUES (?1,?2,?3,?4,?5)",
    )?;
    let mut done = conn.prepare_cached("DELETE FROM search_pending WHERE key=?1")?;
    for (
        key,
        [
            memory_type,
            category,
            title,
            content,
            context,
            reason,
            evidence,
        ],
    ) in &queued
    {
        let found = fields(
            &mut stems,
            Text {
                memory_type,
                category,
                title,
                content,
                context,
                reason,
                evidence,
            },
        );
        let mut columns: [Vec<String>; 4] = Default::default();
        for (stem, field) in found {
            columns[field as usize].push(token(&stem));
        }
        if columns.iter().flatten().any(|t| t.len() > TOKEN_LIMIT) {
            continue;
        }
        let [category, title, kind, body] = columns.map(|tokens| tokens.join(" "));
        forget.execute([key])?;
        insert.execute(params![key, category, title, kind, body])?;
        done.execute([key])?;
    }
    Ok(())
}

/// Rebuilds the keyword index from the memories with this binary's code,
/// inside the caller's write transaction: new keys in stored order, every
/// memory's stems, and the current stamp.
pub(super) fn rebuild(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DELETE FROM search_pending;
DELETE FROM search_keys;
INSERT INTO search_words(search_words) VALUES('delete-all');
INSERT INTO search_keys(id,memory_type,category,scope,project,source,status,updated_at) SELECT id,memory_type,category,scope,project,source,status,updated_at FROM memories ORDER BY rowid;
INSERT INTO search_pending(key) SELECT key FROM search_keys;",
    )?;
    conn.execute(
        "INSERT INTO search_state(id,stemmed_by) VALUES (1,?1) ON CONFLICT(id) DO UPDATE SET stemmed_by=excluded.stemmed_by",
        [stamp()],
    )?;
    drain(conn)
}

/// Rebuilds the keyword index inside the caller's write transaction when
/// other code made it, as after an upgrade that stems words differently.
/// Returns whether it did.
pub(super) fn refresh(conn: &Connection) -> Result<bool> {
    if current(conn)? {
        return Ok(false);
    }
    rebuild(conn)?;
    Ok(true)
}

/// Whether the keyword index of the store at `db` was made by other code,
/// so that search reads every memory until migrate or reindex rebuilds it.
pub fn outdated(db: &Path) -> Result<bool> {
    Ok(!current(&connect(db)?)?)
}

/// Whether search can read the keyword index: it was made by this binary's
/// code and holds every memory. Writes through user-memory leave nothing
/// queued; what another program queued is indexed here first. A store that
/// cannot be written, or a memory too long to index, leaves it unready.
pub(in crate::memory) fn ready(conn: &Connection) -> Result<bool> {
    if !current(conn)? {
        return Ok(false);
    }
    if pending(conn)? && write(conn).and_then(|tx| tx.commit()).is_err() {
        return Ok(false);
    }
    Ok(!pending(conn)?)
}

/// What ranking needs of a candidate, without its text.
#[derive(Debug)]
pub(in crate::memory) struct Entry {
    pub(in crate::memory) id: Uuid,
    /// A project memory.
    pub(in crate::memory) local: bool,
    pub(in crate::memory) updated_at: String,
    /// Orders memories stored at the same time: later stored, higher. The
    /// rowid when read from `memories`, the key when read from the index,
    /// which keys memories in the order they were stored.
    pub(in crate::memory) order: i64,
}

fn entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    let id: String = r.get(0)?;
    Ok(Entry {
        id: Uuid::parse_str(&id).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?,
        local: r.get::<_, String>(1)? == "project",
        updated_at: r.get(2)?,
        order: r.get(3)?,
    })
}

/// The keyword index's answer for a query: how many memories the filter
/// covers, and each covered memory holding a query term with the strongest
/// field holding each term, in query order.
pub(in crate::memory) struct Hits {
    pub(in crate::memory) candidates: usize,
    pub(in crate::memory) found: Vec<(Entry, Vec<Option<Field>>)>,
}

/// Reads the memories `filter` covers that hold any of the stemmed `query`
/// terms, from a [`ready`] index. The covered memories are read once, by
/// the filter alone; the index then gives only the keys holding each term,
/// so a word that most memories hold costs one lookup per memory.
pub(in crate::memory) fn hits(
    conn: &Connection,
    filter: &Filter,
    query: &[String],
) -> Result<Hits> {
    let covered = entries(conn, filter)?;
    let candidates = covered.len();
    let mut slots: HashMap<i64, usize> = HashMap::with_capacity(candidates);
    for (i, entry) in covered.iter().enumerate() {
        slots.insert(entry.order, i);
    }
    let mut held: Vec<Option<Vec<Option<Field>>>> = vec![None; candidates];
    let mut matching =
        conn.prepare_cached("SELECT rowid FROM search_words WHERE search_words MATCH ?1")?;
    for (t, term) in query.iter().enumerate() {
        let token = token(term);
        // Each stem is stored once, at its strongest field.
        for field in Field::ALL {
            let mut rows = matching.query([format!("{} : \"{token}\"", field.column())])?;
            while let Some(r) = rows.next()? {
                if let Some(&i) = slots.get(&r.get::<_, i64>(0)?) {
                    held[i].get_or_insert_with(|| vec![None; query.len()])[t] = Some(field);
                }
            }
        }
    }
    Ok(Hits {
        candidates,
        found: covered
            .into_iter()
            .zip(held)
            .filter_map(|(entry, fields)| Some((entry, fields?)))
            .collect(),
    })
}

/// Every memory `filter` covers, without its text, from a ready index.
pub(in crate::memory) fn entries(conn: &Connection, filter: &Filter) -> Result<Vec<Entry>> {
    let (condition, values) = condition(filter)?;
    Ok(conn
        .prepare(&format!(
            "SELECT id,scope,updated_at,key FROM search_keys AS memories WHERE {condition}"
        ))?
        .query_map(params_from_iter(values), entry)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// What is wrong with a current keyword index: memories without a key,
/// keys without a memory or with columns that differ from it, and stored
/// stems that differ from the memories' words. Empty for an index stamped
/// by other code, which search does not read, and while memories are
/// queued, which the next write indexes.
pub(super) fn problems(conn: &Connection) -> Result<Vec<String>> {
    if !current(conn)? || pending(conn)? {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    let unmatched: i64 = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM memories m WHERE NOT EXISTS (SELECT 1 FROM search_keys k WHERE k.id=m.id AND k.memory_type=m.memory_type AND k.category=m.category AND k.scope=m.scope AND k.project=m.project AND k.source=m.source AND k.status=m.status AND k.updated_at=m.updated_at)) + (SELECT COUNT(*) FROM search_keys k WHERE NOT EXISTS (SELECT 1 FROM memories m WHERE m.id=k.id))",
        [],
        |r| r.get(0),
    )?;
    if unmatched > 0 {
        found.push(format!(
            "search index: {unmatched} keys do not match their memories"
        ));
    }
    let mut expected: HashMap<(i64, String), &'static str> = HashMap::new();
    let mut stems = Stems::new();
    let mut rows = conn
        .prepare(
            "SELECT k.key,m.memory_type,m.category,m.title,m.content,m.context,m.reason,m.evidence FROM search_keys k JOIN memories m ON m.id=k.id",
        )?;
    let mut rows = rows.query([])?;
    while let Some(r) = rows.next()? {
        let key: i64 = r.get(0)?;
        let text: [String; 7] = [
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
            r.get(7)?,
        ];
        let [
            memory_type,
            category,
            title,
            content,
            context,
            reason,
            evidence,
        ] = &text;
        for (stem, field) in fields(
            &mut stems,
            Text {
                memory_type,
                category,
                title,
                content,
                context,
                reason,
                evidence,
            },
        ) {
            expected.insert((key, token(&stem)), field.column());
        }
    }
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS temp.search_entries USING fts5vocab(main,'search_words','instance');",
    )?;
    let mut differing = std::collections::BTreeSet::new();
    let mut stored = conn.prepare("SELECT doc,term,col FROM temp.search_entries")?;
    let mut stored = stored.query([])?;
    while let Some(r) = stored.next()? {
        let (key, term, column): (i64, String, String) = (r.get(0)?, r.get(1)?, r.get(2)?);
        if expected.remove(&(key, term)) != Some(column.as_str()) {
            differing.insert(key);
        }
    }
    differing.extend(expected.into_keys().map(|(key, _)| key));
    conn.execute_batch("DROP TABLE temp.search_entries;")?;
    if !differing.is_empty() {
        found.push(format!(
            "search index: the stored words of {} memories differ from their text",
            differing.len()
        ));
    }
    Ok(found)
}

/// Stored vector for `id`, only if `model` embedded it from `source` at `dimensions`.
fn stored(
    conn: &Connection,
    id: Uuid,
    (model, dimensions): (&str, i64),
    source: &str,
) -> Result<Option<Vec<f32>>> {
    Ok(conn
        .query_row(
            "SELECT vector FROM memory_vectors WHERE id=?1 AND model=?2 AND dimensions=?3 AND source=?4",
            params![id.to_string(), model, dimensions, source],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .optional()?
        .map(|blob| from_blob(&blob)))
}

/// Memories one recall may embed. Embedding takes roughly 0.1 s per memory on
/// a local model, so a store without vectors would otherwise stall its first
/// semantic recall for minutes; the rest fill on later recalls or `reindex`.
const RECALL_EMBED_BUDGET: usize = 32;

/// Vectors for `items` in order, embedding and saving those that are missing
/// or were made by another model, at another length or from older text, up to
/// `budget` of them (newest first). Memories left unembedded get `None`.
/// Returns how many were embedded.
fn vectors(
    conn: &Connection,
    config: &embed::Config,
    items: &[Memory],
    budget: usize,
) -> Result<(Vec<Option<Vec<f32>>>, usize)> {
    let identity = config.identity();
    let sources: Vec<String> = items.iter().map(|m| document(&Draft::from(m))).collect();
    let mut found = Vec::with_capacity(items.len());
    let mut missing = Vec::new();
    for (i, m) in items.iter().enumerate() {
        let vector = stored(conn, m.id, identity, &sources[i])?;
        if vector.is_none() {
            missing.push(i);
        }
        found.push(vector);
    }
    missing.sort_by_key(|&i| std::cmp::Reverse(items[i].sequence));
    missing.truncate(budget);
    if !missing.is_empty() {
        let texts: Vec<String> = missing.iter().map(|&i| sources[i].clone()).collect();
        let fresh = config.embed(&texts)?;
        let tx = write(conn)?;
        for (&i, vector) in missing.iter().zip(fresh) {
            let vector = DraftVector {
                vector,
                identity: (identity.0.to_owned(), identity.1),
                source: sources[i].clone(),
            };
            save_vector(&tx, items[i].id, &vector)?;
            found[i] = Some(vector.vector);
        }
        tx.commit()?;
    }
    Ok((found, missing.len()))
}

/// Cosine similarity between `query` and each memory that has a vector.
pub(super) fn similarities(
    conn: &Connection,
    config: &embed::Config,
    query: &str,
    items: &[Memory],
) -> Result<HashMap<Uuid, f32>> {
    let query = config.embed(&[config.query(query)])?.remove(0);
    let (vectors, _) = vectors(conn, config, items, RECALL_EMBED_BUDGET)?;
    let mut scores = HashMap::with_capacity(items.len());
    let pending = vectors.iter().filter(|v| v.is_none()).count();
    if pending > 0 {
        embed::pending(pending);
    }
    for (m, vector) in items.iter().zip(vectors) {
        let Some(vector) = vector else { continue };
        if vector.len() != query.len() {
            bail!(
                "Embedding endpoint returned {} dimensions for a memory but {} for the query; run user-memory reindex",
                vector.len(),
                query.len()
            );
        }
        scores.insert(m.id, cosine(&query, &vector));
    }
    Ok(scores)
}

/// Rebuilds the keyword index from the stored memories and, when semantic
/// search is configured, embeds every memory that lacks a current vector.
pub fn reindex(db: &Path) -> Result<Value> {
    reindex_with(db, embed::Config::from_env().as_ref())
}

/// [`reindex`] embedding with `config`; `None` rebuilds the keyword index only.
pub(in crate::memory) fn reindex_with(db: &Path, config: Option<&embed::Config>) -> Result<Value> {
    let conn = connect(db)?;
    let tx = write(&conn)?;
    rebuild(&tx)?;
    tx.execute(
        "DELETE FROM memory_vectors WHERE id NOT IN (SELECT id FROM memories)",
        [],
    )?;
    tx.commit()?;
    let items = conn
        .prepare(&format!("{SELECT} ORDER BY rowid"))?
        .query_map([], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let embedded = match config {
        Some(config) => json!(vectors(&conn, config, &items, usize::MAX)?.1),
        None => Value::Null,
    };
    Ok(json!({"reindexed":items.len(),"embedded":embedded}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = connect(&dir.path().join("memories.sqlite3")).unwrap();
        (dir, conn)
    }

    fn changes(conn: &Connection) -> Vec<String> {
        conn.prepare("SELECT id FROM search_changes ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn every_change_to_a_memory_or_its_vector_is_logged_in_order() {
        let (_dir, conn) = store();
        let id = "0bd1ba49-2b63-4aab-8d57-38d9be10a281";
        conn.execute(
            "INSERT INTO memories (id,memory_type,category,title,content) VALUES (?1,'fact','tools','Use Helix','Use Helix')",
            [id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memory_vectors (id,model,dimensions,source,vector) VALUES (?1,'m',0,'s',x'00000000')",
            [id],
        )
        .unwrap();
        conn.execute("UPDATE memory_vectors SET source='t' WHERE id=?1", [id])
            .unwrap();
        conn.execute(
            "UPDATE memories SET last_confirmed_at='2026-11-01' WHERE id=?1",
            [id],
        )
        .unwrap();
        // Deleting the memory deletes its vector, and logs both.
        conn.execute("DELETE FROM memories WHERE id=?1", [id])
            .unwrap();
        assert_eq!(changes(&conn), vec![id; 6]);
    }

    #[test]
    fn writes_keep_the_newest_changes_only() {
        let (_dir, conn) = store();
        let extra = 5;
        conn.execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < ?1) INSERT INTO search_changes(id) SELECT 'id-' || i FROM n",
            [CHANGES_KEPT + extra],
        )
        .unwrap();
        let tx = write(&conn).unwrap();
        tx.commit().unwrap();
        let kept = changes(&conn);
        assert_eq!(kept.len() as i64, CHANGES_KEPT);
        assert_eq!(kept[0], format!("id-{}", extra + 1));
    }
}
