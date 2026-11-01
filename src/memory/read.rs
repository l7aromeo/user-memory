//! Retrieval: ranked search and reading whole memories.
//!
//! Search considers every memory the selection covers (by default the active
//! global memories and the requested projects'). Keyword relevance stems
//! the topic's words, weighs each by how rare it is among the candidates and
//! by the field that holds it (category, title, type, then the text), read
//! from the keyword index when it is current and from every candidate's own
//! text otherwise, with the same result; with an embedding endpoint
//! configured it is fused with semantic similarity by reciprocal rank. A
//! project memory's fused score counts [`PROJECT_BONUS`] more, so it wins an
//! equal match but not a clearly stronger one; without a topic, project
//! memories come first.

pub(super) use super::ids::SHORT_ID;
use super::{
    Memory,
    ids::resolve,
    index::{
        self, Candidates, embed,
        stems::{Field, Stems},
    },
    page::{Paging, page},
    schema::{connect, get},
    selection::{Filter, Selection, candidates},
    similar::{category_group, tokenize},
};
use anyhow::{Result, bail};
use clap::Args;
use rusqlite::Connection;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Mutex, PoisonError},
};
use uuid::Uuid;

/// Smoothing constant of reciprocal rank fusion. The customary 60 suits lists
/// of thousands; a store's matches number tens, and 60 flattens their fused
/// scores until order barely depends on rank. On the retrieval benchmark
/// (evals/retrieval, qwen3-embedding:4b) fused MRR falls smoothly with it:
/// 0.80 at 5, 0.76 at 10, 0.71 at 20, and on a real store of 55 memories
/// 0.80, 0.78, 0.75. Below 5, project memories start losing equal matches.
const FUSION: f64 = 5.0;

/// How much more a project memory's fused score counts than a global one's:
/// enough that a project lesson beats a global rule matching as well, not
/// one matching clearly better. With [`FUSION`] at 5 it is worth about one
/// rank place in both rankings. On the retrieval benchmark 0.3 keeps every
/// project lesson that ties a global rule first, with qwen3-embedding,
/// nomic-embed-text and embeddinggemma alike; 0.2 loses one of the ten on
/// two of them, and 0.4 lowers fused MRR by 0.02 to 0.03.
const PROJECT_BONUS: f64 = 0.3;

/// Memories a topic may match by meaning alone, on top of every keyword
/// match: beyond the closest few, similarity rarely means relevance.
const SEMANTIC_MATCHES: usize = 20;

/// How far below the best match, as a share of the gap between the best and
/// the median similarity of the topic's candidates, a memory still matches
/// by meaning. Relative, because models spread cosines differently: on the
/// retrieval benchmark the best-to-median gap is 0.29 with qwen3-embedding,
/// 0.18 with nomic-embed-text and 0.30 with embeddinggemma, so a fixed
/// cosine margin trimmed nothing for one model and most for another. At
/// 0.75 no target is lost with any of them (their largest relative gaps are
/// 1.03, matched by keyword, 0.83 and 0.66) and a topic lists about 10 to 12
/// memories, not 20; at 0.5 targets start to drop.
const SEMANTIC_SPREAD: f32 = 0.75;

#[derive(Args, Deserialize, Serialize, JsonSchema)]
pub struct Search {
    /// What to look for; without it every memory in scope matches, newest first.
    #[arg(default_value = "")]
    // `query` is the name agents reach for; unknown names are ignored, so
    // without it a search for a topic returned the whole store.
    #[serde(default, alias = "query")]
    pub topic: String,
    #[command(flatten)]
    #[serde(flatten)]
    pub selection: Selection,
    #[command(flatten)]
    #[serde(flatten)]
    pub paging: Paging,
}

#[derive(Args, Deserialize, Serialize, JsonSchema)]
pub struct Read {
    /// Up to 20 memory ids, each in full or as the first 8 characters the index shows. Give ids or a category, not both; with ids, status applies and projects and scope are ignored. Ids that match nothing are listed under unknown.
    #[serde(default)]
    pub ids: Vec<String>,
    /// With a category: which memories of it, as for index and search.
    #[command(flatten)]
    #[serde(flatten)]
    pub selection: Selection,
    /// Pages a category read; ids are at most 20 and need no paging.
    #[command(flatten)]
    #[serde(flatten)]
    pub paging: Paging,
}

/// Most ids one read accepts; more is a listing, which category or index gives.
const READ_IDS: usize = 20;

/// How much a topic word counts, by the strongest field of a memory that
/// holds it; each word counts once per memory. A category names what the
/// memory is about, and a title states the memory in a line, so either
/// beats a word somewhere in the text, and a type such as "preference"
/// beats the text. On the retrieval benchmark (evals/retrieval) the order
/// matters more than the values: these weights score keyword MRR 0.72, and
/// in the simulation the design was chosen with, category 2 or 1 with title
/// 1.5 to 2 came within 0.02 of category 3 with title 2.
const CATEGORY_WEIGHT: f64 = 3.0;
/// See [`CATEGORY_WEIGHT`].
const TITLE_WEIGHT: f64 = 2.0;
/// See [`CATEGORY_WEIGHT`]. Above the text's 1 so the documented order holds,
/// category, then type, then text: the design was measured with type and
/// text both at 1, and on the benchmark 1.5 moves nothing, as topics rarely
/// name a memory's type.
const TYPE_WEIGHT: f64 = 1.5;
/// See [`CATEGORY_WEIGHT`]: content, context, reason and evidence.
const TEXT_WEIGHT: f64 = 1.0;

/// How much a word counts in `field`.
fn weight(field: Field) -> f64 {
    match field {
        Field::Category => CATEGORY_WEIGHT,
        Field::Title => TITLE_WEIGHT,
        Field::Type => TYPE_WEIGHT,
        Field::Body => TEXT_WEIGHT,
    }
}

/// The weight of the strongest field of `m` holding each of `query`'s terms,
/// in order; 0 where none does.
fn field_weights(stems: &mut Stems, query: &[String], m: &Memory) -> Vec<f64> {
    let mut best = vec![0.0; query.len()];
    let text = format!("{} {} {} {}", m.content, m.context, m.reason, m.evidence);
    let labels = category_group(&m.category).join(" ");
    for (field, weight) in [
        (labels.as_str(), CATEGORY_WEIGHT),
        (m.title.as_str(), TITLE_WEIGHT),
        (m.memory_type.as_str(), TYPE_WEIGHT),
        (text.as_str(), TEXT_WEIGHT),
    ] {
        for word in tokenize(field) {
            let stem = stems.of(word);
            if let Some(i) = query.iter().position(|term| term == stem) {
                best[i] = f64::max(best[i], weight);
            }
        }
    }
    best
}

/// Each memory's keyword relevance, written into `scores`, from `weights`:
/// for each of `rows` memories, one row of `terms` weights, that of the
/// strongest field holding each query term (0 where none does). Among
/// `candidates` memories every term counts its field's weight times its
/// rarity, BM25's inverse document frequency, so a word most memories share
/// adds little and one that few hold adds much. Memories holding no term
/// may be left out of `weights`; they add nothing.
fn keyword_scores(
    weights: &[f64],
    terms: usize,
    rows: usize,
    candidates: usize,
    rarity: &mut Vec<f64>,
    scores: &mut Vec<f64>,
) {
    let all = candidates as f64;
    rarity.clear();
    rarity.extend((0..terms).map(|t| {
        let holding = (0..rows).filter(|&r| weights[r * terms + t] > 0.0).count() as f64;
        ((all - holding + 0.5) / (holding + 0.5)).ln_1p()
    }));
    scores.clear();
    scores.extend((0..rows).map(|r| {
        weights[r * terms..(r + 1) * terms]
            .iter()
            .zip(rarity.iter())
            .map(|(weight, idf)| weight * idf)
            .sum::<f64>()
    }));
}

fn recency(m: &Memory) -> (&str, i64) {
    (&m.updated_at, m.sequence)
}

fn add_ranking(order: &[usize], fused: &mut [f64]) {
    for (rank, &i) in order.iter().enumerate() {
        fused[i] += 1.0 / (FUSION + rank as f64 + 1.0);
    }
}

/// What a topic matched, best first, and whether meaning ranked it beside
/// the keywords.
pub(super) struct Ranked {
    pub(super) ids: Vec<Uuid>,
    /// An embeddings endpoint is configured and answered for this topic;
    /// false without a topic, without an endpoint, or when it failed.
    pub(super) semantic: bool,
}

/// Memories matching `topic`, best first. Without a topic every candidate
/// matches. Semantic similarity, when available, adds the `limit` most similar
/// memories to the keyword matches and fuses both rankings.
pub(super) fn rank(
    conn: &Connection,
    filter: &Filter,
    topic: &str,
    limit: usize,
) -> Result<Ranked> {
    rank_with(conn, filter, topic, limit, BUFFERS.take().buffers())
}

/// [`rank`] in `buffers`, which it leaves holding this search's arrays.
fn rank_with(
    conn: &Connection,
    filter: &Filter,
    topic: &str,
    limit: usize,
    buffers: &mut Buffers,
) -> Result<Ranked> {
    let mut stems = Stems::new();
    let query: Vec<String> = stems.all(topic).into_iter().collect();
    let semantic = if query.is_empty() {
        None
    } else {
        embed::Config::from_env()
    };
    let indexed = index::ready(conn)?;
    let Buffers {
        candidates: covered,
        weights,
        rarity,
        scores,
        matched,
        fused,
        by_keyword,
        by_meaning,
        order,
        similar,
        values,
    } = buffers;
    let terms = query.len();
    if indexed {
        index::covered(conn, filter, covered)?;
        weights.clear();
        weights.resize(covered.len() * terms, 0.0);
        if terms > 0 {
            index::hits(conn, covered, &query, |i, t, field| {
                weights[i * terms + t] = weight(field);
            })?;
        }
    } else {
        // Without a current keyword index, search stems every candidate's
        // words itself, so it reads every candidate in full.
        covered.clear();
        weights.clear();
        for m in candidates(conn, filter)? {
            covered.push(m.id, m.scope == "project", &m.updated_at, m.sequence);
            weights.extend(field_weights(&mut stems, &query, &m));
        }
    }
    let count = covered.len();
    keyword_scores(weights, terms, count, count, rarity, scores);
    matched.clear();
    matched.extend(scores.iter().map(|&s| query.is_empty() || s > 0.0));
    fused.clear();
    fused.resize(count, 0.0);
    let mut semantic_ranked = false;
    // Every order compared ends with a memory's key or rowid, which no two
    // share, so an unstable sort gives the one order a stable sort would.
    by_keyword.clear();
    by_keyword.extend((0..count).filter(|&i| scores[i] > 0.0));
    by_keyword.sort_unstable_by(|&a, &b| {
        scores[b]
            .total_cmp(&scores[a])
            .then_with(|| covered.recency(b).cmp(&covered.recency(a)))
    });
    add_ranking(by_keyword, fused);
    if let Some(config) = &semantic {
        match index::similarities(conn, config, topic, covered, similar) {
            Ok(()) => {
                by_meaning.clear();
                by_meaning.extend((0..count).filter(|&i| similar[i].is_some()));
                let value = |i: usize| similar[i].unwrap_or(f32::NEG_INFINITY);
                by_meaning.sort_unstable_by(|&a, &b| {
                    value(b)
                        .total_cmp(&value(a))
                        .then_with(|| covered.recency(b).cmp(&covered.recency(a)))
                });
                let floor = similarity_floor(similar, values);
                for &i in by_meaning.iter().take(limit) {
                    if value(i) >= floor {
                        matched[i] = true;
                    }
                }
                add_ranking(by_meaning, fused);
                semantic_ranked = true;
            }
            Err(err) => embed::warn(format!("{err:#}")),
        }
    }
    let local = |i: usize| covered.local[i];
    let score = |i: usize| {
        if local(i) {
            fused[i] * (1.0 + PROJECT_BONUS)
        } else {
            fused[i]
        }
    };
    order.clear();
    order.extend((0..count).filter(|&i| matched[i]));
    // Without a topic every score is 0, so project memories come first.
    order.sort_unstable_by(|&a, &b| {
        score(b)
            .total_cmp(&score(a))
            .then_with(|| local(b).cmp(&local(a)))
            .then_with(|| covered.recency(b).cmp(&covered.recency(a)))
    });
    Ok(Ranked {
        ids: order.iter().map(|&i| covered.ids[i]).collect(),
        semantic: semantic_ranked,
    })
}

/// The arrays one search ranks with, kept for the next: see
/// [`Candidates`] for why a search does not allocate its own.
#[derive(Default)]
struct Buffers {
    candidates: Candidates,
    /// For each candidate, a row of the weight of the strongest field
    /// holding each query term.
    weights: Vec<f64>,
    rarity: Vec<f64>,
    scores: Vec<f64>,
    matched: Vec<bool>,
    fused: Vec<f64>,
    by_keyword: Vec<usize>,
    by_meaning: Vec<usize>,
    order: Vec<usize>,
    similar: Vec<Option<f32>>,
    values: Vec<f32>,
}

/// Buffers no search is using: a server that runs searches at once takes
/// one set each, so there are as many as searches ever ran together.
struct Pool(Mutex<Vec<Buffers>>);

static BUFFERS: Pool = Pool(Mutex::new(Vec::new()));

impl Pool {
    fn take(&self) -> Borrowed<'_> {
        let mut free = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        Borrowed {
            pool: self,
            buffers: Some(free.pop().unwrap_or_default()),
        }
    }
}

/// A search's set of [`Buffers`], handed back to its pool when it drops.
struct Borrowed<'p> {
    pool: &'p Pool,
    buffers: Option<Buffers>,
}

impl Borrowed<'_> {
    fn buffers(&mut self) -> &mut Buffers {
        self.buffers
            .as_mut()
            .expect("a search holds its buffers until it drops them")
    }
}

impl Drop for Borrowed<'_> {
    fn drop(&mut self) {
        if let Some(buffers) = self.buffers.take() {
            self.pool
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(buffers);
        }
    }
}

/// `memories` as read and search return them: each without its empty
/// fields (an empty text or list, or a null), which a field left out says
/// as well and which cost an agent's context about a third of each memory.
/// Exports keep every field.
fn compact(memories: &[Memory]) -> Result<Value> {
    let mut memories = serde_json::to_value(memories)?;
    for memory in memories.as_array_mut().into_iter().flatten() {
        if let Some(fields) = memory.as_object_mut() {
            fields.retain(|_, value| match value {
                Value::Null => false,
                Value::String(text) => !text.is_empty(),
                Value::Array(items) => !items.is_empty(),
                _ => true,
            });
        }
    }
    Ok(memories)
}

/// Fewest candidates whose similarities say how far apart a topic's
/// matches lie: with one or two, the median is a match itself.
const SPREAD_SAMPLE: usize = 3;

/// The lowest similarity that still matches by meaning: [`SEMANTIC_SPREAD`]
/// of the way from the best similarity down to the median one. Below
/// [`SPREAD_SAMPLE`] candidates every one may match.
fn similarity_floor(similar: &[Option<f32>], values: &mut Vec<f32>) -> f32 {
    values.clear();
    values.extend(similar.iter().flatten());
    if values.len() < SPREAD_SAMPLE {
        return f32::NEG_INFINITY;
    }
    // The element a full sort would put in the middle, without sorting.
    let middle = values.len() / 2;
    let (_, &mut median, _) = values.select_nth_unstable_by(middle, f32::total_cmp);
    let best = values
        .iter()
        .copied()
        .max_by(f32::total_cmp)
        .expect("at least SPREAD_SAMPLE values");
    best - SEMANTIC_SPREAD * (best - median)
}

/// `found` as one page of a result: `memories`, and `next_cursor` when more remain.
fn paged(found: Vec<Memory>, paging: &Paging) -> Result<Value> {
    let ids: Vec<Uuid> = found.iter().map(|m| m.id).collect();
    let (memories, next) = page(found, &ids, paging)?;
    let mut result = json!({ "memories": compact(&memories)? });
    if let Some(next) = next {
        result["next_cursor"] = json!(next);
    }
    Ok(result)
}

/// A search's page unless asked otherwise: agents asked for 5 to 20 of the
/// best matches, and each full memory costs their context about a
/// kilobyte. Index and category reads, which list, keep the larger page.
const SEARCH_PAGE: usize = 20;

/// Memories matching `input`, best first, one page at a time.
pub fn search(db: &Path, input: &Search) -> Result<Value> {
    let conn = connect(db)?;
    let filter = input.selection.filter()?;
    let paging = Paging {
        cursor: input.paging.cursor.clone(),
        page_size: Some(input.paging.page_size.unwrap_or(SEARCH_PAGE)),
    };
    let ranked = rank(&conn, &filter, &input.topic, SEMANTIC_MATCHES)?;
    let (shown, next) = page(ranked.ids.clone(), &ranked.ids, &paging)?;
    // Only the page is read in full. A memory forgotten since it was ranked
    // is left out of the page.
    let mut memories = Vec::with_capacity(shown.len());
    for id in shown {
        memories.extend(get(&conn, id)?);
    }
    let mut result = json!({ "memories": compact(&memories)? });
    if let Some(next) = next {
        result["next_cursor"] = json!(next);
    }
    // Lets an agent tell "nothing found" from "semantic search is off".
    result["semantic"] = json!(ranked.semantic);
    Ok(result)
}

/// The order memories take inside an index group or a category read: project
/// memories first, then those confirmed in more conversations, then newest.
pub(super) fn group_order(a: &Memory, b: &Memory) -> std::cmp::Ordering {
    (a.scope != "project")
        .cmp(&(b.scope != "project"))
        .then_with(|| b.confirmations.cmp(&a.confirmations))
        .then_with(|| recency(b).cmp(&recency(a)))
}

/// Full memories by id or by category, in the shape search returns. Ids
/// that match nothing are listed under `unknown`, and memories whose status
/// the status filter leaves out (inactive ones, by default) under
/// `excluded`, so none is skipped silently.
pub fn read(db: &Path, input: &Read) -> Result<Value> {
    let conn = connect(db)?;
    let selection = &input.selection;
    let category = selection
        .category
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    match (input.ids.is_empty(), category) {
        (false, None) => {}
        (true, Some(_)) => {
            let mut found = candidates(&conn, &selection.filter()?)?;
            found.sort_by(group_order);
            return paged(found, &input.paging);
        }
        _ => bail!(
            "Give either ids or a category; to list memories by project or scope, use index with those filters"
        ),
    }
    // An id names one memory in any scope, so the projects and scope an
    // agent carries over from index change nothing and are let through;
    // type, source and origin would look like they filter, and do not.
    let ignored = Selection {
        status: selection.status,
        projects: selection.projects.clone(),
        scope: selection.scope,
        ..Selection::default()
    };
    if selection.narrows_beyond(&ignored) {
        bail!(
            "Ids are read as they are; only status applies to them, the other filters to a category read"
        );
    }
    if input.paging.cursor.is_some() || input.paging.page_size.is_some() {
        bail!("cursor and page_size page a category read; ids need no paging");
    }
    if input.ids.len() > READ_IDS {
        bail!("Read at most {READ_IDS} ids at a time; read a category for more");
    }
    let (mut memories, mut unknown, mut excluded) = (Vec::new(), Vec::new(), Vec::new());
    for wanted in &input.ids {
        match resolve(&conn, wanted)? {
            None => unknown.push(wanted.trim().to_owned()),
            Some(id) => {
                let Some(memory) = get(&conn, id)? else {
                    unknown.push(wanted.trim().to_owned());
                    continue;
                };
                if selection.status.covers(&memory.status) {
                    memories.push(memory);
                } else {
                    excluded.push(json!({"id": memory.id, "status": memory.status}));
                }
            }
        }
    }
    let mut result = json!({ "memories": compact(&memories)? });
    if !unknown.is_empty() {
        result["unknown"] = json!(unknown);
    }
    if !excluded.is_empty() {
        result["excluded"] = json!(excluded);
    }
    Ok(result)
}

/// Types whose explicit, confirmed memories are standing rules: the index
/// lists them under Rules.
const RULE_TYPES: [&str; 3] = ["preference", "constraint", "value"];

pub(super) fn standing_rule(m: &Memory) -> bool {
    m.source == "explicit"
        && m.status == "confirmed"
        && RULE_TYPES.contains(&m.memory_type.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A seeded xorshift generator.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn a_search_reuses_the_buffers_of_the_one_before() {
        let dir = tempfile::tempdir().unwrap();
        let conn = connect(&dir.path().join("memories.sqlite3")).unwrap();
        for n in 0..300 {
            conn.execute(
                "INSERT INTO memories (id,memory_type,category,title,content) VALUES (?1,'fact','tools',?2,?3)",
                rusqlite::params![
                    Uuid::new_v4().to_string(),
                    format!("Release note {n}"),
                    format!("Write release notes for build {n}")
                ],
            )
            .unwrap();
        }
        let filter = Selection::default().filter().unwrap();
        let mut buffers = Buffers::default();
        let first = rank_with(&conn, &filter, "release notes", 20, &mut buffers).unwrap();
        assert_eq!(first.ids.len(), 300);
        // Room to spare in every buffer: a search that allocated one again
        // would leave it only the room it needs.
        let spare = 100_000;
        buffers.candidates.ids.reserve(spare);
        buffers.weights.reserve(spare);
        buffers.scores.reserve(spare);
        buffers.matched.reserve(spare);
        buffers.fused.reserve(spare);
        buffers.by_keyword.reserve(spare);
        buffers.order.reserve(spare);
        let room = |b: &Buffers| {
            [
                b.candidates.ids.capacity(),
                b.weights.capacity(),
                b.scores.capacity(),
                b.matched.capacity(),
                b.fused.capacity(),
                b.by_keyword.capacity(),
                b.order.capacity(),
            ]
        };
        let before = room(&buffers);
        let second = rank_with(&conn, &filter, "release notes", 20, &mut buffers).unwrap();
        assert_eq!(second.ids, first.ids);
        assert_eq!(room(&buffers), before, "a buffer was allocated again");
    }

    #[test]
    fn buffers_go_back_to_their_pool_for_the_next_search() {
        let pool = Pool(Mutex::new(Vec::new()));
        let place = {
            let mut first = pool.take();
            first.buffers().scores.reserve(1000);
            first.buffers().scores.as_ptr()
        };
        let mut second = pool.take();
        assert_eq!(second.buffers().scores.as_ptr(), place);
        // Searches at once each take a set of their own.
        let mut third = pool.take();
        assert_ne!(third.buffers().scores.as_ptr(), place);
    }

    #[test]
    fn the_spread_floor_is_the_one_a_full_sort_gives() {
        let mut rng = Rng(0x00f1_00a5);
        let mut values = Vec::new();
        for length in [0, 2, 3, 4, 7, 100, 1001] {
            let similar: Vec<Option<f32>> = (0..length)
                .map(|i| match rng.next() % 6 {
                    0 => None,
                    1 => Some(0.5),
                    2 if i % 2 == 0 => Some(-0.0),
                    _ => Some((rng.next() % 2000) as f32 / 1000.0 - 1.0),
                })
                .collect();
            let mut sorted: Vec<f32> = similar.iter().flatten().copied().collect();
            let expected = if sorted.len() < SPREAD_SAMPLE {
                f32::NEG_INFINITY
            } else {
                sorted.sort_by(f32::total_cmp);
                let best = sorted[sorted.len() - 1];
                let median = sorted[sorted.len() / 2];
                best - SEMANTIC_SPREAD * (best - median)
            };
            assert_eq!(
                similarity_floor(&similar, &mut values).to_bits(),
                expected.to_bits(),
                "{length}"
            );
        }
    }

    #[test]
    fn recency_orders_as_the_updated_at_text_does() {
        let stamps = [
            "2026-11-01 10:00:00",
            "2026-11-01 09:59:59",
            "2026-11-01 10:00:00.5",
            "2026-11-01T10:00:00Z",
            "2026-11-01 10:00",
            "",
            "édité",
        ];
        let mut candidates = Candidates::default();
        for (n, stamp) in stamps.iter().enumerate() {
            candidates.push(Uuid::new_v4(), false, stamp, (n % 3) as i64);
        }
        for a in 0..stamps.len() {
            for b in 0..stamps.len() {
                assert_eq!(
                    candidates.recency(a).cmp(&candidates.recency(b)),
                    (stamps[a], (a % 3) as i64).cmp(&(stamps[b], (b % 3) as i64)),
                    "{} and {}",
                    stamps[a],
                    stamps[b]
                );
            }
        }
    }
}
