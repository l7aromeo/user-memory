//! Retrieval: ranked search and reading whole memories.
//!
//! Search considers every memory the selection covers (by default the active
//! global memories and the requested projects'). Keyword relevance weighs
//! category over type over text; with an embedding endpoint configured it is
//! fused with semantic similarity by reciprocal rank. A project memory's
//! fused score counts [`PROJECT_BONUS`] more, so it wins an equal match but
//! not a clearly stronger one; without a topic, project memories come first.

pub(super) use super::ids::SHORT_ID;
use super::{
    Memory,
    ids::resolve,
    index::{self, embed},
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
use std::{collections::BTreeSet, path::Path};
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

fn keyword_score(query: &BTreeSet<String>, m: &Memory) -> usize {
    let overlap = |words: &BTreeSet<String>| query.intersection(words).count();
    let category: BTreeSet<String> = category_group(&m.category)
        .iter()
        .flat_map(|label| tokenize(label))
        .collect();
    4 * overlap(&category)
        + 2 * overlap(&tokenize(&m.memory_type))
        + overlap(&tokenize(&format!(
            "{} {} {} {}",
            m.content, m.context, m.reason, m.evidence
        )))
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
    pub(super) memories: Vec<Memory>,
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
    let query = tokenize(topic);
    let semantic = if query.is_empty() {
        None
    } else {
        embed::Config::from_env()
    };
    let keyword_only = semantic.is_none() && !query.is_empty();
    let items = candidates(conn, filter, keyword_only.then_some(&query))?;
    let scores: Vec<usize> = items.iter().map(|m| keyword_score(&query, m)).collect();
    let mut matched: Vec<bool> = scores.iter().map(|&s| query.is_empty() || s > 0).collect();
    let mut fused = vec![0.0; items.len()];
    let mut semantic_ranked = false;
    let mut by_keyword: Vec<usize> = (0..items.len()).filter(|&i| scores[i] > 0).collect();
    by_keyword.sort_by(|&a, &b| {
        scores[b]
            .cmp(&scores[a])
            .then_with(|| recency(&items[b]).cmp(&recency(&items[a])))
    });
    add_ranking(&by_keyword, &mut fused);
    if let Some(config) = &semantic {
        match index::similarities(conn, config, topic, &items) {
            Ok(similar) => {
                let mut by_meaning: Vec<usize> = (0..items.len())
                    .filter(|&i| similar.contains_key(&items[i].id))
                    .collect();
                by_meaning.sort_by(|&a, &b| {
                    similar[&items[b].id]
                        .total_cmp(&similar[&items[a].id])
                        .then_with(|| recency(&items[b]).cmp(&recency(&items[a])))
                });
                let floor = similarity_floor(similar.values().copied());
                for &i in by_meaning.iter().take(limit) {
                    if similar[&items[i].id] >= floor {
                        matched[i] = true;
                    }
                }
                add_ranking(&by_meaning, &mut fused);
                semantic_ranked = true;
            }
            Err(err) => embed::warn(format!("{err:#}")),
        }
    }
    let local = |i: usize| items[i].scope == "project";
    let score = |i: usize| {
        if local(i) {
            fused[i] * (1.0 + PROJECT_BONUS)
        } else {
            fused[i]
        }
    };
    let mut order: Vec<usize> = (0..items.len()).filter(|&i| matched[i]).collect();
    // Without a topic every score is 0, so project memories come first.
    order.sort_by(|&a, &b| {
        score(b)
            .total_cmp(&score(a))
            .then_with(|| local(b).cmp(&local(a)))
            .then_with(|| recency(&items[b]).cmp(&recency(&items[a])))
    });
    let mut items: Vec<Option<Memory>> = items.into_iter().map(Some).collect();
    Ok(Ranked {
        memories: order.into_iter().filter_map(|i| items[i].take()).collect(),
        semantic: semantic_ranked,
    })
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
fn similarity_floor(similarities: impl Iterator<Item = f32>) -> f32 {
    let mut values: Vec<f32> = similarities.collect();
    if values.len() < SPREAD_SAMPLE {
        return f32::NEG_INFINITY;
    }
    values.sort_by(f32::total_cmp);
    let best = values[values.len() - 1];
    let median = values[values.len() / 2];
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
    let mut result = paged(ranked.memories, &paging)?;
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
            let mut found = candidates(&conn, &selection.filter()?, None)?;
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
