//! The delivery index: a read-only copy of the keyword index's rows for one
//! scope (stems, anchors and each passage's place, no sentence of any
//! memory), written to a local file and looked up there without opening the
//! store (issue #117, measured by #125). `super::matcher` ranks memories'
//! passages against it.
//!
//! No hook builds it yet, so the shipped binary declares this module for
//! its tests only. The delivery benchmark's binary (`user-memory-bench`,
//! evals/delivery/entry.rs) compiles it through a `#[path]` mirror of
//! `src/memory`: use only std, rusqlite and the mirrored modules here.

use crate::memory::index::{
    passages::SLOTS,
    stems::{Field, stamp, token},
};
use rusqlite::{Connection, OpenFlags, params_from_iter};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    path::Path,
};

/// How the index keys a term: a stem or an anchor, as its hex token.
pub(in crate::memory) fn key(anchor: bool, term: &str) -> Vec<u8> {
    let mut key = vec![if anchor { b'a' } else { b's' }];
    key.extend(token(term).into_bytes());
    key
}

/// The file the delivery index is written to starts with this, its format
/// in the last byte.
const MAGIC: &[u8; 8] = b"UMDELIV1";

/// A memory id as the store writes it: a hyphenated UUID.
const ID_BYTES: usize = 36;

/// Bytes of one row, one directory entry and one posting in the file.
pub(in crate::memory) const ROW_BYTES: usize = 14;
const ENTRY_BYTES: usize = 20;
pub(in crate::memory) const POSTING_BYTES: usize = 5;

/// Why a delivery index can't be built or read.
#[derive(Debug)]
pub(crate) enum Error {
    Store(rusqlite::Error),
    File(std::io::Error),
    /// The store's keyword index is missing, stale or has memories queued.
    NotCurrent,
    /// The file isn't a delivery index this code wrote, or was cut short.
    Format,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Store(e) => write!(f, "the store: {e}"),
            Error::File(e) => write!(f, "the delivery index file: {e}"),
            Error::NotCurrent => write!(
                f,
                "the store's keyword index isn't current for this code; run `user-memory reindex`"
            ),
            Error::Format => write!(f, "the delivery index file is not one this code wrote"),
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Store(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::File(e)
    }
}

/// One row of the keyword index, as the delivery index keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::memory) struct Row {
    pub(in crate::memory) memory: u32,
    pub(in crate::memory) slot: u16,
    pub(in crate::memory) start: u32,
    pub(in crate::memory) length: u32,
}

/// One memory's rows for building: its slot 0 and passages, each with its
/// stems by field and its anchors.
pub(crate) struct Source {
    pub(in crate::memory) id: String,
    pub(in crate::memory) rows: Vec<SourceRow>,
}

/// One row of a memory to index (see `index::rows`).
pub(in crate::memory) struct SourceRow {
    pub(in crate::memory) slot: i64,
    pub(in crate::memory) start: usize,
    pub(in crate::memory) length: usize,
    pub(in crate::memory) stems: Vec<(String, Field)>,
    pub(in crate::memory) anchors: Vec<String>,
}

/// One term's rows while the index is built: each row with the field
/// holding the term there, and the memories holding it.
type Postings = (Vec<(u32, u8)>, BTreeSet<u32>);

/// The delivery index: the bytes of its file, read in place.
pub(crate) struct DeliveryIndex {
    pub(in crate::memory) bytes: Vec<u8>,
    pub(in crate::memory) memories: usize,
    pub(in crate::memory) ids_at: usize,
    pub(in crate::memory) rows: usize,
    pub(in crate::memory) rows_at: usize,
    pub(in crate::memory) entries: usize,
    pub(in crate::memory) entries_at: usize,
    pub(in crate::memory) pool_at: usize,
    pub(in crate::memory) postings_at: usize,
}

pub(in crate::memory) fn u32_at(bytes: &[u8], at: usize) -> Result<u32, Error> {
    bytes
        .get(at..at + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or(Error::Format)
}

fn put(bytes: &mut Vec<u8>, value: usize) -> Result<(), Error> {
    bytes.extend(
        u32::try_from(value)
            .map_err(|_| Error::Format)?
            .to_le_bytes(),
    );
    Ok(())
}

impl DeliveryIndex {
    /// The file's bytes for `memories`: their ids, their rows, and each
    /// term's rows sorted by its key, so a lookup reads only its terms.
    pub(crate) fn write(memories: &[Source]) -> Result<Vec<u8>, Error> {
        let mut rows: Vec<Row> = Vec::new();
        // Each term's rows with the field holding it, and the memories
        // holding it.
        let mut postings: BTreeMap<Vec<u8>, Postings> = BTreeMap::new();
        for (m, memory) in memories.iter().enumerate() {
            let m = u32::try_from(m).map_err(|_| Error::Format)?;
            for row in &memory.rows {
                let r = u32::try_from(rows.len()).map_err(|_| Error::Format)?;
                rows.push(Row {
                    memory: m,
                    slot: u16::try_from(row.slot).map_err(|_| Error::Format)?,
                    start: u32::try_from(row.start).map_err(|_| Error::Format)?,
                    length: u32::try_from(row.length).map_err(|_| Error::Format)?,
                });
                let terms = row
                    .stems
                    .iter()
                    .map(|(stem, field)| (key(false, stem), *field as u8))
                    .chain(
                        row.anchors
                            .iter()
                            .map(|anchor| (key(true, anchor), Field::Body as u8)),
                    );
                for (key, field) in terms {
                    let (list, holders) = postings.entry(key).or_default();
                    list.push((r, field));
                    holders.insert(m);
                }
            }
        }
        let mut bytes = MAGIC.to_vec();
        let stamp = stamp();
        put(&mut bytes, stamp.len())?;
        bytes.extend(stamp.as_bytes());
        put(&mut bytes, memories.len())?;
        for memory in memories {
            let mut id = memory.id.clone().into_bytes();
            if id.len() != ID_BYTES {
                return Err(Error::Format);
            }
            bytes.append(&mut id);
        }
        put(&mut bytes, rows.len())?;
        for row in &rows {
            bytes.extend(row.memory.to_le_bytes());
            bytes.extend(row.slot.to_le_bytes());
            bytes.extend(row.start.to_le_bytes());
            bytes.extend(row.length.to_le_bytes());
        }
        put(&mut bytes, postings.len())?;
        let mut pool: Vec<u8> = Vec::new();
        let mut lists: Vec<u8> = Vec::new();
        for (key, (list, holders)) in &postings {
            put(&mut bytes, pool.len())?;
            put(&mut bytes, key.len())?;
            put(&mut bytes, holders.len())?;
            put(&mut bytes, lists.len() / POSTING_BYTES)?;
            put(&mut bytes, list.len())?;
            pool.extend(key);
            for (row, field) in list {
                lists.extend(row.to_le_bytes());
                lists.push(*field);
            }
        }
        put(&mut bytes, pool.len())?;
        bytes.extend(pool);
        bytes.extend(lists);
        Ok(bytes)
    }

    /// Reads the index from `bytes`, checking its sections fit, without
    /// copying them.
    pub(crate) fn read(bytes: Vec<u8>) -> Result<Self, Error> {
        if !bytes.starts_with(MAGIC) {
            return Err(Error::Format);
        }
        let mut at = MAGIC.len();
        let stamp_len = u32_at(&bytes, at)? as usize;
        at += 4;
        if bytes.get(at..at + stamp_len) != Some(stamp().as_bytes()) {
            return Err(Error::Format);
        }
        at += stamp_len;
        let memories = u32_at(&bytes, at)? as usize;
        let ids_at = at + 4;
        at = ids_at + memories * ID_BYTES;
        let rows = u32_at(&bytes, at)? as usize;
        let rows_at = at + 4;
        at = rows_at + rows * ROW_BYTES;
        let entries = u32_at(&bytes, at)? as usize;
        let entries_at = at + 4;
        at = entries_at + entries * ENTRY_BYTES;
        let pool_len = u32_at(&bytes, at)? as usize;
        let pool_at = at + 4;
        let postings_at = pool_at + pool_len;
        let index = Self {
            bytes,
            memories,
            ids_at,
            rows,
            rows_at,
            entries,
            entries_at,
            pool_at,
            postings_at,
        };
        if entries > 0 {
            let (_, _, first, count) = index.entry(entries - 1)?;
            if index.bytes.len() < postings_at + (first + count) * POSTING_BYTES {
                return Err(Error::Format);
            }
        } else if index.bytes.len() < postings_at {
            return Err(Error::Format);
        }
        Ok(index)
    }

    /// Reads the index from its file at `path`.
    pub(crate) fn load(path: &Path) -> Result<Self, Error> {
        Self::read(fs::read(path)?)
    }

    /// How many memories it holds.
    pub(crate) fn len(&self) -> usize {
        self.memories
    }

    /// The id of memory `m`; a memory past the count is a damaged file.
    pub(crate) fn id(&self, m: usize) -> Result<&str, Error> {
        if m >= self.memories {
            return Err(Error::Format);
        }
        let at = self.ids_at + m * ID_BYTES;
        self.bytes
            .get(at..at + ID_BYTES)
            .and_then(|id| std::str::from_utf8(id).ok())
            .ok_or(Error::Format)
    }

    /// Row `r`, checked where it is read rather than in a pass over every
    /// row at load, which a hook's time can't afford: a row past the count,
    /// or naming a memory past it, is a damaged file.
    pub(in crate::memory) fn row(&self, r: usize) -> Result<Row, Error> {
        if r >= self.rows {
            return Err(Error::Format);
        }
        let at = self.rows_at + r * ROW_BYTES;
        let slot = self
            .bytes
            .get(at + 4..at + 6)
            .and_then(|b| b.try_into().ok())
            .map(u16::from_le_bytes)
            .ok_or(Error::Format)?;
        let memory = u32_at(&self.bytes, at)?;
        if memory as usize >= self.memories {
            return Err(Error::Format);
        }
        Ok(Row {
            memory,
            slot,
            start: u32_at(&self.bytes, at + 6)?,
            length: u32_at(&self.bytes, at + 10)?,
        })
    }

    /// Entry `e` of the directory: its key, how many memories hold it, and
    /// its first posting and their count.
    pub(in crate::memory) fn entry(&self, e: usize) -> Result<(&[u8], usize, usize, usize), Error> {
        let at = self.entries_at + e * ENTRY_BYTES;
        let key_at = self.pool_at + u32_at(&self.bytes, at)? as usize;
        let key_len = u32_at(&self.bytes, at + 4)? as usize;
        let key = self
            .bytes
            .get(key_at..key_at + key_len)
            .ok_or(Error::Format)?;
        Ok((
            key,
            u32_at(&self.bytes, at + 8)? as usize,
            u32_at(&self.bytes, at + 12)? as usize,
            u32_at(&self.bytes, at + 16)? as usize,
        ))
    }
}

/// The memories search may deliver in a scope: active ones of the global
/// scope and of `projects`, never a guess or an observation about the user
/// (inferred memories, types `behavior` and `pattern`).
const CANDIDATES: &str = "SELECT k.key,m.id FROM search_keys k JOIN memories m ON m.id=k.id \
     WHERE m.status IN ('confirmed','tentative') AND m.source!='inferred' \
     AND m.memory_type NOT IN ('behavior','pattern') \
     AND (m.scope='global' OR (m.scope='project' AND m.project IN (SELECT value FROM json_each(?1)))) \
     ORDER BY k.key";

/// The rows of the store's keyword index at `db` for the memories search
/// may deliver in the global scope and `projects`, read without writing.
pub(crate) fn sources(db: &Path, projects: &[String]) -> Result<Vec<Source>, Error> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let current: Option<String> = conn
        .query_row("SELECT stemmed_by FROM search_state WHERE id=1", [], |r| {
            r.get(0)
        })
        .ok();
    let queued: i64 = conn.query_row("SELECT count(*) FROM search_pending", [], |r| r.get(0))?;
    if current.as_deref() != Some(stamp().as_str()) || queued > 0 {
        return Err(Error::NotCurrent);
    }
    let scope = format!(
        "[{}]",
        projects
            .iter()
            .map(|p| format!("\"{}\"", p.replace('\\', "\\\\").replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut keys: BTreeMap<i64, usize> = BTreeMap::new();
    let mut sources = Vec::new();
    let mut statement = conn.prepare(CANDIDATES)?;
    let mut found = statement.query(params_from_iter([scope]))?;
    while let Some(r) = found.next()? {
        keys.insert(r.get(0)?, sources.len());
        sources.push(Source {
            id: r.get(1)?,
            rows: Vec::new(),
        });
    }
    let mut statement = conn.prepare(
        "SELECT rowid,category,title,type,body,anchors,start,length FROM search_words ORDER BY rowid",
    )?;
    let mut rows = statement.query([])?;
    while let Some(r) = rows.next()? {
        let number: i64 = r.get(0)?;
        let Some(&m) = keys.get(&number.div_euclid(SLOTS)) else {
            continue;
        };
        let mut stems = Vec::new();
        for (c, field) in Field::ALL.into_iter().enumerate() {
            let column: String = r.get(c + 1)?;
            stems.extend(column.split_whitespace().map(|t| (decode(t), field)));
        }
        let anchors: String = r.get(5)?;
        let start: Option<i64> = r.get(6)?;
        let length: Option<i64> = r.get(7)?;
        sources[m].rows.push(SourceRow {
            slot: number.rem_euclid(SLOTS),
            start: usize::try_from(start.unwrap_or(0)).unwrap_or(0),
            length: usize::try_from(length.unwrap_or(0)).unwrap_or(0),
            stems,
            anchors: anchors.split_whitespace().map(decode).collect(),
        });
    }
    Ok(sources)
}

/// The term a hex token stands for (see `stems::token`); a token that isn't
/// one stays as it is, and matches nothing a window holds.
fn decode(token: &str) -> String {
    let bytes: Option<Vec<u8>> = (0..token.len())
        .step_by(2)
        .map(|at| {
            token
                .get(at..at + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect();
    bytes
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_else(|| token.to_owned())
}
