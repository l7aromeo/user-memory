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
    similar::{
        DUPLICATE_OVERLAP, Draft, RELATED_OVERLAP, Thresholds, category_group, cosine, document,
        overlap, stored_vectors, thresholds, tokenize,
    },
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
    /// Projects whose memories are reviewed with the global ones: project keys or git remote URLs, never local paths. Without any, only global memories are reviewed.
    // --projects is the tool's own name for it, which agents copy into the CLI.
    #[arg(long = "project", visible_alias = "projects")]
    #[serde(default)]
    pub projects: Vec<String>,
    /// Most groups to return: 5 unless set, at most 20.
    #[arg(long, default_value_t = GROUPS)]
    #[serde(default = "default_groups")]
    pub max_groups: usize,
    /// Members of a group the user chose to leave as they are, in full or as the index's 8 characters: every pair among them is marked distinct, changing no memory, so review stops grouping them. Needs user_confirmed.
    #[arg(long = "keep")]
    #[serde(default)]
    pub keep: Vec<MemoryRef>,
    /// True only when the user, in this conversation, chose to leave the memories in keep as they are.
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
            "Keeping memories as they are requires the user's approval".into(),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Duplicate,
    Redundant,
    Shared,
    Overlap,
    Stale,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Duplicate => "duplicate",
            Self::Redundant => "redundant",
            Self::Shared => "shared",
            Self::Overlap => "overlap",
            Self::Stale => "stale",
        }
    }
    fn reason(self) -> &'static str {
        match self {
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

/// How strongly two memories match: cosine when both have comparable stored
/// vectors, otherwise content-word overlap within one category group.
#[derive(Clone, Copy)]
struct Pair {
    duplicate: bool,
    related: bool,
    score: f32,
}

/// `cutoffs` holds the model's cutoffs by vector length; a length without
/// them is compared by keywords.
fn compare(
    a: &Memory,
    b: &Memory,
    vectors: &HashMap<Uuid, Vec<f32>>,
    cutoffs: &HashMap<usize, Thresholds>,
) -> Pair {
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

fn recency(m: &Memory) -> (&str, i64) {
    (&m.updated_at, m.sequence)
}

/// Members of `component`, closest first (strongest link, then id), at most
/// `MAX_MEMBERS`, and how many were left out.
fn closest(
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
    let memories = conn
        .prepare(&format!(
            "{SELECT} WHERE status IN ('confirmed','tentative','stale') AND (scope='global' OR (scope='project' AND project IN (SELECT value FROM json_each(?1)))) ORDER BY rowid"
        ))?
        .query_map([serde_json::to_string(&projects)?], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let distinct: HashSet<(Uuid, Uuid)> = conn
        .prepare("SELECT a,b FROM memory_distinct")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .filter_map(|pair| {
            let (a, b) = pair.ok()?;
            Some((Uuid::parse_str(&a).ok()?, Uuid::parse_str(&b).ok()?))
        })
        .collect();
    let (model, vectors) = match identity {
        Some(identity) => {
            let stored = stored_vectors(conn, identity)?;
            let current = memories
                .iter()
                .filter_map(|m| {
                    let (source, vector) = stored.get(&m.id)?;
                    (*source == document(&Draft::from(m))).then(|| (m.id, vector.clone()))
                })
                .collect();
            (identity.0.clone(), current)
        }
        None => (String::new(), HashMap::new()),
    };
    let mut cutoffs: HashMap<usize, Thresholds> = HashMap::new();
    for length in vectors.values().map(Vec::len).collect::<BTreeSet<_>>() {
        if let Some(found) = thresholds(conn, &model, length)? {
            cutoffs.insert(length, found);
        }
    }
    let active: Vec<usize> = (0..memories.len())
        .filter(|&i| memories[i].status != "stale")
        .collect();
    let mut duplicates = Vec::new();
    let mut related = Vec::new();
    for (n, &i) in active.iter().enumerate() {
        for &j in &active[n + 1..] {
            let (a, b) = (&memories[i], &memories[j]);
            if distinct.contains(&(a.id.min(b.id), a.id.max(b.id)))
                || (a.source == "observed" && b.source == "observed")
            {
                continue;
            }
            let pair = compare(a, b, &vectors, &cutoffs);
            if pair.duplicate {
                duplicates.push((i, j, pair.score));
            } else if pair.related {
                related.push((i, j, pair.score));
            }
        }
    }
    // Each group with how many members it left out.
    let mut groups: Vec<(Kind, Vec<usize>, usize)> = Vec::new();
    let mut joined = Groups::new(memories.len());
    for &(a, b, _) in &duplicates {
        joined.join(a, b);
    }
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
        let (members, left_out) = closest(&component, &duplicates, &memories);
        groups.push((kind, members, left_out));
    }
    let related: Vec<_> = related
        .into_iter()
        .filter(|(a, b, _)| !grouped.contains(a) && !grouped.contains(b))
        .collect();
    for group in overlaps(&related) {
        let (members, _) = closest(&group, &related, &memories);
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
                    json!({"id":m.id,"title":m.title,"content":m.content,"type":m.memory_type,"category":m.category,"scope":m.scope,"project":m.project,"status":m.status,"source":m.source,"confirmations":m.confirmations})
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
        id
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
        assert!(groups(&conn, None, true).is_empty());
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
}
