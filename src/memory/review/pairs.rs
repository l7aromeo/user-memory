//! The pairs of memories review links, found without comparing every pair
//! by words: each memory's words, category group and vector norm are worked
//! out once, an exact prefix-filter join over the words finds every pair
//! that reaches [`RELATED_OVERLAP`], and the vectors are compared with a
//! fast dot product that only picks the pairs worth the exact one. Every
//! pair found is then decided exactly as before ([`compare`]), and the lists
//! come out in the same order, by the lower index, then the higher.

use super::super::{
    Memory,
    similar::{
        DUPLICATE_OVERLAP, RELATED_OVERLAP, Thresholds, category_group, cosine_normed, norm,
    },
    words::tokenize,
};
use super::Groups;
use std::collections::{BTreeSet, HashMap, HashSet};
use uuid::Uuid;

/// Pairs review links, each with its score: duplicates and related ones,
/// lower index first, in order of (lower, higher) index.
pub(super) struct Links {
    pub(super) duplicates: Vec<(usize, usize, f32)>,
    pub(super) related: Vec<(usize, usize, f32)>,
}

/// What comparing a memory needs, worked out once per memory.
struct Prepared<'a> {
    /// Its content words as ids, rarest in the store first.
    words: Vec<u32>,
    /// Its own category label's id.
    category: u32,
    /// The ids of the labels in its category group.
    group: Vec<u32>,
    /// The stored vector and its norm, computed as [`cosine`] would.
    ///
    /// [`cosine`]: super::super::similar::cosine
    vector: Option<(&'a [f32], f32)>,
}

/// How strongly two memories match: cosine when both have comparable stored
/// vectors, otherwise content-word overlap within one category group.
#[derive(Clone, Copy)]
struct Pair {
    duplicate: bool,
    related: bool,
    score: f32,
}

/// Ids for strings, in the order first seen.
#[derive(Default)]
struct Ids(HashMap<String, u32>);

impl Ids {
    fn of(&mut self, text: &str) -> u32 {
        let next = u32::try_from(self.0.len()).expect("fewer than 2^32 distinct words in a store");
        *self.0.entry(text.to_owned()).or_insert(next)
    }
}

/// The duplicate and related pairs among `active` (memory indexes, in
/// ascending order) that are not marked distinct, both observed, or one
/// evidence of the other. `cutoffs` holds the model's cutoffs by vector
/// length; a length without them is compared by keywords.
pub(super) fn links(
    memories: &[Memory],
    active: &[usize],
    distinct: &HashSet<(Uuid, Uuid)>,
    vectors: &HashMap<Uuid, Vec<f32>>,
    cutoffs: &HashMap<usize, Thresholds>,
) -> Links {
    let prepared = prepare(memories, active, vectors);
    let mut pairs = by_words(&prepared);
    pairs.extend(by_vectors(&prepared, cutoffs));
    pairs.sort_unstable();
    pairs.dedup();
    let mut links = Links {
        duplicates: Vec::new(),
        related: Vec::new(),
    };
    for (p, q) in pairs {
        let (i, j) = (active[p], active[q]);
        let (a, b) = (&memories[i], &memories[j]);
        if distinct.contains(&(a.id.min(b.id), a.id.max(b.id)))
            || (a.source == "observed" && b.source == "observed")
            || a.evidence_ids.contains(&b.id)
            || b.evidence_ids.contains(&a.id)
        {
            continue;
        }
        let pair = compare(&prepared[p], &prepared[q], cutoffs);
        if pair.duplicate {
            links.duplicates.push((i, j, pair.score));
        } else if pair.related {
            links.related.push((i, j, pair.score));
        }
    }
    links
}

/// Each of `active`'s memories prepared, in the same order: words ranked
/// rarest first across them, labels as ids, vectors with their norms.
fn prepare<'a>(
    memories: &[Memory],
    active: &[usize],
    vectors: &'a HashMap<Uuid, Vec<f32>>,
) -> Vec<Prepared<'a>> {
    let words: Vec<BTreeSet<String>> = active
        .iter()
        .map(|&i| tokenize(&memories[i].content))
        .collect();
    let mut frequency: HashMap<&str, usize> = HashMap::new();
    for set in &words {
        for word in set {
            *frequency.entry(word).or_default() += 1;
        }
    }
    let mut order: Vec<(&str, usize)> = frequency.into_iter().collect();
    order.sort_unstable_by(|(a, fa), (b, fb)| fa.cmp(fb).then_with(|| a.cmp(b)));
    let rank: HashMap<&str, u32> = order
        .iter()
        .zip(0..)
        .map(|((word, _), rank)| (*word, rank))
        .collect();
    let mut labels = Ids::default();
    active
        .iter()
        .zip(&words)
        .map(|(&i, set)| {
            let m = &memories[i];
            let mut ranked: Vec<u32> = set.iter().map(|w| rank[w.as_str()]).collect();
            ranked.sort_unstable();
            Prepared {
                words: ranked,
                category: labels.of(&m.category),
                group: category_group(&m.category)
                    .iter()
                    .map(|label| labels.of(label))
                    .collect(),
                vector: vectors.get(&m.id).map(|v| (v.as_slice(), norm(v))),
            }
        })
        .collect()
}

/// The Jaccard overlap of two sets of word ids, each sorted, computed as
/// [`overlap`] computes it for the same words.
///
/// [`overlap`]: super::super::similar::overlap
fn word_overlap(a: &[u32], b: &[u32]) -> f32 {
    let (mut x, mut y, mut both) = (0, 0, 0);
    while x < a.len() && y < b.len() {
        match a[x].cmp(&b[y]) {
            std::cmp::Ordering::Less => x += 1,
            std::cmp::Ordering::Greater => y += 1,
            std::cmp::Ordering::Equal => {
                both += 1;
                x += 1;
                y += 1;
            }
        }
    }
    let union = a.len() + b.len() - both;
    if union == 0 {
        return 0.0;
    }
    both as f32 / union as f32
}

/// The pair of `pa` and `pb`, `pa` the lower index: `pa`'s category group
/// holds `pb`'s category, as recording checks a new memory against a
/// stored one.
fn compare(pa: &Prepared, pb: &Prepared, cutoffs: &HashMap<usize, Thresholds>) -> Pair {
    let semantic = match (pa.vector, pb.vector) {
        (Some((x, nx)), Some((y, ny))) if x.len() == y.len() => cutoffs
            .get(&x.len())
            .map(|cutoffs| (cosine_normed(x, nx, y, ny), *cutoffs)),
        _ => None,
    };
    let same_group = pa.group.contains(&pb.category);
    let words = if same_group {
        word_overlap(&pa.words, &pb.words)
    } else {
        0.0
    };
    let duplicate = semantic.is_some_and(|(score, cutoffs)| score >= cutoffs.duplicate)
        || words >= DUPLICATE_OVERLAP;
    let related = same_group
        && (semantic.is_some_and(|(score, cutoffs)| score >= cutoffs.related)
            || words >= RELATED_OVERLAP);
    Pair {
        duplicate,
        related,
        score: semantic.map_or(words, |(score, _)| score),
    }
}

/// The words of a set of `n` indexed and probed: two sets overlapping by a
/// quarter or more share at least a quarter of each one's words, so they
/// share one among the first `n - ceil(n / 4) + 1` of each, with the words
/// of every set in one order.
fn prefix(n: usize) -> usize {
    if n == 0 { 0 } else { n - n.div_ceil(4) + 1 }
}

/// A pair ruled out for the probe under way.
const RULED_OUT: u32 = u32::MAX;

/// A prefix word's place in the index: the set holding it, its class (see
/// [`classes`]), where the word is in that set, and the set's size.
#[derive(Clone, Copy)]
struct Entry {
    class: u32,
    set: u32,
    position: u32,
    size: u32,
}

/// Each set's class: sets of different classes are never in one category
/// group, so they are never compared. A set's label and its group's labels
/// are joined into one class.
fn classes(prepared: &[Prepared]) -> Vec<u32> {
    let labels = prepared
        .iter()
        .flat_map(|p| p.group.iter().chain([&p.category]))
        .max()
        .map_or(0, |&top| top as usize + 1);
    let mut joined = Groups::new(labels);
    for p in prepared {
        for &label in &p.group {
            joined.join(p.category as usize, label as usize);
        }
    }
    prepared
        .iter()
        .map(|p| {
            u32::try_from(joined.root(p.category as usize)).expect("labels are numbered by u32 ids")
        })
        .collect()
}

/// Every pair of positions in `prepared`, lower first, in one category
/// group, whose content words overlap by [`RELATED_OVERLAP`] (a quarter) or
/// more, the bar for both related and duplicate.
///
/// PPJoin, the standard exact join for Jaccard overlap: an index of each
/// set's [`prefix`], words rarest first, and for each set the earlier ones
/// of its class sharing a prefix word, kept only while they can still reach
/// the overlap: in one category group, sizes within four times of each
/// other (a quarter overlap needs it), and enough words left after the
/// shared ones to reach `ceil((n + m) / 5)` in common, which an overlap of a
/// quarter means for sets of `n` and `m`. Each one left is then counted
/// exactly. The f32 test review applies agrees with these integer bounds: a
/// ratio i/u below a quarter is at most 1/4 - 1/(4u), far more than f32's
/// rounding near a quarter (about 3e-8) for any set of fewer than millions
/// of words.
fn by_words(prepared: &[Prepared]) -> Vec<(usize, usize)> {
    let class = classes(prepared);
    let vocabulary = prepared
        .iter()
        .flat_map(|p| p.words.iter())
        .max()
        .map_or(0, |&top| top as usize + 1);
    let mut placed: Vec<(u32, Entry)> = Vec::new();
    for ((p, set), &class) in prepared.iter().enumerate().zip(&class) {
        let at = u32::try_from(p).expect("fewer than 2^32 memories in a store");
        let size = u32::try_from(set.words.len()).expect("fewer than 2^32 words in a memory");
        for (&word, position) in set.words[..prefix(set.words.len())].iter().zip(0..) {
            let entry = Entry {
                class,
                set: at,
                position,
                size,
            };
            placed.push((word, entry));
        }
    }
    // One list per word, by class, then set: a probe reads only its own
    // class's part, up to its own set.
    placed.sort_unstable_by_key(|(word, e)| (*word, e.class, e.set));
    let mut starts = vec![0; vocabulary + 1];
    for (word, _) in &placed {
        starts[*word as usize + 1] += 1;
    }
    for w in 0..vocabulary {
        starts[w + 1] += starts[w];
    }
    let index: Vec<Entry> = placed.into_iter().map(|(_, e)| e).collect();
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let found: Vec<Vec<(usize, usize)>> = std::thread::scope(|scope| {
        let runs: Vec<_> = (0..threads)
            .map(|t| {
                let (index, starts, class) = (&index, &starts, &class);
                scope.spawn(move || {
                    let mut found = Vec::new();
                    let mut shared = vec![0u32; prepared.len()];
                    let mut met: Vec<usize> = Vec::new();
                    // Sets dealt in turn, so the long late probes and the
                    // short early ones are shared out evenly.
                    for q in (t..prepared.len()).step_by(threads) {
                        let x = &prepared[q].words;
                        let n = x.len();
                        for (px, &word) in x[..prefix(n)].iter().enumerate() {
                            let list = &index[starts[word as usize]..starts[word as usize + 1]];
                            let own = list.partition_point(|e| e.class < class[q]);
                            for e in &list[own..] {
                                let p = e.set as usize;
                                if e.class != class[q] || p >= q {
                                    break;
                                }
                                if shared[p] == RULED_OUT {
                                    continue;
                                }
                                let m = e.size as usize;
                                if shared[p] == 0 {
                                    met.push(p);
                                    if 4 * n.min(m) < n.max(m)
                                        || !prepared[p].group.contains(&prepared[q].category)
                                    {
                                        shared[p] = RULED_OUT;
                                        continue;
                                    }
                                }
                                let left = (n - px - 1).min(m - e.position as usize - 1);
                                if shared[p] as usize + 1 + left < (n + m).div_ceil(5) {
                                    shared[p] = RULED_OUT;
                                } else {
                                    shared[p] += 1;
                                }
                            }
                        }
                        for p in met.drain(..) {
                            if shared[p] != RULED_OUT
                                && word_overlap(&prepared[p].words, x) >= RELATED_OVERLAP
                            {
                                found.push((p, q));
                            }
                            shared[p] = 0;
                        }
                    }
                    found
                })
            })
            .collect();
        runs.into_iter()
            .map(|run| run.join().expect("a word comparison thread never panics"))
            .collect()
    });
    found.into_iter().flatten().collect()
}

/// How far the fast dot product's cosine may be from the exact one, for
/// vectors of `length`: a proved bound, not a fitted one. Summing n
/// products in any order is within about n units of rounding (u = 2^-24)
/// of the true sum, relative to the sum of their sizes, which is at most
/// the norms' product. So the two sums' cosines are within 2nu of each
/// other; this is twice that again, with room for the division, 6.1e-4 at
/// 2,560 dimensions. The largest difference the tests produce, a large
/// product that swallows every small one after it in the sequential sum, is
/// 1.8e-5 there, 34 times less: real sums stay far inside the bound, so a
/// tenth of it would still pass them, though it no longer bounds every sum.
fn margin(length: usize) -> f32 {
    (length as f32 + 4.0) * 2.0 * f32::EPSILON
}

/// Rows compared against every later vector in one pass, so each later
/// vector is read once per block rather than once per row: 32 rows of
/// 2,560 dimensions are 320 KiB, which stays in a core's cache.
const BLOCK: usize = 32;

/// Vectors on each side of one tile of dot products: 4 by 4 reuses each
/// value loaded four times, and its sums fit a core's vector registers.
const TILE: usize = 4;

/// A vector to compare: its memory's position, the vector and its norm.
type Row<'a> = (usize, &'a [f32], f32);

/// Every pair of positions in `prepared`, lower first, whose stored
/// vectors, of one length with cutoffs, may be at a cutoff: the fast cosine
/// is within [`margin`] of the lower of the two. Exact work is left to
/// [`compare`]. Every vector pair is compared: for cosine the norms divide
/// out, and a bound on part of the vector stays above the cutoffs when
/// models spread a vector evenly, so no exact method skips pairs at these
/// lengths.
///
/// So the cost is n²/2 · d multiply-adds for n vectors of d dimensions:
/// 3.2·10¹² at 50,000 of 2,560, 77 s on GitHub's macOS runner (3 cpus,
/// about 14·10⁹ a second on each). It passes the connector's 150 s
/// (`FORWARDED_CALL_LIMIT` in src/remote.rs) there at about 70,000 vectors
/// of 2,560 dimensions, or 100,000 of 1,024.
fn by_vectors(prepared: &[Prepared], cutoffs: &HashMap<usize, Thresholds>) -> Vec<(usize, usize)> {
    let mut by_length: HashMap<usize, Vec<Row>> = HashMap::new();
    for (p, set) in prepared.iter().enumerate() {
        if let Some((vector, norm)) = set.vector
            && cutoffs.contains_key(&vector.len())
        {
            by_length
                .entry(vector.len())
                .or_default()
                .push((p, vector, norm));
        }
    }
    let mut pairs = Vec::new();
    for (length, rows) in by_length {
        let lowest = cutoffs[&length].related.min(cutoffs[&length].duplicate) - margin(length);
        pairs.extend(near(&rows, lowest));
    }
    pairs
}

/// The pairs of `rows` (position, vector, norm), lower first, whose fast
/// cosine is `lowest` or more, compared on scoped threads.
fn near(rows: &[Row], lowest: f32) -> Vec<(usize, usize)> {
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let starts: Vec<usize> = (0..rows.len()).step_by(BLOCK).collect();
    let found: Vec<Vec<(usize, usize)>> = std::thread::scope(|scope| {
        let runs: Vec<_> = (0..threads)
            .map(|t| {
                let starts = &starts;
                scope.spawn(move || {
                    let mut found = Vec::new();
                    let mut keep = |a: usize, b: usize, dot: f32| {
                        let ((p, _, na), (q, _, nb)) = (rows[a], rows[b]);
                        let scale = na * nb;
                        let cosine = if scale > 0.0 { dot / scale } else { 0.0 };
                        if cosine >= lowest {
                            found.push((p, q));
                        }
                    };
                    // Blocks dealt in turn, so the long early rows and the
                    // short late ones are shared out evenly.
                    for &start in starts.iter().skip(t).step_by(threads) {
                        let end = (start + BLOCK).min(rows.len());
                        for b in start + 1..end {
                            for a in start..b {
                                keep(a, b, fast_dot(rows[a].1, rows[b].1));
                            }
                        }
                        let mut b = end;
                        while b < rows.len() {
                            let mut a = start;
                            while a < end {
                                if a + TILE <= end && b + TILE <= rows.len() {
                                    let dots = tile(
                                        std::array::from_fn(|k| rows[a + k].1),
                                        std::array::from_fn(|k| rows[b + k].1),
                                    );
                                    for (k, row) in dots.iter().enumerate() {
                                        for (l, &dot) in row.iter().enumerate() {
                                            keep(a + k, b + l, dot);
                                        }
                                    }
                                } else {
                                    for k in a..(a + TILE).min(end) {
                                        for l in b..(b + TILE).min(rows.len()) {
                                            keep(k, l, fast_dot(rows[k].1, rows[l].1));
                                        }
                                    }
                                }
                                a += TILE;
                            }
                            b += TILE;
                        }
                    }
                    found
                })
            })
            .collect();
        runs.into_iter()
            .map(|run| run.join().expect("a vector comparison thread never panics"))
            .collect()
    });
    found.into_iter().flatten().collect()
}

/// The vector width a [`tile`] step works on: four f32 fill one 128-bit
/// register, the width every target this builds for has.
const LANES: usize = 4;

/// Independent sums [`fast_dot`] keeps: enough to keep the vector units
/// busy, 16 runs about four times faster than four on arm64 and x86_64.
const PARTS: usize = 16;

/// `sums` plus `a` times `b`, lane by lane. Written as one step on whole
/// arrays, so the compiler keeps each in one register; as a loop over the
/// lanes it vectorised across steps instead and summed every lane one by
/// one, five times slower.
#[inline(always)]
fn add_products(sums: &mut [f32; LANES], a: &[f32; LANES], b: &[f32; LANES]) {
    *sums = std::array::from_fn(|lane| sums[lane] + a[lane] * b[lane]);
}

/// The dot products of each of `xs` with each of `ys`, all of one length,
/// summed in [`LANES`] interleaved parts, which differs from the sequential
/// sum only in rounding ([`margin`]).
fn tile(xs: [&[f32]; TILE], ys: [&[f32]; TILE]) -> [[f32; TILE]; TILE] {
    let whole = xs[0].len() / LANES;
    // Cut to one length, so the compiler sees every index below in bounds.
    let x = xs.map(|v| &v.as_chunks::<LANES>().0[..whole]);
    let y = ys.map(|v| &v.as_chunks::<LANES>().0[..whole]);
    let mut sums = [[[0.0f32; LANES]; TILE]; TILE];
    for c in 0..whole {
        for k in 0..TILE {
            for l in 0..TILE {
                add_products(&mut sums[k][l], &x[k][c], &y[l][c]);
            }
        }
    }
    std::array::from_fn(|k| {
        std::array::from_fn(|l| {
            let rest: f32 = xs[k][whole * LANES..]
                .iter()
                .zip(&ys[l][whole * LANES..])
                .map(|(a, b)| a * b)
                .sum();
            sums[k][l].iter().sum::<f32>() + rest
        })
    })
}

/// The dot product of `a` and `b` summed in [`PARTS`] interleaved parts,
/// which differs from the sequential sum only in rounding ([`margin`]).
fn fast_dot(a: &[f32], b: &[f32]) -> f32 {
    let (whole_a, rest_a) = a.as_chunks::<PARTS>();
    let (whole_b, rest_b) = b.as_chunks::<PARTS>();
    let mut sums = [0.0f32; PARTS];
    for (x, y) in whole_a.iter().zip(whole_b) {
        for part in 0..PARTS {
            sums[part] += x[part] * y[part];
        }
    }
    let rest: f32 = rest_a.iter().zip(rest_b).map(|(x, y)| x * y).sum();
    sums.iter().sum::<f32>() + rest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::similar::{cosine, overlap};

    /// Review's pairs as it found them before, comparing every pair: the
    /// reference the index must match exactly.
    fn every_pair(
        memories: &[Memory],
        active: &[usize],
        distinct: &HashSet<(Uuid, Uuid)>,
        vectors: &HashMap<Uuid, Vec<f32>>,
        cutoffs: &HashMap<usize, Thresholds>,
    ) -> Links {
        let mut links = Links {
            duplicates: Vec::new(),
            related: Vec::new(),
        };
        for (n, &i) in active.iter().enumerate() {
            for &j in &active[n + 1..] {
                let (a, b) = (&memories[i], &memories[j]);
                if distinct.contains(&(a.id.min(b.id), a.id.max(b.id)))
                    || (a.source == "observed" && b.source == "observed")
                    || a.evidence_ids.contains(&b.id)
                    || b.evidence_ids.contains(&a.id)
                {
                    continue;
                }
                let semantic = match (vectors.get(&a.id), vectors.get(&b.id)) {
                    (Some(x), Some(y)) if x.len() == y.len() => cutoffs
                        .get(&x.len())
                        .map(|cutoffs| (cosine(x, y), *cutoffs)),
                    _ => None,
                };
                let same_group = category_group(&a.category).contains(&b.category);
                let words = if same_group {
                    overlap(&tokenize(&a.content), &tokenize(&b.content))
                } else {
                    0.0
                };
                let duplicate = semantic.is_some_and(|(score, c)| score >= c.duplicate)
                    || words >= DUPLICATE_OVERLAP;
                let related = same_group
                    && (semantic.is_some_and(|(score, c)| score >= c.related)
                        || words >= RELATED_OVERLAP);
                let score = semantic.map_or(words, |(score, _)| score);
                if duplicate {
                    links.duplicates.push((i, j, score));
                } else if related {
                    links.related.push((i, j, score));
                }
            }
        }
        links
    }

    /// The links as comparable values, scores by their bits.
    fn bits(links: &Links) -> [Vec<(usize, usize, u32)>; 2] {
        let as_bits = |pairs: &[(usize, usize, f32)]| {
            pairs.iter().map(|&(i, j, s)| (i, j, s.to_bits())).collect()
        };
        [as_bits(&links.duplicates), as_bits(&links.related)]
    }

    /// A small fixed-seed generator: the same stores on every run.
    struct Seeded(u64);

    impl Seeded {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            usize::try_from(self.next() % n as u64).expect("a value below a usize fits one")
        }
        fn unit(&mut self) -> f32 {
            (self.below(2001) as f32 - 1000.0) / 1000.0
        }
    }

    const WORDS: [&str; 24] = [
        "lantern", "quartz", "cobalt", "walnut", "ember", "harbor", "violet", "copper", "meadow",
        "granite", "falcon", "willow", "saffron", "beacon", "glacier", "orchid", "pebble",
        "thistle", "juniper", "marble", "nickel", "tundra", "velvet", "zephyr",
    ];

    /// A word of `vocabulary`, the first ones far more often, as in text.
    fn pick<'a>(rng: &mut Seeded, vocabulary: &'a [String]) -> &'a str {
        let reach = rng.below(vocabulary.len()) + 1;
        &vocabulary[rng.below(reach)]
    }

    /// Words made of two of [`WORDS`]: a vocabulary of 576.
    fn compounds() -> Vec<String> {
        WORDS
            .iter()
            .flat_map(|a| WORDS.iter().map(move |b| format!("{a}{b}")))
            .collect()
    }

    /// Labels in and out of one alias group, cased as an agent might.
    const CATEGORIES: [&str; 8] = [
        "programming",
        "coding",
        "Coding",
        "workflow",
        "process",
        "Process",
        "testing",
        "tooling",
    ];

    /// Cutoffs for vectors of 24 dimensions; 40 dimensions have none, and
    /// are compared by words.
    fn cutoffs() -> HashMap<usize, Thresholds> {
        HashMap::from([(
            24,
            Thresholds {
                duplicate: 0.9,
                related: 0.75,
            },
        )])
    }

    /// A store of `size` memories from `seed`: many copied from an earlier
    /// one with a few words changed, so overlaps fall near a quarter and a
    /// half, with vectors near the cutoffs, some of another length, some
    /// missing, and distinct, observed and evidence pairs.
    #[expect(clippy::type_complexity, reason = "a test's whole input in one tuple")]
    fn store(
        seed: u64,
        size: usize,
        vocabulary: &[String],
        longest: usize,
    ) -> (
        Vec<Memory>,
        Vec<usize>,
        HashSet<(Uuid, Uuid)>,
        HashMap<Uuid, Vec<f32>>,
    ) {
        let mut rng = Seeded(seed);
        let mut memories: Vec<Memory> = Vec::new();
        let mut vectors = HashMap::new();
        for n in 0..size {
            let copy = (n > 0 && rng.below(3) > 0).then(|| rng.below(n));
            let mut words: Vec<&str> = match copy {
                Some(k) => memories[k].content.split(' ').collect(),
                None => (0..2 + rng.below(longest - 1))
                    .map(|_| pick(&mut rng, vocabulary))
                    .collect(),
            };
            for _ in 0..rng.below(4) {
                let at = rng.below(words.len() + 1);
                if at < words.len() && rng.below(2) == 0 {
                    words[at] = pick(&mut rng, vocabulary);
                } else {
                    words.insert(at.min(words.len()), pick(&mut rng, vocabulary));
                }
            }
            let category = match copy {
                Some(k) if rng.below(2) == 0 => memories[k].category.clone(),
                _ => CATEGORIES[rng.below(CATEGORIES.len())].to_owned(),
            };
            let id = Uuid::from_u64_pair(seed, n as u64);
            let mut m = Memory {
                id,
                category,
                content: words.join(" "),
                source: if rng.below(4) == 0 {
                    "observed"
                } else {
                    "explicit"
                }
                .to_owned(),
                ..Memory::default()
            };
            if n > 0 && rng.below(10) == 0 {
                m.evidence_ids.push(memories[rng.below(n)].id);
            }
            let length = [24, 24, 24, 40][rng.below(4)];
            let vector: Option<Vec<f32>> = match (copy, rng.below(5)) {
                (_, 0) => None,
                (Some(k), _)
                    if vectors
                        .get(&memories[k].id)
                        .is_some_and(|v: &Vec<f32>| v.len() == length) =>
                {
                    let spread = [0.02, 0.1, 0.3, 0.6][rng.below(4)];
                    Some(
                        vectors[&memories[k].id]
                            .iter()
                            .map(|x| x + spread * rng.unit())
                            .collect(),
                    )
                }
                _ => Some((0..length).map(|_| rng.unit()).collect()),
            };
            if let Some(vector) = vector {
                vectors.insert(id, vector);
            }
            memories.push(m);
        }
        let mut distinct = HashSet::new();
        for _ in 0..size / 10 {
            let (a, b) = (memories[rng.below(size)].id, memories[rng.below(size)].id);
            distinct.insert((a.min(b), a.max(b)));
        }
        let active = (0..size).filter(|_| rng.below(8) > 0).collect();
        (memories, active, distinct, vectors)
    }

    // The fast cosine stays within the margin of the exact one, at the
    // length real stores use.
    #[test]
    fn the_fast_cosine_stays_within_its_margin() {
        let mut rng = Seeded(7);
        // The second length leaves a tail after every whole step.
        for length in [2560, 2557] {
            for _ in 0..100 {
                let x: Vec<f32> = (0..length).map(|_| rng.unit()).collect();
                let y: Vec<f32> = x.iter().map(|v| v + 0.3 * rng.unit()).collect();
                let exact = cosine(&x, &y);
                for fast in fast_cosines(&x, &y) {
                    assert!(
                        (fast - exact).abs() <= margin(length),
                        "{fast} against {exact}"
                    );
                }
            }
        }
        let length = 2560;
        // A worst case for the sequential sum: a large product first, so
        // every small one after it is lost to rounding until a large
        // negative one cancels it, while the lanes keep them.
        let tiny = 2.0f32.powi(-13);
        let mut x = vec![tiny; length];
        let mut y = vec![tiny; length];
        (x[0], y[0], x[length - 1], y[length - 1]) = (1.0, 1.0, 1.0, -1.0);
        let exact = cosine(&x, &y);
        for fast in fast_cosines(&x, &y) {
            assert!(fast != exact, "the sums agree, so this case shows nothing");
            assert!(
                (fast - exact).abs() <= margin(length),
                "{fast} against {exact}, {} apart, margin {}",
                (fast - exact).abs(),
                margin(length)
            );
        }
    }

    /// The cosine of `x` and `y` by [`fast_dot`] and by [`tile`].
    fn fast_cosines(x: &[f32], y: &[f32]) -> [f32; 2] {
        let scale = norm(x) * norm(y);
        let tiled = tile([x; TILE], [y; TILE]);
        assert!(tiled.iter().flatten().all(|&dot| dot == tiled[0][0]));
        [fast_dot(x, y) / scale, tiled[0][0] / scale]
    }

    // A pair whose fast cosine falls just under a cutoff, within the
    // margin, is still compared exactly.
    #[test]
    fn a_pair_within_the_margin_of_a_cutoff_is_compared() {
        let mut rng = Seeded(11);
        let x: Vec<f32> = (0..24).map(|_| rng.unit()).collect();
        let y: Vec<f32> = x.iter().map(|v| v + 0.2 * rng.unit()).collect();
        let fast = fast_dot(&x, &y) / (norm(&x) * norm(&y));
        let at = fast + margin(24) / 2.0;
        let cutoffs = HashMap::from([(
            24,
            Thresholds {
                duplicate: at,
                related: at,
            },
        )]);
        let prepared = [&x, &y].map(|v| Prepared {
            words: Vec::new(),
            category: 0,
            group: Vec::new(),
            vector: Some((v.as_slice(), norm(v))),
        });
        assert_eq!(by_vectors(&prepared, &cutoffs), [(0, 1)]);
    }

    // The index finds exactly the pairs, scores and order comparing every
    // pair found, over stores full of pairs at the cutoffs.
    #[test]
    fn the_index_links_exactly_what_comparing_every_pair_links() {
        let cutoffs = cutoffs();
        let small: Vec<String> = WORDS.iter().map(|w| (*w).to_owned()).collect();
        let large = compounds();
        let mut linked = 0;
        // Short memories from few words, and long ones from many, where
        // the prefixes are a small part of each set.
        for (seed, vocabulary, longest) in (1..=30)
            .map(|seed| (seed, &small, 10))
            .chain((31..=50).map(|seed| (seed, &large, 60)))
        {
            let (memories, active, distinct, vectors) = store(seed, 160, vocabulary, longest);
            let found = links(&memories, &active, &distinct, &vectors, &cutoffs);
            let expected = every_pair(&memories, &active, &distinct, &vectors, &cutoffs);
            assert_eq!(bits(&found), bits(&expected), "seed {seed}");
            linked += expected.duplicates.len() + expected.related.len();
        }
        assert!(linked > 1000, "the stores hold few pairs to find: {linked}");
    }
}
