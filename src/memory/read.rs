//! Retrieval: ranked search and reading whole memories.
//!
//! Search considers every memory the selection covers (by default the active
//! global memories and the requested projects'). Keyword relevance weighs
//! category over type over text; with an embedding endpoint configured it is
//! fused with semantic similarity by reciprocal rank. Within the matches,
//! project memories come before global ones.

use super::{
    Memory,
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

/// Smoothing constant of reciprocal rank fusion; 60 is the customary value.
const FUSION: f64 = 60.0;

/// Memories a topic matches by meaning alone, on top of every keyword match:
/// beyond the closest few, similarity rarely means relevance.
const SEMANTIC_MATCHES: usize = 20;

#[derive(Args, Deserialize, Serialize, JsonSchema)]
pub struct Search {
    /// What to look for; without it every memory in scope matches, newest first.
    #[arg(default_value = "")]
    #[serde(default)]
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
    /// Up to 20 memory ids, each in full or as the first 8 characters the index shows. Give ids or a category, not both; ids take only the status filter.
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
/// Shortest id prefix accepted: the 8 characters the index shows. Fewer would
/// often match several memories.
pub(super) const SHORT_ID: usize = 8;

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

/// Memories matching `topic`, best first. Without a topic every candidate
/// matches. Semantic similarity, when available, adds the `limit` most similar
/// memories to the keyword matches and fuses both rankings.
pub(super) fn rank(
    conn: &Connection,
    filter: &Filter,
    topic: &str,
    limit: usize,
) -> Result<Vec<Memory>> {
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
                for &i in by_meaning.iter().take(limit) {
                    matched[i] = true;
                }
                add_ranking(&by_meaning, &mut fused);
            }
            Err(err) => embed::warn(format!("{err:#}")),
        }
    }
    let mut order: Vec<usize> = (0..items.len()).filter(|&i| matched[i]).collect();
    order.sort_by(|&a, &b| {
        let local = |i: usize| items[i].scope == "project";
        local(b)
            .cmp(&local(a))
            .then_with(|| fused[b].total_cmp(&fused[a]))
            .then_with(|| recency(&items[b]).cmp(&recency(&items[a])))
    });
    let mut items: Vec<Option<Memory>> = items.into_iter().map(Some).collect();
    Ok(order.into_iter().filter_map(|i| items[i].take()).collect())
}

/// `found` as one page of a result: `memories`, and `next_cursor` when more remain.
fn paged(found: Vec<Memory>, paging: &Paging) -> Result<Value> {
    let ids: Vec<Uuid> = found.iter().map(|m| m.id).collect();
    let (memories, next) = page(found, &ids, paging)?;
    let mut result = json!({ "memories": memories });
    if let Some(next) = next {
        result["next_cursor"] = json!(next);
    }
    Ok(result)
}

/// Memories matching `input`, best first, one page at a time.
pub fn search(db: &Path, input: &Search) -> Result<Value> {
    let conn = connect(db)?;
    let filter = input.selection.filter()?;
    paged(
        rank(&conn, &filter, &input.topic, SEMANTIC_MATCHES)?,
        &input.paging,
    )
}

/// The order memories take inside an index group or a category read: project
/// memories first, then those confirmed in more conversations, then newest.
pub(super) fn group_order(a: &Memory, b: &Memory) -> std::cmp::Ordering {
    (a.scope != "project")
        .cmp(&(b.scope != "project"))
        .then_with(|| b.confirmations.cmp(&a.confirmations))
        .then_with(|| recency(b).cmp(&recency(a)))
}

/// Ids that `wanted` names: a full id, or a prefix of at least [`SHORT_ID`]
/// characters. A prefix matching several memories is an error that lists them.
fn resolve(conn: &Connection, wanted: &str) -> Result<Option<Uuid>> {
    let wanted = wanted.trim().to_lowercase();
    if let Ok(id) = Uuid::parse_str(&wanted) {
        return Ok(get(conn, id)?.map(|m| m.id));
    }
    if wanted.chars().count() < SHORT_ID
        || !wanted.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
    {
        bail!("{wanted} is not a memory id: give a full id or its first {SHORT_ID} characters");
    }
    let matches: Vec<String> = conn
        .prepare("SELECT id FROM memories WHERE substr(id,1,length(?1))=?1 ORDER BY id")?
        .query_map([&wanted], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    match matches.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(Uuid::parse_str(one)?)),
        many => bail!("{wanted} matches several memories: {}", many.join(", ")),
    }
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
        _ => bail!("Give either ids or a category"),
    }
    let only_status = Selection {
        status: selection.status,
        ..Selection::default()
    };
    if selection.narrows_beyond(&only_status) {
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
    let mut result = json!({ "memories": memories });
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
