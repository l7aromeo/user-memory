//! Search indexes derived from stored memories: an FTS5 keyword index kept in
//! step by triggers, and embedding vectors filled lazily at recall time.

use super::{
    Memory,
    schema::{SELECT, connect, row, write},
    similar::{Draft, DraftVector, cosine, document, from_blob, save_vector},
};
use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path};
use uuid::Uuid;

pub(super) mod embed;

/// Keyed by memory id rather than rowid: VACUUM may renumber the rowids of a
/// table without an INTEGER PRIMARY KEY. Part of the stored schema: bump
/// `schema::SCHEMA_VERSION` whenever this SQL changes.
const SEARCH: &str = "CREATE VIRTUAL TABLE memory_search USING fts5(id UNINDEXED,memory_type,category,title,content,context,reason,evidence,tokenize='unicode61 remove_diacritics 0');
INSERT INTO memory_search(id,memory_type,category,title,content,context,reason,evidence) SELECT id,memory_type,category,title,content,context,reason,evidence FROM memories;
CREATE TABLE IF NOT EXISTS memory_vectors (
    id TEXT PRIMARY KEY NOT NULL,
    model TEXT NOT NULL,
    dimensions INTEGER NOT NULL,
    source TEXT NOT NULL,
    vector BLOB NOT NULL
);
CREATE TRIGGER IF NOT EXISTS memory_search_inserted AFTER INSERT ON memories BEGIN
    INSERT INTO memory_search(id,memory_type,category,title,content,context,reason,evidence) VALUES (NEW.id,NEW.memory_type,NEW.category,NEW.title,NEW.content,NEW.context,NEW.reason,NEW.evidence);
END;
CREATE TRIGGER IF NOT EXISTS memory_search_changed AFTER UPDATE OF memory_type,category,title,content,context,reason,evidence ON memories BEGIN
    UPDATE memory_search SET memory_type=NEW.memory_type,category=NEW.category,title=NEW.title,content=NEW.content,context=NEW.context,reason=NEW.reason,evidence=NEW.evidence WHERE id=OLD.id;
END;
CREATE TRIGGER IF NOT EXISTS memory_search_deleted AFTER DELETE ON memories BEGIN
    DELETE FROM memory_search WHERE id=OLD.id;
    DELETE FROM memory_vectors WHERE id=OLD.id;
END;";

fn installed(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='memory_search')",
        [],
        |r| r.get(0),
    )
}

/// Creates the search structures after the memories schema exists and
/// indexes memories stored before the index existed.
pub(super) fn install(conn: &Connection) -> Result<()> {
    if installed(conn)? {
        return Ok(());
    }
    // Re-check under the write lock so concurrent first connections index once.
    let tx = write(conn)?;
    if !installed(&tx)? {
        tx.execute_batch(SEARCH)?;
    }
    tx.commit()?;
    Ok(())
}

/// FTS5 query matching any of `tokens`, each quoted so it is never read as syntax.
pub(super) fn any_token(tokens: &[&str]) -> String {
    tokens
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
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
    tx.execute_batch("DELETE FROM memory_search;
INSERT INTO memory_search(id,memory_type,category,title,content,context,reason,evidence) SELECT id,memory_type,category,title,content,context,reason,evidence FROM memories;
DELETE FROM memory_vectors WHERE id NOT IN (SELECT id FROM memories);")?;
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
