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
use std::{collections::HashMap, fmt, fs, path::Path};

/// How the index keys a term: a stem or an anchor, as its hex token.
pub(in crate::memory) fn key(anchor: bool, term: &str) -> Vec<u8> {
    let mut key = vec![prefix(anchor)];
    key.extend(token(term).into_bytes());
    key
}

/// The first byte of a term's key: an anchor's sorts before a stem's.
fn prefix(anchor: bool) -> u8 {
    if anchor { b'a' } else { b's' }
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
/// stems by field and its anchors. Tests make these by hand.
#[cfg(test)]
pub(crate) struct Source {
    pub(in crate::memory) id: String,
    pub(in crate::memory) rows: Vec<SourceRow>,
}

/// One row of a memory to index (see `index::rows`).
#[cfg(test)]
pub(in crate::memory) struct SourceRow {
    pub(in crate::memory) slot: i64,
    pub(in crate::memory) start: usize,
    pub(in crate::memory) length: usize,
    pub(in crate::memory) stems: Vec<(String, Field)>,
    pub(in crate::memory) anchors: Vec<String>,
}

/// One posting while an index is built: the term's number in the order
/// first met, the row, the field holding the term there, and the row's
/// memory.
#[derive(Clone, Copy, Default)]
struct Posting {
    term: u32,
    row: u32,
    field: u8,
    memory: u32,
}

/// An index being built: its rows, each term's key kept once, and every
/// posting in the order met, which is row order. The file is written with
/// one counting sort of the postings by term: keying a map by term for
/// each posting took two thirds of a build.
#[derive(Default)]
struct Builder {
    rows: Vec<Row>,
    numbers: HashMap<Box<[u8]>, u32>,
    keys: Vec<Box<[u8]>>,
    postings: Vec<Posting>,
}

impl Builder {
    /// Adds a row of memory `memory`; its terms follow, each with
    /// [`Builder::term`].
    fn row(&mut self, memory: usize, slot: i64, start: usize, length: usize) -> Result<(), Error> {
        self.rows.push(Row {
            memory: u32::try_from(memory).map_err(|_| Error::Format)?,
            slot: u16::try_from(slot).map_err(|_| Error::Format)?,
            start: u32::try_from(start).map_err(|_| Error::Format)?,
            length: u32::try_from(length).map_err(|_| Error::Format)?,
        });
        Ok(())
    }

    /// Adds the term keyed `key` to the last row, held there in `field`.
    fn term(&mut self, key: &[u8], field: Field) -> Result<(), Error> {
        let row = self.rows.len().checked_sub(1).ok_or(Error::Format)?;
        let memory = self.rows[row].memory;
        let term = match self.numbers.get(key) {
            Some(&term) => term,
            None => {
                let term = u32::try_from(self.keys.len()).map_err(|_| Error::Format)?;
                self.keys.push(key.into());
                self.numbers.insert(key.into(), term);
                term
            }
        };
        self.postings.push(Posting {
            term,
            row: u32::try_from(row).map_err(|_| Error::Format)?,
            field: field as u8,
            memory,
        });
        Ok(())
    }

    /// The file's bytes for the memories `ids` names, in the order their
    /// rows name them: their ids, their rows, and each term's rows sorted by
    /// its key, so a lookup reads only its terms.
    fn bytes<'i>(self, ids: impl ExactSizeIterator<Item = &'i str>) -> Result<Vec<u8>, Error> {
        let Self {
            rows,
            keys,
            postings,
            ..
        } = self;
        // Terms in key order, and each term's place in it.
        let mut order: Vec<usize> = (0..keys.len()).collect();
        order.sort_unstable_by(|&a, &b| keys[a].cmp(&keys[b]));
        let mut place = vec![0; keys.len()];
        for (at, &term) in order.iter().enumerate() {
            place[term] = at;
        }
        // Each term's postings, in the order met.
        let mut starts = vec![0; keys.len() + 1];
        for posting in &postings {
            starts[place[posting.term as usize] + 1] += 1;
        }
        for at in 0..keys.len() {
            starts[at + 1] += starts[at];
        }
        let mut sorted = vec![Posting::default(); postings.len()];
        let mut next = starts.clone();
        for posting in postings {
            let at = &mut next[place[posting.term as usize]];
            sorted[*at] = posting;
            *at += 1;
        }
        let mut bytes = MAGIC.to_vec();
        let stamp = stamp();
        put(&mut bytes, stamp.len())?;
        bytes.extend(stamp.as_bytes());
        put(&mut bytes, ids.len())?;
        for id in ids {
            if id.len() != ID_BYTES {
                return Err(Error::Format);
            }
            bytes.extend(id.as_bytes());
        }
        put(&mut bytes, rows.len())?;
        for row in &rows {
            bytes.extend(row.memory.to_le_bytes());
            bytes.extend(row.slot.to_le_bytes());
            bytes.extend(row.start.to_le_bytes());
            bytes.extend(row.length.to_le_bytes());
        }
        put(&mut bytes, keys.len())?;
        let mut pool: Vec<u8> = Vec::new();
        let mut lists: Vec<u8> = Vec::with_capacity(sorted.len() * POSTING_BYTES);
        for (at, &term) in order.iter().enumerate() {
            let list = &sorted[starts[at]..starts[at + 1]];
            // Rows are in memory order, so a term's holders are its runs.
            let holders = list
                .iter()
                .enumerate()
                .filter(|(i, p)| *i == 0 || list[i - 1].memory != p.memory)
                .count();
            put(&mut bytes, pool.len())?;
            put(&mut bytes, keys[term].len())?;
            put(&mut bytes, holders)?;
            put(&mut bytes, lists.len() / POSTING_BYTES)?;
            put(&mut bytes, list.len())?;
            pool.extend_from_slice(&keys[term]);
            for posting in list {
                lists.extend(posting.row.to_le_bytes());
                lists.push(posting.field);
            }
        }
        put(&mut bytes, pool.len())?;
        bytes.extend(pool);
        bytes.extend(lists);
        Ok(bytes)
    }
}

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
    /// The file's bytes for `memories`, built from their terms as given:
    /// for tests, which make their memories by hand.
    #[cfg(test)]
    pub(crate) fn write(memories: &[Source]) -> Result<Vec<u8>, Error> {
        let mut built = Builder::default();
        for (m, memory) in memories.iter().enumerate() {
            for row in &memory.rows {
                built.row(m, row.slot, row.start, row.length)?;
                for (stem, field) in &row.stems {
                    built.term(&key(false, stem), *field)?;
                }
                for anchor in &row.anchors {
                    built.term(&key(true, anchor), Field::Body)?;
                }
            }
        }
        built.bytes(memories.iter().map(|m| m.id.as_str()))
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
pub(in crate::memory) const CANDIDATES: &str = "SELECT k.key,m.id FROM search_keys k JOIN memories m ON m.id=k.id \
     WHERE m.status IN ('confirmed','tentative') AND m.source!='inferred' \
     AND m.memory_type NOT IN ('behavior','pattern') \
     AND (m.scope='global' OR (m.scope='project' AND m.project IN (SELECT value FROM json_each(?1)))) \
     ORDER BY k.key";

/// The delivery index file of the memories search may deliver in the
/// global scope and `projects`, from the store at `db`, read without
/// writing. One pass over the keyword index's rows: each stored token is
/// already its term's key, so no term is decoded and encoded again.
pub(crate) fn build(db: &Path, projects: &[String]) -> Result<Vec<u8>, Error> {
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
    let mut keys: HashMap<i64, usize> = HashMap::new();
    let mut ids: Vec<String> = Vec::new();
    let mut statement = conn.prepare(CANDIDATES)?;
    let mut found = statement.query(params_from_iter([scope]))?;
    while let Some(r) = found.next()? {
        keys.insert(r.get(0)?, ids.len());
        ids.push(r.get(1)?);
    }
    let mut built = Builder::default();
    let (mut key, mut raw) = (Vec::new(), Vec::new());
    let mut statement = conn.prepare(
        "SELECT rowid,category,title,type,body,anchors,start,length FROM search_words ORDER BY rowid",
    )?;
    let mut rows = statement.query([])?;
    while let Some(r) = rows.next()? {
        let number: i64 = r.get(0)?;
        let Some(&m) = keys.get(&number.div_euclid(SLOTS)) else {
            continue;
        };
        let start: Option<i64> = r.get(6)?;
        let length: Option<i64> = r.get(7)?;
        built.row(
            m,
            number.rem_euclid(SLOTS),
            usize::try_from(start.unwrap_or(0)).unwrap_or(0),
            usize::try_from(length.unwrap_or(0)).unwrap_or(0),
        )?;
        for (c, field) in Field::ALL.into_iter().enumerate() {
            for token in column(r, c + 1)?.split_whitespace() {
                stored_key(false, token, &mut key, &mut raw);
                built.term(&key, field)?;
            }
        }
        for token in column(r, 5)?.split_whitespace() {
            stored_key(true, token, &mut key, &mut raw);
            built.term(&key, Field::Body)?;
        }
    }
    built.bytes(ids.iter().map(String::as_str))
}

/// Text column `at` of `row` as stored, borrowed; empty when null.
fn column<'r>(row: &'r rusqlite::Row<'_>, at: usize) -> Result<&'r str, Error> {
    let value = row.get_ref(at)?;
    value
        .as_str_or_null()
        .map(Option::unwrap_or_default)
        .map_err(|e| {
            Error::Store(rusqlite::Error::FromSqlConversionFailure(
                at,
                value.data_type(),
                Box::new(e),
            ))
        })
}

/// Writes into `key` the key of the term the stored token `token` stands
/// for: the token itself after its prefix when it is the lowercase hex of
/// UTF-8, as `stems::token` writes every token, else the key of the term
/// [`decode`] makes of it, so the file is the same either way. `raw` is
/// scratch room for the decoded bytes.
pub(in crate::memory) fn stored_key(
    anchor: bool,
    token: &str,
    key: &mut Vec<u8>,
    raw: &mut Vec<u8>,
) {
    key.clear();
    key.push(prefix(anchor));
    raw.clear();
    let (pairs, odd) = token.as_bytes().as_chunks::<2>();
    let hex = odd.is_empty()
        && pairs
            .iter()
            .all(|&[high, low]| match (nibble(high), nibble(low)) {
                (Some(high), Some(low)) => {
                    raw.push(high << 4 | low);
                    true
                }
                _ => false,
            });
    if hex && std::str::from_utf8(raw).is_ok() {
        key.extend_from_slice(token.as_bytes());
    } else {
        key.extend(self::token(&decode(token)).into_bytes());
    }
}

/// The value of a lowercase hex digit.
fn nibble(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

/// The term a hex token stands for (see `stems::token`); a token that isn't
/// one stays as it is, and matches nothing a window holds.
pub(in crate::memory) fn decode(token: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A stored token keys its term as decoding and encoding it again would,
    /// whether or not it is the lowercase hex of UTF-8.
    #[test]
    fn a_stored_token_keys_its_term_as_the_term_itself_would() {
        let (mut stored, mut raw) = (Vec::new(), Vec::new());
        for token in [
            "6465706c6f79", // "deploy"
            "c3a9",         // "é"
            "e6bca2e5ad97", // "漢字"
            "",
            "6A",       // uppercase: decodes to "j", whose token is "6a"
            "abc",      // odd length: stays as it is
            "ff",       // not UTF-8: stays as it is
            "zz",       // not hex
            "64 65",    // never split like this, but not a token either
            "2f746d70", // "/tmp"
        ] {
            for anchor in [false, true] {
                stored_key(anchor, token, &mut stored, &mut raw);
                assert_eq!(stored, key(anchor, &decode(token)), "{token:?}");
            }
        }
    }
}
