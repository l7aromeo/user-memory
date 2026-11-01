//! Every current vector of one embedding model, held by the process side
//! by side, so a search compares the query with all of them in one pass,
//! with exactly the arithmetic of [`cosine`](crate::memory::similar::cosine).
//! A long-running server keeps them between searches and reloads only the
//! memories the store's change log names.

use crate::memory::{
    Scope,
    similar::{Draft, document},
};
use anyhow::Result;
use rusqlite::Connection;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Mutex, MutexGuard, PoisonError},
};
use uuid::Uuid;

#[cfg(test)]
use crate::memory::similar::cosine;

/// Vectors compared at once: one per SIMD lane of a 512-bit register, and
/// two passes of a 256-bit one. Each memory's sum still runs in the order
/// `cosine` adds, one dimension after another; the lanes span memories.
const LANES: usize = 16;

/// Vectors of one length, laid out in blocks of [`LANES`] memories, each
/// block dimension by dimension: dimension `d` of the memory in lane `l` of
/// block `b` is at `(b * length + d) * LANES + l`.
#[derive(Debug, Default)]
pub(in crate::memory) struct Matrix {
    length: usize,
    values: Vec<f32>,
    /// Each slot's norm, computed as `cosine` computes it.
    norms: Vec<f32>,
    slots: usize,
}

/// The norm `cosine` divides by, in its order of operations.
fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

impl Matrix {
    pub(in crate::memory) fn new(length: usize) -> Self {
        Self {
            length,
            ..Self::default()
        }
    }

    pub(in crate::memory) fn length(&self) -> usize {
        self.length
    }

    /// Adds `vector`, of this matrix's length, and returns its slot.
    pub(in crate::memory) fn push(&mut self, vector: &[f32]) -> usize {
        let slot = self.slots;
        self.slots += 1;
        if slot.is_multiple_of(LANES) {
            self.values
                .resize(self.values.len() + self.length * LANES, 0.0);
        }
        self.norms.push(0.0);
        self.set(slot, vector);
        slot
    }

    /// Replaces the vector in `slot`.
    pub(in crate::memory) fn set(&mut self, slot: usize, vector: &[f32]) {
        debug_assert_eq!(vector.len(), self.length);
        let (block, lane) = (slot / LANES, slot % LANES);
        let base = block * self.length * LANES + lane;
        for (d, value) in vector.iter().enumerate() {
            self.values[base + d * LANES] = *value;
        }
        self.norms[slot] = norm(vector);
    }

    /// The cosine similarity of `query` with the vector in every slot, each
    /// bit for bit what `cosine(query, vector)` returns.
    pub(in crate::memory) fn cosines(&self, query: &[f32]) -> Vec<f32> {
        debug_assert_eq!(query.len(), self.length);
        let query_norm = norm(query);
        let mut found = Vec::with_capacity(self.slots);
        for (block, values) in self.values.chunks_exact(self.length * LANES).enumerate() {
            // `Sum` for f32 starts from -0.0 and adds in order.
            let mut dots = [-0.0f32; LANES];
            for (q, column) in query.iter().zip(values.as_chunks::<LANES>().0) {
                for (dot, v) in dots.iter_mut().zip(column) {
                    *dot += q * v;
                }
            }
            let start = block * LANES;
            for (lane, dot) in dots.iter().enumerate().take(self.slots - start) {
                let scale = query_norm * self.norms[start + lane];
                found.push(if scale > 0.0 { dot / scale } else { 0.0 });
            }
        }
        found
    }
}

/// The vectors one process holds: those of one model, at one requested
/// length, that are current for their memory's text.
pub(in crate::memory) struct Held {
    db: PathBuf,
    identity: (String, i64),
    /// The last change in `search_changes` these vectors reflect.
    seen: i64,
    /// Made with the first vector, at its length.
    matrix: Option<Matrix>,
    slots: HashMap<Uuid, usize>,
    /// Current vectors of another length than the matrix's, by memory: a
    /// model at its default length may answer at more than one.
    odd: HashMap<Uuid, usize>,
    free: Vec<usize>,
}

/// The vectors this process holds, for the store and model it last
/// searched.
static HELD: Mutex<Option<Held>> = Mutex::new(None);

/// A memory's current vector and the text it must have been embedded from:
/// the stored `source` must equal `document` of the memory as it is now.
const CURRENT: &str = "SELECT m.id,m.category,m.memory_type,m.content,m.context,m.reason,m.evidence,v.source,v.vector FROM memory_vectors v JOIN memories m ON m.id=v.id WHERE v.model=?1 AND v.dimensions=?2";

impl Held {
    fn new(db: PathBuf, identity: (String, i64)) -> Self {
        Self {
            db,
            identity,
            seen: 0,
            matrix: None,
            slots: HashMap::new(),
            odd: HashMap::new(),
            free: Vec::new(),
        }
    }

    /// The vectors for the store behind `conn` and the model `identity`,
    /// brought up to date with the store: reloaded in full when they are
    /// another store's or model's or the log no longer reaches back to
    /// them, otherwise only the memories changed since.
    pub(in crate::memory) fn current(
        conn: &Connection,
        identity: (&str, i64),
    ) -> Result<MutexGuard<'static, Option<Self>>> {
        let mut held = HELD.lock().unwrap_or_else(PoisonError::into_inner);
        let db = PathBuf::from(conn.path().unwrap_or_default());
        // One snapshot for the log and the rows it describes.
        let snapshot = conn.unchecked_transaction()?;
        let (oldest, latest): (Option<i64>, Option<i64>) =
            snapshot.query_row("SELECT min(seq),max(seq) FROM search_changes", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        let latest = latest.unwrap_or(0);
        let reusable = held.as_ref().is_some_and(|h| {
            h.db == db
                && h.identity.0 == identity.0
                && h.identity.1 == identity.1
                && h.seen <= latest
                && oldest.is_none_or(|oldest| oldest <= h.seen + 1)
        });
        if !reusable {
            let mut fresh = Self::new(db, (identity.0.to_owned(), identity.1));
            fresh.load(&snapshot, None)?;
            fresh.seen = latest;
            *held = Some(fresh);
        } else if let Some(h) = held.as_mut()
            && h.seen < latest
        {
            let changed: Vec<String> = snapshot
                .prepare("SELECT DISTINCT id FROM search_changes WHERE seq > ?1")?
                .query_map([h.seen], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for id in &changed {
                if let Ok(id) = Uuid::parse_str(id) {
                    h.remove(id);
                }
            }
            h.load(&snapshot, Some(&changed))?;
            h.seen = latest;
        }
        drop(snapshot);
        Ok(held)
    }

    /// Adds the current vector of every memory, or of the memories `only`
    /// names.
    fn load(&mut self, conn: &Connection, only: Option<&[String]>) -> Result<()> {
        let sql = match only {
            None => CURRENT.to_owned(),
            Some(_) => format!("{CURRENT} AND v.id IN (SELECT value FROM json_each(?3))"),
        };
        let mut statement = conn.prepare(&sql)?;
        let mut rows = match only {
            None => statement.query(rusqlite::params![self.identity.0, self.identity.1])?,
            Some(ids) => statement.query(rusqlite::params![
                self.identity.0,
                self.identity.1,
                serde_json::to_string(ids)?
            ])?,
        };
        let mut vector = Vec::new();
        while let Some(r) = rows.next()? {
            let text: [String; 6] = [
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ];
            let [category, memory_type, content, context, reason, evidence] = &text;
            let draft = Draft {
                memory_type,
                category,
                content,
                context,
                reason,
                evidence,
                scope: Scope::Global,
                project: "",
            };
            if r.get_ref(7)?.as_str()? != document(&draft) {
                continue;
            }
            let Ok(id) = Uuid::parse_str(r.get_ref(0)?.as_str()?) else {
                continue;
            };
            vector.clear();
            vector.extend(
                r.get_ref(8)?
                    .as_blob()?
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_le_bytes(*b)),
            );
            self.insert(id, &vector);
        }
        Ok(())
    }

    /// Holds `vector` as the current one of memory `id`.
    pub(in crate::memory) fn insert(&mut self, id: Uuid, vector: &[f32]) {
        self.remove(id);
        let matrix = self.matrix.get_or_insert_with(|| Matrix::new(vector.len()));
        if vector.len() != matrix.length() {
            self.odd.insert(id, vector.len());
            return;
        }
        let slot = match self.free.pop() {
            Some(slot) => {
                matrix.set(slot, vector);
                slot
            }
            None => matrix.push(vector),
        };
        self.slots.insert(id, slot);
    }

    fn remove(&mut self, id: Uuid) {
        self.odd.remove(&id);
        // A freed slot is never read: no memory maps to it until a vector
        // overwrites it.
        if let Some(slot) = self.slots.remove(&id) {
            self.free.push(slot);
        }
    }

    /// The length of memory `id`'s current vector, if it has one.
    pub(in crate::memory) fn length(&self, id: Uuid) -> Option<usize> {
        if let Some(length) = self.odd.get(&id) {
            return Some(*length);
        }
        self.slots
            .contains_key(&id)
            .then(|| self.matrix.as_ref().map_or(0, Matrix::length))
    }

    /// For each memory in `ids`, in order, the cosine similarity of `query`,
    /// of the matrix's length, with its current vector there, if it has one.
    pub(in crate::memory) fn cosines(&self, query: &[f32], ids: &[Uuid]) -> Vec<Option<f32>> {
        let Some(matrix) = &self.matrix else {
            return vec![None; ids.len()];
        };
        let all = matrix.cosines(query);
        ids.iter()
            .map(|id| self.slots.get(id).map(|&slot| all[slot]))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A seeded xorshift generator.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
        }
    }

    #[test]
    fn every_cosine_is_bit_for_bit_the_scalar_one() {
        let mut rng = Rng(0x00c0_51e5);
        for length in [1, 3, 16, 17, 768, 1024, 2560] {
            let vectors: Vec<Vec<f32>> = (0..37)
                .map(|i| match i {
                    0 => vec![0.0; length],
                    1 => vec![-0.0; length],
                    2 => (0..length).map(|_| rng.next() * 1e-30).collect(),
                    3 => (0..length).map(|_| rng.next() * 1e30).collect(),
                    _ => (0..length).map(|_| rng.next()).collect(),
                })
                .collect();
            let mut matrix = Matrix::new(length);
            for vector in &vectors {
                matrix.push(vector);
            }
            // Every product -0.0: the dot is -0.0 only when the sum starts
            // from -0.0, as `Sum` does, and the cosine keeps that sign.
            let mut signed = vec![-0.0; length];
            signed[0] = 1.0;
            let mut against = vec![1.0; length];
            against[0] = -0.0;
            let mut matrix_signed = Matrix::new(length);
            matrix_signed.push(&against);
            if length > 1 {
                assert_eq!(cosine(&signed, &against).to_bits(), (-0.0f32).to_bits());
            }
            assert_eq!(
                matrix_signed.cosines(&signed)[0].to_bits(),
                cosine(&signed, &against).to_bits()
            );
            for query in [
                (0..length).map(|_| rng.next()).collect::<Vec<f32>>(),
                vec![0.0; length],
                vectors[5].clone(),
            ] {
                let found = matrix.cosines(&query);
                assert_eq!(found.len(), vectors.len());
                for (vector, cosine_found) in vectors.iter().zip(&found) {
                    assert_eq!(
                        cosine_found.to_bits(),
                        cosine(&query, vector).to_bits(),
                        "length {length}"
                    );
                }
            }
            matrix.set(4, &vectors[7]);
            assert_eq!(
                matrix.cosines(&vectors[9])[4].to_bits(),
                cosine(&vectors[9], &vectors[7]).to_bits()
            );
        }
    }
}
