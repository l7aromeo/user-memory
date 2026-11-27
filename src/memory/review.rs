//! Consolidation review: groups of memories the user may want to merge, trim
//! or re-derive. Read-only and never embeds: it compares stored vectors and
//! content words with the same signals and cutoffs as recording, and only
//! proposes; merges and forgets happen with the user's approval.

use super::{
    Memory,
    ids::{MemoryRef, resolve_all, uuids},
    index::embed,
    project_key,
    schema::{SELECT, connect, get, row, write},
    selection::refuse_local_paths,
    similar::{Draft, Thresholds, document, stored_vectors, thresholds},
    write::mark_pair,
};
use anyhow::{Result, bail};
use clap::Args;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    cmp::Ordering,
    collections::{BTreeSet, HashMap, HashSet},
    path::Path,
};
use uuid::Uuid;

pub(super) mod kept;
mod pairs;

/// Groups returned unless asked otherwise: a few for the user to decide on
/// at a time.
const GROUPS: usize = 5;
/// Most groups one review returns: about as many as a user decides on in one
/// sitting.
const MAX_GROUPS: usize = 20;

fn default_groups() -> usize {
    GROUPS
}

#[derive(Args, Deserialize, Serialize, JsonSchema)]
pub struct Review {
    /// Projects reviewed with the global memories: project keys or git remote URLs, never local paths; none reviews only the global ones.
    // --projects is the tool's own name for it, which agents copy into the CLI.
    #[arg(long = "project", visible_alias = "projects")]
    #[serde(default)]
    pub projects: Vec<String>,
    /// Most groups to return: 5 unless set, at most 20.
    #[arg(long, default_value_t = GROUPS)]
    #[serde(default = "default_groups")]
    pub max_groups: usize,
    /// Members of a group the user chose to leave as they are, full ids or the index's 8 characters: each pair is marked distinct and no memory changes, so review stops grouping them. Needs user_confirmed.
    #[arg(long = "keep")]
    #[serde(default)]
    pub keep: Vec<MemoryRef>,
    /// True only when the user, in this conversation, chose to leave the memories in keep as they are. A message they typed, even mid-turn, or an answer they picked counts.
    #[arg(skip)]
    #[serde(default)]
    pub user_confirmed: bool,
}

/// Most memories one keep names: more than a group lists, for a group the
/// user chose to keep whole after reading some it left out.
const KEEP_LIMIT: usize = 20;

/// Why `keep` cannot be applied, or `None` once every pair among its
/// memories is marked distinct; how many pairs were new goes to `kept`.
fn keep(db: &Path, input: &Review, kept: &mut usize) -> Result<Option<String>> {
    if !input.user_confirmed {
        return Ok(Some(
            "Keeping memories as they are needs the user's choice: pass user_confirmed when the user chose to keep them.".into(),
        ));
    }
    let mut members = input.keep.clone();
    let conn = connect(db)?;
    let tx = write(&conn)?;
    if let Some(reason) = resolve_all(&tx, &mut members)? {
        return Ok(Some(reason));
    }
    let members = uuids(&members);
    if members.len() < 2 {
        return Ok(Some(
            "keep names at least two memories the user leaves as they are".into(),
        ));
    }
    if members.len() > KEEP_LIMIT {
        return Ok(Some(format!("keep names at most {KEEP_LIMIT} memories")));
    }
    for id in &members {
        if get(&tx, *id)?.is_none() {
            return Ok(Some("Memory does not exist".into()));
        }
    }
    for (i, one) in members.iter().enumerate() {
        for other in &members[i + 1..] {
            *kept += mark_pair(&tx, *one, *other)?;
        }
    }
    tx.commit()?;
    Ok(None)
}

/// Members listed per group. A larger group of duplicates or stale patterns
/// lists its closest members and says how many it left out; overlap groups
/// never grow past it.
const MAX_MEMBERS: usize = 8;

/// How far back the `recent` group looks: the rules and agents' memories a
/// user may not have seen yet, as long as a month between sessions.
const RECENT_AGE: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);
/// Members the `recent` group lists: more than other groups, since session
/// start sends the user here to read every new rule and agent's memory.
const RECENT_MEMBERS: usize = 20;
/// The history operations that add or change a rule: what it says, where
/// it applies, or that it applies again.
const RULE_CHANGES: &str = "'record','update','import','merge','revive'";
/// The history operations that add a memory.
const ADDITIONS: &str = "'record','import','merge'";
/// How far back the `ended` group looks: every end an agent made since the
/// user last reviewed, with as long as a month between sessions, as
/// `recent` does.
const REVIEW_ENDED_DAYS: u32 = 30;
/// How long a tag or text condition stays open before `open_condition`
/// asks whether it still holds: two months, long enough that a release
/// or a fix that was coming has had time to come.
const OPEN_CONDITION_DAYS: u32 = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    /// First, since session start's message points the user here.
    Recent,
    /// Next: an end an agent made loosens a rule, which the user checks.
    Ended,
    Duplicate,
    Redundant,
    Shared,
    Overlap,
    Stale,
    EndedSupport,
    OpenCondition,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Recent => "recent",
            Self::Duplicate => "duplicate",
            Self::Redundant => "redundant",
            Self::Shared => "shared",
            Self::Overlap => "overlap",
            Self::Stale => "stale",
            Self::Ended => "ended",
            Self::EndedSupport => "ended_support",
            Self::OpenCondition => "open_condition",
        }
    }
    fn reason(self) -> &'static str {
        match self {
            Self::Recent => {
                "Rules added or changed, then memories agents recorded, in the last 30 days. Show them to the user; forget a rule the user did not give, only with their yes."
            }
            Self::Duplicate => {
                "These say the same thing. With the user's approval, merge them by recording one memory that supersedes them all, or forget the extras."
            }
            Self::Redundant => {
                "A project memory repeats a global one. With the user's approval, forget the project copy; the global memory already applies there."
            }
            Self::Shared => {
                "The same rule in several projects, each needing it: a candidate for one global rule, only with the user's yes. Forget no copy otherwise."
            }
            Self::Overlap => {
                "Check these together: they may overlap, or one may be an exception to another. Change nothing unless the user asks."
            }
            Self::Stale => {
                "Patterns whose supporting memories changed or were forgotten. With the user's approval, forget them or re-derive them from current evidence."
            }
            Self::Ended => {
                "Conditions that ended in the last 30 days, with how and the evidence. Show them to the user; if one ended wrongly, update its until with their yes, which reopens it, or forget it."
            }
            Self::EndedSupport => {
                "Patterns citing a memory whose date has passed. With the user's approval, forget them or re-derive them from memories in force."
            }
            Self::OpenCondition => {
                "Tag or text conditions unchanged for over 60 days. Ask the user whether each still holds; with their yes, update its until or forget it."
            }
        }
    }
}

/// Connected components by union-find over memory indexes.
struct Groups(Vec<usize>);

impl Groups {
    fn new(size: usize) -> Self {
        Self((0..size).collect())
    }
    fn root(&mut self, mut i: usize) -> usize {
        while self.0[i] != i {
            self.0[i] = self.0[self.0[i]];
            i = self.0[i];
        }
        i
    }
    fn join(&mut self, a: usize, b: usize) {
        let (a, b) = (self.root(a), self.root(b));
        self.0[a.max(b)] = a.min(b);
    }
    /// Components with at least two members, each sorted by index.
    fn components(&mut self) -> Vec<Vec<usize>> {
        let mut found: HashMap<usize, Vec<usize>> = HashMap::new();
        for i in 0..self.0.len() {
            let root = self.root(i);
            found.entry(root).or_default().push(i);
        }
        found.into_values().filter(|c| c.len() > 1).collect()
    }
}

/// The standing rules among `memories` added or changed within
/// [`RECENT_AGE`], newest first, then the memories agents recorded in that
/// time: by their history's time, never `created_at` or `updated_at`, which
/// an import keeps from the file. Changes an undone import cancelled are
/// passed over, so a memory the undo gave back is as recent as the version
/// it is. A memory with no event in that time (a history pruned, or no
/// history) is not listed. At most
/// [`RECENT_MEMBERS`], with how many were left out.
fn recent(conn: &rusqlite::Connection, memories: &[Memory]) -> Result<(Vec<usize>, usize)> {
    let since = format!("-{} seconds", RECENT_AGE.as_secs());
    let mut when: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    let mut statement = conn.prepare(&format!(
        "SELECT memory_id,max(CASE WHEN op IN ({RULE_CHANGES}) THEN at END),max(CASE WHEN op IN ({ADDITIONS}) THEN at END) FROM memory_events e WHERE at>=datetime('now',?1) AND NOT {} GROUP BY memory_id",
        super::history::undone("e")
    ))?;
    let rows = statement.query_map([since], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?;
    for row in rows {
        let (id, at): (String, _) = row?;
        when.insert(id, at);
    }
    let mut found: Vec<(bool, String, usize)> = memories
        .iter()
        .enumerate()
        .filter(|(_, m)| m.status != "stale")
        .filter_map(|(i, m)| {
            let (changed, added) = when.get(&m.id.to_string())?;
            if super::read::standing_rule(m) {
                Some((false, changed.clone()?, i))
            } else if m.source != "explicit" {
                Some((true, added.clone()?, i))
            } else {
                None
            }
        })
        .collect();
    // Rules first, each part newest first.
    found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
    let left_out = found.len().saturating_sub(RECENT_MEMBERS);
    Ok((
        found
            .into_iter()
            .take(RECENT_MEMBERS)
            .map(|(_, _, i)| i)
            .collect(),
        left_out,
    ))
}

/// The end records written in the last [`REVIEW_ENDED_DAYS`], by memory:
/// when, how, the evidence and the conversation.
fn ended(conn: &rusqlite::Connection) -> Result<HashMap<Uuid, Value>> {
    let mut found = HashMap::new();
    let mut statement = conn.prepare(
        "SELECT memory_id,ended_at,how,evidence,origin FROM memory_ended WHERE ended_at>=datetime('now',?1)",
    )?;
    let rows = statement.query_map([format!("-{REVIEW_ENDED_DAYS} days")], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    for row in rows {
        let (id, at, how, evidence, origin) = row?;
        let Ok(id) = Uuid::parse_str(&id) else {
            continue;
        };
        let mut end = json!({"at": at, "how": how, "evidence": evidence});
        if !origin.is_empty() {
            end["origin"] = json!(origin);
        }
        found.insert(id, end);
    }
    Ok(found)
}

fn recency(m: &Memory) -> (&str, i64) {
    (&m.updated_at, m.sequence)
}

/// Each memory's strongest score among `links`, by memory index, for
/// [`closest`]; `f32::MIN` for a memory without one.
fn strongest(links: &[(usize, usize, f32)], size: usize) -> Vec<f32> {
    let mut best = vec![f32::MIN; size];
    for &(a, b, score) in links {
        for i in [a, b] {
            best[i] = best[i].max(score);
        }
    }
    best
}

/// Members of `component`, closest first (strongest link in `best`, from
/// [`strongest`], then id), at most `MAX_MEMBERS`, and how many were left
/// out.
fn closest(component: &[usize], best: &[f32], memories: &[Memory]) -> (Vec<usize>, usize) {
    let mut members = component.to_vec();
    members.sort_by(|&a, &b| {
        best[b]
            .partial_cmp(&best[a])
            .unwrap_or(Ordering::Equal)
            .then_with(|| memories[a].id.cmp(&memories[b].id))
    });
    let left_out = members.len().saturating_sub(MAX_MEMBERS);
    members.truncate(MAX_MEMBERS);
    (members, left_out)
}

/// Overlap groups from related `links`, strongest first, in which every
/// member is related to every other. A link joins a group holding one of
/// its memories when the other is related to all of that group's members and
/// the group has room; otherwise it starts a group of its own. So a memory
/// related to two that are not related to each other is listed with each,
/// never chaining them into one group, and every related pair is listed
/// together in some group.
fn overlaps(links: &[(usize, usize, f32)]) -> Vec<Vec<usize>> {
    let mut sorted = links.to_vec();
    sorted.sort_by(|x, y| y.2.partial_cmp(&x.2).unwrap_or(Ordering::Equal));
    let linked: HashSet<(usize, usize)> = links
        .iter()
        .map(|&(a, b, _)| (a.min(b), a.max(b)))
        .collect();
    let related = |a: usize, b: usize| linked.contains(&(a.min(b), a.max(b)));
    let mut groups: Vec<Vec<usize>> = Vec::new();
    // The groups holding each memory, by index in `groups`, ascending.
    let mut holding: HashMap<usize, Vec<usize>> = HashMap::new();
    for (a, b, _) in sorted {
        let of_a = holding.get(&a).map_or(&[][..], Vec::as_slice);
        let of_b = holding.get(&b).map_or(&[][..], Vec::as_slice);
        if of_a.iter().any(|&g| groups[g].contains(&b)) {
            continue;
        }
        let fits = |g: usize, held: usize, other: usize| {
            groups[g].len() < MAX_MEMBERS
                && groups[g].iter().all(|&m| m == held || related(m, other))
        };
        let joined = of_a
            .iter()
            .find(|&&g| fits(g, a, b))
            .map(|&g| (g, b))
            .or_else(|| of_b.iter().find(|&&g| fits(g, b, a)).map(|&g| (g, a)));
        match joined {
            Some((g, joining)) => {
                groups[g].push(joining);
                let held = holding.entry(joining).or_default();
                held.insert(held.partition_point(|&h| h < g), g);
            }
            None => {
                for i in [a, b] {
                    holding.entry(i).or_default().push(groups.len());
                }
                groups.push(vec![a, b]);
            }
        }
    }
    groups
}

/// Groups of memories worth consolidating, in a deterministic order:
/// duplicates, redundant project copies, rules shared by several projects,
/// overlaps, then stale patterns; within
/// a kind by the newest member, then the smallest id.
pub fn review(db: &Path, input: &Review) -> Result<Value> {
    let mut kept = 0;
    if !input.keep.is_empty()
        && let Some(reason) = keep(db, input, &mut kept)?
    {
        return Ok(json!({"kept":false,"reason":reason}));
    }
    let identity = embed::Config::from_env().map(|config| {
        let (model, dimensions) = config.identity();
        (model.to_owned(), dimensions)
    });
    let mut groups = review_with(&connect(db)?, input, identity.as_ref())?;
    if !input.keep.is_empty() {
        groups["kept"] = json!(kept);
    }
    Ok(groups)
}

/// `review` comparing stored vectors made with `identity` (model, requested
/// dimensions); `None` compares content words only.
fn review_with(
    conn: &rusqlite::Connection,
    input: &Review,
    identity: Option<&(String, i64)>,
) -> Result<Value> {
    refuse_local_paths(&input.projects)?;
    if !(1..=MAX_GROUPS).contains(&input.max_groups) {
        bail!("max_groups must be between 1 and {MAX_GROUPS}");
    }
    let projects: Vec<String> = input
        .projects
        .iter()
        .map(|p| project_key(p))
        .filter(|p| !p.is_empty())
        .collect();
    // One read snapshot for the memories, their vectors and the kept pairs,
    // so a vector saved meanwhile is told apart (`kept::keep`).
    let snapshot = conn.unchecked_transaction()?;
    let memories = snapshot
        .prepare(&format!(
            "{SELECT} WHERE status IN ('confirmed','tentative','stale') AND (scope='global' OR (scope='project' AND project IN (SELECT value FROM json_each(?1)))) ORDER BY rowid"
        ))?
        .query_map([serde_json::to_string(&projects)?], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let distinct: HashSet<(Uuid, Uuid)> = snapshot
        .prepare("SELECT a,b FROM memory_distinct")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .filter_map(|pair| {
            let (a, b) = pair.ok()?;
            Some((Uuid::parse_str(&a).ok()?, Uuid::parse_str(&b).ok()?))
        })
        .collect();
    // Memories out of force by their condition are compared with nothing:
    // a merge or trim of what no longer holds helps no one.
    let out_of_force: HashSet<Uuid> = snapshot
        .prepare("SELECT id FROM out_of_force")?
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(|id| Uuid::parse_str(&id.ok()?).ok())
        .collect();
    let mut cutoffs: HashMap<usize, Thresholds> = HashMap::new();
    let (vectors, near) = match identity {
        Some(identity) => {
            // Every stored vector of the model, of any memory: the kept
            // pairs cover them all, whatever this review looks at.
            let mut stored = stored_vectors(&snapshot, identity)?;
            for (_, vector) in stored.values() {
                if !cutoffs.contains_key(&vector.len())
                    && let Some(found) = thresholds(&snapshot, &identity.0, vector.len())?
                {
                    cutoffs.insert(vector.len(), found);
                }
            }
            let kept = kept::read(&snapshot, identity)?;
            let refreshed = kept::refresh(&stored, &cutoffs, &kept);
            snapshot.finish()?;
            // What was found holds whether or not it can be kept: a store
            // busy past the call's budget only leaves it for the next review.
            let _ = kept::keep(conn, identity, &kept, &refreshed);
            let current = memories
                .iter()
                .filter_map(|m| {
                    let (source, vector) = stored.remove(&m.id)?;
                    (source == document(&Draft::from(m))).then_some((m.id, vector))
                })
                .collect();
            (current, refreshed.near)
        }
        None => {
            snapshot.finish()?;
            (HashMap::new(), Vec::new())
        }
    };
    let active: Vec<usize> = (0..memories.len())
        .filter(|&i| memories[i].status != "stale" && !out_of_force.contains(&memories[i].id))
        .collect();
    let pairs::Links {
        duplicates,
        related,
    } = pairs::links(&memories, &active, &distinct, &vectors, &cutoffs, &near);
    // Each group with how many members it left out.
    let mut groups: Vec<(Kind, Vec<usize>, usize)> = Vec::new();
    let mut joined = Groups::new(memories.len());
    for &(a, b, _) in &duplicates {
        joined.join(a, b);
    }
    let strongest_duplicate = strongest(&duplicates, memories.len());
    let mut grouped = BTreeSet::new();
    for component in joined.components() {
        let place = |i: &usize| (&memories[*i].scope, &memories[*i].project);
        // Project copies are redundant only beside a global memory that
        // already applies there; without one, each project needs its own.
        let kind = if component.iter().all(|i| place(i) == place(&component[0])) {
            Kind::Duplicate
        } else if component.iter().any(|i| memories[*i].scope == "global") {
            Kind::Redundant
        } else {
            Kind::Shared
        };
        grouped.extend(component.iter().copied());
        let (members, left_out) = closest(&component, &strongest_duplicate, &memories);
        groups.push((kind, members, left_out));
    }
    let related: Vec<_> = related
        .into_iter()
        .filter(|(a, b, _)| !grouped.contains(a) && !grouped.contains(b))
        .collect();
    let strongest_related = strongest(&related, memories.len());
    for group in overlaps(&related) {
        let (members, _) = closest(&group, &strongest_related, &memories);
        groups.push((Kind::Overlap, members, 0));
    }
    let mut stale: Vec<usize> = (0..memories.len())
        .filter(|&i| memories[i].status == "stale")
        .collect();
    stale.sort_by(|&a, &b| {
        recency(&memories[b])
            .cmp(&recency(&memories[a]))
            .then_with(|| memories[a].id.cmp(&memories[b].id))
    });
    let left_out = stale.len().saturating_sub(MAX_MEMBERS);
    stale.truncate(MAX_MEMBERS);
    if !stale.is_empty() {
        groups.push((Kind::Stale, stale, left_out));
    }
    let (recent, left_out) = recent(conn, &memories)?;
    if !recent.is_empty() {
        groups.push((Kind::Recent, recent, left_out));
    }
    let ends = ended(conn)?;
    let mut ended_members: Vec<usize> = (0..memories.len())
        .filter(|&i| ends.contains_key(&memories[i].id))
        .collect();
    ended_members.sort_by(|&a, &b| {
        ends[&memories[b].id]["at"]
            .as_str()
            .cmp(&ends[&memories[a].id]["at"].as_str())
            .then_with(|| memories[a].id.cmp(&memories[b].id))
    });
    let left_out = ended_members.len().saturating_sub(RECENT_MEMBERS);
    ended_members.truncate(RECENT_MEMBERS);
    if !ended_members.is_empty() {
        groups.push((Kind::Ended, ended_members, left_out));
    }
    let passed: HashSet<Uuid> = conn
        .prepare(
            "SELECT id FROM memories WHERE until_kind='date' AND until_value<=strftime('%Y-%m-%dT%H:%M:%SZ','now')",
        )?
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(|id| Uuid::parse_str(&id.ok()?).ok())
        .collect();
    let mut citing: Vec<usize> = (0..memories.len())
        .filter(|&i| {
            memories[i].status != "stale"
                && memories[i]
                    .evidence_ids
                    .iter()
                    .any(|id| passed.contains(id))
        })
        .collect();
    citing.sort_by(|&a, &b| recency(&memories[b]).cmp(&recency(&memories[a])));
    let left_out = citing.len().saturating_sub(MAX_MEMBERS);
    citing.truncate(MAX_MEMBERS);
    if !citing.is_empty() {
        groups.push((Kind::EndedSupport, citing, left_out));
    }
    let open_before: String = conn.query_row(
        "SELECT datetime('now',?1)",
        [format!("-{OPEN_CONDITION_DAYS} days")],
        |r| r.get(0),
    )?;
    let mut open: Vec<usize> = (0..memories.len())
        .filter(|&i| {
            let m = &memories[i];
            m.until
                .as_ref()
                .is_some_and(|u| u.tag.is_some() || u.text.is_some())
                && !out_of_force.contains(&m.id)
                && m.status != "stale"
                && m.updated_at.as_str() < open_before.as_str()
        })
        .collect();
    open.sort_by(|&a, &b| recency(&memories[a]).cmp(&recency(&memories[b])));
    let left_out = open.len().saturating_sub(MAX_MEMBERS);
    open.truncate(MAX_MEMBERS);
    if !open.is_empty() {
        groups.push((Kind::OpenCondition, open, left_out));
    }
    let newest = |members: &[usize]| members.iter().map(|&i| recency(&memories[i])).max();
    let smallest = |members: &[usize]| members.iter().map(|&i| memories[i].id).min();
    groups.sort_by(|(ka, a, _), (kb, b, _)| {
        ka.cmp(kb)
            .then_with(|| newest(b).cmp(&newest(a)))
            .then_with(|| smallest(a).cmp(&smallest(b)))
    });
    let groups: Vec<Value> = groups
        .into_iter()
        .take(input.max_groups)
        .map(|(kind, members, left_out)| {
            let members: Vec<Value> = members
                .into_iter()
                .map(|i| {
                    let m = &memories[i];
                    let mut member = json!({"id":m.id,"title":m.title,"content":m.content,"type":m.memory_type,"category":m.category,"scope":m.scope,"project":m.project,"status":m.status,"source":m.source,"confirmations":m.confirmations});
                    if let Some(until) = &m.until {
                        member["until"] = json!(until);
                    }
                    if kind == Kind::Ended
                        && let Some(end) = ends.get(&m.id)
                    {
                        member["ended"] = end.clone();
                    }
                    super::note_tentative(&mut member);
                    member
                })
                .collect();
            let mut group = json!({"kind":kind.name(),"reason":kind.reason(),"members":members});
            if left_out > 0 {
                group["left_out"] = json!(left_out);
            }
            group
        })
        .collect();
    Ok(json!({ "groups": groups }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::similar::{DraftVector, built_in, save_vector};
    use rusqlite::{Connection, params};

    const MODEL: &str = "qwen3-embedding:4b";
    const LENGTH: usize = 2560;

    fn identity() -> (String, i64) {
        (MODEL.into(), 0)
    }

    /// A unit vector at cosine `similarity` to the first axis, leaning
    /// towards `axis` (memories leaning on different axes stay apart).
    fn towards(axis: usize, similarity: f32) -> Vec<f32> {
        let mut v = vec![0.0; LENGTH];
        v[0] = similarity;
        v[axis] = (1.0 - similarity * similarity).sqrt();
        v
    }

    fn unit(similarity: f32) -> Vec<f32> {
        towards(1, similarity)
    }

    fn store() -> (tempfile::TempDir, std::path::PathBuf, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memory.sqlite3");
        let conn = connect(&db).unwrap();
        (dir, db, conn)
    }

    struct New<'a> {
        memory_type: &'a str,
        category: &'a str,
        content: &'a str,
        project: &'a str,
        source: &'a str,
        status: &'a str,
    }

    fn preference<'a>(category: &'a str, content: &'a str) -> New<'a> {
        New {
            memory_type: "preference",
            category,
            content,
            project: "",
            source: "explicit",
            status: "confirmed",
        }
    }

    fn insert(conn: &Connection, m: New) -> Uuid {
        let id = Uuid::new_v4();
        let scope = if m.project.is_empty() {
            "global"
        } else {
            "project"
        };
        let evidence = if m.source == "observed" {
            "seen once"
        } else {
            ""
        };
        conn.execute(
            "INSERT INTO memories (id,memory_type,category,content,scope,project,source,status,evidence,title) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?4)",
            params![id.to_string(), m.memory_type, m.category, m.content, scope, m.project, m.source, m.status, evidence],
        )
        .unwrap();
        // Recorded long ago: only the tests of the recent group give a
        // memory a recent history.
        conn.execute(
            "UPDATE memory_events SET at=datetime('now','-400 days') WHERE memory_id=?1",
            [id.to_string()],
        )
        .unwrap();
        id
    }

    /// Gives `id` a history event `op` made `days` ago.
    fn event(conn: &Connection, id: Uuid, op: &str, days: i64) {
        conn.execute(
            "INSERT INTO memory_events (memory_id,op,at) VALUES (?1,?2,datetime('now',?3))",
            params![id.to_string(), op, format!("-{days} days")],
        )
        .unwrap();
    }

    /// The members of the recent group, or none.
    fn recent_group(conn: &Connection) -> Vec<Uuid> {
        groups(conn, None, false)
            .into_iter()
            .find(|(kind, _)| kind == "recent")
            .map(|(_, ids)| ids)
            .unwrap_or_default()
    }

    /// A condition's end shows in `ended` with its evidence, a pattern
    /// citing a memory past its date in `ended_support`, and a tag or text
    /// condition unchanged for over 60 days in `open_condition`; a memory
    /// out of force joins no duplicate group.
    #[test]
    fn conditions_give_groups_of_their_own() {
        let (_dir, _db, conn) = store();
        let set = |id: Uuid, columns: &str| {
            conn.execute(
                &format!("UPDATE memories SET {columns} WHERE id=?1"),
                [id.to_string()],
            )
            .unwrap();
        };
        let ended = insert(&conn, preference("workflow", "Skip the slow tests always"));
        // Its duplicate, in force.
        let twin = insert(&conn, preference("workflow", "Skip the slow tests always"));
        set(ended, "until_kind='text',until_value='the parser ships'");
        conn.execute(
            "INSERT INTO memory_ended (memory_id,how,evidence,origin) VALUES (?1,'evidence','ran the release: parser 2.0 out','chat/end')",
            [ended.to_string()],
        )
        .unwrap();
        let dated = insert(&conn, preference("tooling", "Merge without review"));
        set(
            dated,
            "until_kind='date',until_value='2001-01-01T00:00:00Z'",
        );
        let other = insert(&conn, preference("testing", "Name tests by scenario"));
        let pattern = insert(
            &conn,
            New {
                memory_type: "pattern",
                source: "inferred",
                status: "tentative",
                ..preference("tooling", "Moves fast on small changes")
            },
        );
        conn.execute(
            "UPDATE memories SET evidence_ids=?2 WHERE id=?1",
            params![pattern.to_string(), json!([dated, other]).to_string()],
        )
        .unwrap();
        let open = insert(&conn, preference("release", "Hold the release notes"));
        set(
            open,
            "until_kind='text',until_value='the docs ship',updated_at=datetime('now','-61 days')",
        );
        let young = insert(&conn, preference("design", "Sketch before building"));
        set(
            young,
            "until_kind='tag',until_value='v1.0.0',until_project='github.com/example/app',updated_at=datetime('now','-59 days')",
        );
        let found = groups(&conn, None, false);
        let group = |kind: &str| {
            found
                .iter()
                .filter(|(k, _)| k == kind)
                .flat_map(|(_, ids)| ids.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(group("ended"), [ended], "{found:?}");
        assert_eq!(group("ended_support"), [pattern], "{found:?}");
        assert_eq!(group("open_condition"), [open], "{found:?}");
        assert!(
            group("duplicate").is_empty(),
            "{twin} grouped with an ended memory: {found:?}"
        );
        let reply = review_with(
            &conn,
            &Review {
                projects: Vec::new(),
                max_groups: 20,
                keep: Vec::new(),
                user_confirmed: false,
            },
            None,
        )
        .unwrap();
        let member = reply["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["kind"] == "ended")
            .unwrap()["members"][0]
            .clone();
        assert_eq!(
            member["ended"]["evidence"],
            "ran the release: parser 2.0 out"
        );
        assert_eq!(member["ended"]["origin"], "chat/end");
        assert_eq!(member["until"], json!({"text":"the parser ships"}));
    }

    #[test]
    fn the_recent_group_lists_rules_then_agents_memories_by_their_history() {
        let (_dir, _db, conn) = store();
        let old_rule = insert(&conn, preference("workflow", "Indent with tabs always"));
        // A duplicate of it, recorded long ago: a group of another kind.
        insert(&conn, preference("workflow", "Indent with tabs always"));
        let new_rule = insert(&conn, preference("tooling", "Prefer the project runner"));
        let imported = insert(&conn, preference("testing", "Name tests by scenario"));
        let confirmed = insert(&conn, preference("style", "Write short sentences"));
        let seen = insert(
            &conn,
            New {
                memory_type: "fact",
                source: "observed",
                status: "tentative",
                ..preference("workflow", "Runs the formatter before commits")
            },
        );
        let stated = insert(
            &conn,
            New {
                memory_type: "fact",
                ..preference("workflow", "Works in the morning mostly")
            },
        );
        event(&conn, old_rule, "update", 31);
        event(&conn, new_rule, "update", 2);
        // An import keeps the file's dates; its event is from today.
        event(&conn, imported, "import", 1);
        // A confirmation changes nothing the rule says.
        event(&conn, confirmed, "confirm", 1);
        event(&conn, seen, "record", 3);
        event(&conn, stated, "record", 3);
        assert_eq!(recent_group(&conn), [imported, new_rule, seen]);
        let found = review_with(
            &conn,
            &Review {
                projects: Vec::new(),
                max_groups: 1,
                keep: Vec::new(),
                user_confirmed: false,
            },
            None,
        )
        .unwrap();
        assert_eq!(found["groups"][0]["kind"], "recent", "first: {found}");
        assert!(
            groups(&conn, None, false)
                .iter()
                .any(|(kind, _)| kind == "duplicate"),
            "another kind to come before"
        );
        // History pruned: nothing recent to list.
        conn.execute("DELETE FROM memory_events", []).unwrap();
        assert_eq!(recent_group(&conn), Vec::<Uuid>::new());
    }

    fn embed_stored(conn: &Connection, id: Uuid, vector: Vec<f32>) {
        let m = conn
            .query_row(&format!("{SELECT} WHERE id=?1"), [id.to_string()], row)
            .unwrap();
        let v = DraftVector {
            vector,
            identity: identity(),
            source: document(&Draft::from(&m)),
        };
        save_vector(conn, id, &v).unwrap();
    }

    fn groups(
        conn: &Connection,
        project: Option<&str>,
        semantic: bool,
    ) -> Vec<(String, Vec<Uuid>)> {
        let input = Review {
            projects: project.map(str::to_owned).into_iter().collect(),
            max_groups: 20,
            keep: Vec::new(),
            user_confirmed: false,
        };
        let id = identity();
        let found = review_with(conn, &input, semantic.then_some(&id)).unwrap();
        found["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| {
                let ids = g["members"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|m| Uuid::parse_str(m["id"].as_str().unwrap()).unwrap())
                    .collect();
                (g["kind"].as_str().unwrap().to_owned(), ids)
            })
            .collect()
    }

    fn sorted(mut ids: Vec<Uuid>) -> Vec<Uuid> {
        ids.sort();
        ids
    }

    #[test]
    fn every_listed_project_is_reviewed_with_the_global_memories() {
        let (_dir, _db, conn) = store();
        let mut pairs = Vec::new();
        for (project, content) in [
            ("lab", "Run tests before committing"),
            ("shop", "Deploy only on Mondays"),
            ("elsewhere", "Write changelogs in past tense"),
        ] {
            let pair: Vec<Uuid> = [content.to_owned(), format!("{content} always")]
                .iter()
                .map(|content| {
                    insert(
                        &conn,
                        New {
                            project,
                            ..preference("workflow", content)
                        },
                    )
                })
                .collect();
            pairs.push(pair);
        }
        let reviewed = |projects: &[&str]| -> Vec<Vec<Uuid>> {
            let found = review_with(
                &conn,
                &Review {
                    projects: projects.iter().map(|p| (*p).to_owned()).collect(),
                    max_groups: 20,
                    keep: Vec::new(),
                    user_confirmed: false,
                },
                None,
            )
            .unwrap();
            let mut groups: Vec<Vec<Uuid>> = found["groups"]
                .as_array()
                .unwrap()
                .iter()
                .map(|g| {
                    let mut ids: Vec<Uuid> = g["members"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|m| m["id"].as_str().unwrap().parse().unwrap())
                        .collect();
                    ids.sort();
                    ids
                })
                .collect();
            groups.sort();
            groups
        };
        let sorted = |pair: &Vec<Uuid>| {
            let mut pair = pair.clone();
            pair.sort();
            pair
        };
        let mut both = vec![sorted(&pairs[0]), sorted(&pairs[1])];
        both.sort();
        assert_eq!(reviewed(&["lab", "shop"]), both);
        assert_eq!(reviewed(&["lab"]), [sorted(&pairs[0])]);
        assert!(reviewed(&[]).is_empty());
    }

    #[test]
    fn duplicates_in_one_place_and_project_copies_of_global_memories() {
        let (_dir, _db, conn) = store();
        let a = insert(
            &conn,
            preference("programming", "Prefer Rust for command-line tools"),
        );
        let b = insert(
            &conn,
            preference("coding", "Prefer Rust for command-line tools always"),
        );
        let local = insert(
            &conn,
            New {
                project: "lab",
                ..preference("programming", "Prefer Rust for command-line tools")
            },
        );
        insert(
            &conn,
            New {
                project: "elsewhere",
                ..preference("programming", "Prefer Rust for command-line tools")
            },
        );
        let replaced = insert(
            &conn,
            preference("programming", "Prefer Rust for command-line tools"),
        );
        conn.execute(
            "UPDATE memories SET status='superseded',superseded_by=?1 WHERE id=?2",
            [a.to_string(), replaced.to_string()],
        )
        .unwrap();
        let found = groups(&conn, None, false);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "duplicate");
        assert_eq!(sorted(found[0].1.clone()), sorted(vec![a, b]));
        let here = groups(&conn, Some("lab"), false);
        assert_eq!(here.len(), 1);
        assert_eq!(here[0].0, "redundant");
        assert_eq!(sorted(here[0].1.clone()), sorted(vec![a, b, local]));
    }

    // The same rule in two places is redundant only where a global memory
    // already covers a project; in sibling projects alone each copy is that
    // project's rule, shared rather than redundant.
    #[test]
    fn copies_of_one_rule_are_shared_across_projects_and_redundant_beside_a_global_one() {
        const RULE: &str = "Run the linter before every commit";
        for (places, kind) in [
            (&["lab", "shop"][..], "shared"),
            (&["", "lab"], "redundant"),
            (&["", "lab", "shop"], "redundant"),
            (&["lab", "lab", "shop"], "shared"),
        ] {
            let (_dir, _db, conn) = store();
            let ids: Vec<Uuid> = places
                .iter()
                .enumerate()
                .map(|(n, project)| {
                    let content = if n == 0 {
                        RULE.to_owned()
                    } else {
                        format!("{RULE} always")
                    };
                    insert(
                        &conn,
                        New {
                            project,
                            ..preference("workflow", &content)
                        },
                    )
                })
                .collect();
            let found = review_with(
                &conn,
                &Review {
                    projects: vec!["lab".into(), "shop".into()],
                    max_groups: 20,
                    keep: Vec::new(),
                    user_confirmed: false,
                },
                None,
            )
            .unwrap();
            let groups = found["groups"].as_array().unwrap();
            assert_eq!(groups.len(), 1, "{places:?}: {found}");
            assert_eq!(groups[0]["kind"], kind, "{places:?}: {found}");
            let mut members: Vec<Uuid> = groups[0]["members"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["id"].as_str().unwrap().parse().unwrap())
                .collect();
            members.sort();
            assert_eq!(members, sorted(ids), "{places:?}");
        }
    }

    /// A unit vector mixing `parts` (axis, weight), scaled to length one.
    fn mix(parts: &[(usize, f32)]) -> Vec<f32> {
        let mut v = vec![0.0; LENGTH];
        for &(axis, weight) in parts {
            v[axis] += weight;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / norm).collect()
    }

    /// Workflow preferences with unrelated words, so only their vectors
    /// relate them.
    fn rules(conn: &Connection, n: usize) -> Vec<Uuid> {
        const WORDS: [&str; 12] = [
            "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
            "juliet", "kilo", "lima",
        ];
        const MORE: [&str; 12] = [
            "mercury", "venus", "earth", "mars", "jupiter", "saturn", "uranus", "neptune", "pluto",
            "ceres", "eris", "haumea",
        ];
        (0..n)
            .map(|i| {
                let content = format!("{} {}", WORDS[i], MORE[i]);
                insert(conn, preference("workflow", &content))
            })
            .collect()
    }

    fn holding(found: &[(String, Vec<Uuid>)], a: Uuid, b: Uuid) -> bool {
        found
            .iter()
            .any(|(_, ids)| ids.contains(&a) && ids.contains(&b))
    }

    // A memory related to two that are not related to each other links each
    // pair, never the two: review does not chain unrelated memories.
    #[test]
    fn a_chain_of_related_memories_never_groups_its_ends() {
        let (_dir, _db, conn) = store();
        let [a, b, c] = rules(&conn, 3)[..] else {
            unreachable!()
        };
        // a·b = b·c ≈ 0.71, a·c ≈ 0.25: related only along the chain.
        embed_stored(&conn, a, mix(&[(0, 0.5), (1, 0.707), (2, 0.5)]));
        embed_stored(&conn, b, mix(&[(1, 0.5), (2, 0.707), (3, 0.5)]));
        embed_stored(&conn, c, mix(&[(2, 0.5), (3, 0.707), (4, 0.5)]));
        let found = groups(&conn, None, true);
        assert!(found.iter().all(|(kind, _)| kind == "overlap"), "{found:?}");
        assert!(holding(&found, a, b), "{found:?}");
        assert!(holding(&found, b, c), "{found:?}");
        assert!(!holding(&found, a, c), "{found:?}");
    }

    // However many memories a run of links connects, a weak pair at its edge
    // is still listed together, not cut off to fit a group.
    #[test]
    fn a_long_run_of_links_hides_no_pair() {
        let (_dir, _db, conn) = store();
        let ids = rules(&conn, 12);
        for (i, id) in ids.iter().take(10).enumerate() {
            let axis = 10 + i;
            embed_stored(
                &conn,
                *id,
                mix(&[(axis, 0.5), (axis + 1, 0.707), (axis + 2, 0.5)]),
            );
        }
        // x relates to the last of the run (0.6), y only to x (0.56).
        let last = mix(&[(19, 0.5), (20, 0.707), (21, 0.5)]);
        let x: Vec<f32> = last
            .iter()
            .enumerate()
            .map(|(axis, v)| 0.6 * v + if axis == 40 { 0.8 } else { 0.0 })
            .collect();
        let y: Vec<f32> = x
            .iter()
            .enumerate()
            .map(|(axis, v)| {
                0.56 * v
                    + if axis == 41 {
                        (1.0_f32 - 0.56 * 0.56).sqrt()
                    } else {
                        0.0
                    }
            })
            .collect();
        embed_stored(&conn, ids[10], x);
        embed_stored(&conn, ids[11], y);
        let found = groups(&conn, None, true);
        assert!(holding(&found, ids[10], ids[11]), "{found:?}");
        assert!(
            found
                .iter()
                .all(|(_, members)| members.len() <= MAX_MEMBERS)
        );
    }

    // Ten memories all related to each other do not fit one group: they are
    // split into groups of at most eight, and every pair is listed together
    // in one of them.
    #[test]
    fn related_memories_too_many_for_one_group_are_split_without_losing_a_pair() {
        let (_dir, _db, conn) = store();
        let ids = rules(&conn, 10);
        // Pairwise cosine 0.6: related, not duplicates.
        for (i, id) in ids.iter().enumerate() {
            embed_stored(
                &conn,
                *id,
                mix(&[(0, 0.6_f32.sqrt()), (i + 1, 0.4_f32.sqrt())]),
            );
        }
        let found = groups(&conn, None, true);
        assert!(
            found.iter().all(|(_, m)| m.len() <= MAX_MEMBERS),
            "{found:?}"
        );
        for (n, a) in ids.iter().enumerate() {
            for b in &ids[n + 1..] {
                assert!(holding(&found, *a, *b), "{found:?}");
            }
        }
    }

    // More duplicates than a group lists: the closest are listed and the
    // group says how many more there are, so none disappears unannounced.
    #[test]
    fn a_group_too_large_to_list_says_how_many_it_left_out() {
        let (_dir, _db, conn) = store();
        for i in 0..10 {
            insert(
                &conn,
                preference("workflow", &format!("Run the linter before committing {i}")),
            );
        }
        let found = review_with(
            &conn,
            &Review {
                projects: Vec::new(),
                max_groups: 20,
                keep: Vec::new(),
                user_confirmed: false,
            },
            None,
        )
        .unwrap();
        let group = &found["groups"][0];
        assert_eq!(group["kind"], "duplicate", "{found}");
        assert_eq!(group["members"].as_array().unwrap().len(), MAX_MEMBERS);
        assert_eq!(group["left_out"], 2, "{found}");
    }

    #[test]
    fn related_memories_and_exceptions_are_overlaps_within_a_category_group() {
        let (_dir, _db, conn) = store();
        let rust = insert(&conn, preference("programming", "Prefer Rust for new code"));
        let go = insert(
            &conn,
            New {
                memory_type: "constraint",
                ..preference("programming", "Use Go for the services in this repository")
            },
        );
        let cooking = insert(&conn, preference("cooking", "Vegetarian recipes only"));
        embed_stored(&conn, rust, unit(1.0));
        embed_stored(&conn, go, towards(1, 0.65));
        embed_stored(&conn, cooking, towards(2, 0.70));
        let found = groups(&conn, None, true);
        assert_eq!(found, [("overlap".to_owned(), sorted(vec![rust, go]))]);
        // Without comparable vectors the content words share nothing.
        assert!(groups(&conn, None, false).is_empty());
    }

    #[test]
    fn keyword_overlap_between_the_related_and_duplicate_cutoffs_is_an_overlap() {
        let (_dir, _db, conn) = store();
        let a = insert(&conn, preference("tools", "Use Helix as the text editor"));
        let b = insert(&conn, preference("tools", "Helix editor keybindings"));
        let found = groups(&conn, None, false);
        assert_eq!(found, [("overlap".to_owned(), sorted(vec![a, b]))]);
    }

    #[test]
    fn distinct_pairs_and_observation_pairs_are_never_linked() {
        let (_dir, _db, conn) = store();
        let a = insert(&conn, preference("tools", "Use Helix as the text editor"));
        let b = insert(
            &conn,
            preference("tools", "Use Helix as the text editor please"),
        );
        let (low, high) = (a.min(b), a.max(b));
        conn.execute(
            "INSERT INTO memory_distinct (a,b) VALUES (?1,?2)",
            [low.to_string(), high.to_string()],
        )
        .unwrap();
        let observed = |content| New {
            memory_type: "behavior",
            source: "observed",
            status: "tentative",
            ..preference("workflow", content)
        };
        let seen = insert(&conn, observed("Asked for a demo during review"));
        let again = insert(&conn, observed("Asked for a demo during review again"));
        assert!(groups(&conn, None, false).is_empty());
        // An observation still matches an explicit memory.
        let stated = insert(
            &conn,
            preference("workflow", "Asked for a demo during review please"),
        );
        let found = groups(&conn, None, false);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            sorted(found[0].1.clone()),
            sorted(vec![seen, again, stated])
        );
    }

    fn infer(conn: &Connection, content: &str, supports: &[Uuid]) -> Uuid {
        let id = insert(
            conn,
            New {
                memory_type: "behavior",
                source: "inferred",
                ..preference("workflow", content)
            },
        );
        conn.execute(
            "UPDATE memories SET evidence_ids=?2 WHERE id=?1",
            params![id.to_string(), serde_json::to_string(supports).unwrap()],
        )
        .unwrap();
        id
    }

    #[test]
    fn an_inference_is_never_grouped_with_its_own_evidence() {
        let (_dir, _db, conn) = store();
        let one = insert(
            &conn,
            preference("workflow", "Asked for a demo during review"),
        );
        let two = insert(
            &conn,
            preference("workflow", "Asked for a demo during review again"),
        );
        let pattern = infer(&conn, "Asks for a demo during review", &[one, two]);
        let found = groups(&conn, None, false);
        assert!(
            found.iter().all(|(_, ids)| !ids.contains(&pattern)),
            "{found:?}"
        );
    }

    #[test]
    fn an_explicit_memory_outside_the_evidence_still_matches_the_pattern() {
        let (_dir, _db, conn) = store();
        let one = insert(&conn, preference("workflow", "Reviews start with a demo"));
        let two = insert(&conn, preference("workflow", "Reviews begin with a demo"));
        let pattern = infer(&conn, "Asks for a demo during review", &[one, two]);
        let stated = insert(
            &conn,
            preference("workflow", "Asks for a demo during review"),
        );
        let found = groups(&conn, None, false);
        assert!(
            found
                .iter()
                .any(|(_, ids)| ids.contains(&pattern) && ids.contains(&stated)),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .all(|(_, ids)| !(ids.contains(&pattern)
                    && (ids.contains(&one) || ids.contains(&two)))),
            "{found:?}"
        );
    }

    #[test]
    fn links_are_transitive_and_groups_keep_their_closest_eight() {
        let (_dir, _db, conn) = store();
        let ids: Vec<Uuid> = (0..10)
            .map(|i| insert(&conn, preference("tools", &format!("Editor note {i}"))))
            .collect();
        // A chain: each memory matches its neighbour, the ends do not match.
        for (i, &id) in ids.iter().enumerate() {
            let angle = i as f32 * 0.5;
            let mut v = vec![0.0; LENGTH];
            v[0] = angle.cos();
            v[1] = angle.sin();
            embed_stored(&conn, id, v);
        }
        let found = groups(&conn, None, true);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, "duplicate");
        assert_eq!(found[0].1.len(), MAX_MEMBERS);
    }

    #[test]
    fn large_groups_keep_the_members_with_the_strongest_links() {
        let (_dir, _db, conn) = store();
        // Memory k is at cosine 0.80 + 0.02k to the first axis, so pairs of
        // them meet at the product of their cosines: the weakest two link
        // only to the strongest and are the ones left out.
        let ids: Vec<Uuid> = (0..10)
            .map(|k| {
                let id = insert(&conn, preference("tools", &format!("Editor note {k}")));
                embed_stored(&conn, id, towards(k + 1, 0.80 + 0.02 * k as f32));
                id
            })
            .collect();
        let found = groups(&conn, None, true);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(sorted(found[0].1.clone()), sorted(ids[2..].to_vec()));
        assert_eq!(sorted(found[0].1[..2].to_vec()), sorted(ids[8..].to_vec()));
    }

    #[test]
    fn stale_patterns_form_their_own_group_last() {
        let (_dir, _db, conn) = store();
        let stale = insert(
            &conn,
            New {
                memory_type: "pattern",
                source: "inferred",
                status: "stale",
                ..preference("learning", "Learns by example")
            },
        );
        let a = insert(&conn, preference("tools", "Use Helix as the text editor"));
        let b = insert(
            &conn,
            preference("tools", "Use Helix as the text editor please"),
        );
        let found = groups(&conn, None, false);
        assert_eq!(
            found,
            [
                ("duplicate".to_owned(), sorted(vec![a, b])),
                ("stale".to_owned(), vec![stale])
            ]
        );
        let older = insert(
            &conn,
            New {
                memory_type: "pattern",
                source: "inferred",
                status: "stale",
                ..preference("learning", "Reads the manual first")
            },
        );
        for (newest, other) in [(stale, older), (older, stale)] {
            conn.execute("UPDATE memories SET updated_at='2020-01-01 00:00:00'", [])
                .unwrap();
            conn.execute(
                "UPDATE memories SET updated_at='2030-01-01 00:00:00' WHERE id=?1",
                [newest.to_string()],
            )
            .unwrap();
            let found = groups(&conn, None, false);
            assert_eq!(found[1], ("stale".to_owned(), vec![newest, other]));
        }
    }

    #[test]
    fn members_of_a_duplicate_group_join_no_overlap() {
        let (_dir, _db, conn) = store();
        let a = insert(&conn, preference("tools", "Use Helix as the text editor"));
        let b = insert(
            &conn,
            preference("tools", "Use Helix as the text editor please"),
        );
        insert(&conn, preference("tools", "Helix editor keybindings"));
        assert_eq!(
            groups(&conn, None, false),
            [("duplicate".to_owned(), sorted(vec![a, b]))]
        );
    }

    #[test]
    fn groups_are_ordered_by_kind_then_newest_member_and_limited() {
        let (_dir, _db, conn) = store();
        let old_a = insert(&conn, preference("tools", "Use Helix as the text editor"));
        let old_b = insert(
            &conn,
            preference("tools", "Use Helix as the text editor please"),
        );
        let new_a = insert(
            &conn,
            preference("shell", "Use Fish as the interactive shell"),
        );
        let new_b = insert(
            &conn,
            preference("shell", "Use Fish as the interactive shell please"),
        );
        let new = sorted(vec![new_a, new_b]);
        let old = sorted(vec![old_a, old_b]);
        for (newest, first, second) in [(new_b, &new, &old), (old_a, &old, &new)] {
            conn.execute("UPDATE memories SET updated_at='2020-01-01 00:00:00'", [])
                .unwrap();
            conn.execute(
                "UPDATE memories SET updated_at='2030-01-01 00:00:00' WHERE id=?1",
                [newest.to_string()],
            )
            .unwrap();
            let found = groups(&conn, None, false);
            assert_eq!(
                found,
                [
                    ("duplicate".to_owned(), first.clone()),
                    ("duplicate".to_owned(), second.clone())
                ]
            );
        }
        let review = |max_groups| {
            review_with(
                &conn,
                &Review {
                    projects: Vec::new(),
                    max_groups,
                    keep: Vec::new(),
                    user_confirmed: false,
                },
                None,
            )
        };
        assert_eq!(review(1).unwrap()["groups"].as_array().unwrap().len(), 1);
        for out_of_range in [0, 21] {
            assert_eq!(
                review(out_of_range).unwrap_err().to_string(),
                "max_groups must be between 1 and 20"
            );
        }
        let refused = review_with(
            &conn,
            &Review {
                projects: vec!["~/lab".into()],
                max_groups: 5,
                keep: Vec::new(),
                user_confirmed: false,
            },
            None,
        )
        .unwrap_err();
        assert!(refused.to_string().contains("is a local path"), "{refused}");
    }

    #[test]
    fn members_carry_the_fields_an_agent_needs_to_propose_a_merge() {
        let (_dir, _db, conn) = store();
        let a = insert(&conn, preference("tools", "Use Helix as the text editor"));
        insert(
            &conn,
            preference("tools", "Use Helix as the text editor please"),
        );
        conn.execute(
            "INSERT INTO memory_origins (memory_id,origin) VALUES (?1,'claude:session/a'),(?1,'codex:thread/b')",
            [a.to_string()],
        )
        .unwrap();
        let found = review_with(
            &conn,
            &Review {
                projects: Vec::new(),
                max_groups: 5,
                keep: Vec::new(),
                user_confirmed: false,
            },
            None,
        )
        .unwrap();
        let group = &found["groups"][0];
        assert!(
            group["reason"]
                .as_str()
                .unwrap()
                .contains("user's approval")
        );
        let member = group["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == a.to_string())
            .unwrap();
        for field in [
            "content", "type", "category", "scope", "project", "status", "source",
        ] {
            assert!(member.get(field).is_some(), "{field}");
        }
        assert_eq!(member["confirmations"], 2);
    }

    #[test]
    fn tool_calls_without_max_groups_return_five_groups() {
        let input: Review = serde_json::from_value(json!({})).unwrap();
        assert_eq!(
            (input.projects, input.max_groups),
            (Vec::<String>::new(), 5)
        );
    }

    #[test]
    fn review_never_writes_to_the_store() {
        let (_dir, db, conn) = store();
        let a = insert(&conn, preference("tools", "Use Helix as the text editor"));
        let b = insert(
            &conn,
            preference("tools", "Use Helix as the text editor please"),
        );
        embed_stored(&conn, a, unit(1.0));
        embed_stored(&conn, b, unit(0.9));
        drop(conn);
        let before = std::fs::read(&db).unwrap();
        let found = review(
            &db,
            &Review {
                projects: vec!["lab".into()],
                max_groups: 5,
                keep: Vec::new(),
                user_confirmed: false,
            },
        )
        .unwrap();
        assert_eq!(found["groups"].as_array().unwrap().len(), 1);
        assert_eq!(std::fs::read(&db).unwrap(), before);
    }

    #[test]
    fn vectors_of_outdated_text_are_not_compared() {
        let (_dir, _db, conn) = store();
        let a = insert(&conn, preference("tools", "Prefer one editor"));
        let b = insert(&conn, preference("tools", "Prefer another thing"));
        embed_stored(&conn, a, unit(1.0));
        embed_stored(&conn, b, unit(0.95));
        conn.execute(
            "UPDATE memories SET content='Plant tomatoes in spring' WHERE id=?1",
            [b.to_string()],
        )
        .unwrap();
        // The change is recent; no other group.
        assert_eq!(recent_group(&conn), [b]);
        assert!(
            groups(&conn, None, true)
                .iter()
                .all(|(kind, _)| kind == "recent")
        );
    }

    #[test]
    fn project_exceptions_just_above_the_related_cutoff_are_overlaps() {
        // Cosine of this exact pair with qwen3-embedding:4b at each length.
        for (length, measured) in [(2560, 0.568), (1024, 0.578)] {
            let (_dir, _db, conn) = store();
            let related = built_in(MODEL, length).unwrap().related;
            // A unit vector at cosine `similarity` to the first axis.
            let at = |axis: usize, similarity: f32| {
                let mut v = vec![0.0; length];
                v[0] = similarity;
                v[axis] = (1.0 - similarity * similarity).sqrt();
                v
            };
            let rule = insert(
                &conn,
                preference("programming", "Prefer Rust for command-line tools"),
            );
            let exception = insert(
                &conn,
                New {
                    memory_type: "constraint",
                    project: "lab",
                    ..preference("programming", "Use Go in this repository")
                },
            );
            let keys = insert(&conn, preference("tools", "Use 1Password for passwords"));
            let keyboard = insert(
                &conn,
                preference("tools", "Prefers a split mechanical keyboard"),
            );
            embed_stored(&conn, rule, at(1, 1.0));
            embed_stored(&conn, exception, at(2, measured));
            embed_stored(&conn, keys, at(3, 0.0));
            // Just below the cutoff to `keys`; far from the programming pair.
            let mut below = at(4, related - 0.005);
            below.swap(0, 3);
            embed_stored(&conn, keyboard, below);
            assert_eq!(
                groups(&conn, Some("lab"), true),
                [("overlap".to_owned(), sorted(vec![rule, exception]))],
                "{length}"
            );
        }
    }

    #[test]
    fn vectors_of_different_lengths_are_not_compared() {
        let (_dir, _db, conn) = store();
        let a = insert(&conn, preference("tools", "Prefer one editor"));
        let b = insert(&conn, preference("tools", "Plant tomatoes in spring"));
        embed_stored(&conn, a, unit(1.0));
        embed_stored(&conn, b, unit(1.0)[..1024].to_vec());
        assert!(groups(&conn, None, true).is_empty());
    }

    /// Overlap groups as they were built by scanning every group for each
    /// link, the reference for [`overlaps`].
    fn overlaps_scanning_every_group(links: &[(usize, usize, f32)]) -> Vec<Vec<usize>> {
        let mut sorted = links.to_vec();
        sorted.sort_by(|x, y| y.2.partial_cmp(&x.2).unwrap_or(Ordering::Equal));
        let linked: HashSet<(usize, usize)> = links
            .iter()
            .map(|&(a, b, _)| (a.min(b), a.max(b)))
            .collect();
        let related = |a: usize, b: usize| linked.contains(&(a.min(b), a.max(b)));
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for (a, b, _) in sorted {
            if groups.iter().any(|g| g.contains(&a) && g.contains(&b)) {
                continue;
            }
            let fits = |group: &Vec<usize>, held: usize, other: usize| {
                group.contains(&held)
                    && group.len() < MAX_MEMBERS
                    && group.iter().all(|&m| m == held || related(m, other))
            };
            match groups
                .iter()
                .position(|g| fits(g, a, b))
                .map(|i| (i, b))
                .or_else(|| groups.iter().position(|g| fits(g, b, a)).map(|i| (i, a)))
            {
                Some((i, joining)) => groups[i].push(joining),
                None => groups.push(vec![a, b]),
            }
        }
        groups
    }

    /// Members closest first as they were found by scanning every link, the
    /// reference for [`closest`].
    fn closest_scanning_every_link(
        component: &[usize],
        links: &[(usize, usize, f32)],
        memories: &[Memory],
    ) -> (Vec<usize>, usize) {
        let mut best: HashMap<usize, f32> = HashMap::new();
        for &(a, b, score) in links {
            for i in [a, b] {
                if component.contains(&i) {
                    let entry = best.entry(i).or_insert(f32::MIN);
                    *entry = entry.max(score);
                }
            }
        }
        let mut members = component.to_vec();
        members.sort_by(|&a, &b| {
            best[&b]
                .partial_cmp(&best[&a])
                .unwrap_or(Ordering::Equal)
                .then_with(|| memories[a].id.cmp(&memories[b].id))
        });
        let left_out = members.len().saturating_sub(MAX_MEMBERS);
        members.truncate(MAX_MEMBERS);
        (members, left_out)
    }

    // Groups indexed by member come out as scanning every group and link
    // made them, over dense link sets with tied scores, so that groups fill
    // up, members sit in several groups and order decides.
    #[test]
    fn indexed_groups_match_scanning_every_group() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move |below: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % below
        };
        let mut groups_seen = 0;
        for size in [6, 12, 25, 40] {
            for _ in 0..60 {
                let memories: Vec<Memory> = (0..size)
                    .map(|_| Memory {
                        id: Uuid::from_u128(u128::from(next(u64::MAX))),
                        ..Memory::default()
                    })
                    .collect();
                let mut links = Vec::new();
                for a in 0..size {
                    for b in a + 1..size {
                        if next(3) == 0 {
                            let score = [0.0, 0.25, 0.5, 0.75, 1.0][next(5) as usize];
                            links.push((a, b, score));
                        }
                    }
                }
                let groups = overlaps(&links);
                assert_eq!(groups, overlaps_scanning_every_group(&links));
                let best = strongest(&links, size);
                for group in &groups {
                    assert_eq!(
                        closest(group, &best, &memories),
                        closest_scanning_every_link(group, &links, &memories)
                    );
                }
                groups_seen += groups.len();
            }
        }
        assert!(groups_seen > 1000, "few groups to compare: {groups_seen}");
    }

    /// The model the kept-pairs tests embed with, and its vectors' length.
    const KEPT_MODEL: &str = "kept-test";
    const KEPT_LENGTH: usize = 8;

    /// A seeded xorshift stream, for the kept-pairs tests.
    struct Stream(u64);

    impl Stream {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            usize::try_from(self.0 % n as u64).unwrap()
        }
        /// A value in [-1, 1).
        fn unit(&mut self) -> f32 {
            let n = u16::try_from(self.below(1 << 16)).unwrap();
            f32::from(n) / 32768.0 - 1.0
        }
    }

    /// A vector near one of four directions, so pairs fall on both sides
    /// of the cutoffs.
    fn clustered(stream: &mut Stream) -> Vec<f32> {
        let axis = stream.below(4);
        (0..KEPT_LENGTH)
            .map(|d| f32::from(u8::from(d == axis)) + 0.2 * stream.unit())
            .collect()
    }

    fn kept_identity() -> (String, i64) {
        (KEPT_MODEL.into(), 0)
    }

    fn calibrate(conn: &Connection, duplicate: f32, related: f32) {
        conn.execute(
            "INSERT OR REPLACE INTO embedding_cutoffs (model,length,duplicate,related,calibrated_at) VALUES (?1,?2,?3,?4,datetime('now'))",
            params![KEPT_MODEL, i64::try_from(KEPT_LENGTH).unwrap(), duplicate, related],
        )
        .unwrap();
    }

    /// Saves `vector` for `id` from its present text, as made by `model`.
    fn embed_as(conn: &Connection, id: Uuid, model: &str, vector: Vec<f32>) {
        let m = conn
            .query_row(&format!("{SELECT} WHERE id=?1"), [id.to_string()], row)
            .unwrap();
        let v = DraftVector {
            vector,
            identity: (model.into(), 0),
            source: document(&Draft::from(&m)),
        };
        save_vector(conn, id, &v).unwrap();
    }

    /// Review of `projects` from the kept pairs, and from a copy of the store
    /// that keeps none, which compares every pair.
    fn kept_and_full(conn: &Connection, dir: &Path, projects: &[&str]) -> (Value, Value) {
        let input = Review {
            projects: projects.iter().map(|p| (*p).to_owned()).collect(),
            max_groups: MAX_GROUPS,
            keep: Vec::new(),
            user_confirmed: false,
        };
        let identity = kept_identity();
        let kept = review_with(conn, &input, Some(&identity)).unwrap();
        let copy = dir.join("full.sqlite3");
        let _ = std::fs::remove_file(&copy);
        conn.execute("VACUUM INTO ?1", [copy.to_str().unwrap()])
            .unwrap();
        let full_conn = connect(&copy).unwrap();
        full_conn
            .execute_batch("DELETE FROM memory_pairs_state; DELETE FROM memory_pairs;")
            .unwrap();
        let full = review_with(&full_conn, &input, Some(&identity)).unwrap();
        (kept, full)
    }

    // Review from kept pairs answers as comparing every pair does, after
    // every kind of change a store goes through: memories recorded,
    // edited, re-embedded, forgotten, superseded and brought back, every
    // vector dropped, the cutoffs raised and lowered, another model's
    // vector saved in place of one. Each review also brings the kept pairs
    // up to date, so the next one starts from what this one kept.
    #[test]
    fn reviews_from_kept_pairs_answer_as_comparing_every_pair() {
        let words = [
            "rust", "tests", "release", "format", "tabs", "review", "push", "docs",
        ];
        let (mut compared, mut reused, mut updated) = (0, 0, 0);
        for seed in 1..=6u64 {
            let (dir, _db, conn) = store();
            calibrate(&conn, 0.97, 0.9);
            let mut stream = Stream(0x9e37_79b9_7f4a_7c15 ^ seed);
            let mut ids: Vec<Uuid> = Vec::new();
            for step in 0..30 {
                // Up to three changes between reviews, so several vectors
                // can be due at once.
                for _ in 0..=stream.below(3) {
                    let choice = if ids.len() < 4 { 0 } else { stream.below(12) };
                    match choice {
                        0..=3 => {
                            let content: Vec<&str> =
                                (0..3).map(|_| words[stream.below(words.len())]).collect();
                            let content = content.join(" ");
                            let category = ["programming", "workflow"][stream.below(2)];
                            let project = ["", "github.com/x/y"][stream.below(2)];
                            let id = insert(
                                &conn,
                                New {
                                    project,
                                    ..preference(category, &content)
                                },
                            );
                            embed_as(&conn, id, KEPT_MODEL, clustered(&mut stream));
                            ids.push(id);
                        }
                        4 => {
                            let id = ids[stream.below(ids.len())];
                            conn.execute(
                                "UPDATE memories SET content=content||' edited' WHERE id=?1",
                                [id.to_string()],
                            )
                            .unwrap();
                        }
                        5 | 6 => {
                            let id = ids[stream.below(ids.len())];
                            embed_as(&conn, id, KEPT_MODEL, clustered(&mut stream));
                        }
                        7 => {
                            let id = ids.swap_remove(stream.below(ids.len()));
                            conn.execute("DELETE FROM memories WHERE id=?1", [id.to_string()])
                                .unwrap();
                        }
                        8 => {
                            let (id, by) = (ids[0], ids[1 + stream.below(ids.len() - 1)]);
                            let by = (stream.below(2) == 0).then(|| by.to_string());
                            let status = if by.is_some() {
                                "superseded"
                            } else {
                                "confirmed"
                            };
                            conn.execute(
                                "UPDATE memories SET status=?2,superseded_by=?3 WHERE id=?1",
                                params![id.to_string(), status, by],
                            )
                            .unwrap();
                        }
                        9 => {
                            let (duplicate, related) =
                                [(0.97, 0.9), (0.95, 0.85), (0.99, 0.95)][stream.below(3)];
                            calibrate(&conn, duplicate, related);
                        }
                        10 => {
                            let id = ids[stream.below(ids.len())];
                            embed_as(&conn, id, "other-model", clustered(&mut stream));
                        }
                        _ => {
                            if stream.below(4) == 0 {
                                conn.execute("DELETE FROM memory_vectors", []).unwrap();
                            } else {
                                for &id in &ids {
                                    embed_as(&conn, id, KEPT_MODEL, clustered(&mut stream));
                                }
                            }
                        }
                    }
                }
                let (state, due, pairs): (i64, i64, i64) = conn
                    .query_row(
                        "SELECT (SELECT COUNT(*) FROM memory_pairs_state),(SELECT COUNT(*) FROM memory_pairs_due WHERE model=?1),(SELECT COUNT(*) FROM memory_pairs)",
                        [KEPT_MODEL],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .unwrap();
                if state > 0 && pairs > 0 {
                    if due > 0 {
                        updated += 1;
                    } else {
                        reused += 1;
                    }
                }
                for projects in [&[][..], &["github.com/x/y"][..]] {
                    let (kept, full) = kept_and_full(&conn, dir.path(), projects);
                    assert_eq!(kept, full, "seed {seed}, step {step}, {projects:?}");
                    compared += kept["groups"].as_array().unwrap().len();
                }
            }
        }
        assert!(compared > 500, "few groups to compare: {compared}");
        // Reviews that took kept pairs as they were, and ones that brought
        // them up to date first.
        assert!(
            reused > 20 && updated > 50,
            "reused {reused}, updated {updated}"
        );
    }

    /// The due rows of the kept-pairs model, by number.
    fn due_rows(conn: &Connection) -> Vec<(i64, String)> {
        conn.prepare("SELECT seq,id FROM memory_pairs_due WHERE model=?1 ORDER BY seq")
            .unwrap()
            .query_map([KEPT_MODEL], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    // A review that cannot have the write turn still answers as comparing
    // every pair does, and leaves what it compared due, so the next review
    // compares it again; once the turn is free, a review keeps its pairs.
    #[test]
    fn a_review_kept_from_writing_answers_and_leaves_its_vectors_due() {
        let (dir, db, conn) = store();
        calibrate(&conn, 0.97, 0.9);
        let mut stream = Stream(7);
        let add = |conn: &Connection, stream: &mut Stream| {
            let id = insert(conn, preference("programming", "Prefer Rust"));
            embed_as(conn, id, KEPT_MODEL, clustered(stream));
        };
        for _ in 0..12 {
            add(&conn, &mut stream);
        }
        let (kept, full) = kept_and_full(&conn, dir.path(), &[]);
        assert_eq!(kept, full);
        assert!(
            due_rows(&conn).is_empty(),
            "the first review keeps its pairs"
        );
        for _ in 0..3 {
            add(&conn, &mut stream);
        }
        let due = due_rows(&conn);
        assert_eq!(due.len(), 3);
        let holder = Connection::open(&db).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        conn.busy_timeout(std::time::Duration::from_millis(50))
            .unwrap();
        let (kept, full) = kept_and_full(&conn, dir.path(), &[]);
        assert_eq!(kept, full);
        assert_eq!(due_rows(&conn), due, "nothing was kept");
        holder.execute_batch("ROLLBACK").unwrap();
        let (kept, full) = kept_and_full(&conn, dir.path(), &[]);
        assert_eq!(kept, full);
        assert!(due_rows(&conn).is_empty(), "kept once the turn is free");
    }
}
