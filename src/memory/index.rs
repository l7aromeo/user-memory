//! Search indexes derived from stored memories: a keyword index of each
//! memory's stemmed words, kept in step with the memories in the transaction
//! that changes them, and embedding vectors filled lazily at recall time and
//! held by the process between searches.

use super::{
    Memory,
    schema::{SELECT, connect, row, write},
    selection::{Filter, condition},
    similar::{Draft, DraftVector, document, save_vector},
};
use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ops::Range,
    path::Path,
};
use uuid::Uuid;

pub(super) mod embed;
pub(super) mod passages;
pub(super) mod stems;
pub(super) mod vectors;

use super::{similar::category_group, words::tokenize};
use stems::{Field, Stems, Text, stamp, token};

/// The keyword index and the vectors, created with every store and by the
/// migration to version 6. Part of the stored schema: bump
/// `schema::SCHEMA_VERSION` whenever this SQL changes.
///
/// - `search_keys` gives each memory a key, which VACUUM keeps (memories
///   are keyed by their text id, and VACUUM may renumber a rowid), in the
///   order the memories were stored. It copies the columns search filters
///   on under their `memories` names, so one filter serves both tables.
/// - `search_words` ([`WORDS`]) holds each memory's stems and anchors in
///   rows numbered from its key ([`SLOTS`]; the triggers write the number
///   out).
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
/// delete trigger for it. A changed memory already queued is queued by an
/// upsert, not `INSERT OR IGNORE`: the update's own conflict clause, such as
/// `UPDATE OR ABORT`, replaces the clauses of the statements in its
/// triggers, but not an upsert's.
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
    DELETE FROM search_words WHERE rowid BETWEEN (SELECT key*256 FROM search_keys WHERE id=NEW.id) AND (SELECT key*256+255 FROM search_keys WHERE id=NEW.id);
    DELETE FROM search_pending WHERE key IN (SELECT key FROM search_keys WHERE id=NEW.id);
    DELETE FROM search_keys WHERE id=NEW.id;
    INSERT INTO search_keys(id,memory_type,category,scope,project,source,status,updated_at) VALUES (NEW.id,NEW.memory_type,NEW.category,NEW.scope,NEW.project,NEW.source,NEW.status,NEW.updated_at);
    INSERT INTO search_pending(key) SELECT key FROM search_keys WHERE id=NEW.id;
    INSERT INTO search_changes(id) VALUES (NEW.id);
END;
CREATE TRIGGER IF NOT EXISTS search_changed AFTER UPDATE ON memories BEGIN
    UPDATE search_keys SET id=NEW.id,memory_type=NEW.memory_type,category=NEW.category,scope=NEW.scope,project=NEW.project,source=NEW.source,status=NEW.status,updated_at=NEW.updated_at WHERE id=OLD.id;
    INSERT INTO search_pending(key) SELECT key FROM search_keys WHERE id=NEW.id AND (NEW.memory_type IS NOT OLD.memory_type OR NEW.category IS NOT OLD.category OR NEW.title IS NOT OLD.title OR NEW.content IS NOT OLD.content OR NEW.context IS NOT OLD.context OR NEW.reason IS NOT OLD.reason OR NEW.evidence IS NOT OLD.evidence) ON CONFLICT(key) DO NOTHING;
    INSERT INTO search_changes(id) VALUES (NEW.id);
END;
CREATE TRIGGER IF NOT EXISTS search_deleted AFTER DELETE ON memories BEGIN
    DELETE FROM search_words WHERE rowid BETWEEN (SELECT key*256 FROM search_keys WHERE id=OLD.id) AND (SELECT key*256+255 FROM search_keys WHERE id=OLD.id);
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

/// The keyword index: each memory's [`rows`], numbered `key * SLOTS +
/// slot`, each stem in the column of the strongest field holding it in the
/// row and each anchor in `anchors`, as hex tokens (see `stems::token`),
/// with a passage's place in its text (`start`, `length`, in bytes). It
/// holds no text and no positions: search asks only which rows hold a stem
/// or an anchor, and in which field. The index stores its columns, the same
/// tokens, because FTS5's `secure-delete` (see [`secure`]) works only on an
/// index that can read a deleted entry's tokens back. Part of the stored
/// schema, as [`SEARCH`] is.
const WORDS: &str = "CREATE VIRTUAL TABLE IF NOT EXISTS search_words USING fts5(category,title,type,body,anchors,start UNINDEXED,length UNINDEXED,detail=column,tokenize='ascii');";

/// Rows the keyword index numbers for each memory: the memory's rows are
/// `key * SLOTS` up to the next memory's. Slot 0 holds what counts for the
/// whole memory, slots from 1 the content's passages, and [`CONTEXT_SLOT`]
/// the context's. The triggers in [`SEARCH`] write this number out.
pub(in crate::memory) const SLOTS: i64 = 256;

/// The slot of a memory's context, a passage of its own: it says when the
/// memory applies.
pub(in crate::memory) const CONTEXT_SLOT: i64 = SLOTS - 1;

const _: () = assert!(
    passages::MAX_PASSAGES as i64 + 1 < CONTEXT_SLOT,
    "every content passage has a slot before the context's"
);

/// Creates the search tables and triggers where missing, as every open of a
/// current store does; a store created now gets an empty index stamped as
/// this binary's.
pub(super) fn create(conn: &Connection) -> Result<()> {
    conn.execute_batch(SEARCH)?;
    conn.execute_batch(WORDS)?;
    Ok(())
}

/// Readies the index of a store just created: empty, so current, and
/// removing deleted words at once.
pub(super) fn start(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO search_state(id,stemmed_by) VALUES (1,?1)",
        [stamp()],
    )?;
    secure(conn)
}

/// Makes the keyword index take a deleted or replaced entry's tokens out of
/// its pages in the deleting transaction, so a forgotten memory's words
/// leave the store without merging the whole index. FTS5 keeps it in the
/// index's own config table, not in its SQL, so each creation of the table
/// sets it.
fn secure(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO search_words(search_words, rank) VALUES('secure-delete', 1)",
        [],
    )?;
    Ok(())
}

/// Whether the keyword index stores its columns, as [`WORDS`] makes it; a
/// store not yet upgraded, which repair may mend, has a contentless one.
fn stored(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='search_words_content')",
        [],
        |r| r.get(0),
    )?)
}

/// Whether the keyword index removes deleted words at once (see [`secure`]).
fn secured(conn: &Connection) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT v FROM search_words_config WHERE k='secure-delete'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        == Some(1))
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

/// One row of a memory in the keyword index (see [`rows`]).
#[derive(Debug, PartialEq)]
pub(in crate::memory) struct Row {
    pub(in crate::memory) slot: i64,
    /// Each stem of the row with the strongest field holding it there.
    pub(in crate::memory) stems: Vec<(String, Field)>,
    /// The row's literal anchors (see [`passages::anchors`]).
    pub(in crate::memory) anchors: Vec<String>,
    /// The passage's bytes in its text: the content's, or the context's in
    /// [`CONTEXT_SLOT`]. Empty in slot 0.
    pub(in crate::memory) place: Range<usize>,
}

/// A memory's fields that hold its words.
impl<'a> From<&'a Memory> for Text<'a> {
    fn from(m: &'a Memory) -> Self {
        Self {
            memory_type: &m.memory_type,
            category: &m.category,
            title: &m.title,
            content: &m.content,
            context: &m.context,
            reason: &m.reason,
            evidence: &m.evidence,
        }
    }
}

/// The rows a memory is indexed and ranked in. Slot 0 holds the category,
/// title, type, reason and evidence, which say what the whole memory is
/// about, so they count for each passage; then a row for each passage of
/// the content and one for a non-empty context. A passage row holds only
/// the stems and anchors slot 0 does not, which count the same either way.
pub(in crate::memory) fn rows(stems: &mut Stems, text: Text) -> Vec<Row> {
    let labels = category_group(text.category).join(" ");
    let whole = format!("{} {}", text.reason, text.evidence);
    let mut held: BTreeMap<String, Field> = BTreeMap::new();
    for (field, words) in Field::ALL.into_iter().zip([
        labels.as_str(),
        text.title,
        text.memory_type,
        whole.as_str(),
    ]) {
        for word in tokenize(words) {
            held.entry(stems.of(word).to_owned()).or_insert(field);
        }
    }
    let anchored: BTreeSet<String> = [text.title, text.reason, text.evidence]
        .into_iter()
        .flat_map(passages::anchors)
        .collect();
    let content = passages::split(text.content)
        .into_iter()
        .zip(1..)
        .map(|(place, slot)| (slot, text.content, place));
    let context =
        (!text.context.is_empty()).then_some((CONTEXT_SLOT, text.context, 0..text.context.len()));
    let mut rows = vec![Row {
        slot: 0,
        stems: held
            .iter()
            .map(|(stem, &field)| (stem.clone(), field))
            .collect(),
        anchors: anchored.iter().cloned().collect(),
        place: 0..0,
    }];
    for (slot, source, place) in content.chain(context) {
        let part = &source[place.clone()];
        let own: BTreeSet<String> = tokenize(part)
            .into_iter()
            .map(|word| stems.of(word).to_owned())
            .filter(|stem| !held.contains_key(stem))
            .collect();
        rows.push(Row {
            slot,
            stems: own.into_iter().map(|stem| (stem, Field::Body)).collect(),
            anchors: passages::anchors(part)
                .into_iter()
                .filter(|anchor| !anchored.contains(anchor))
                .collect(),
            place,
        });
    }
    rows
}

/// The first and last row number of the memory keyed `key`.
fn row_numbers(key: i64) -> (i64, i64) {
    (key * SLOTS, key * SLOTS + SLOTS - 1)
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
    let mut forget =
        conn.prepare_cached("DELETE FROM search_words WHERE rowid BETWEEN ?1 AND ?2")?;
    let mut insert = conn.prepare_cached(
        "INSERT INTO search_words(rowid,category,title,type,body,anchors,start,length) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
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
        let rows = rows(
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
        let stored: Vec<_> = rows.iter().map(|row| (row, columns(row))).collect();
        if stored
            .iter()
            .flat_map(|(_, columns)| columns)
            .any(|tokens| tokens.split(' ').any(|t| t.len() > TOKEN_LIMIT))
        {
            continue;
        }
        let (first, last) = row_numbers(*key);
        forget.execute([first, last])?;
        for (row, [category, title, kind, body, anchors]) in stored {
            let place = (row.slot != 0).then(|| (row.place.start, row.place.len()));
            insert.execute(params![
                first + row.slot,
                category,
                title,
                kind,
                body,
                anchors,
                place.map(|(start, _)| start),
                place.map(|(_, length)| length),
            ])?;
        }
        done.execute([key])?;
    }
    Ok(())
}

/// The token columns of `row` as the index stores them: category, title,
/// type, body and anchors.
fn columns(row: &Row) -> [String; 5] {
    let mut columns: [Vec<String>; 5] = Default::default();
    for (stem, field) in &row.stems {
        columns[*field as usize].push(token(stem));
    }
    columns[4] = row.anchors.iter().map(|anchor| token(anchor)).collect();
    columns.map(|tokens| tokens.join(" "))
}

/// Whether the keyword index has this binary's layout ([`WORDS`]), rather
/// than an earlier version's, which a store not yet upgraded has.
fn ours(conn: &Connection) -> Result<bool> {
    let sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='search_words'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let wanted = WORDS.replacen("IF NOT EXISTS ", "", 1);
    Ok(sql.is_some_and(|sql| sql == wanted.trim_end_matches(';')))
}

/// Rebuilds the keyword index from the memories with this binary's code,
/// inside the caller's write transaction: new keys in stored order, every
/// memory's rows, and the current stamp. FTS5's `delete-all` works only on
/// a contentless index, as a store not yet upgraded may have, and deleting
/// row by row reads the index a repair mends; emptying the stored columns
/// and rebuilding the index from them drops whatever it held instead,
/// without freeing the table's own pages, which a miscounted free-page list
/// would refuse. An earlier version's index, which repair may mend before
/// the upgrade, is only emptied and left unstamped: this binary cannot fill
/// it, and the upgrade replaces it and fills the new one.
pub(super) fn rebuild(conn: &Connection) -> Result<()> {
    if stored(conn)? {
        conn.execute_batch(
            "DELETE FROM search_words_content;
INSERT INTO search_words(search_words) VALUES('rebuild');",
        )?;
    } else {
        conn.execute_batch("INSERT INTO search_words(search_words) VALUES('delete-all');")?;
    }
    conn.execute_batch(
        "DELETE FROM search_pending;
DELETE FROM search_keys;
INSERT INTO search_keys(id,memory_type,category,scope,project,source,status,updated_at) SELECT id,memory_type,category,scope,project,source,status,updated_at FROM memories ORDER BY rowid;
INSERT INTO search_pending(key) SELECT key FROM search_keys;",
    )?;
    if !ours(conn)? {
        conn.execute("DELETE FROM search_state", [])?;
        return Ok(());
    }
    secure(conn)?;
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

/// What ranking needs of the memories a search covers, without their text,
/// as arrays one search fills and the next reuses. Buffers the size of the
/// store, allocated by each search and freed at its end, are kept resident
/// by the allocator for reuse: a keyword-only server on 100,000 memories
/// grew to 350 MB that way while holding 1 MB of live data.
#[derive(Debug, Default)]
pub(in crate::memory) struct Candidates {
    pub(in crate::memory) ids: Vec<Uuid>,
    /// Whether each is a project memory.
    pub(in crate::memory) local: Vec<bool>,
    /// Orders memories stored at the same time: later stored, higher. The
    /// rowid when read from `memories`, the key when read from the index,
    /// which keys memories in the order they were stored.
    pub(in crate::memory) orders: Vec<i64>,
    /// Every memory's `updated_at`, one after another, so none needs a
    /// string of its own.
    stamps: Vec<u8>,
    /// Where each memory's `updated_at` ends in `stamps`.
    stamp_ends: Vec<usize>,
}

impl Candidates {
    pub(in crate::memory) fn clear(&mut self) {
        self.ids.clear();
        self.local.clear();
        self.orders.clear();
        self.stamps.clear();
        self.stamp_ends.clear();
    }

    pub(in crate::memory) fn push(&mut self, id: Uuid, local: bool, updated_at: &str, order: i64) {
        self.ids.push(id);
        self.local.push(local);
        self.orders.push(order);
        self.stamps.extend_from_slice(updated_at.as_bytes());
        self.stamp_ends.push(self.stamps.len());
    }

    pub(in crate::memory) fn len(&self) -> usize {
        self.ids.len()
    }

    /// When memory `i` was last changed, then when it was stored: of two,
    /// the greater is the more recent. Bytes compare as the text does.
    pub(in crate::memory) fn recency(&self, i: usize) -> (&[u8], i64) {
        let start = if i == 0 { 0 } else { self.stamp_ends[i - 1] };
        (&self.stamps[start..self.stamp_ends[i]], self.orders[i])
    }

    /// The memory with key `key`, when it is covered and the candidates
    /// were read from the index, which reads them in key order.
    fn at_key(&self, key: i64) -> Option<usize> {
        self.orders.binary_search(&key).ok()
    }
}

/// What a row holds of a search's topic: query stem `t` at the strongest
/// field holding it there, or query anchor `a`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::memory) enum Hit {
    Term(usize, Field),
    Anchor(usize),
}

/// For each memory of `covered`, read from a [`ready`] index, and each row
/// holding one of the stemmed `query` terms or the `anchors`, calls
/// `held(memory, slot, hit)`. The index gives only the rows holding each
/// term, so a word most memories hold costs one lookup per row.
pub(in crate::memory) fn hits(
    conn: &Connection,
    covered: &Candidates,
    query: &[String],
    anchors: &[String],
    mut held: impl FnMut(usize, i64, Hit),
) -> Result<()> {
    let mut matching =
        conn.prepare_cached("SELECT rowid FROM search_words WHERE search_words MATCH ?1")?;
    let columns = Field::ALL
        .into_iter()
        .flat_map(|field| {
            query
                .iter()
                .enumerate()
                .map(move |(t, term)| (field.column(), token(term), Hit::Term(t, field)))
        })
        .chain(
            anchors
                .iter()
                .enumerate()
                .map(|(a, anchor)| ("anchors", token(anchor), Hit::Anchor(a))),
        );
    for (column, token, hit) in columns {
        let mut rows = matching.query([format!("{column} : \"{token}\"")])?;
        while let Some(r) = rows.next()? {
            let number: i64 = r.get(0)?;
            if let Some(i) = covered.at_key(number.div_euclid(SLOTS)) {
                held(i, number.rem_euclid(SLOTS), hit);
            }
        }
    }
    Ok(())
}

/// [`hits`] for memory `i` from its `rows`, for a search without the
/// index: the same calls, for the same rows, as the index makes.
pub(in crate::memory) fn row_hits(
    rows: &[Row],
    i: usize,
    query: &[String],
    anchors: &[String],
    mut held: impl FnMut(usize, i64, Hit),
) {
    for row in rows {
        for (stem, field) in &row.stems {
            if let Ok(t) = query.binary_search(stem) {
                held(i, row.slot, Hit::Term(t, *field));
            }
        }
        for anchor in &row.anchors {
            if let Ok(a) = anchors.binary_search(anchor) {
                held(i, row.slot, Hit::Anchor(a));
            }
        }
    }
}

/// Reads every memory `filter` covers into `into`, without its text, in
/// key order, from a ready index.
pub(in crate::memory) fn covered(
    conn: &Connection,
    filter: &Filter,
    into: &mut Candidates,
) -> Result<()> {
    into.clear();
    let (condition, values) = condition(filter)?;
    // Keys are the rowids of `search_keys`: SQLite reads them in this order
    // without sorting.
    let mut statement = conn.prepare(&format!(
        "SELECT id,scope,updated_at,key FROM search_keys AS memories WHERE {condition} ORDER BY key"
    ))?;
    let mut rows = statement.query(params_from_iter(values))?;
    while let Some(r) = rows.next()? {
        into.push(
            Uuid::parse_str(r.get_ref(0)?.as_str()?)?,
            r.get_ref(1)?.as_str()? == "project",
            r.get_ref(2)?.as_str()?,
            r.get(3)?,
        );
    }
    Ok(())
}

/// What is wrong with a current keyword index: memories without a key,
/// keys without a memory or with columns that differ from it, an index
/// that keeps deleted words, and stored rows that differ from the
/// memories' words and passages. Empty for an index stamped by other code,
/// which search does not read, and while memories are queued, which the
/// next write indexes.
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
    if stored(conn)? && !secured(conn)? {
        found.push("search index: it keeps deleted words until its segments merge".to_owned());
    }
    let mut expected: HashMap<(i64, String), &'static str> = HashMap::new();
    let mut places: HashMap<i64, Option<(i64, i64)>> = HashMap::new();
    let mut stems = Stems::new();
    let mut memories = conn
        .prepare(
            "SELECT k.key,m.memory_type,m.category,m.title,m.content,m.context,m.reason,m.evidence FROM search_keys k JOIN memories m ON m.id=k.id",
        )?;
    let mut memories = memories.query([])?;
    while let Some(r) = memories.next()? {
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
        let (first, _) = row_numbers(key);
        for row in rows(
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
            let number = first + row.slot;
            for (stem, field) in &row.stems {
                expected.insert((number, token(stem)), field.column());
            }
            for anchor in &row.anchors {
                expected.insert((number, token(anchor)), "anchors");
            }
            places.insert(
                number,
                (row.slot != 0).then(|| (row.place.start as i64, row.place.len() as i64)),
            );
        }
    }
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS temp.search_entries USING fts5vocab(main,'search_words','instance');",
    )?;
    let mut differing = BTreeSet::new();
    let mut stored_entries = conn.prepare("SELECT doc,term,col FROM temp.search_entries")?;
    let mut stored_entries = stored_entries.query([])?;
    while let Some(r) = stored_entries.next()? {
        let (number, term, column): (i64, String, String) = (r.get(0)?, r.get(1)?, r.get(2)?);
        if expected.remove(&(number, term)) != Some(column.as_str()) {
            differing.insert(number.div_euclid(SLOTS));
        }
    }
    differing.extend(
        expected
            .into_keys()
            .map(|(number, _)| number.div_euclid(SLOTS)),
    );
    conn.execute_batch("DROP TABLE temp.search_entries;")?;
    let mut stored_places = conn.prepare("SELECT rowid,start,length FROM search_words")?;
    let mut stored_places = stored_places.query([])?;
    while let Some(r) = stored_places.next()? {
        let number: i64 = r.get(0)?;
        let place: Option<(i64, i64)> = match (r.get(1)?, r.get(2)?) {
            (Some(start), Some(length)) => Some((start, length)),
            _ => None,
        };
        if places.remove(&number) != Some(place) {
            differing.insert(number.div_euclid(SLOTS));
        }
    }
    differing.extend(places.into_keys().map(|number| number.div_euclid(SLOTS)));
    if !differing.is_empty() {
        found.push(format!(
            "search index: the stored words of {} memories differ from their text",
            differing.len()
        ));
    }
    Ok(found)
}

/// Memories one recall may embed. Embedding takes roughly 0.1 s per memory on
/// a local model, so a store without vectors would otherwise stall its first
/// semantic recall for minutes; the rest fill on later recalls or `reindex`.
const RECALL_EMBED_BUDGET: usize = 32;

/// Embeds and saves `todo`, memories with their document text, newest
/// first as given, and returns their vectors.
fn embed_and_save(
    conn: &Connection,
    config: &embed::Config,
    todo: &[(Uuid, String)],
) -> Result<Vec<Vec<f32>>> {
    let identity = config.identity();
    let texts: Vec<String> = todo.iter().map(|(_, text)| text.clone()).collect();
    let fresh = config.embed(&texts)?;
    let tx = write(conn)?;
    for ((id, source), vector) in todo.iter().zip(&fresh) {
        let vector = DraftVector {
            vector: vector.clone(),
            identity: (identity.0.to_owned(), identity.1),
            source: source.clone(),
        };
        save_vector(&tx, *id, &vector)?;
    }
    tx.commit()?;
    Ok(fresh)
}

/// Embeds every memory of `items` that lacks a current vector: one made by
/// this model, at this length, from the memory's present text. Returns how
/// many were embedded.
fn embed_missing(conn: &Connection, config: &embed::Config, items: &[Memory]) -> Result<usize> {
    let guard = vectors::Held::current(conn, config.identity())?;
    let held = guard.as_ref().expect("Held::current always holds vectors");
    let mut missing: Vec<&Memory> = items
        .iter()
        .filter(|m| held.length(m.id).is_none())
        .collect();
    drop(guard);
    missing.sort_by_key(|m| std::cmp::Reverse(m.sequence));
    let todo: Vec<(Uuid, String)> = missing
        .iter()
        .map(|m| (m.id, document(&Draft::from(*m))))
        .collect();
    if !todo.is_empty() {
        embed_and_save(conn, config, &todo)?;
    }
    Ok(todo.len())
}

/// For each candidate, in order, the cosine similarity between `query` and
/// its current vector, if it has one, written into `similar`, after
/// embedding up to [`RECALL_EMBED_BUDGET`] candidates that lack one, newest
/// first.
pub(super) fn similarities(
    conn: &Connection,
    config: &embed::Config,
    query: &str,
    candidates: &Candidates,
    similar: &mut Vec<Option<f32>>,
) -> Result<()> {
    let query = config.embed(&[config.query(query)])?.remove(0);
    let mut guard = vectors::Held::current(conn, config.identity())?;
    let held = guard.as_mut().expect("Held::current always holds vectors");
    let mut missing: Vec<usize> = (0..candidates.len())
        .filter(|&i| held.length(candidates.ids[i]).is_none())
        .collect();
    missing.sort_by_key(|&i| std::cmp::Reverse(candidates.orders[i]));
    missing.truncate(RECALL_EMBED_BUDGET);
    if !missing.is_empty() {
        let ids: Vec<String> = missing
            .iter()
            .map(|&i| candidates.ids[i].to_string())
            .collect();
        let mut texts: HashMap<Uuid, String> = conn
            .prepare(&format!(
                "{SELECT} WHERE id IN (SELECT value FROM json_each(?1))"
            ))?
            .query_map([serde_json::to_string(&ids)?], row)?
            .map(|m| m.map(|m| (m.id, document(&Draft::from(&m)))))
            .collect::<rusqlite::Result<_>>()?;
        // A memory forgotten since the search began has no text to embed.
        let todo: Vec<(Uuid, String)> = missing
            .iter()
            .filter_map(|&i| {
                let id = candidates.ids[i];
                texts.remove(&id).map(|text| (id, text))
            })
            .collect();
        for ((id, _), vector) in todo.iter().zip(embed_and_save(conn, config, &todo)?) {
            held.insert(*id, &vector);
        }
    }
    let pending = candidates
        .ids
        .iter()
        .filter(|id| held.length(**id).is_none())
        .count();
    if pending > 0 {
        embed::pending(pending);
    }
    if let Some(length) = candidates
        .ids
        .iter()
        .filter_map(|id| held.length(*id))
        .find(|&length| length != query.len())
    {
        bail!(
            "Embedding endpoint returned {length} dimensions for a memory but {} for the query; run user-memory reindex",
            query.len()
        );
    }
    held.cosines(&query, &candidates.ids, similar);
    Ok(())
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
        Some(config) => json!(embed_missing(&conn, config, &items)?),
        None => Value::Null,
    };
    Ok(json!({"reindexed":items.len(),"embedded":embedded}))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Slot 0 holds each word of the category, title, type, reason and
    // evidence at the strongest field holding it; a passage holds only the
    // words and anchors slot 0 does not, and the context is its own
    // passage in the last slot.
    #[test]
    fn a_memorys_rows_keep_each_word_once_at_its_strongest_field() {
        let mut stems = Stems::new();
        let rows = rows(
            &mut stems,
            Text {
                memory_type: "preference",
                category: "coding",
                title: "Releases need notes",
                content: "Write release notes in `CHANGELOG.md` for each tag",
                context: "When tagging",
                reason: "Users read `CHANGELOG.md` first",
                evidence: "",
            },
        );
        let slots: Vec<i64> = rows.iter().map(|row| row.slot).collect();
        assert_eq!(slots, [0, 1, CONTEXT_SLOT]);
        let field = |slot: usize, stem: &str| {
            rows[slot]
                .stems
                .iter()
                .find(|(s, _)| s == stem)
                .map(|(_, field)| *field)
        };
        // An alias of the category counts as the category.
        assert_eq!(field(0, "program"), Some(Field::Category));
        assert_eq!(field(0, "releas"), Some(Field::Title));
        assert_eq!(field(0, "prefer"), Some(Field::Type));
        assert_eq!(field(0, "user"), Some(Field::Body));
        assert_eq!(field(1, "write"), Some(Field::Body));
        assert_eq!(field(1, "releas"), None, "slot 0 holds it");
        assert_eq!(field(2, "tag"), Some(Field::Body));
        assert_eq!(rows[0].anchors, ["changelog.md"]);
        assert!(rows[1].anchors.is_empty(), "slot 0 holds it");
        assert_eq!(rows[1].place, 0..50);
        assert_eq!(rows[2].place, 0..12);
    }

    // The triggers write the row numbering out in SQL.
    #[test]
    fn the_triggers_number_rows_as_the_index_does() {
        assert_eq!(SEARCH.matches(&format!("key*{SLOTS}")).count(), 4);
        assert_eq!(
            SEARCH
                .matches(&format!("key*{SLOTS}+{}", SLOTS - 1))
                .count(),
            2
        );
    }

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
        let id = "e0000001-0000-4000-8000-000000000001";
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

#[cfg(test)]
mod held_tests {
    use super::{vectors::Held, *};
    use crate::memory::similar::{cosine, to_blob};

    /// A seeded xorshift generator.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
        fn vector(&mut self, length: usize) -> Vec<f32> {
            (0..length)
                .map(|_| (self.next() >> 40) as f32 / (1u64 << 24) as f32 - 0.5)
                .collect()
        }
    }

    const MODEL: (&str, i64) = ("test-model", 0);
    const OTHER: (&str, i64) = ("another-model", 0);
    const LENGTH: usize = 24;

    /// What search compared before the held vectors: each memory's stored
    /// vector, looked up by id, model, length and present text, by `cosine`.
    fn oracle(conn: &Connection, model: (&str, i64), query: &[f32]) -> HashMap<Uuid, u32> {
        let memories = conn
            .prepare(&format!("{SELECT} ORDER BY rowid"))
            .unwrap()
            .query_map([], row)
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        memories
            .iter()
            .filter_map(|m| {
                conn.query_row(
                    "SELECT vector FROM memory_vectors WHERE id=?1 AND model=?2 AND dimensions=?3 AND source=?4",
                    params![m.id.to_string(), model.0, model.1, document(&Draft::from(m))],
                    |r| r.get::<_, Vec<u8>>(0),
                )
                .optional()
                .unwrap()
                .map(|blob| {
                    let vector = crate::memory::similar::from_blob(&blob);
                    (m.id, vector)
                })
            })
            .filter(|(_, vector)| vector.len() == query.len())
            .map(|(id, vector)| (id, cosine(query, &vector).to_bits()))
            .collect()
    }

    fn held(conn: &Connection, model: (&str, i64), query: &[f32]) -> HashMap<Uuid, u32> {
        let ids: Vec<Uuid> = conn
            .prepare("SELECT id FROM memories")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|id| Uuid::parse_str(&id.unwrap()).unwrap())
            .collect();
        let mut guard = Held::current(conn, model).unwrap();
        let mut found = Vec::new();
        guard.as_mut().unwrap().cosines(query, &ids, &mut found);
        found
            .into_iter()
            .zip(&ids)
            .filter_map(|(cosine, id)| cosine.map(|cosine| (*id, cosine.to_bits())))
            .collect()
    }

    fn insert(conn: &Connection, rng: &mut Rng, n: usize) -> Uuid {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&rng.next().to_le_bytes());
        bytes[8..].copy_from_slice(&rng.next().to_le_bytes());
        let id = uuid::Builder::from_random_bytes(bytes).into_uuid();
        conn.execute(
            "INSERT INTO memories (id,memory_type,category,title,content,context) VALUES (?1,'fact','tools',?2,?3,?4)",
            params![id.to_string(), format!("Memory {n}"), format!("Content {n}"), if n.is_multiple_of(3) { "When testing" } else { "" }],
        )
        .unwrap();
        id
    }

    fn save(conn: &Connection, id: Uuid, model: (&str, i64), source: &str, vector: &[f32]) {
        conn.execute(
            "INSERT OR REPLACE INTO memory_vectors (id,model,dimensions,source,vector) VALUES (?1,?2,?3,?4,?5)",
            params![id.to_string(), model.0, model.1, source, to_blob(vector)],
        )
        .unwrap();
    }

    fn current_source(conn: &Connection, id: Uuid) -> String {
        let m = conn
            .query_row(&format!("{SELECT} WHERE id=?1"), [id.to_string()], row)
            .unwrap();
        document(&Draft::from(&m))
    }

    #[test]
    fn held_vectors_answer_as_the_store_after_every_kind_of_change() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memories.sqlite3");
        let conn = connect(&db).unwrap();
        // Another process's writes reach the log the same way.
        let other = connect(&db).unwrap();
        let mut rng = Rng(0x00c4_c4e5);
        let mut ids = Vec::new();
        for n in 0..40 {
            let id = insert(&conn, &mut rng, n);
            match n % 5 {
                0 => {}
                1 => save(&conn, id, MODEL, "an older text", &rng.vector(LENGTH)),
                2 => save(
                    &conn,
                    id,
                    OTHER,
                    &current_source(&conn, id),
                    &rng.vector(LENGTH),
                ),
                _ => save(
                    &conn,
                    id,
                    MODEL,
                    &current_source(&conn, id),
                    &rng.vector(LENGTH),
                ),
            }
            ids.push(id);
        }
        for round in 0..40 {
            let query = rng.vector(LENGTH);
            assert_eq!(
                held(&conn, MODEL, &query),
                oracle(&conn, MODEL, &query),
                "round {round}"
            );
            let writer = if round % 2 == 0 { &conn } else { &other };
            let id = ids[rng.below(ids.len())];
            match rng.below(7) {
                // The memory's text changes, so its vector is no longer current.
                0 => {
                    writer
                        .execute(
                            "UPDATE memories SET content=content||' more' WHERE id=?1",
                            [id.to_string()],
                        )
                        .unwrap();
                }
                // A current vector is saved or replaced.
                1 | 2 => {
                    let source = current_source(writer, id);
                    save(writer, id, MODEL, &source, &rng.vector(LENGTH));
                }
                // The memory is forgotten, with its vector.
                3 => {
                    writer
                        .execute("DELETE FROM memories WHERE id=?1", [id.to_string()])
                        .unwrap();
                    ids.retain(|kept| *kept != id);
                }
                // A new memory arrives, with its vector.
                4 => {
                    let new = insert(writer, &mut rng, 100 + round);
                    let source = current_source(writer, new);
                    save(writer, new, MODEL, &source, &rng.vector(LENGTH));
                    ids.push(new);
                }
                // Another model's vector changes; ours stay.
                5 => {
                    let source = current_source(writer, id);
                    save(writer, id, OTHER, &source, &rng.vector(LENGTH));
                }
                // A title change keeps the vector current.
                _ => {
                    writer
                        .execute(
                            "UPDATE memories SET title=title||'!' WHERE id=?1",
                            [id.to_string()],
                        )
                        .unwrap();
                }
            }
            if round == 25 {
                // A change, then more than the log keeps: the change is
                // pruned, and the held vectors must reload to see it.
                let source = current_source(writer, ids[0]);
                save(writer, ids[0], MODEL, &source, &rng.vector(LENGTH));
                writer
                    .execute(
                        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < ?1) INSERT INTO search_changes(id) SELECT 'noise' FROM n",
                        [CHANGES_KEPT + 1],
                    )
                    .unwrap();
                write(writer).unwrap().commit().unwrap();
            }
            if round == 32 {
                // A store whose log starts again, as one restored from a
                // backup under a running server: the held vectors reload.
                writer
                    .execute_batch(
                        "DELETE FROM search_changes; DELETE FROM sqlite_sequence WHERE name='search_changes';",
                    )
                    .unwrap();
                let source = current_source(writer, ids[1]);
                save(writer, ids[1], MODEL, &source, &rng.vector(LENGTH));
            }
        }
        // Another model's vectors are held in their place.
        let query = rng.vector(LENGTH);
        assert_eq!(held(&conn, OTHER, &query), oracle(&conn, OTHER, &query));
        // And another store's.
        let elsewhere = tempfile::tempdir().unwrap();
        let second = connect(&elsewhere.path().join("memories.sqlite3")).unwrap();
        let id = insert(&second, &mut rng, 1);
        save(
            &second,
            id,
            MODEL,
            &current_source(&second, id),
            &rng.vector(LENGTH),
        );
        assert_eq!(held(&second, MODEL, &query), oracle(&second, MODEL, &query));
        assert_eq!(held(&conn, MODEL, &query), oracle(&conn, MODEL, &query));
    }

    #[test]
    fn a_vector_of_another_length_is_held_and_reported_not_compared() {
        let dir = tempfile::tempdir().unwrap();
        let conn = connect(&dir.path().join("memories.sqlite3")).unwrap();
        let mut rng = Rng(0x0dd);
        let (a, b) = (insert(&conn, &mut rng, 1), insert(&conn, &mut rng, 2));
        save(
            &conn,
            a,
            MODEL,
            &current_source(&conn, a),
            &rng.vector(LENGTH),
        );
        save(
            &conn,
            b,
            MODEL,
            &current_source(&conn, b),
            &rng.vector(LENGTH + 1),
        );
        let mut guard = Held::current(&conn, MODEL).unwrap();
        let held = guard.as_mut().unwrap();
        assert_eq!(held.length(a), Some(LENGTH));
        assert_eq!(held.length(b), Some(LENGTH + 1));
        let mut found = Vec::new();
        held.cosines(&rng.vector(LENGTH), &[a, b], &mut found);
        assert!(found[0].is_some() && found[1].is_none());
    }
}
