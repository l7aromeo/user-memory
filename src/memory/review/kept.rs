//! Vector pairs kept in the store between reviews, so a review compares only
//! the vectors that changed since the last one instead of every pair.
//!
//! - `memory_pairs`: pairs of stored vectors of one model and length whose
//!   fast cosine is at or above that length's floor, the smaller id first.
//! - `memory_pairs_state`: the model, dimensions and lengths whose pairs are
//!   complete down to their floor: the lower cutoff minus the fast dot
//!   product's [`margin`] when they were compared.
//! - `memory_pairs_saved`: each vector's last save, numbered in the order
//!   saved, and whether a review has compared it since. One not compared
//!   is due.
//!
//! Triggers on `memory_vectors` keep them in step with every writer: a saved
//! vector loses its pairs and gets a new row, due; a deleted one loses both.
//! A `REPLACE` that removes the old vector fires no delete trigger without
//! `recursive_triggers`, so the insert trigger removes the old pairs. A
//! migration that rebuilds `memory_vectors` drops these triggers with it and
//! must create them again.
//!
//! A review marks a row compared and never deletes it, so a save after a
//! review's read stays visible to it however other reviews keep meanwhile:
//! a save removes the row it replaces, and nothing else does.
//!
//! A pair lives as long as both vectors, whatever their memories' status:
//! review reads only the pairs of memories it is looking at, and a memory
//! back in force, or a review of other projects, finds its pairs there.

use super::{
    super::{
        schema::write,
        similar::{Thresholds, norm},
    },
    pairs::{Row, margin, near, near_due},
};
use anyhow::Result;
use rusqlite::{Connection, params};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// The tables and triggers of the kept pairs. Part of the stored schema, so
/// a change to it bumps `SCHEMA_VERSION`.
pub(in crate::memory) const PAIRS: &str = "CREATE TABLE IF NOT EXISTS memory_pairs (
    a TEXT NOT NULL,
    b TEXT NOT NULL,
    model TEXT NOT NULL,
    dimensions INTEGER NOT NULL,
    PRIMARY KEY(a, b, model, dimensions),
    CHECK(a < b)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS memory_pairs_by_b ON memory_pairs(b);
CREATE TABLE IF NOT EXISTS memory_pairs_state (
    model TEXT NOT NULL,
    dimensions INTEGER NOT NULL,
    length INTEGER NOT NULL CHECK(length > 0),
    floor REAL NOT NULL,
    PRIMARY KEY(model, dimensions, length)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS memory_pairs_saved (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    model TEXT NOT NULL,
    dimensions INTEGER NOT NULL,
    compared INTEGER NOT NULL DEFAULT 0 CHECK(compared IN (0, 1))
);
CREATE INDEX IF NOT EXISTS memory_pairs_due ON memory_pairs_saved(model, dimensions) WHERE compared=0;
CREATE TRIGGER IF NOT EXISTS memory_pairs_vector_inserted AFTER INSERT ON memory_vectors BEGIN
    DELETE FROM memory_pairs WHERE a=NEW.id OR b=NEW.id;
    DELETE FROM memory_pairs_saved WHERE id=NEW.id;
    INSERT INTO memory_pairs_saved(id,model,dimensions) VALUES (NEW.id,NEW.model,NEW.dimensions);
END;
CREATE TRIGGER IF NOT EXISTS memory_pairs_vector_changed AFTER UPDATE ON memory_vectors BEGIN
    DELETE FROM memory_pairs WHERE a=OLD.id OR b=OLD.id OR a=NEW.id OR b=NEW.id;
    DELETE FROM memory_pairs_saved WHERE id=OLD.id OR id=NEW.id;
    INSERT INTO memory_pairs_saved(id,model,dimensions) VALUES (NEW.id,NEW.model,NEW.dimensions);
END;
CREATE TRIGGER IF NOT EXISTS memory_pairs_vector_deleted AFTER DELETE ON memory_vectors BEGIN
    DELETE FROM memory_pairs WHERE a=OLD.id OR b=OLD.id;
    DELETE FROM memory_pairs_saved WHERE id=OLD.id;
END;";

/// What the store keeps for one model and dimensions, read in the same
/// snapshot as the vectors. Empty for a store that keeps nothing yet.
#[derive(Default)]
pub(super) struct Kept {
    /// Floors by vector length, for the lengths whose pairs are complete.
    floors: HashMap<usize, f32>,
    pairs: Vec<(Uuid, Uuid)>,
    /// Due memories with the number of their last save.
    due: HashMap<Uuid, i64>,
    /// The highest save number when read: a memory with a higher one saved
    /// its vector since.
    last: i64,
}

/// The floors stamped for `identity`, by vector length.
fn floors(conn: &Connection, (model, dimensions): &(String, i64)) -> Result<HashMap<usize, f32>> {
    Ok(conn
        .prepare("SELECT length,floor FROM memory_pairs_state WHERE model=?1 AND dimensions=?2")?
        .query_map(params![model, dimensions], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
        })?
        .filter_map(|row| {
            let (length, floor) = row.ok()?;
            #[expect(clippy::cast_possible_truncation, reason = "stored from an f32")]
            let floor = floor as f32;
            Some((usize::try_from(length).ok()?, floor))
        })
        .collect())
}

/// Reads what the store keeps for `identity` (model, requested dimensions).
pub(super) fn read(conn: &Connection, identity: &(String, i64)) -> Result<Kept> {
    let (model, dimensions) = identity;
    let floors = floors(conn, identity)?;
    let pairs = conn
        .prepare("SELECT a,b FROM memory_pairs WHERE model=?1 AND dimensions=?2")?
        .query_map(params![model, dimensions], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .filter_map(|row| {
            let (a, b) = row.ok()?;
            Some((Uuid::parse_str(&a).ok()?, Uuid::parse_str(&b).ok()?))
        })
        .collect();
    let due = conn
        .prepare(
            "SELECT id,seq FROM memory_pairs_saved WHERE model=?1 AND dimensions=?2 AND compared=0",
        )?
        .query_map(params![model, dimensions], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?
        .filter_map(|row| {
            let (id, seq) = row.ok()?;
            Some((Uuid::parse_str(&id).ok()?, seq))
        })
        .collect();
    let last = conn.query_row(
        "SELECT coalesce(max(seq),0) FROM memory_pairs_saved",
        [],
        |r| r.get(0),
    )?;
    Ok(Kept {
        floors,
        pairs,
        due,
        last,
    })
}

/// The pairs worth an exact look, and what to keep of them.
pub(super) struct Refreshed {
    /// Every pair of stored vectors whose cosine may be at a cutoff.
    pub(super) near: Vec<(Uuid, Uuid)>,
    /// Pairs found now, to keep.
    found: Vec<(Uuid, Uuid)>,
    /// Lengths compared in full: their floor, and every memory of that
    /// length, whose kept pairs the found ones replace.
    replaced: Vec<(usize, f32, Vec<Uuid>)>,
    /// Lengths without cutoffs: nothing is kept for them.
    dropped: Vec<usize>,
    /// Due memories compared now, with the number each was noted under.
    compared: Vec<(Uuid, i64)>,
}

/// Every pair of `stored` vectors (by memory: the text embedded, the
/// vector) of a length with `cutoffs` whose fast cosine is within [`margin`]
/// of the lower cutoff or above. A length whose kept pairs are complete
/// down to that point, with at most half its vectors due, takes its kept
/// pairs and compares the due ones against every vector of that length:
/// k·n·d multiply-adds for k due of n vectors of d dimensions. Any other
/// length compares every pair, n²/2 · d: 3.2·10¹² at 50,000 of 2,560, 77 s
/// on GitHub's macOS runner (3 cpus, about 14·10⁹ a second on each), which
/// passes the connector's 150 s (`FORWARDED_CALL_LIMIT` in src/remote.rs)
/// there at about 70,000 vectors of 2,560 dimensions, or 100,000 of 1,024.
/// That happens once for a length, at the first review after the upgrade or
/// after the cutoffs were lowered.
pub(super) fn refresh(
    stored: &HashMap<Uuid, (String, Vec<f32>)>,
    cutoffs: &HashMap<usize, Thresholds>,
    kept: &Kept,
) -> Refreshed {
    let mut by_length: HashMap<usize, Vec<Uuid>> = HashMap::new();
    for (id, (_, vector)) in stored {
        by_length.entry(vector.len()).or_default().push(*id);
    }
    let mut refreshed = Refreshed {
        near: Vec::new(),
        found: Vec::new(),
        replaced: Vec::new(),
        dropped: Vec::new(),
        compared: Vec::new(),
    };
    for (length, mut ids) in by_length {
        ids.sort_unstable();
        refreshed.compared.extend(
            ids.iter()
                .filter_map(|id| kept.due.get(id).map(|&seq| (*id, seq))),
        );
        let Some(cutoffs) = cutoffs.get(&length) else {
            refreshed.dropped.push(length);
            continue;
        };
        let lowest = cutoffs.related.min(cutoffs.duplicate) - margin(length);
        let rows: Vec<Row> = ids
            .iter()
            .enumerate()
            .map(|(r, id)| {
                let vector = stored[id].1.as_slice();
                (r, vector, norm(vector))
            })
            .collect();
        let due: Vec<bool> = ids.iter().map(|id| kept.due.contains_key(id)).collect();
        let changed = due.iter().filter(|&&due| due).count();
        let pair = |(p, q): (usize, usize)| (ids[p], ids[q]);
        match kept.floors.get(&length) {
            Some(&floor) if floor <= lowest && 2 * changed <= ids.len() => {
                // A due vector's pairs come from comparing it now; any kept
                // for it, by a review that read after its save, are left out.
                let here: HashSet<&Uuid> = ids.iter().collect();
                let current = |id: &Uuid| here.contains(id) && !kept.due.contains_key(id);
                refreshed.near.extend(
                    kept.pairs
                        .iter()
                        .copied()
                        .filter(|(a, b)| current(a) && current(b)),
                );
                let found: Vec<(Uuid, Uuid)> =
                    near_due(&rows, &due, floor).into_iter().map(pair).collect();
                refreshed.near.extend(&found);
                refreshed.found.extend(found);
            }
            _ => {
                let found: Vec<(Uuid, Uuid)> = near(&rows, lowest).into_iter().map(pair).collect();
                refreshed.near.extend(&found);
                refreshed.found.extend(found);
                refreshed.replaced.push((length, lowest, ids));
            }
        }
    }
    refreshed
}

/// Keeps what `refreshed` found for `identity`, in one write within the
/// call's budget. A vector saved since `kept` was read, by any writer and
/// whatever another review kept meanwhile, is left as it is: its pairs are
/// neither forgotten, as another review's may be current, nor added from
/// the vector this review compared. Any pair of it there was written by a
/// review that read after that save, so it is current. Where this review
/// compared every pair of a length and stamps its floor, every vector saved
/// since is made due again: the review that kept its pairs may have had
/// other cutoffs, and the next one compares it at the floor stamped. Which
/// ids those are is read inside the same write, an immediate transaction,
/// so no other review keeps between that read and these writes.
///
/// Keeps nothing when a floor was stamped, changed or dropped since the
/// read: what this review compared, and which vectors it may mark
/// compared, assumed the floors it read, and the next review compares
/// again. This holds for the whole keep, not one length: the vectors of
/// one model and requested dimensions share a length, short of a model
/// that changed its output size. Fails, keeping nothing, when the write
/// turn cannot be had; the review's answer does not depend on it.
pub(super) fn keep(
    conn: &Connection,
    identity: &(String, i64),
    kept: &Kept,
    refreshed: &Refreshed,
) -> Result<()> {
    let (model, dimensions) = identity;
    if refreshed.found.is_empty()
        && refreshed.replaced.is_empty()
        && refreshed.dropped.is_empty()
        && refreshed.compared.is_empty()
    {
        return Ok(());
    }
    let tx = write(conn)?;
    if floors(&tx, identity)? != kept.floors {
        return Ok(());
    }
    let changed: HashSet<Uuid> = tx
        .prepare("SELECT id FROM memory_pairs_saved WHERE seq>?1")?
        .query_map([kept.last], |r| r.get::<_, String>(0))?
        .filter_map(|id| Uuid::parse_str(&id.ok()?).ok())
        .collect();
    {
        // An unchanged vector's pairs, but not those with a vector saved
        // since the read: another review may have kept them, current.
        let mut forget = tx.prepare(
            "DELETE FROM memory_pairs WHERE model=?1 AND dimensions=?2 AND (a=?3 OR b=?3) AND NOT EXISTS (SELECT 1 FROM memory_pairs_saved WHERE seq>?4 AND id IN (memory_pairs.a, memory_pairs.b))",
        )?;
        let mut stamp = tx.prepare(
            "INSERT OR REPLACE INTO memory_pairs_state (model,dimensions,length,floor) VALUES (?1,?2,?3,?4)",
        )?;
        for (length, floor, ids) in &refreshed.replaced {
            for id in ids {
                forget.execute(params![model, dimensions, id.to_string(), kept.last])?;
            }
            stamp.execute(params![
                model,
                dimensions,
                i64::try_from(*length)?,
                f64::from(*floor)
            ])?;
        }
        if !refreshed.replaced.is_empty() {
            // Their pairs were kept at another review's floor, maybe higher
            // than the one stamped here.
            tx.execute(
                "UPDATE memory_pairs_saved SET compared=0 WHERE model=?1 AND dimensions=?2 AND seq>?3",
                params![model, dimensions, kept.last],
            )?;
        }
        let mut drop = tx.prepare(
            "DELETE FROM memory_pairs_state WHERE model=?1 AND dimensions=?2 AND length=?3",
        )?;
        for length in &refreshed.dropped {
            drop.execute(params![model, dimensions, i64::try_from(*length)?])?;
        }
        // Only while both vectors are still the ones compared.
        let mut add = tx.prepare(
            "INSERT OR IGNORE INTO memory_pairs (a,b,model,dimensions) SELECT ?1,?2,?3,?4 WHERE EXISTS (SELECT 1 FROM memory_vectors WHERE id=?1 AND model=?3 AND dimensions=?4) AND EXISTS (SELECT 1 FROM memory_vectors WHERE id=?2 AND model=?3 AND dimensions=?4)",
        )?;
        for (a, b) in &refreshed.found {
            if changed.contains(a) || changed.contains(b) {
                continue;
            }
            let (a, b) = (a.min(b), a.max(b));
            add.execute(params![a.to_string(), b.to_string(), model, dimensions])?;
        }
        // The save compared, not a later one, which keeps its own row.
        let mut done =
            tx.prepare("UPDATE memory_pairs_saved SET compared=1 WHERE id=?1 AND seq=?2")?;
        for (id, seq) in &refreshed.compared {
            done.execute(params![id.to_string(), seq])?;
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::super::{schema::connect, similar::DraftVector, similar::save_vector};
    use super::*;

    const MODEL: &str = "kept-test";

    fn identity() -> (String, i64) {
        (MODEL.into(), 0)
    }

    /// A store with a memory for each of `count` ids, each with `vector`.
    fn store(count: usize, vector: &[f32]) -> (tempfile::TempDir, Connection, Vec<Uuid>) {
        let dir = tempfile::tempdir().unwrap();
        let conn = connect(&dir.path().join("memory.sqlite3")).unwrap();
        let ids: Vec<Uuid> = (0..count).map(|_| Uuid::new_v4()).collect();
        for id in &ids {
            conn.execute(
                "INSERT INTO memories (id,memory_type,category,content,title) VALUES (?1,'preference','workflow','Prefer Rust','Prefer Rust')",
                [id.to_string()],
            )
            .unwrap();
            save(&conn, *id, vector);
        }
        conn.execute(
            "INSERT INTO embedding_cutoffs (model,length,duplicate,related,calibrated_at) VALUES (?1,?2,0.9,0.8,datetime('now'))",
            params![MODEL, i64::try_from(vector.len()).unwrap()],
        )
        .unwrap();
        (dir, conn, ids)
    }

    fn save(conn: &Connection, id: Uuid, vector: &[f32]) {
        let vector = DraftVector {
            vector: vector.to_vec(),
            identity: identity(),
            source: String::new(),
        };
        save_vector(conn, id, &vector).unwrap();
    }

    fn count(conn: &Connection, sql: &str, id: Uuid) -> i64 {
        conn.query_row(sql, [id.to_string()], |r| r.get(0)).unwrap()
    }

    /// Reads, compares and keeps, as a review does, with `between` run
    /// after the read: another writer at work while the review compares.
    fn review(conn: &Connection, between: impl FnOnce()) {
        let kept = read(conn, &identity()).unwrap();
        let stored = super::super::super::similar::stored_vectors(conn, &identity()).unwrap();
        let cutoffs = HashMap::from([(
            3,
            Thresholds {
                duplicate: 0.9,
                related: 0.8,
            },
        )]);
        let refreshed = refresh(&stored, &cutoffs, &kept);
        between();
        keep(conn, &identity(), &kept, &refreshed).unwrap();
    }

    const PAIRS_OF: &str = "SELECT COUNT(*) FROM memory_pairs WHERE a=?1 OR b=?1";
    const DUE: &str = "SELECT COUNT(*) FROM memory_pairs_saved WHERE id=?1 AND compared=0";
    const ROWS: &str = "SELECT COUNT(*) FROM memory_pairs_saved WHERE id=?1";
    const SEQ: &str = "SELECT seq FROM memory_pairs_saved WHERE id=?1";

    // A saved vector is due and loses its pairs, whichever way it is
    // saved; a removed one leaves neither a pair nor a due row. A review
    // marks a row compared and keeps it; saving that vector again replaces
    // it with one row, due, under a later number.
    #[test]
    fn a_saved_or_removed_vector_takes_its_pairs_with_it() {
        let (_dir, conn, ids) = store(3, &[1.0, 0.0, 0.0]);
        assert!(ids.iter().all(|&id| count(&conn, DUE, id) == 1));
        review(&conn, || ());
        assert!(ids.iter().all(|&id| count(&conn, DUE, id) == 0));
        assert!(ids.iter().all(|&id| count(&conn, ROWS, id) == 1));
        assert_eq!(count(&conn, PAIRS_OF, ids[0]), 2);
        let compared = count(&conn, SEQ, ids[0]);
        save(&conn, ids[0], &[1.0, 0.0, 0.0]);
        assert_eq!(
            (count(&conn, PAIRS_OF, ids[0]), count(&conn, DUE, ids[0])),
            (0, 1)
        );
        assert_eq!(count(&conn, ROWS, ids[0]), 1);
        assert!(count(&conn, SEQ, ids[0]) > compared);
        review(&conn, || ());
        assert_eq!(count(&conn, PAIRS_OF, ids[0]), 2);
        conn.execute(
            "UPDATE memory_vectors SET source='edited' WHERE id=?1",
            [ids[1].to_string()],
        )
        .unwrap();
        assert_eq!(
            (count(&conn, PAIRS_OF, ids[1]), count(&conn, DUE, ids[1])),
            (0, 1)
        );
        conn.execute("DELETE FROM memories WHERE id=?1", [ids[2].to_string()])
            .unwrap();
        assert_eq!(
            (count(&conn, PAIRS_OF, ids[2]), count(&conn, DUE, ids[2])),
            (0, 0)
        );
    }

    // A vector saved again while a review compares stays due, and the
    // review keeps no pair of it, though it compared the old one; a vector
    // removed meanwhile gets no pair back.
    #[test]
    fn a_vector_changed_while_a_review_compares_stays_due() {
        let (_dir, conn, ids) = store(4, &[1.0, 0.0, 0.0]);
        review(&conn, || {
            save(&conn, ids[0], &[0.0, 1.0, 0.0]);
            conn.execute("DELETE FROM memories WHERE id=?1", [ids[1].to_string()])
                .unwrap();
        });
        assert_eq!(
            (count(&conn, PAIRS_OF, ids[0]), count(&conn, DUE, ids[0])),
            (0, 1)
        );
        assert_eq!(
            (count(&conn, PAIRS_OF, ids[1]), count(&conn, DUE, ids[1])),
            (0, 0)
        );
        assert_eq!(
            count(&conn, PAIRS_OF, ids[2]),
            1,
            "the pair of the other two"
        );
        assert_eq!(count(&conn, DUE, ids[2]), 0);
    }

    // Two vectors saved since the last review are compared with each
    // other, not only with the vectors kept before.
    #[test]
    fn vectors_saved_together_are_compared_with_each_other() {
        let (_dir, conn, ids) = store(4, &[1.0, 0.0, 0.0]);
        review(&conn, || ());
        save(&conn, ids[0], &[0.0, 1.0, 0.0]);
        save(&conn, ids[1], &[0.0, 1.0, 0.0]);
        review(&conn, || ());
        let pair: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_pairs WHERE a=?1 AND b=?2",
                [
                    ids[0].min(ids[1]).to_string(),
                    ids[0].max(ids[1]).to_string(),
                ],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pair, 1);
        assert_eq!(count(&conn, PAIRS_OF, ids[0]), 1, "and with nothing else");
    }
}
#[cfg(test)]
mod race {
    use super::super::super::{
        schema::connect, similar::DraftVector, similar::save_vector, similar::stored_vectors,
    };
    use super::*;

    const MODEL: &str = "kept-test";
    const PAIRS_OF: &str = "SELECT COUNT(*) FROM memory_pairs WHERE a=?1 OR b=?1";
    const DUE: &str = "SELECT COUNT(*) FROM memory_pairs_saved WHERE id=?1 AND compared=0";

    fn identity() -> (String, i64) {
        (MODEL.into(), 0)
    }
    fn save(conn: &Connection, id: Uuid, vector: &[f32]) {
        let vector = DraftVector {
            vector: vector.to_vec(),
            identity: identity(),
            source: String::new(),
        };
        save_vector(conn, id, &vector).unwrap();
    }
    fn count(conn: &Connection, sql: &str, id: Uuid) -> i64 {
        conn.query_row(sql, [id.to_string()], |r| r.get(0)).unwrap()
    }
    fn cutoffs(duplicate: f32, related: f32) -> HashMap<usize, Thresholds> {
        HashMap::from([(3, Thresholds { duplicate, related })])
    }
    /// A review's read and comparison at `cutoffs`, without its keep.
    fn compare_at(conn: &Connection, cutoffs: &HashMap<usize, Thresholds>) -> (Kept, Refreshed) {
        let kept = read(conn, &identity()).unwrap();
        let stored = stored_vectors(conn, &identity()).unwrap();
        let refreshed = refresh(&stored, cutoffs, &kept);
        (kept, refreshed)
    }
    fn compare(conn: &Connection) -> (Kept, Refreshed) {
        compare_at(conn, &cutoffs(0.9, 0.8))
    }
    fn near_of(refreshed: &Refreshed, id: Uuid) -> usize {
        refreshed
            .near
            .iter()
            .filter(|(a, b)| *a == id || *b == id)
            .count()
    }
    fn store(count: usize, vector: &[f32]) -> (tempfile::TempDir, Connection, Vec<Uuid>) {
        let dir = tempfile::tempdir().unwrap();
        let conn = connect(&dir.path().join("memory.sqlite3")).unwrap();
        let ids = store_into(&conn, count, vector);
        (dir, conn, ids)
    }
    /// Adds `count` memories with `vector`.
    fn store_into(conn: &Connection, count: usize, vector: &[f32]) -> Vec<Uuid> {
        let ids: Vec<Uuid> = (0..count).map(|_| Uuid::new_v4()).collect();
        for id in &ids {
            conn.execute(
                "INSERT INTO memories (id,memory_type,category,content,title) VALUES (?1,'preference','workflow','Prefer Rust','Prefer Rust')",
                [id.to_string()],
            ).unwrap();
            save(conn, *id, vector);
        }
        ids
    }

    // R0 keeps. R1 reads at lower cutoffs, so it compares every pair, while
    // X is unlike the rest; X is then saved like the rest, and R2, at R0's
    // cutoffs, compares X and keeps its pairs. R1 keeps last: it forgets
    // R2's pairs of X, keeps none, and X is no longer due, so no later
    // review finds X's three pairs.
    #[test]
    fn a_late_full_review_drops_the_pairs_of_a_vector_saved_after_its_read() {
        let low = cutoffs(0.9, 0.6);
        let (_dir, conn, ids) = store(4, &[1.0, 0.0, 0.0]);
        let x = ids[0];
        let (k0, r0) = compare(&conn);
        keep(&conn, &identity(), &k0, &r0).unwrap();
        save(&conn, x, &[0.0, 1.0, 0.0]);
        let (kept1, refreshed1) = compare_at(&conn, &low);
        save(&conn, x, &[1.0, 0.0, 0.0]);
        let (kept2, refreshed2) = compare(&conn);
        keep(&conn, &identity(), &kept2, &refreshed2).unwrap();
        keep(&conn, &identity(), &kept1, &refreshed1).unwrap();
        assert_eq!(
            (count(&conn, PAIRS_OF, x), count(&conn, DUE, x)),
            (3, 1),
            "R2's pairs of X stay, and X is due at R1's floor"
        );
        let (_, refreshed3) = compare_at(&conn, &low);
        assert_eq!(near_of(&refreshed3, x), 3, "X is like the other three");
    }

    // R0 keeps at high cutoffs. R1 reads at low ones, so it compares every
    // pair; a new vector N, a cosine of 0.7 from the rest, is then saved,
    // and R3 compares N at R0's floor and keeps first: no pair of N, which
    // is marked compared. R1 keeps last and stamps its lower floor, so N
    // must be due again, or no later review finds N's three pairs.
    #[test]
    fn a_late_full_review_at_a_lower_floor_makes_a_vector_saved_since_due() {
        let low = cutoffs(0.9, 0.6);
        let high = cutoffs(0.99, 0.95);
        let (_dir, conn, _) = store(3, &[1.0, 0.0, 0.0]);
        let (k0, r0) = compare_at(&conn, &high);
        keep(&conn, &identity(), &k0, &r0).unwrap();
        let (kept1, refreshed1) = compare_at(&conn, &low);
        let n = store_into(&conn, 1, &[0.7, 0.714_142_8, 0.0])[0];
        let (kept3, refreshed3) = compare_at(&conn, &high);
        keep(&conn, &identity(), &kept3, &refreshed3).unwrap();
        assert_eq!(
            (count(&conn, PAIRS_OF, n), count(&conn, DUE, n)),
            (0, 0),
            "R3 keeps no pair of N and marks it compared"
        );
        keep(&conn, &identity(), &kept1, &refreshed1).unwrap();
        let (_, refreshed4) = compare_at(&conn, &low);
        assert_eq!(near_of(&refreshed4, n), 3, "N is near the other three");
    }

    // X is due and like the rest when R1 reads; it is then saved unlike
    // the rest, and R2 compares it and takes it off the due list. R1 keeps
    // last and keeps X's three pairs from the vector it read.
    #[test]
    fn a_late_review_keeps_pairs_of_a_vector_since_replaced() {
        let (_dir, conn, ids) = store(4, &[1.0, 0.0, 0.0]);
        let x = ids[0];
        let (k0, r0) = compare(&conn);
        keep(&conn, &identity(), &k0, &r0).unwrap();
        save(&conn, x, &[1.0, 0.0, 0.0]);
        let (kept1, refreshed1) = compare(&conn);
        save(&conn, x, &[0.0, 1.0, 0.0]);
        let (kept2, refreshed2) = compare(&conn);
        keep(&conn, &identity(), &kept2, &refreshed2).unwrap();
        keep(&conn, &identity(), &kept1, &refreshed1).unwrap();
        assert_eq!(
            (count(&conn, PAIRS_OF, x), count(&conn, DUE, x)),
            (0, 0),
            "X is unlike the rest"
        );
    }

    // R0 keeps at high cutoffs. R1 reads at low ones, so it compares every
    // pair. N, a cosine of 0.7 from the rest, is saved. R3 reads at the
    // high cutoffs, takes R0's floor and compares N there: no pair. R1
    // keeps, stamping its lower floor and making N due again; R3 keeps
    // last and marks N compared under N's save number, so the state says
    // complete at R1's floor while N was compared only at R0's.
    #[test]
    fn a_high_floor_review_keeping_last_leaves_a_vector_compared_above_the_stamped_floor() {
        let low = cutoffs(0.9, 0.6);
        let high = cutoffs(0.99, 0.95);
        let (_dir, conn, _) = store(3, &[1.0, 0.0, 0.0]);
        let (k0, r0) = compare_at(&conn, &high);
        keep(&conn, &identity(), &k0, &r0).unwrap();
        let (kept1, refreshed1) = compare_at(&conn, &low);
        let n = store_into(&conn, 1, &[0.7, 0.714_142_8, 0.0])[0];
        let (kept3, refreshed3) = compare_at(&conn, &high);
        keep(&conn, &identity(), &kept1, &refreshed1).unwrap();
        keep(&conn, &identity(), &kept3, &refreshed3).unwrap();
        let (_, refreshed4) = compare_at(&conn, &low);
        assert_eq!(near_of(&refreshed4, n), 3, "N is near the other three");
    }

    // R0 keeps. N is saved, and R1 compares it on the fast path, finding
    // its three pairs. R2 has no cutoffs for the length and drops its
    // state. R1 keeps last against a state that is gone: it keeps nothing.
    // Keeping would give no wrong answer, as a length
    // without state is compared in full, but it would write against floors
    // that no longer hold.
    #[test]
    fn a_review_keeping_after_its_length_was_dropped_keeps_nothing() {
        let (_dir, conn, _) = store(3, &[1.0, 0.0, 0.0]);
        let (k0, r0) = compare(&conn);
        keep(&conn, &identity(), &k0, &r0).unwrap();
        let n = store_into(&conn, 1, &[1.0, 0.0, 0.0])[0];
        let (kept1, refreshed1) = compare(&conn);
        assert_eq!(near_of(&refreshed1, n), 3, "R1 finds N's pairs");
        let (kept2, refreshed2) = compare_at(&conn, &HashMap::new());
        keep(&conn, &identity(), &kept2, &refreshed2).unwrap();
        keep(&conn, &identity(), &kept1, &refreshed1).unwrap();
        assert_eq!(count(&conn, PAIRS_OF, n), 0, "R1 keeps no pair of N");
    }

    // #253's first order: R1 and R2 both read with nothing kept, X saved
    // between them. R2 compares every pair, keeps X's pairs and stamps its
    // floor first, so R1, whose read had no floor, keeps nothing.
    #[test]
    fn a_late_full_review_after_another_stamped_keeps_nothing() {
        let (_dir, conn, ids) = store(4, &[1.0, 0.0, 0.0]);
        let x = ids[0];
        save(&conn, x, &[0.0, 1.0, 0.0]);
        let (kept1, refreshed1) = compare(&conn);
        save(&conn, x, &[1.0, 0.0, 0.0]);
        let (kept2, refreshed2) = compare(&conn);
        keep(&conn, &identity(), &kept2, &refreshed2).unwrap();
        keep(&conn, &identity(), &kept1, &refreshed1).unwrap();
        assert_eq!(
            (count(&conn, PAIRS_OF, x), count(&conn, DUE, x)),
            (3, 0),
            "R2's keep stands and R1 keeps nothing"
        );
        let (_, refreshed3) = compare(&conn);
        assert_eq!(near_of(&refreshed3, x), 3, "X is like the other three");
    }

    // The lower-floor order with nothing kept: R1 reads at low cutoffs, N
    // is saved, and R2 compares every pair at high cutoffs and stamps its
    // floor first. R1 keeps nothing, so the state stays R2's and the next
    // low-cutoff review compares every pair.
    #[test]
    fn a_late_full_review_after_a_higher_floor_was_stamped_keeps_nothing() {
        let low = cutoffs(0.9, 0.6);
        let (_dir, conn, _) = store(3, &[1.0, 0.0, 0.0]);
        let (kept1, refreshed1) = compare_at(&conn, &low);
        let n = store_into(&conn, 1, &[0.7, 0.714_142_8, 0.0])[0];
        let (kept2, refreshed2) = compare_at(&conn, &cutoffs(0.99, 0.95));
        keep(&conn, &identity(), &kept2, &refreshed2).unwrap();
        let stamped = read(&conn, &identity()).unwrap().floors;
        keep(&conn, &identity(), &kept1, &refreshed1).unwrap();
        assert_eq!(read(&conn, &identity()).unwrap().floors, stamped);
        let (_, refreshed3) = compare_at(&conn, &low);
        assert_eq!(near_of(&refreshed3, n), 3, "N is near the other three");
    }
}
