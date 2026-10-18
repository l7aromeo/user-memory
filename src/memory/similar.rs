//! Similarity primitives shared by recall, recording and review: the text a
//! memory is embedded as, keyword tokens and category aliases, a bounded
//! document embedding for a memory about to be recorded, and the search for
//! memories that may say the same thing.
//!
//! The server never decides that two memories mean the same: `similar` only
//! lists candidates, and the caller makes the agent choose.

use super::{
    Memory, Scope, clean,
    index::embed,
    schema::{SELECT, row},
};
use anyhow::Result;
use rusqlite::{Connection, params};
use std::{cmp::Ordering, collections::BTreeSet, collections::HashMap, time::Duration};
use uuid::Uuid;

/// Category labels that mean the same at query time. Stored labels are kept
/// as the user gave them; unlisted labels match only themselves.
const CATEGORY_ALIASES: &[&[&str]] = &[
    &[
        "programming",
        "coding",
        "software development",
        "development",
    ],
    &["communication", "comms", "writing style"],
    &["workflow", "process"],
];

/// Function words carry no topic; one shared "in" must not make two memories
/// look alike or outrank a semantic match.
const STOP: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "can", "could", "do", "does", "for",
    "from", "how", "i", "if", "in", "into", "is", "it", "its", "me", "my", "of", "on", "or", "our",
    "should", "so", "than", "that", "the", "their", "them", "then", "there", "these", "they",
    "this", "to", "was", "we", "were", "what", "when", "where", "which", "who", "why", "will",
    "with", "would", "you", "your",
];

/// How long recording waits for an embedding. Covers a local model's cold
/// start (measured 4.9 s for qwen3-embedding:4b); a slower endpoint falls back
/// to the keyword signal.
pub(super) const RECORD_EMBED_TIMEOUT: Duration = Duration::from_secs(6);

/// Keyword signal: Jaccard overlap of content words within one category group
/// at or above which a memory is listed as a possible duplicate. Words catch
/// only close rewordings: in the labelled pairs, related memories overlap at
/// most 0.22 while paraphrases range 0.00–0.62, so this stays well above the
/// related class and leaves paraphrase detection to the semantic signal.
/// Long memories (1,000–4,000 characters, similarity-long-pairs.json) dilute
/// the overlap further: restated duplicates 0.22–0.36, exceptions 0.16–0.24,
/// unrelated rules sharing vocabulary 0.09–0.12. None is listed, and review
/// still links three of the five duplicates.
pub(super) const DUPLICATE_OVERLAP: f32 = 0.5;
/// Keyword overlap at or above which memories are worth reviewing together
/// (labelled unrelated pairs: 0.00; related: up to 0.22).
pub(super) const RELATED_OVERLAP: f32 = 0.25;

/// Semantic cutoffs for one embedding model at one vector length.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Thresholds {
    /// Cosine at or above which a memory is listed as a possible duplicate.
    pub(super) duplicate: f32,
    /// Cosine at or above which memories are worth reviewing together.
    pub(super) related: f32,
}

/// Cutoffs chosen by hand, by (model, vector length). Scores from different
/// models or lengths are not comparable: anything not listed gets cutoffs
/// computed by `calibration` from the same labelled pairs and the rules
/// below, stored with the store's vectors, and is matched by keywords only
/// until then or when the pairs cannot be separated. These rows always win.
///
/// Derived from the 78 labelled pairs in calibration/similarity-pairs.json
/// with `calibration_report` (local Ollama 0.40.4, 2026-10-15). Cosine per
/// class, min–max:
///
/// | class                       | 2560        | 1024        |
/// |-----------------------------|-------------|-------------|
/// | same                        | 0.834–0.972 | 0.844–0.973 |
/// | contradiction               | 0.797–0.944 | 0.826–0.946 |
/// | exception                   | 0.568–0.812 | 0.578–0.831 |
/// | related                     | 0.588–0.764 | 0.616–0.787 |
/// | unrelated, one category grp | 0.514–0.758 | 0.525–0.784 |
/// | unrelated, other groups     | 0.311–0.544 | 0.299–0.561 |
///
/// `duplicate` is the midpoint of the gap between related and same or
/// contradiction. Narrower project exceptions mostly score like related
/// memories and surface in review as overlaps; one that restates the same
/// setting with another value scores like a contradiction and is flagged at
/// record time, where the agent marks it distinct.
///
/// Long memories (1,000–4,000 characters, the 15 pairs in
/// calibration/similarity-long-pairs.json; local Ollama 0.40.6, 2026-10-19)
/// stay inside these cutoffs, cosine min–max:
///
/// | class                       | 2560        | 1024        |
/// |-----------------------------|-------------|-------------|
/// | same                        | 0.842–0.962 | 0.860–0.968 |
/// | exception                   | 0.719–0.857 | 0.727–0.871 |
/// | unrelated, one category grp | 0.543–0.721 | 0.599–0.739 |
///
/// Three of the five long exceptions score like duplicates and are flagged at
/// record time, where the agent marks them distinct; no cutoff can spare
/// them, since they overlap the long restatements.
///
/// `related` (review only) sits 0.01 or more below every exception and
/// related pair. No cutoff also keeps unrelated memories of one category
/// group apart (their range overlaps the exceptions'), so review proposes
/// some of them as overlaps; the user's approval decides. Memories in
/// different category groups are never linked, whatever their score.
const CALIBRATED: &[(&str, usize, Thresholds)] = &[
    (
        "qwen3-embedding:4b",
        2560,
        Thresholds {
            duplicate: 0.78,
            related: 0.55,
        },
    ),
    (
        "qwen3-embedding:4b",
        1024,
        Thresholds {
            duplicate: 0.81,
            related: 0.56,
        },
    ),
];

/// The built-in cutoffs for `model` at vector `length`, if any.
pub(super) fn built_in(model: &str, length: usize) -> Option<Thresholds> {
    CALIBRATED
        .iter()
        .find(|(m, l, _)| *m == model && *l == length)
        .map(|(_, _, t)| *t)
}

/// Cutoffs for `model` at vector `length`: the built-in ones, which a
/// computed result never overrides, else those calibration stored.
pub(super) fn thresholds(
    conn: &Connection,
    model: &str,
    length: usize,
) -> Result<Option<Thresholds>> {
    if let Some(cutoffs) = built_in(model, length) {
        return Ok(Some(cutoffs));
    }
    Ok(match super::calibration::state(conn, model, length)? {
        super::calibration::Calibration::Calibrated { duplicate, related } => {
            Some(Thresholds { duplicate, related })
        }
        _ => None,
    })
}

pub(super) fn tokenize(text: &str) -> BTreeSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && !STOP.contains(w))
        .map(str::to_owned)
        .collect()
}

/// `label` and every label sharing its alias group. The label is never cut,
/// so a long filter cannot match a shorter category it begins with.
pub(super) fn category_group(label: &str) -> Vec<String> {
    let label = clean(label, usize::MAX).to_lowercase();
    match CATEGORY_ALIASES
        .iter()
        .find(|group| group.contains(&label.as_str()))
    {
        Some(group) => group.iter().map(|l| (*l).to_owned()).collect(),
        None => vec![label],
    }
}

/// Labels of every alias group that a query word belongs to.
pub(super) fn aliased_categories(query: &BTreeSet<String>) -> Vec<String> {
    CATEGORY_ALIASES
        .iter()
        .filter(|group| group.iter().any(|l| !tokenize(l).is_disjoint(query)))
        .flat_map(|group| group.iter().map(|l| (*l).to_owned()))
        .collect()
}

/// The normalized fields of a memory, as stored or about to be recorded.
#[derive(Clone, Copy, Debug)]
pub(super) struct Draft<'a> {
    pub(super) memory_type: &'a str,
    pub(super) category: &'a str,
    pub(super) content: &'a str,
    pub(super) context: &'a str,
    pub(super) reason: &'a str,
    pub(super) evidence: &'a str,
    pub(super) scope: Scope,
    pub(super) project: &'a str,
}

impl<'a> From<&'a Memory> for Draft<'a> {
    fn from(m: &'a Memory) -> Self {
        Self {
            memory_type: &m.memory_type,
            category: &m.category,
            content: &m.content,
            context: &m.context,
            reason: &m.reason,
            evidence: &m.evidence,
            scope: if m.scope == "project" {
                Scope::Project
            } else {
                Scope::Global
            },
            project: &m.project,
        }
    }
}

/// The text a memory is embedded as, at record time and at recall time alike;
/// when it changes the memory is re-embedded.
pub(super) fn document(d: &Draft) -> String {
    let mut text = format!("{} ({}): {}", d.category, d.memory_type, d.content);
    for (label, value) in [
        ("Context", d.context),
        ("Reason", d.reason),
        ("Evidence", d.evidence),
    ] {
        if !value.is_empty() {
            text.push_str(&format!("\n{label}: {value}"));
        }
    }
    text
}

pub(super) fn to_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|v| v.to_le_bytes()).collect()
}
pub(super) fn from_blob(blob: &[u8]) -> Vec<f32> {
    blob.as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

pub(super) fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
    let scale = norm(a) * norm(b);
    if scale > 0.0 { dot / scale } else { 0.0 }
}

/// Jaccard overlap of two token sets; two empty sets do not overlap.
pub(super) fn overlap(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f32 {
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(b).count() as f32 / union as f32
}

/// A document embedding of a memory about to be recorded, with what makes
/// stored vectors comparable to it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct DraftVector {
    pub(super) vector: Vec<f32>,
    /// Model and requested dimensions (0 = the model's default length).
    pub(super) identity: (String, i64),
    /// The embedded text, `document(draft)`.
    pub(super) source: String,
}

/// Embeds `draft` as a document (never with the query instruction), waiting at
/// most `RECORD_EMBED_TIMEOUT`. `None` when semantic search is not configured
/// or the endpoint fails, after one warning per process. Call it before opening
/// a write transaction: it may block for the whole timeout.
pub(super) fn embed_draft(draft: &Draft) -> Option<DraftVector> {
    embed_draft_with(
        embed::Config::from_env().as_ref(),
        draft,
        RECORD_EMBED_TIMEOUT,
    )
}

fn embed_draft_with(
    config: Option<&embed::Config>,
    draft: &Draft,
    timeout: Duration,
) -> Option<DraftVector> {
    let config = config?;
    let source = document(draft);
    match config.embed_within(std::slice::from_ref(&source), timeout) {
        Ok(mut vectors) => {
            let (model, dimensions) = config.identity();
            Some(DraftVector {
                vector: vectors.remove(0),
                identity: (model.to_owned(), dimensions),
                source,
            })
        }
        Err(err) => {
            embed::warn(format!("{err:#}"));
            None
        }
    }
}

/// Saves `vector` as the embedding of memory `id`, only if that memory exists.
/// Runs no transaction of its own, so it can be part of the caller's.
pub(super) fn save_vector(conn: &Connection, id: Uuid, vector: &DraftVector) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO memory_vectors (id,model,dimensions,source,vector) SELECT ?1,?2,?3,?4,?5 WHERE EXISTS (SELECT 1 FROM memories WHERE id=?1)",
        params![
            id.to_string(),
            vector.identity.0,
            vector.identity.1,
            vector.source,
            to_blob(&vector.vector)
        ],
    )?;
    Ok(())
}

/// Where a memory about to be recorded applies.
#[derive(Clone, Copy, Debug)]
pub(super) enum Pool<'a> {
    /// A global memory: compared with global memories.
    Global,
    /// A project memory: compared with global memories and that project's.
    Project(&'a str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Signal {
    Semantic,
    Keyword,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Candidate {
    pub(super) id: Uuid,
    pub(super) signal: Signal,
    /// Cosine for a semantic match, Jaccard overlap for a keyword match.
    pub(super) score: f32,
}

/// Active memories in `pool` that may say the same as `draft`, best first:
/// semantic matches by score, then keyword matches by score, then id. Reads
/// only, so it can run inside the caller's write transaction.
///
/// - semantic: cosine of `vector` and a stored vector with the same identity
///   whose source is the stored memory's current document, at or above the
///   calibrated cutoff for that model and vector length (none: no semantic
///   signal);
/// - keyword: same category group and content-word overlap at or above
///   `DUPLICATE_OVERLAP`.
pub(super) fn similar(
    conn: &Connection,
    draft: &Draft,
    vector: Option<&DraftVector>,
    pool: Pool,
) -> Result<Vec<Candidate>> {
    let project = match pool {
        Pool::Global => "",
        Pool::Project(project) => project,
    };
    let memories = conn
        .prepare(&format!(
            "{SELECT} WHERE status IN ('confirmed','tentative') AND (scope='global' OR (scope='project' AND project=?1 AND ?1!=''))"
        ))?
        .query_map([project], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let semantic = match vector {
        Some(v) => thresholds(conn, &v.identity.0, v.vector.len())?.map(|t| (v, t.duplicate)),
        None => None,
    };
    let stored = match semantic {
        Some((v, _)) => stored_vectors(conn, &v.identity)?,
        None => HashMap::new(),
    };
    let group = category_group(draft.category);
    let words = tokenize(draft.content);
    let mut found = Vec::new();
    for m in &memories {
        let semantic_score = semantic.and_then(|(v, cutoff)| {
            let (source, other) = stored.get(&m.id)?;
            (other.len() == v.vector.len() && *source == document(&Draft::from(m)))
                .then(|| cosine(&v.vector, other))
                .filter(|&score| score >= cutoff)
        });
        let candidate = match semantic_score {
            Some(score) => Some((Signal::Semantic, score)),
            None => group
                .contains(&m.category)
                .then(|| overlap(&words, &tokenize(&m.content)))
                .filter(|&score| score >= DUPLICATE_OVERLAP)
                .map(|score| (Signal::Keyword, score)),
        };
        if let Some((signal, score)) = candidate {
            found.push(Candidate {
                id: m.id,
                signal,
                score,
            });
        }
    }
    found.sort_by(|a, b| {
        let rank = |c: &Candidate| c.signal == Signal::Keyword;
        rank(a)
            .cmp(&rank(b))
            .then_with(|| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal))
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(found)
}

/// Stored vectors made with `identity`, with the text they were embedded from.
pub(super) fn stored_vectors(
    conn: &Connection,
    (model, dimensions): &(String, i64),
) -> Result<HashMap<Uuid, (String, Vec<f32>)>> {
    let mut statement = conn
        .prepare("SELECT id,source,vector FROM memory_vectors WHERE model=?1 AND dimensions=?2")?;
    let rows = statement.query_map(params![model, dimensions], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Vec<u8>>(2)?,
        ))
    })?;
    let mut found = HashMap::new();
    for row in rows {
        let (id, source, blob) = row?;
        if let Ok(id) = Uuid::parse_str(&id) {
            found.insert(id, (source, from_blob(&blob)));
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn draft<'a>(memory_type: &'a str, category: &'a str, content: &'a str) -> Draft<'a> {
        Draft {
            memory_type,
            category,
            content,
            context: "",
            reason: "",
            evidence: "",
            scope: Scope::Global,
            project: "",
        }
    }

    #[test]
    fn documents_carry_only_the_conditions_that_are_set() {
        let mut d = draft("preference", "tools", "Use Helix");
        d.context = "When editing config";
        assert_eq!(
            document(&d),
            "tools (preference): Use Helix\nContext: When editing config"
        );
        d.context = "";
        d.reason = "Modal editing";
        d.evidence = "Said so twice";
        assert_eq!(
            document(&d),
            "tools (preference): Use Helix\nReason: Modal editing\nEvidence: Said so twice"
        );
    }

    #[test]
    fn cosine_is_scale_invariant_and_zero_safe() {
        let close = |a: f32, b: f32| (a - b).abs() < 1e-6;
        assert!(close(cosine(&[3.0, 4.0], &[3.0, 4.0]), 1.0));
        assert!(close(cosine(&[3.0, 4.0], &[6.0, 8.0]), 1.0));
        assert!(close(cosine(&[1.0, 0.0], &[0.0, 2.0]), 0.0));
        assert!(close(
            cosine(&[2.0, 0.0], &[1.0, 1.0]),
            std::f32::consts::FRAC_1_SQRT_2
        ));
        assert!(close(cosine(&[1.0, 0.0], &[-3.0, 0.0]), -1.0));
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn vectors_round_trip_through_blobs() {
        let vector = vec![0.5, -1.25, 3.0e-8];
        assert_eq!(from_blob(&to_blob(&vector)), vector);
    }

    #[test]
    fn overlap_is_jaccard_of_content_words_without_function_words() {
        let words = |text: &str| tokenize(text);
        assert_eq!(overlap(&words("Prefer Rust"), &words("prefer rust")), 1.0);
        assert_eq!(
            overlap(
                &words("Use the Helix editor"),
                &words("Helix is the editor")
            ),
            2.0 / 3.0
        );
        assert_eq!(
            overlap(&words("Prefer Rust"), &words("Prefer Python")),
            1.0 / 3.0
        );
        assert_eq!(overlap(&words("the a of"), &words("in on")), 0.0);
        assert_eq!(overlap(&words("Rust"), &words("Go")), 0.0);
    }

    const MODEL: &str = "qwen3-embedding:4b";
    const LENGTH: usize = 2560;

    /// A unit vector at cosine `similarity` to `unit(1.0)`.
    fn unit(similarity: f32) -> Vec<f32> {
        let mut v = vec![0.0; LENGTH];
        v[0] = similarity;
        v[1] = (1.0 - similarity * similarity).sqrt();
        v
    }

    fn store() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = super::super::connect(&dir.path().join("memory.sqlite3")).unwrap();
        (dir, conn)
    }

    /// Inserts an active explicit memory and returns its id.
    fn insert(conn: &Connection, category: &str, content: &str, project: &str) -> Uuid {
        let id = Uuid::new_v4();
        conn.execute(
            "INSERT INTO memories (id,memory_type,category,content,scope,project,title) VALUES (?1,'preference',?2,?3,?4,?5,?3)",
            params![id.to_string(), category, content, if project.is_empty() { "global" } else { "project" }, project],
        )
        .unwrap();
        id
    }

    /// Stores `vector` for memory `id` as embedded from its current document.
    fn embed_stored(conn: &Connection, id: Uuid, vector: Vec<f32>) {
        let m = conn
            .query_row(&format!("{SELECT} WHERE id=?1"), [id.to_string()], row)
            .unwrap();
        let v = DraftVector {
            vector,
            identity: (MODEL.into(), 0),
            source: document(&Draft::from(&m)),
        };
        save_vector(conn, id, &v).unwrap();
    }

    fn new_vector(draft: &Draft, vector: Vec<f32>) -> DraftVector {
        DraftVector {
            vector,
            identity: (MODEL.into(), 0),
            source: document(draft),
        }
    }

    fn found(
        conn: &Connection,
        d: &Draft,
        v: Option<&DraftVector>,
        pool: Pool,
    ) -> Vec<(Uuid, Signal)> {
        similar(conn, d, v, pool)
            .unwrap()
            .into_iter()
            .map(|c| (c.id, c.signal))
            .collect()
    }

    #[test]
    fn semantic_candidates_need_the_calibrated_cutoff_identity_and_current_text() {
        let (_dir, conn) = store();
        let close = insert(&conn, "tools", "Helix is my editor of choice", "");
        let far = insert(&conn, "tools", "Use ripgrep instead of grep", "");
        let outdated = insert(&conn, "tools", "Edit text with Helix", "");
        embed_stored(&conn, close, unit(0.80));
        embed_stored(&conn, far, unit(0.77));
        embed_stored(&conn, outdated, unit(0.99));
        conn.execute(
            "UPDATE memories SET content='Edit text with Kakoune' WHERE id=?1",
            [outdated.to_string()],
        )
        .unwrap();
        let d = draft("preference", "tools", "Use Helix as the text editor");
        let v = new_vector(&d, unit(1.0));
        assert_eq!(
            found(&conn, &d, Some(&v), Pool::Global),
            [(close, Signal::Semantic)]
        );
        let other_model = DraftVector {
            identity: ("other-model".into(), 0),
            ..v.clone()
        };
        assert!(found(&conn, &d, Some(&other_model), Pool::Global).is_empty());
        let other_length = DraftVector {
            identity: (MODEL.into(), 1024),
            ..v.clone()
        };
        assert!(found(&conn, &d, Some(&other_length), Pool::Global).is_empty());
        let uncalibrated = DraftVector {
            vector: v.vector[..3].to_vec(),
            ..v.clone()
        };
        assert!(found(&conn, &d, Some(&uncalibrated), Pool::Global).is_empty());
        assert!(found(&conn, &d, None, Pool::Global).is_empty());
    }

    #[test]
    fn cutoffs_exist_only_for_calibrated_models_at_calibrated_lengths() {
        assert_eq!(built_in(MODEL, 2560).unwrap().duplicate, 0.78);
        assert_eq!(built_in(MODEL, 1024).unwrap().duplicate, 0.81);
        assert_eq!(built_in(MODEL, 2560).unwrap().related, 0.55);
        assert_eq!(built_in(MODEL, 1024).unwrap().related, 0.56);
        assert_eq!(built_in(MODEL, 512), None);
        assert_eq!(built_in("other-model", 2560), None);
        assert_eq!(built_in("other-model", 1024), None);
    }

    #[test]
    fn drafts_of_stored_memories_keep_their_scope() {
        let project = Memory {
            scope: "project".into(),
            project: "lab".into(),
            ..Default::default()
        };
        let draft = Draft::from(&project);
        assert_eq!(draft.scope, Scope::Project);
        assert_eq!(draft.project, "lab");
        let global = Memory {
            scope: "global".into(),
            ..Default::default()
        };
        assert_eq!(Draft::from(&global).scope, Scope::Global);
    }

    #[test]
    fn stored_vectors_from_another_model_are_never_compared() {
        let (_dir, conn) = store();
        let foreign = insert(&conn, "tools", "Helix is my editor of choice", "");
        let m = conn
            .query_row(&format!("{SELECT} WHERE id=?1"), [foreign.to_string()], row)
            .unwrap();
        let stored = DraftVector {
            vector: unit(0.99),
            identity: ("other-model".into(), 0),
            source: document(&Draft::from(&m)),
        };
        save_vector(&conn, foreign, &stored).unwrap();
        let d = draft("preference", "tools", "Use Helix as the text editor");
        let v = new_vector(&d, unit(1.0));
        assert!(found(&conn, &d, Some(&v), Pool::Global).is_empty());
    }

    #[test]
    fn a_category_group_keeps_the_whole_label() {
        let long = "c".repeat(85);
        assert_eq!(category_group(&long), std::slice::from_ref(&long));
        assert_eq!(category_group("  Coding  "), category_group("programming"));
    }

    #[test]
    fn keyword_candidates_need_the_category_group_and_enough_shared_words() {
        let (_dir, conn) = store();
        let alias = insert(&conn, "coding", "Prefer Rust for command-line tools", "");
        insert(&conn, "cooking", "Prefer Rust for command-line tools", "");
        insert(&conn, "programming", "Prefer Go for web services", "");
        let d = draft(
            "preference",
            "programming",
            "Prefer Rust for new command-line tools",
        );
        let found = similar(&conn, &d, None, Pool::Global).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].id, alias);
        assert_eq!(found[0].signal, Signal::Keyword);
        assert!(
            (found[0].score - 5.0 / 6.0).abs() < 1e-6,
            "{}",
            found[0].score
        );
    }

    #[test]
    fn the_pool_is_active_memories_where_the_new_memory_applies() {
        let (_dir, conn) = store();
        let global = insert(&conn, "tools", "Use Helix as the text editor", "");
        let here = insert(&conn, "tools", "Use Helix as the text editor", "lab");
        insert(&conn, "tools", "Use Helix as the text editor", "elsewhere");
        let stale = insert(&conn, "tools", "Use Helix as the text editor", "");
        conn.execute(
            "UPDATE memories SET memory_type='pattern',source='inferred',status='stale' WHERE id=?1",
            [stale.to_string()],
        )
        .unwrap();
        let d = draft("preference", "tools", "Use Helix as the text editor");
        let ids = |pool| -> Vec<Uuid> {
            let mut ids: Vec<Uuid> = found(&conn, &d, None, pool)
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            ids.sort();
            ids
        };
        assert_eq!(ids(Pool::Global), [global]);
        let mut both = vec![global, here];
        both.sort();
        assert_eq!(ids(Pool::Project("lab")), both);
        assert_eq!(ids(Pool::Project("")), [global]);
    }

    #[test]
    fn candidates_are_ordered_semantic_then_keyword_then_by_score_and_id() {
        let (_dir, conn) = store();
        let words = |n: usize| -> Vec<Uuid> {
            (0..n)
                .map(|_| insert(&conn, "tools", "Use Helix as the text editor", ""))
                .collect()
        };
        let mut tied = words(2);
        let strong = insert(&conn, "tools", "Helix for every edit", "");
        let weak = insert(&conn, "tools", "Helix editor please", "");
        let keyword_only = insert(&conn, "tools", "Use Helix as text editor daily", "");
        embed_stored(&conn, strong, unit(0.95));
        embed_stored(&conn, weak, unit(0.85));
        let d = draft("preference", "tools", "Use Helix as the text editor");
        let v = new_vector(&d, unit(1.0));
        tied.sort();
        assert_eq!(
            found(&conn, &d, Some(&v), Pool::Global),
            [
                (strong, Signal::Semantic),
                (weak, Signal::Semantic),
                (tied[0], Signal::Keyword),
                (tied[1], Signal::Keyword),
                (keyword_only, Signal::Keyword),
            ]
        );
    }

    #[test]
    fn save_vector_joins_the_callers_transaction_and_skips_missing_memories() {
        let (_dir, mut conn) = store();
        let d = draft("preference", "tools", "Use Helix");
        let v = new_vector(&d, unit(1.0));
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let id = insert(&tx, "tools", "Use Helix", "");
        save_vector(&tx, id, &v).unwrap();
        save_vector(&tx, Uuid::new_v4(), &v).unwrap();
        tx.commit().unwrap();
        let rows: Vec<(String, String, i64, String)> = conn
            .prepare("SELECT id,model,dimensions,source FROM memory_vectors")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            [(
                id.to_string(),
                MODEL.into(),
                0,
                "tools (preference): Use Helix".into()
            )]
        );
    }

    /// An embeddings endpoint that records each input and answers after `delay`.
    fn endpoint(delay: Duration) -> (embed::Config, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/embeddings", listener.local_addr().unwrap());
        let inputs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = inputs.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':')
                        && key.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let request: Value = serde_json::from_slice(&body).unwrap();
                for input in request["input"].as_array().unwrap() {
                    seen.lock()
                        .unwrap()
                        .push(input.as_str().unwrap().to_owned());
                }
                std::thread::sleep(delay);
                let reply =
                    serde_json::json!({"data":[{"index":0,"embedding":[0.6,0.8]}]}).to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        let settings = move |key: &str| match key {
            "USER_MEMORY_EMBED_URL" => Some(url.clone()),
            "USER_MEMORY_EMBED_MODEL" => Some("doc-model".into()),
            "USER_MEMORY_EMBED_DIMENSIONS" => Some("2".into()),
            "USER_MEMORY_EMBED_QUERY_INSTRUCTION" => Some("Instruct: find\\nQuery:".into()),
            _ => None,
        };
        (
            embed::Config::from_settings(&settings).unwrap().unwrap(),
            inputs,
        )
    }

    #[test]
    fn drafts_are_embedded_as_documents_without_the_query_instruction() {
        let (config, inputs) = endpoint(Duration::ZERO);
        let mut d = draft("preference", "tools", "Use Helix");
        d.reason = "Modal editing";
        let v = embed_draft_with(Some(&config), &d, Duration::from_secs(5)).unwrap();
        assert_eq!(v.vector, [0.6, 0.8]);
        assert_eq!(v.identity, ("doc-model".into(), 2));
        assert_eq!(
            v.source,
            "tools (preference): Use Helix\nReason: Modal editing"
        );
        assert_eq!(*inputs.lock().unwrap(), std::slice::from_ref(&v.source));
        assert!(embed_draft_with(None, &d, Duration::from_secs(5)).is_none());
    }

    // An endpoint that never answers is given up on at the timeout: only
    // the timeout can end the wait, and a wait that never ends fails the
    // test once a hang is certain instead of stalling the run.
    #[test]
    fn a_slow_endpoint_gives_up_after_the_record_timeout() {
        /// Long enough that only a hang reaches it.
        const STUCK: Duration = Duration::from_secs(60);
        let (silent, _) = endpoint(Duration::from_secs(24 * 60 * 60));
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let d = draft("preference", "tools", "Use Helix");
            let _ = done.send(embed_draft_with(
                Some(&silent),
                &d,
                Duration::from_millis(300),
            ));
        });
        let given_up = finished
            .recv_timeout(STUCK)
            .expect("the embedding waited past its timeout");
        assert!(given_up.is_none());
        let (slow, _) = endpoint(Duration::from_millis(300));
        let d = draft("preference", "tools", "Use Helix");
        assert!(embed_draft_with(Some(&slow), &d, Duration::from_secs(5)).is_some());
    }

    use super::super::calibration::{Labelled, draft as fixture_draft, flagged, misplaced};

    fn fixture() -> Vec<Labelled> {
        super::super::calibration::pairs().unwrap()
    }

    /// Cosine per labelled pair with the configured endpoint, or `None` when
    /// USER_MEMORY_EMBED_URL is not set.
    /// Model, vector length and (class, cosine) per labelled pair.
    struct Embedded {
        model: String,
        length: usize,
        scores: Vec<(String, f32)>,
    }

    fn embedded_scores() -> Option<Embedded> {
        let config = embed::Config::from_env()?;
        let pairs = fixture();
        let texts: Vec<String> = pairs
            .iter()
            .flat_map(|l| l.pair.iter().map(|m| document(&fixture_draft(m))))
            .collect();
        let vectors = config.embed(&texts).expect("embedding endpoint");
        let scores = pairs
            .iter()
            .zip(vectors.chunks(2))
            .map(|(l, v)| (label(l), cosine(&v[0], &v[1])))
            .collect();
        Some(Embedded {
            model: config.identity().0.to_owned(),
            length: vectors[0].len(),
            scores,
        })
    }

    /// The class, with "long " in front for a long pair, as the report and
    /// the ranges key them.
    fn label(l: &Labelled) -> String {
        if l.long {
            format!("long {}", l.class)
        } else {
            l.class.clone()
        }
    }

    fn range(scores: &[(String, f32)], class: &str) -> (f32, f32) {
        let values = scores.iter().filter(|(c, _)| c == class).map(|(_, s)| *s);
        values.fold((f32::MAX, f32::MIN), |(lo, hi), s| (lo.min(s), hi.max(s)))
    }

    /// Prints the per-class score ranges used to choose the constants:
    /// USER_MEMORY_EMBED_URL=… [USER_MEMORY_EMBED_DIMENSIONS=1024] \
    ///   cargo test --locked calibration_report -- --ignored --nocapture
    #[test]
    #[ignore = "needs a real embedding endpoint"]
    fn calibration_report() {
        let Embedded {
            model,
            length,
            scores,
        } = embedded_scores().expect("set USER_MEMORY_EMBED_URL");
        println!("model {model}, {length} dimensions, {} pairs", scores.len());
        for prefix in ["", "long "] {
            for class in ["same", "contradiction", "exception", "related", "unrelated"] {
                let class = format!("{prefix}{class}");
                if scores.iter().any(|(c, _)| *c == class) {
                    let (lo, hi) = range(&scores, &class);
                    println!("  cosine {class:18} min {lo:.3} max {hi:.3}");
                }
            }
        }
        // The cutoffs come from the short pairs, as they were calibrated.
        let flagged_min = scores
            .iter()
            .filter(|(c, _)| flagged(c))
            .map(|(_, s)| *s)
            .fold(f32::MAX, f32::min);
        let (_, related_max) = range(&scores, "related");
        let (_, unrelated_max) = range(&scores, "unrelated");
        let (related_min, _) = range(&scores, "related");
        let (exception_min, _) = range(&scores, "exception");
        println!(
            "  duplicate gap {:.3}..{flagged_min:.3} → midpoint {:.3}",
            related_max.max(unrelated_max),
            (related_max.max(unrelated_max) + flagged_min) / 2.0
        );
        println!(
            "  related at most {:.3} (exception and related min - 0.01)",
            related_min.min(exception_min) - 0.01
        );
        // Review links only memories in one category group: unrelated pairs
        // in one group above `related` are proposed as overlaps.
        let same_group = |[a, b]: &[Value; 2]| {
            category_group(a["category"].as_str().unwrap())
                .contains(&b["category"].as_str().unwrap().to_owned())
        };
        let unrelated_in_group = fixture()
            .iter()
            .zip(&scores)
            .filter(|(l, _)| l.class == "unrelated" && !l.long && same_group(&l.pair))
            .map(|(_, (_, s))| *s)
            .fold(f32::MIN, f32::max);
        println!("  unrelated in one category group: max {unrelated_in_group:.3}");
        for (l, (label, score)) in fixture().iter().zip(&scores) {
            let [a, b] = &l.pair;
            let overlap = overlap(
                &tokenize(fixture_draft(a).content),
                &tokenize(fixture_draft(b).content),
            );
            let start = |m: &Value| {
                m["content"]
                    .as_str()
                    .unwrap()
                    .chars()
                    .take(60)
                    .collect::<String>()
            };
            println!(
                "  {label:18} cosine {score:.3} keyword {overlap:.2}  {} | {}",
                start(a),
                start(b)
            );
        }
    }

    /// Without embeddings, the keyword signal never lists an unrelated pair,
    /// short or long, for review, nor an exception as a duplicate. It catches
    /// only close rewordings, so restated duplicates are left to the semantic
    /// signal (see DUPLICATE_OVERLAP).
    #[test]
    fn keywords_never_link_unrelated_memories_short_or_long() {
        for l in fixture() {
            let [a, b] = &l.pair;
            let overlap = overlap(
                &tokenize(fixture_draft(a).content),
                &tokenize(fixture_draft(b).content),
            );
            let pair = format!(
                "{} {overlap:.2}: {} | {}",
                label(&l),
                a["content"],
                b["content"]
            );
            match l.class.as_str() {
                "unrelated" | "related" => assert!(overlap < RELATED_OVERLAP, "{pair}"),
                "exception" => assert!(overlap < DUPLICATE_OVERLAP, "{pair}"),
                _ => {}
            }
        }
    }

    /// With USER_MEMORY_EMBED_URL set, the calibrated cutoffs for the
    /// configured model must separate the labelled classes, short and long,
    /// with a margin.
    #[test]
    fn calibrated_cutoffs_classify_the_labelled_pairs() {
        let Some(Embedded {
            model,
            length,
            scores,
        }) = embedded_scores()
        else {
            return;
        };
        let cutoffs = built_in(&model, length)
            .unwrap_or_else(|| panic!("no calibrated cutoffs for {model} at {length}"));
        let labelled = fixture();
        let scored: Vec<(&Labelled, f32)> = labelled
            .iter()
            .zip(scores.iter().map(|(_, s)| *s))
            .collect();
        assert_eq!(misplaced(cutoffs, &scored), None, "{model} at {length}");
    }
}
