//! The delivery index: a read-only copy of the keyword index's rows for one
//! scope (stems, anchors and each passage's place, no sentence of any
//! memory), written to a local file and looked up there without opening the
//! store (issue #117, measured by #125). `super::matcher` ranks memories'
//! passages against it.
//!
//! The hooks keep it (`super::delivery_files`) and match on it. The
//! delivery benchmark's binary (`user-memory-bench`,
//! evals/delivery/entry.rs) compiles it through a `#[path]` mirror of
//! `src/memory`: use only std, rusqlite and the mirrored modules here.

use crate::memory::in_force::live;
use crate::memory::index::{
    passages::SLOTS,
    stems::{Field, stamp, token},
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params_from_iter};
use std::{collections::HashMap, fmt, path::Path};

/// The first byte of a term's key: an anchor's sorts before a stem's.
pub(in crate::memory) fn prefix(anchor: bool) -> u8 {
    if anchor { b'a' } else { b's' }
}

/// The file the delivery index is written to starts with this, its format
/// in the last byte: 2 added the header of [`Header`].
const MAGIC: &[u8; 8] = b"UMDELIV2";

/// The file format this code writes and reads, as a client names it to a
/// server (#180).
pub(crate) fn format() -> &'static str {
    // The magic is ASCII.
    std::str::from_utf8(MAGIC).unwrap_or_default()
}

/// The stamp of the stems this code makes, which a file's terms must have
/// been made with for a lookup to find them.
pub(crate) fn stems() -> String {
    stamp()
}

/// Which of a scope's two files an index is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Part {
    /// The scope's memories at one position of the store.
    Base,
    /// The memories changed since the base's position, and the base's
    /// memories they replace.
    Overlay,
}

impl Part {
    /// The byte the file names it by.
    fn byte(self) -> u8 {
        match self {
            Part::Base => b'B',
            Part::Overlay => b'O',
        }
    }
}

/// What a delivery file says of itself before its memories.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    pub(crate) part: Part,
    /// The store's position (#119) the file holds the memories of. It names
    /// the store by a digest of its id, never the id.
    pub(crate) position: String,
    /// The position of the base an overlay was made for; a base names its
    /// own.
    pub(crate) made_for: String,
    /// The scope's project keys, global memories being in every scope.
    pub(crate) projects: Vec<String>,
    /// The base's memories an overlay hides: each memory changed since the
    /// base's position, by id only. A base hides none.
    pub(crate) skip: Vec<String>,
}

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
    /// The store's keyword index is missing, stale or has memories queued,
    /// or the store counts no changes.
    NotCurrent,
    /// The store no longer says which memories changed since the base's
    /// position: it pruned the record of removals made before it.
    Pruned,
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
            Error::Pruned => write!(f, "the store no longer counts the changes since this base"),
        }
    }
}

impl std::error::Error for Error {}

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

/// The hasher of a build's maps, which hash every term and memory key in
/// the scope: with std's SipHash a build at 10,000 memories took 40 ms on
/// macOS, with this 37 ms. SipHash's resistance to chosen keys is not
/// needed here, as the keys are the user's own memories, hashed on their
/// machine. A fixed seed hashes alike on every run, so builds time alike;
/// the file never depends on a map's order.
type Fast = foldhash::fast::FixedState;

/// An index being built: its rows, each term's key kept once, and every
/// posting in the order met, which is row order. The file is written with
/// one counting sort of the postings by term: keying a map by term for
/// each posting took two thirds of a build.
#[derive(Default)]
struct Builder {
    rows: Vec<Row>,
    numbers: HashMap<Box<[u8]>, u32, Fast>,
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
        self.term_field(key, field as u8)
    }

    /// [`Builder::term`] with the field as its byte.
    fn term_field(&mut self, key: &[u8], field: u8) -> Result<(), Error> {
        let row = self.rows.len().checked_sub(1).ok_or(Error::Format)?;
        let term = self.number(key)?;
        self.posting(term, u32::try_from(row).map_err(|_| Error::Format)?, field)
    }

    /// The number of the term keyed `key`, given it when first met.
    fn number(&mut self, key: &[u8]) -> Result<u32, Error> {
        if let Some(&term) = self.numbers.get(key) {
            return Ok(term);
        }
        let term = u32::try_from(self.keys.len()).map_err(|_| Error::Format)?;
        self.keys.push(key.into());
        self.numbers.insert(key.into(), term);
        Ok(term)
    }

    /// Adds term number `term` to row `row`, held there in `field`. A term's
    /// postings go in row order, as the file keeps them.
    fn posting(&mut self, term: u32, row: u32, field: u8) -> Result<(), Error> {
        let memory = self.rows.get(row as usize).ok_or(Error::Format)?.memory;
        self.postings.push(Posting {
            term,
            row,
            field,
            memory,
        });
        Ok(())
    }

    /// The file's bytes for the memories `ids` names, in the order their
    /// rows name them: their ids, their rows, and each term's rows sorted by
    /// its key, so a lookup reads only its terms.
    fn bytes<'i>(
        self,
        header: &Header,
        ids: impl ExactSizeIterator<Item = &'i str>,
    ) -> Result<Vec<u8>, Error> {
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
        put_text(&mut bytes, &stamp())?;
        bytes.push(header.part.byte());
        put_text(&mut bytes, &header.position)?;
        put_text(&mut bytes, &header.made_for)?;
        put(&mut bytes, header.projects.len())?;
        for project in &header.projects {
            put_text(&mut bytes, project)?;
        }
        put(&mut bytes, header.skip.len())?;
        for id in &header.skip {
            if id.len() != ID_BYTES {
                return Err(Error::Format);
            }
            bytes.extend(id.as_bytes());
        }
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

/// The delivery index: the bytes of its file, read in place, and its
/// header.
pub(crate) struct DeliveryIndex {
    pub(crate) header: Header,
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

/// Writes `text` as its length and its bytes.
fn put_text(bytes: &mut Vec<u8>, text: &str) -> Result<(), Error> {
    put(bytes, text.len())?;
    bytes.extend(text.as_bytes());
    Ok(())
}

/// The text [`put_text`] wrote at `*at`, moving `*at` past it.
fn text_at<'b>(bytes: &'b [u8], at: &mut usize) -> Result<&'b str, Error> {
    let len = u32_at(bytes, *at)? as usize;
    let text = bytes
        .get(*at + 4..*at + 4 + len)
        .and_then(|text| std::str::from_utf8(text).ok())
        .ok_or(Error::Format)?;
    *at += 4 + len;
    Ok(text)
}

impl DeliveryIndex {
    /// A base file's bytes for `memories`, built from their terms as given:
    /// for tests, which make their memories by hand.
    #[cfg(test)]
    pub(crate) fn write(memories: &[Source]) -> Result<Vec<u8>, Error> {
        let header = Header {
            part: Part::Base,
            position: String::new(),
            made_for: String::new(),
            projects: Vec::new(),
            skip: Vec::new(),
        };
        Self::write_with(&header, memories)
    }

    /// [`DeliveryIndex::write`] with `header`.
    #[cfg(test)]
    pub(crate) fn write_with(header: &Header, memories: &[Source]) -> Result<Vec<u8>, Error> {
        let mut built = Builder::default();
        for (m, memory) in memories.iter().enumerate() {
            for row in &memory.rows {
                built.row(m, row.slot, row.start, row.length)?;
                for (stem, field) in &row.stems {
                    built.term(&super::matcher::key(false, stem), *field)?;
                }
                for anchor in &row.anchors {
                    built.term(&super::matcher::key(true, anchor), Field::Body)?;
                }
            }
        }
        built.bytes(header, memories.iter().map(|m| m.id.as_str()))
    }

    /// Reads the index from `bytes`, checking its sections fit, without
    /// copying them.
    pub(crate) fn read(bytes: Vec<u8>) -> Result<Self, Error> {
        if !bytes.starts_with(MAGIC) {
            return Err(Error::Format);
        }
        let mut at = MAGIC.len();
        if text_at(&bytes, &mut at)? != stamp() {
            return Err(Error::Format);
        }
        let part = match bytes.get(at) {
            Some(b'B') => Part::Base,
            Some(b'O') => Part::Overlay,
            _ => return Err(Error::Format),
        };
        at += 1;
        let position = text_at(&bytes, &mut at)?.to_owned();
        let made_for = text_at(&bytes, &mut at)?.to_owned();
        let count = u32_at(&bytes, at)? as usize;
        at += 4;
        let mut projects = Vec::new();
        for _ in 0..count {
            projects.push(text_at(&bytes, &mut at)?.to_owned());
        }
        let count = u32_at(&bytes, at)? as usize;
        at += 4;
        let mut skip = Vec::new();
        for _ in 0..count {
            let id = bytes
                .get(at..at + ID_BYTES)
                .and_then(|id| std::str::from_utf8(id).ok())
                .ok_or(Error::Format)?;
            skip.push(id.to_owned());
            at += ID_BYTES;
        }
        let header = Header {
            part,
            position,
            made_for,
            projects,
            skip,
        };
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
            header,
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

    /// The rows of each memory in `memories`, by memory, each with its terms
    /// (key and field) sorted: one pass over every posting, marking the
    /// rows sought, whatever their number.
    fn terms_of(&self, memories: &[usize]) -> Result<HashMap<usize, Vec<Terms>>, Error> {
        let mut sought = vec![None; self.rows];
        let mut found: HashMap<usize, Vec<Terms>> = HashMap::new();
        for &m in memories {
            // A memory's rows are contiguous, in memory order.
            let (mut low, mut high) = (0, self.rows);
            while low < high {
                let mid = (low + high) / 2;
                if (self.row(mid)?.memory as usize) < m {
                    low = mid + 1;
                } else {
                    high = mid;
                }
            }
            let rows = found.entry(m).or_default();
            let mut r = low;
            while r < self.rows && self.row(r)?.memory as usize == m {
                let row = self.row(r)?;
                sought[r] = Some((m, rows.len()));
                rows.push(Terms {
                    slot: i64::from(row.slot),
                    start: row.start,
                    length: row.length,
                    keys: Vec::new(),
                });
                r += 1;
            }
        }
        for e in 0..self.entries {
            let (key, _, first, count) = self.entry(e)?;
            for p in first..first + count {
                let at = self.postings_at + p * POSTING_BYTES;
                let row = u32_at(&self.bytes, at)? as usize;
                if let Some(Some((m, i))) = sought.get(row).copied() {
                    let field = *self.bytes.get(at + 4).ok_or(Error::Format)?;
                    if let Some(rows) = found.get_mut(&m) {
                        rows[i].keys.push((key.to_vec(), field));
                    }
                }
            }
        }
        for rows in found.values_mut() {
            for row in rows {
                row.keys.sort();
            }
        }
        Ok(found)
    }

    /// Every memory's id with its rows and their terms, by id: what two
    /// files hold, compared in tests whatever the memories' order.
    #[cfg(test)]
    pub(crate) fn contents(&self) -> Result<std::collections::BTreeMap<String, String>, Error> {
        let terms = self.terms_of(&(0..self.memories).collect::<Vec<_>>())?;
        (0..self.memories)
            .map(|m| Ok((self.id(m)?.to_owned(), format!("{:?}", terms.get(&m)))))
            .collect()
    }

    /// The position of memory `id` in the file, if it holds it.
    fn find(&self, ids: &HashMap<String, usize>, id: &str) -> Option<usize> {
        ids.get(id).copied()
    }

    /// Every memory's id, with its position in the file.
    fn ids(&self) -> Result<HashMap<String, usize>, Error> {
        (0..self.memories)
            .map(|m| Ok((self.id(m)?.to_owned(), m)))
            .collect()
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

/// One row of a memory with its terms, as a file holds them or as the store
/// holds them now: compared to tell a reworded memory.
#[derive(Debug, PartialEq, Eq)]
struct Terms {
    slot: i64,
    start: u32,
    length: u32,
    /// Each term's key and the field holding it there, sorted.
    keys: Vec<(Vec<u8>, u8)>,
}

/// An overlay file, and whether its read found the base holding words it
/// should not.
pub(crate) struct Overlay {
    pub(crate) bytes: Vec<u8>,
    /// The base holds a memory removed since its position, or one whose
    /// terms differ from the memory's current ones: a writer that knew
    /// nothing of these files, or one killed before it rewrote them, left
    /// words a removal took out. Such a base is deleted, never used.
    pub(crate) stale: bool,
}

/// The memories changed since a position that a scope can see, in key
/// order: [`CANDIDATES`] for the ids in `?2`.
const CHANGED_CANDIDATES: &str = concat!(
    "SELECT k.key,m.id FROM search_keys k JOIN memories m ON m.id=k.id WHERE ",
    live!("m"),
    " AND m.source!='inferred' \
     AND m.memory_type NOT IN ('behavior','pattern') \
     AND (m.scope='global' OR (m.scope='project' AND m.project IN (SELECT value FROM json_each(?1)))) \
     AND m.id IN (SELECT value FROM json_each(?2)) \
     ORDER BY k.key"
);

/// A JSON array of `items`, for `json_each`.
fn json_array(items: &[String]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|p| format!("\"{}\"", p.replace('\\', "\\\\").replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// The rows of the memory keyed `key` in the store's keyword index, with
/// their terms.
fn current_rows(conn: &Connection, key: i64) -> Result<Vec<Terms>, Error> {
    let mut statement = conn.prepare_cached(
        "SELECT rowid,category,title,type,body,anchors,start,length FROM search_words \
         WHERE rowid BETWEEN ?1 AND ?2 ORDER BY rowid",
    )?;
    let mut found = statement.query([key * SLOTS, key * SLOTS + SLOTS - 1])?;
    let (mut term, mut raw) = (Vec::new(), Vec::new());
    let mut rows = Vec::new();
    while let Some(r) = found.next()? {
        let number: i64 = r.get(0)?;
        let start: Option<i64> = r.get(6)?;
        let length: Option<i64> = r.get(7)?;
        let mut keys = Vec::new();
        for (c, field) in Field::ALL.into_iter().enumerate() {
            for token in column(r, c + 1)?.split_whitespace() {
                stored_key(false, token, &mut term, &mut raw);
                keys.push((term.clone(), field as u8));
            }
        }
        for token in column(r, 5)?.split_whitespace() {
            stored_key(true, token, &mut term, &mut raw);
            keys.push((term.clone(), Field::Body as u8));
        }
        rows.push(Terms {
            slot: number.rem_euclid(SLOTS),
            start: u32::try_from(start.unwrap_or(0)).map_err(|_| Error::Format)?,
            length: u32::try_from(length.unwrap_or(0)).map_err(|_| Error::Format)?,
            keys,
        });
    }
    Ok(rows)
}

/// The store's id, latest change and the change it no longer counts
/// before, read in `conn`'s transaction, after checking its keyword index
/// is current for this code with nothing queued.
fn current_changes(conn: &Connection) -> Result<(String, i64, i64), Error> {
    let state: (String, i64, i64) = conn
        .query_row(
            "SELECT store_id,last_change,pruned_through FROM store_changes WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(|_| Error::NotCurrent)?;
    let current: Option<String> = conn
        .query_row("SELECT stemmed_by FROM search_state WHERE id=1", [], |r| {
            r.get(0)
        })
        .ok();
    let queued: i64 = conn.query_row("SELECT count(*) FROM search_pending", [], |r| r.get(0))?;
    if current.as_deref() != Some(stamp().as_str()) || queued > 0 {
        return Err(Error::NotCurrent);
    }
    Ok(state)
}

/// Each memory changed after change `since`, in change order, with whether
/// it was removed.
fn changed_since(conn: &Connection, since: i64) -> Result<Vec<(String, bool)>, Error> {
    let mut changed = Vec::new();
    let mut statement = conn.prepare(
        "SELECT memory_id,removed_at IS NOT NULL FROM memory_changes WHERE change>?1 ORDER BY change",
    )?;
    let mut rows = statement.query([since])?;
    while let Some(r) = rows.next()? {
        changed.push((r.get(0)?, r.get(1)?));
    }
    Ok(changed)
}

/// The overlay file of the memories of `skip` that `projects` can see, each
/// as it is now, at most `limit` of them, with `skip` as its skip list.
/// Past `limit`: none when `whole`, else the file of the first `limit`.
fn overlay_file(
    conn: &Connection,
    projects: &[String],
    skip: Vec<String>,
    (limit, whole): (usize, bool),
    position: String,
    made_for: String,
) -> Result<Option<Vec<u8>>, Error> {
    let mut ids: Vec<String> = Vec::new();
    let mut built = Builder::default();
    let mut statement = conn.prepare(CHANGED_CANDIDATES)?;
    let mut found = statement.query(params_from_iter([json_array(projects), json_array(&skip)]))?;
    while let Some(r) = found.next()? {
        if ids.len() == limit {
            if whole {
                return Ok(None);
            }
            break;
        }
        let key: i64 = r.get(0)?;
        let m = ids.len();
        ids.push(r.get(1)?);
        for row in current_rows(conn, key)? {
            built.row(m, row.slot, row.start as usize, row.length as usize)?;
            for (term, field) in &row.keys {
                built.term_field(term, *field)?;
            }
        }
    }
    let header = Header {
        part: Part::Overlay,
        position,
        made_for,
        projects: projects.to_vec(),
        skip,
    };
    built
        .bytes(&header, ids.iter().map(String::as_str))
        .map(Some)
}

/// The overlay of `base` from the store at `db`, read without writing in
/// one transaction: the rows of at most `limit` memories the scope can see
/// that changed after the base's change `since`, each as it is now, and
/// every memory changed since in its skip list, so the base's older rows
/// of it are never used. A memory past `limit` is in the skip list alone,
/// and waits for the next base. With the position
/// `position` makes of the store's id and latest change, and whether the
/// base holds words a removal took out ([`Overlay::stale`]).
pub(crate) fn overlay(
    db: &Path,
    base: &DeliveryIndex,
    since: i64,
    limit: usize,
    position: impl FnOnce(&str, i64) -> String,
) -> Result<Overlay, Error> {
    let mut conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let conn = conn.transaction()?;
    let (store_id, change, pruned) = current_changes(&conn)?;
    if since < pruned {
        return Err(Error::Pruned);
    }
    let changed = changed_since(&conn, since)?;
    let skip: Vec<String> = changed.iter().map(|(id, _)| id.clone()).collect();
    let stale = !changed.is_empty() && holds_removed(&conn, base, &changed)?;
    let position = position(&store_id, change);
    let bytes = overlay_file(
        &conn,
        &base.header.projects,
        skip,
        (limit, false),
        position,
        base.header.position.clone(),
    )?
    .ok_or(Error::Format)?;
    Ok(Overlay { bytes, stale })
}

/// For a client of the server whose store is at `db` (#180): the overlay
/// of the memories `projects` can see that changed since the client's base
/// at `made_for`, read without writing in one transaction, each memory as
/// it is now and every changed one in its skip list; with the position
/// `position` makes of the store's id and latest change. `since` gives the
/// base's change from the store's id and latest change, none when the
/// base is of another store or past its latest change. None as well when
/// the store no longer counts the changes since, or more than `limit` of
/// them are seen: the client then takes a whole base. Whether the base
/// holds words the changes took out is the client's to tell
/// ([`removes`]), from the base it alone has.
pub(crate) fn delta(
    db: &Path,
    projects: &[String],
    made_for: &str,
    since: impl FnOnce(&str, i64) -> Option<i64>,
    limit: usize,
    position: impl FnOnce(&str, i64) -> String,
) -> Result<Option<Vec<u8>>, Error> {
    let mut conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let conn = conn.transaction()?;
    let (store_id, change, pruned) = current_changes(&conn)?;
    let Some(since) = since(&store_id, change).filter(|since| *since >= pruned) else {
        return Ok(None);
    };
    let skip = changed_since(&conn, since)?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    overlay_file(
        &conn,
        projects,
        skip,
        (limit, true),
        position(&store_id, change),
        made_for.to_owned(),
    )
}

/// Whether `overlay` takes words out of `base`, the client's own: `base`
/// holds a memory of its skip list that the overlay doesn't hold (removed,
/// or no longer in the scope), or holds with other terms (reworded). What
/// [`holds_removed`] tells for a local store, told from the overlay.
pub(crate) fn removes(base: &DeliveryIndex, overlay: &DeliveryIndex) -> Result<bool, Error> {
    let in_base = base.ids()?;
    let in_overlay = overlay.ids()?;
    let mut held = Vec::new();
    for id in &overlay.header.skip {
        let Some(m) = base.find(&in_base, id) else {
            continue;
        };
        let Some(o) = overlay.find(&in_overlay, id) else {
            return Ok(true);
        };
        held.push((m, o));
    }
    if held.is_empty() {
        return Ok(false);
    }
    let then = base.terms_of(&held.iter().map(|(m, _)| *m).collect::<Vec<_>>())?;
    let now = overlay.terms_of(&held.iter().map(|(_, o)| *o).collect::<Vec<_>>())?;
    Ok(held.iter().any(|(m, o)| then.get(m) != now.get(o)))
}

/// Adds to `built` the memories of `index` that `keep` keeps, their ids
/// to `ids`, and their rows: each row's number in the new file, none for a
/// row dropped.
fn renumber<'i>(
    index: &'i DeliveryIndex,
    ids: &mut Vec<&'i str>,
    built: &mut Builder,
    keep: &dyn Fn(&str) -> bool,
) -> Result<Vec<Option<u32>>, Error> {
    let mut memories = vec![None; index.len()];
    for (m, number) in memories.iter_mut().enumerate() {
        let id = index.id(m)?;
        if keep(id) {
            *number = Some(ids.len());
            ids.push(id);
        }
    }
    let mut rows = vec![None; index.rows];
    for (r, number) in rows.iter_mut().enumerate() {
        let row = index.row(r)?;
        if let Some(m) = memories[row.memory as usize] {
            *number = Some(u32::try_from(built.rows.len()).map_err(|_| Error::Format)?);
            built.row(
                m,
                i64::from(row.slot),
                row.start as usize,
                row.length as usize,
            )?;
        }
    }
    Ok(rows)
}

/// The base of `base` with `overlay`, made for it, folded in: the base's
/// memories but those the overlay's skip list names, then the overlay's,
/// at the overlay's position. The client of a server writes it in place of
/// a base whose words an overlay took out, so they leave its disk without
/// a download. One pass over each file's postings.
pub(crate) fn fold(base: &DeliveryIndex, overlay: &DeliveryIndex) -> Result<Vec<u8>, Error> {
    let skipped: std::collections::HashSet<&str, Fast> =
        overlay.header.skip.iter().map(String::as_str).collect();
    let mut ids: Vec<&str> = Vec::new();
    let mut built = Builder::default();
    let base_rows = renumber(base, &mut ids, &mut built, &|id| !skipped.contains(id))?;
    let overlay_rows = renumber(overlay, &mut ids, &mut built, &|_| true)?;
    for (index, rows) in [(base, &base_rows), (overlay, &overlay_rows)] {
        for e in 0..index.entries {
            let (key, _, first, count) = index.entry(e)?;
            let mut term = None;
            for p in first..first + count {
                let at = index.postings_at + p * POSTING_BYTES;
                let row = u32_at(&index.bytes, at)? as usize;
                let Some(Some(new)) = rows.get(row).copied() else {
                    continue;
                };
                let field = *index.bytes.get(at + 4).ok_or(Error::Format)?;
                let term = match term {
                    Some(term) => term,
                    None => *term.insert(built.number(key)?),
                };
                built.posting(term, new, field)?;
            }
        }
    }
    let header = Header {
        part: Part::Base,
        position: overlay.header.position.clone(),
        made_for: overlay.header.position.clone(),
        projects: base.header.projects.clone(),
        skip: Vec::new(),
    };
    built.bytes(&header, ids.into_iter())
}

/// Whether `base` holds a memory of `changed` (each id, and whether it was
/// removed) that was removed, or whose terms now differ from the base's.
fn holds_removed(
    conn: &Connection,
    base: &DeliveryIndex,
    changed: &[(String, bool)],
) -> Result<bool, Error> {
    let ids = base.ids()?;
    let mut held = Vec::new();
    for (id, removed) in changed {
        let Some(m) = base.find(&ids, id) else {
            continue;
        };
        if *removed {
            return Ok(true);
        }
        held.push((m, id));
    }
    if held.is_empty() {
        return Ok(false);
    }
    let then = base.terms_of(&held.iter().map(|(m, _)| *m).collect::<Vec<_>>())?;
    for (m, id) in held {
        let key: Option<i64> = conn
            .query_row("SELECT key FROM search_keys WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        let Some(key) = key else {
            return Ok(true);
        };
        let mut now = current_rows(conn, key)?;
        for row in &mut now {
            row.keys.sort();
        }
        if then.get(&m).map(Vec::as_slice) != Some(now.as_slice()) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The overlay of no change made for the base whose header is `base`: what
/// a writer puts beside a base it built.
pub(crate) fn empty_overlay(base: &Header) -> Result<Vec<u8>, Error> {
    let header = Header {
        part: Part::Overlay,
        position: base.position.clone(),
        made_for: base.position.clone(),
        projects: base.projects.clone(),
        skip: Vec::new(),
    };
    Builder::default().bytes(&header, std::iter::empty())
}

/// A scope's base and the overlay made for it, as a reader takes them: the
/// overlay's memories, and the base's but those the overlay's skip list
/// names.
pub(crate) struct View {
    pub(crate) base: DeliveryIndex,
    pub(crate) overlay: Option<DeliveryIndex>,
}

impl View {
    /// `base` with `overlay` when the overlay was made for it: an overlay
    /// made for another base, older or newer, is left out, so a reader
    /// that read between a writer's two renames never pairs them.
    pub(crate) fn new(base: DeliveryIndex, overlay: Option<DeliveryIndex>) -> Option<Self> {
        if base.header.part != Part::Base {
            return None;
        }
        let overlay = overlay.filter(|overlay| {
            overlay.header.part == Part::Overlay
                && overlay.header.made_for == base.header.position
                && overlay.header.projects == base.header.projects
        });
        Some(Self { base, overlay })
    }
}

/// The memories search may deliver in a scope: active ones of the global
/// scope and of `projects`, never a guess or an observation about the user
/// (inferred memories, types `behavior` and `pattern`).
pub(in crate::memory) const CANDIDATES: &str = concat!(
    "SELECT k.key,m.id FROM search_keys k JOIN memories m ON m.id=k.id WHERE ",
    live!("m"),
    " AND m.source!='inferred' \
     AND m.memory_type NOT IN ('behavior','pattern') \
     AND (m.scope='global' OR (m.scope='project' AND m.project IN (SELECT value FROM json_each(?1)))) \
     ORDER BY k.key"
);

/// The base file of the memories search may deliver in the global scope
/// and `projects`, from the store at `db`, read without writing, with the
/// position `position` makes of the store's id and latest change. One read
/// transaction, so the position is that of the rows. One pass over the
/// keyword index's rows: each stored token is already its term's key, so
/// no term is decoded and encoded again.
pub(crate) fn build(
    db: &Path,
    projects: &[String],
    position: impl FnOnce(&str, i64) -> String,
) -> Result<Vec<u8>, Error> {
    let mut conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let conn = conn.transaction()?;
    let (store_id, change): (String, i64) = conn
        .query_row(
            "SELECT store_id,last_change FROM store_changes WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| Error::NotCurrent)?;
    let position = position(&store_id, change);
    let current: Option<String> = conn
        .query_row("SELECT stemmed_by FROM search_state WHERE id=1", [], |r| {
            r.get(0)
        })
        .ok();
    let queued: i64 = conn.query_row("SELECT count(*) FROM search_pending", [], |r| r.get(0))?;
    if current.as_deref() != Some(stamp().as_str()) || queued > 0 {
        return Err(Error::NotCurrent);
    }
    let scope = json_array(projects);
    let mut keys: HashMap<i64, usize, Fast> = HashMap::default();
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
    let header = Header {
        part: Part::Base,
        made_for: position.clone(),
        position,
        projects: projects.to_vec(),
        skip: Vec::new(),
    };
    built.bytes(&header, ids.iter().map(String::as_str))
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
                assert_eq!(
                    stored,
                    crate::memory::matcher::key(anchor, &decode(token)),
                    "{token:?}"
                );
            }
        }
    }
}
