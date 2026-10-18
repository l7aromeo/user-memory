//! Recording, corrections and deletion with provenance checks.

use super::{
    Memory, Scope, Source, TITLE_LIMIT, clean,
    ids::{MemoryRef, full_id, resolve_all, uuids},
    project_key,
    schema::{connect, get, write},
    selection::refuse_local_paths,
    similar::{self, Candidate, Draft, Pool, Signal},
};
use anyhow::Result;
use clap::Args;
use rusqlite::{Connection, OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashSet},
    path::Path,
};
use uuid::Uuid;

/// A global memory loads in every project, so only the user decides one.
const GLOBAL_NEEDS_YES: &str = "A global memory applies in every project: ask the user whether it does and pass user_confirmed, or record it with scope project.";

fn default_type() -> String {
    "fact".into()
}

#[derive(Clone, Args, Deserialize, Serialize, JsonSchema)]
pub struct Record {
    /// Memory type, fact unless set. A rule the user wants followed is a preference, constraint or value: only these types become rules in the index and the instruction files. Otherwise decision, goal, fact, behavior, pattern, or another label.
    #[arg(long = "type", default_value = "fact")]
    #[serde(rename = "type", default = "default_type")]
    pub memory_type: String,
    /// Topic/category independent from type, such as programming or communication; reuse a category the index's Categories line lists when one fits.
    #[arg(long)]
    pub category: String,
    /// One line of at most 120 characters stating the memory so an agent can act on it from the index alone, such as the rule itself: "Push, publish or tag only with the user's go-ahead", not "Release policy".
    #[arg(long, allow_hyphen_values = true)]
    pub title: String,
    /// The memory in full, readable without the conversation: the user's preference, choice, fact or observed behavior with its conditions and exceptions; at most 4,000 characters.
    #[arg(long, allow_hyphen_values = true)]
    pub content: String,
    /// Situation where this memory applies, at most 2,000 characters; do not copy a raw transcript.
    #[arg(long, allow_hyphen_values = true, default_value = "")]
    #[serde(default)]
    pub context: String,
    /// Stated rationale, or evidence-based explanation for a tentative inference; at most 2,000 characters.
    #[arg(long, allow_hyphen_values = true, default_value = "")]
    #[serde(default)]
    pub reason: String,
    /// global only for what holds in every project, and only with the user's yes (user_confirmed); project for anything about one repository.
    #[arg(long, value_enum)]
    pub scope: Scope,
    /// For scope project: a key the session start's "Projects: …" line lists, or a git remote URL; blank for global scope.
    #[arg(long, default_value = "")]
    #[serde(default)]
    pub project: String,
    /// explicit (default) for what the user stated, with user_confirmed; observed needs evidence; inferred needs evidence_ids.
    #[arg(long, value_enum, default_value = "explicit")]
    #[serde(default)]
    pub source: Source,
    /// Concrete observed user action or statement, not an invented motive; at most 2,000 characters.
    #[arg(long, allow_hyphen_values = true, default_value = "")]
    #[serde(default)]
    pub evidence: String,
    /// Only inferences cite supporting memories: at least two independent active explicit/observed memories.
    #[arg(long = "evidence-id")]
    #[serde(default)]
    pub evidence_ids: Vec<MemoryRef>,
    /// Conversation origin printed at session start, such as claude:session/<id>; empty when none was given. Never invent one.
    #[arg(long, default_value = "")]
    #[serde(default)]
    pub origin: String,
    /// IDs of active memories in the same scope that this one replaces because the user changed their mind (or, with merge, consolidates).
    #[arg(long = "supersedes")]
    #[serde(default)]
    pub supersedes: Vec<MemoryRef>,
    /// With supersedes naming 2+ memories the user approved as duplicates: consolidate them, keeping all their conversations. Without it, supersedes means the user changed their mind.
    #[arg(long)]
    #[serde(default)]
    pub merge: bool,
    /// ID of an active explicit memory with the same scope that the user stated again; records the restatement instead of a new memory.
    #[arg(long)]
    #[serde(default)]
    pub confirms: Option<MemoryRef>,
    /// IDs of the similar memories reported for this record that the user means differently; every reported one must be listed.
    #[arg(long = "distinct-from")]
    #[serde(default)]
    pub distinct_from: Vec<MemoryRef>,
    /// True only when the user, in this conversation, stated or approved this exact memory in their own words and, for scope global, that it holds in every project; for an observation or inference, only once the user approved recording it.
    #[arg(skip)]
    #[serde(default)]
    pub user_confirmed: bool,
}

#[derive(Args, Deserialize, Serialize, JsonSchema)]
pub struct Update {
    /// The memory to correct or confirm, by its full id or the 8 characters the index shows; a superseded one whose replacement was forgotten becomes active again. Rewording only its title, context, reason or evidence keeps an observation or inference the agent's; any other change, or none (a confirmation), makes it the user's explicit statement.
    pub memory_id: MemoryRef,
    /// New type: preference, constraint or value for a rule the user wants followed (only these become rules in the index and the instruction files); otherwise decision, goal, fact, behavior, pattern, or another label.
    #[arg(long = "type")]
    #[serde(rename = "type", default)]
    pub memory_type: Option<String>,
    /// New category; reuse a category the index's Categories line lists when one fits.
    #[arg(long)]
    #[serde(default)]
    pub category: Option<String>,
    /// New one-line title. Given alone it only rewords the index line: conversations, status and dependent patterns stay as they are.
    #[arg(long, allow_hyphen_values = true)]
    #[serde(default)]
    pub title: Option<String>,
    /// Corrected content, at most 4,000 characters. When it changes the choice itself, give its reason too ("" for none), or record the new choice with supersedes.
    #[arg(long, allow_hyphen_values = true)]
    #[serde(default)]
    pub content: Option<String>,
    /// Corrected context, at most 2,000 characters.
    #[arg(long, allow_hyphen_values = true)]
    #[serde(default)]
    pub context: Option<String>,
    /// The user's stated reason, at most 2,000 characters; replaces the stored reason, which the update otherwise keeps ("" clears it).
    #[arg(long, allow_hyphen_values = true)]
    #[serde(default)]
    pub reason: Option<String>,
    /// New evidence, such as the user's own words, at most 2,000 characters; replaces the stored evidence, which the update otherwise keeps.
    #[arg(long, allow_hyphen_values = true)]
    #[serde(default)]
    pub evidence: Option<String>,
    /// New scope: global only for what holds in every project, and only with the user's yes (user_confirmed); project for anything about one repository.
    #[arg(long, value_enum)]
    #[serde(default)]
    pub scope: Option<Scope>,
    /// For scope project: a key the session start's "Projects: …" line lists, or a git remote URL; blank for global scope.
    #[arg(long)]
    #[serde(default)]
    pub project: Option<String>,
    /// Conversation origin printed at session start: adds this conversation to the memory's origins, which the update otherwise keeps.
    #[arg(long)]
    #[serde(default)]
    pub origin: Option<String>,
    /// With origin, for a full restatement in a new conversation: the memory keeps only that origin.
    #[arg(long)]
    #[serde(default)]
    pub replace_origins: bool,
    /// IDs of memories the user says mean something different from this one, so review stops grouping them. Give alone: marking them distinct changes no memory.
    #[arg(long = "distinct-from")]
    #[serde(default)]
    pub distinct_from: Vec<MemoryRef>,
    /// True only when the user explicitly asked for this correction or confirmation; never for your own revised guess.
    #[arg(skip)]
    #[serde(default)]
    pub user_confirmed: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct Forget {
    /// The memory to delete, by its full id or the 8 characters the index shows. Give exactly one of memory_id or origin.
    #[serde(default)]
    pub memory_id: Option<MemoryRef>,
    /// Delete every memory recorded with this origin, such as a deleted conversation.
    #[serde(default)]
    pub origin: Option<String>,
    /// True only when the user explicitly asked to forget this memory or this conversation's memories.
    #[serde(default)]
    pub user_confirmed: bool,
}

/// Longest content: a memory is one rule or fact with its conditions and
/// exceptions, read in full only when an agent opens it (the index and the
/// instruction files show titles), so a rule never has to be split to fit.
/// Longer text is refused, never cut.
const CONTENT_LIMIT: usize = 4000;
/// Longest context, reason or evidence: a note about the memory, shorter than
/// the memory itself. Longer text is refused, never cut.
const NOTE_LIMIT: usize = 2000;
/// Longest type: a word or two naming the kind of memory.
const TYPE_LIMIT: usize = 40;
/// Longest category: a short label that groups rules in the index.
const CATEGORY_LIMIT: usize = 80;
/// Longest project key: a host and repository path, with room to spare.
const PROJECT_LIMIT: usize = 240;
/// Advice for a label, key or title over its limit.
const SHORTEN: &str = "shorten it";
/// Advice for a project key over its limit: it usually comes from a git
/// remote, whose key a repository's sessions always use.
const RENAME_PROJECT: &str =
    "use a shorter project key; a key that long from a git remote needs a shorter remote URL";
/// Advice for text over its limit, which may hold more than one memory.
const SHORTEN_OR_SPLIT: &str = "shorten it or split the memory";
/// Longest accepted origin reference; longer ones are refused, never truncated.
const ORIGIN_LIMIT: usize = 240;
const ACTIVE: &str = "status IN ('confirmed','tentative')";
/// Superseded memories whose replacement no longer exists.
const ORPHANED: &str = "status='superseded' AND NOT EXISTS (SELECT 1 FROM memories m WHERE m.id=memories.superseded_by)";

/// How `check_support` treats a supporting memory that exists nowhere.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Missing {
    /// The support must exist: new records and active inferences.
    Refuse,
    /// Forgetting a support leaves its id behind, so stale or superseded
    /// inferences may cite memories that no longer exist.
    Allow,
}

/// Structural evidence rules shared by recording and import: each support
/// exists in the store or in `extra` (memories arriving together with this
/// one), is not itself an inference, and project evidence never supports a
/// global or other-project inference. Supports listed only in `extra` are
/// checked by the caller. Whether supports are active and independent is
/// checked only when recording.
pub(super) fn check_support(
    conn: &Connection,
    input: &Record,
    extra: &HashSet<Uuid>,
    missing: Missing,
) -> Result<Option<&'static str>> {
    for id in uuids(&input.evidence_ids) {
        let Some(support) = get(conn, id)? else {
            if extra.contains(&id) || missing == Missing::Allow {
                continue;
            }
            return Ok(Some("Supporting memory does not exist"));
        };
        if support.source == "inferred" {
            return Ok(Some(
                "Evidence must be active explicit or observed memory, not another inference",
            ));
        }
        if support.scope == "project"
            && (input.scope != Scope::Project || input.project != support.project)
        {
            return Ok(Some(
                "Inferences cannot broaden or cross the scope of their evidence",
            ));
        }
    }
    Ok(None)
}

/// Why a title cannot be stored, or `None` when it fits on one index line.
fn title_problem(title: &str) -> Option<String> {
    if title.is_empty() {
        return Some(
            "title is required: write one line that states the memory, such as the rule itself"
                .into(),
        );
    }
    // Agents often miss by a few characters, so the refusal says by how many.
    let over = title.chars().count().saturating_sub(TITLE_LIMIT);
    let unit = if over == 1 { "character" } else { "characters" };
    too_long(
        "title",
        title,
        TITLE_LIMIT,
        &format!("remove at least {over} {unit}"),
    )
}

/// Refusal for a field over its limit, naming the field, its length, the
/// limit and what to do.
fn too_long(field: &str, text: &str, limit: usize, advice: &str) -> Option<String> {
    let length = text.chars().count();
    (length > limit)
        .then(|| format!("{field} has {length} characters; the limit is {limit}; {advice}"))
}

pub(super) fn normalize(input: &mut Record) -> Option<String> {
    input.memory_type = clean(&input.memory_type, usize::MAX).to_lowercase();
    input.category = clean(&input.category, usize::MAX).to_lowercase();
    input.title = clean(&input.title, usize::MAX);
    input.content = clean(&input.content, usize::MAX);
    input.context = clean(&input.context, usize::MAX);
    input.reason = clean(&input.reason, usize::MAX);
    input.evidence = clean(&input.evidence, usize::MAX);
    if let Err(local) = refuse_local_paths(std::slice::from_ref(&input.project)) {
        return Some(local.to_string());
    }
    input.project = project_key(&input.project);
    input.origin = clean(&input.origin, usize::MAX);
    input.evidence_ids.sort_unstable();
    input.evidence_ids.dedup();
    for ids in [&mut input.supersedes, &mut input.distinct_from] {
        ids.sort_unstable();
        ids.dedup();
    }
    if input.memory_type.is_empty() || input.category.is_empty() || input.content.is_empty() {
        return Some("type, category, and content are required".into());
    }
    for (field, text, limit, advice) in [
        ("type", &input.memory_type, TYPE_LIMIT, SHORTEN),
        ("category", &input.category, CATEGORY_LIMIT, SHORTEN),
        ("project", &input.project, PROJECT_LIMIT, RENAME_PROJECT),
        ("content", &input.content, CONTENT_LIMIT, SHORTEN_OR_SPLIT),
        ("context", &input.context, NOTE_LIMIT, SHORTEN_OR_SPLIT),
        ("reason", &input.reason, NOTE_LIMIT, SHORTEN_OR_SPLIT),
        ("evidence", &input.evidence, NOTE_LIMIT, SHORTEN_OR_SPLIT),
    ] {
        if let Some(problem) = too_long(field, text, limit, advice) {
            return Some(problem);
        }
    }
    if let Some(problem) = title_problem(&input.title) {
        return Some(problem);
    }
    if input.scope == Scope::Project && input.project.is_empty() {
        return Some("Project scope requires a project identifier".into());
    }
    if input.scope == Scope::Global && !input.project.is_empty() {
        return Some("Global memories must not name a project".into());
    }
    if input.scope == Scope::Global && !input.user_confirmed {
        return Some(GLOBAL_NEEDS_YES.into());
    }
    if input.source == Source::Explicit && !input.user_confirmed {
        return Some("Explicit memory must be grounded in the user's own statement".into());
    }
    if input.source == Source::Observed && input.evidence.is_empty() {
        return Some("Observed behavior requires a concrete evidence note".into());
    }
    if input.origin.chars().count() > ORIGIN_LIMIT {
        return Some("Origin must be a short reference of at most 240 characters".into());
    }
    if input.source != Source::Inferred && !input.evidence_ids.is_empty() {
        return Some("Only inferred patterns cite supporting memories".into());
    }
    if input.source == Source::Inferred && input.evidence_ids.len() < 2 {
        return Some("Inferred patterns require at least two distinct supporting memories".into());
    }
    if input.evidence_ids.len() > 20
        || input
            .evidence_ids
            .iter()
            .filter_map(MemoryRef::full)
            .any(|id| id.is_nil())
    {
        return Some("Evidence must contain at most 20 non-nil UUID memory IDs".into());
    }
    None
}

/// Most similar memories reported to the agent when a record is held back.
const REPORTED: usize = 3;

fn draft(input: &Record) -> Draft<'_> {
    Draft {
        memory_type: &input.memory_type,
        category: &input.category,
        content: &input.content,
        context: &input.context,
        reason: &input.reason,
        evidence: &input.evidence,
        scope: input.scope,
        project: &input.project,
    }
}

fn refused(reason: &str) -> Result<Value> {
    Ok(json!({"stored":false,"reason":reason}))
}

pub fn record(db: &Path, mut input: Record) -> Result<Value> {
    if let Some(reason) = normalize(&mut input) {
        return refused(&reason);
    }
    let resolutions = usize::from(input.confirms.is_some())
        + usize::from(!input.supersedes.is_empty())
        + usize::from(!input.distinct_from.is_empty());
    if resolutions > 1 {
        return refused("Give at most one of confirms, supersedes or distinct_from");
    }
    // Embedding may wait seconds for a cold model: never while holding the write lock.
    let vector = match input.confirms {
        Some(_) => None,
        None => similar::embed_draft(&draft(&input)),
    };
    let conn = connect(db)?;
    let tx = write(&conn)?;
    // Ids may be the index's first characters: each becomes the full id it
    // names, or the call is refused.
    for refs in [
        &mut input.evidence_ids,
        &mut input.supersedes,
        &mut input.distinct_from,
    ] {
        if let Some(reason) = resolve_all(&tx, refs)? {
            return refused(&reason);
        }
    }
    if input.merge && input.supersedes.len() < 2 {
        return refused("merge needs at least two memories in supersedes");
    }
    if let Some(named) = &input.confirms {
        let target = match full_id(&tx, named)? {
            Ok(target) => target,
            Err(reason) => return refused(&reason),
        };
        let result = confirm(&tx, &input, target)?;
        tx.commit()?;
        return Ok(result);
    }
    if let Some(reason) = check_support(&tx, &input, &HashSet::new(), Missing::Refuse)? {
        return refused(reason);
    }
    // Each origin-less support counts as its own conversation; origins are
    // counted once across all supports.
    let mut origins = BTreeSet::new();
    let mut unknown = 0;
    for id in uuids(&input.evidence_ids) {
        // check_support found it in this transaction.
        let support = listed(&tx, id)?;
        if !matches!(support.status.as_str(), "confirmed" | "tentative") {
            return refused(
                "Evidence must be active explicit or observed memory, not another inference",
            );
        }
        if support.origins.is_empty() {
            unknown += 1;
        }
        origins.extend(support.origins);
    }
    if input.source == Source::Inferred && unknown + origins.len() < 2 {
        return refused(
            "Inferred patterns require at least two independent observations; memories from one conversation count once",
        );
    }
    let supports = uuids(&input.evidence_ids);
    for target in uuids(&input.supersedes) {
        let Some(old) = get(&tx, target)? else {
            return refused("Superseded memory does not exist");
        };
        if old.status == "superseded" {
            return refused("Memory was already superseded");
        }
        if old.scope != input.scope.name() || old.project != input.project {
            return refused("A memory can only supersede one with the same scope and project");
        }
        if old.source == "explicit" && input.source != Source::Explicit {
            return refused("Only the user's explicit statement can supersede an explicit memory");
        }
        if supports.contains(&target) {
            return refused("A memory cannot cite the memory it supersedes");
        }
    }
    for other in uuids(&input.distinct_from) {
        if get(&tx, other)?.is_none() {
            return refused("distinct_from names a memory that does not exist");
        }
    }
    let status = if input.source == Source::Explicit {
        "confirmed"
    } else {
        "tentative"
    };
    let ids = serde_json::to_string(&input.evidence_ids)?;
    let duplicate: Option<String> = tx.query_row(&format!("SELECT id FROM memories WHERE memory_type=?1 AND category=?2 AND content=?3 AND context=?4 AND reason=?5 AND scope=?6 AND project=?7 AND source=?8 AND evidence=?9 AND evidence_ids=?10 AND {ACTIVE} AND (?11='' AND NOT EXISTS (SELECT 1 FROM memory_origins WHERE memory_id=memories.id) OR EXISTS (SELECT 1 FROM memory_origins WHERE memory_id=memories.id AND origin=?11)) LIMIT 1"),params![input.memory_type,input.category,input.content,input.context,input.reason,input.scope.name(),input.project,input.source.name(),input.evidence,ids,input.origin],|r|r.get(0)).optional()?;
    let (id, duplicate) = match duplicate {
        Some(id) => (Uuid::parse_str(&id)?, true),
        None => {
            if input.supersedes.is_empty() {
                let held = held_back(&tx, &input, vector.as_ref())?;
                if !held.is_empty() {
                    return Ok(
                        json!({"stored":false,"reason":"similar memories exist","similar":held,
                        "resolve":"call record again with confirms, supersedes or distinct_from"}),
                    );
                }
            }
            let id = Uuid::new_v4();
            tx.execute("INSERT INTO memories (id,memory_type,category,context,title,content,reason,scope,project,source,status,evidence,evidence_ids) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",params![id.to_string(),input.memory_type,input.category,input.context,input.title,input.content,input.reason,input.scope.name(),input.project,input.source.name(),status,input.evidence,ids])?;
            if let Some(vector) = &vector {
                similar::save_vector(&tx, id, vector)?;
            }
            (id, false)
        }
    };
    let mut result = json!({"stored":true,"id":id,"status":status});
    if duplicate {
        result["duplicate"] = json!(true);
    }
    if !input.supersedes.is_empty() {
        if uuids(&input.supersedes).contains(&id) {
            return refused("A memory cannot supersede itself");
        }
        // A merge of same-meaning memories keeps every conversation that
        // stated them; a changed mind keeps only this conversation.
        if input.merge {
            tx.execute(
            "INSERT OR IGNORE INTO memory_origins (memory_id,origin,first_seen) SELECT ?1,origin,MIN(first_seen) FROM memory_origins WHERE memory_id IN (SELECT value FROM json_each(?2)) GROUP BY origin",
            params![id.to_string(), serde_json::to_string(&input.supersedes)?],
            )?;
        }
        for target in &input.supersedes {
            // Marking it superseded changes its status, so the support trigger stales dependents.
            tx.execute(
                "UPDATE memories SET status='superseded',superseded_by=?1,updated_at=CURRENT_TIMESTAMP WHERE id=?2",
                params![id.to_string(), target.to_string()],
            )?;
        }
        result["superseded"] = json!(input.supersedes);
    }
    add_origin(&tx, id, &input.origin)?;
    for other in uuids(&input.distinct_from) {
        mark_pair(&tx, id, other)?;
    }
    tx.commit()?;
    Ok(result)
}

/// Remembers that the user means two memories differently; returns 1 when the
/// pair is new, 0 when it was already known.
pub(super) fn mark_pair(conn: &Connection, one: Uuid, other: Uuid) -> Result<usize> {
    let (a, b) = (one.min(other), one.max(other));
    Ok(conn.execute(
        "INSERT OR IGNORE INTO memory_distinct (a,b) VALUES (?1,?2)",
        params![a.to_string(), b.to_string()],
    )?)
}

/// The similar memories that hold this record back: the top `REPORTED`
/// candidates, minus those `distinct_from` already names. A record is held back
/// only while one of today's top candidates is unlisted, so a candidate that
/// vanished since the agent's first call (an embedder that went away) never
/// blocks, while a newly appearing one does.
fn held_back(
    conn: &Connection,
    input: &Record,
    vector: Option<&similar::DraftVector>,
) -> Result<Vec<Value>> {
    let draft = draft(input);
    let pool = match draft.scope {
        Scope::Global => Pool::Global,
        Scope::Project => Pool::Project(draft.project),
    };
    let mut top = Vec::new();
    for candidate in similar::similar(conn, &draft, vector, pool)? {
        let memory = listed(conn, candidate.id)?;
        // Observations from different conversations are independent evidence, never duplicates.
        let same_conversation = !input.origin.is_empty() && memory.origins.contains(&input.origin);
        if input.source == Source::Observed && memory.source == "observed" && !same_conversation {
            continue;
        }
        top.push((candidate, memory));
        if top.len() == REPORTED {
            break;
        }
    }
    let distinct = uuids(&input.distinct_from);
    if top.iter().all(|(c, _)| distinct.contains(&c.id)) {
        return Ok(Vec::new());
    }
    Ok(top.iter().map(|(c, m)| reported(c, m)).collect())
}

/// A memory this transaction already found in the store: listed by a query,
/// or a support check_support found. Missing only when the store is damaged
/// (a row its own index cannot find), which is an error to report, not a
/// reason to stop answering.
fn listed(conn: &Connection, id: Uuid) -> Result<Memory> {
    get(conn, id)?.ok_or_else(|| {
        anyhow::anyhow!(
            "The store found memory {id} but cannot read it back, so it may be damaged; run user-memory doctor"
        )
    })
}

fn reported(candidate: &Candidate, m: &Memory) -> Value {
    let signal = match candidate.signal {
        Signal::Semantic => "semantic",
        Signal::Keyword => "keyword",
    };
    json!({"id":m.id,"content":m.content,"type":m.memory_type,"category":m.category,
        "scope":m.scope,"project":m.project,"status":m.status,"source":m.source,
        "match":format!("{signal} {:.2}", candidate.score)})
}

/// Records the user's restatement of an existing explicit memory: adds this
/// conversation to it instead of storing a copy. Valid for any active explicit
/// memory in the same scope, whether or not it was reported as similar, so the
/// outcome does not depend on the embedder being reachable on this call.
fn confirm(conn: &Connection, input: &Record, target: Uuid) -> Result<Value> {
    if input.source != Source::Explicit {
        return refused("Only the user's explicit statement can confirm a memory");
    }
    let usable = get(conn, target)?.is_some_and(|m| {
        m.source == "explicit"
            && matches!(m.status.as_str(), "confirmed" | "tentative")
            && m.scope == input.scope.name()
            && m.project == input.project
    });
    if !usable {
        return refused(
            "confirms must name an active explicit memory with the same scope and project",
        );
    }
    add_origin(conn, target, &input.origin)?;
    conn.execute(
        "UPDATE memories SET last_confirmed_at=CURRENT_TIMESTAMP WHERE id=?1",
        [target.to_string()],
    )?;
    let confirmations = get(conn, target)?.map_or(0, |m| m.confirmations);
    Ok(json!({"stored":true,"id":target,"confirmed":true,"confirmations":confirmations}))
}

/// Records that `origin` (if any) stated memory `id`.
fn add_origin(conn: &Connection, id: Uuid, origin: &str) -> Result<()> {
    if !origin.is_empty() {
        conn.execute(
            "INSERT OR IGNORE INTO memory_origins (memory_id,origin) VALUES (?1,?2)",
            params![id.to_string(), origin],
        )?;
    }
    Ok(())
}

pub fn update(db: &Path, input: Update) -> Result<Value> {
    if !input.user_confirmed {
        let reason = if input.scope == Some(Scope::Global) {
            GLOBAL_NEEDS_YES
        } else {
            "Updating memory requires an explicit user request"
        };
        return Ok(json!({"updated":false,"reason":reason}));
    }
    if !input.distinct_from.is_empty() {
        return mark_distinct(db, input);
    }
    let conn = connect(db)?;
    let tx = write(&conn)?;
    let memory_id = match full_id(&tx, &input.memory_id)? {
        Ok(id) => id,
        Err(reason) => return Ok(json!({"updated":false,"reason":reason})),
    };
    let Some(old) = get(&tx, memory_id)? else {
        return Ok(json!({"updated":false,"reason":"Memory does not exist"}));
    };
    // A superseded memory is corrected through the one that replaced it.
    // Once that one is forgotten, the user's update makes it active again.
    let revived = old.status == "superseded";
    if revived
        && let Some(by) = old.superseded_by
        && get(&tx, by)?.is_some()
    {
        return Ok(
            json!({"updated":false,"reason":"Memory was superseded; correct the memory that replaced it"}),
        );
    }
    let rewording_only = input.memory_type.is_none()
        && input.category.is_none()
        && input.content.is_none()
        && input.context.is_none()
        && input.reason.is_none()
        && input.evidence.is_none()
        && input.scope.is_none()
        && input.project.is_none()
        && input.origin.is_none()
        && !input.replace_origins;
    if let (true, false, Some(title)) = (rewording_only, revived, &input.title) {
        // A title is only wording: the memory keeps its conversations,
        // status and source, and no pattern goes stale; it was still updated.
        let title = clean(title, usize::MAX);
        if let Some(reason) = title_problem(&title) {
            return Ok(json!({"updated":false,"reason":reason}));
        }
        tx.execute(
            "UPDATE memories SET title=?2,updated_at=CURRENT_TIMESTAMP WHERE id=?1",
            params![memory_id.to_string(), title],
        )?;
        tx.commit()?;
        return Ok(json!({"updated":true,"id":memory_id,"status":old.status}));
    }
    // Wording (title, context, reason or evidence note) is not the user
    // stating the memory: alone it leaves an observation or inference the
    // agent's, with its source, status and supports. Changing what the memory
    // says or where it applies, or an update giving none of these (the user
    // confirming it), makes it the user's explicit statement.
    let wording = input.title.is_some()
        || input.context.is_some()
        || input.reason.is_some()
        || input.evidence.is_some();
    let new_reason = input.reason.is_some();
    let (old_source, old_status, old_supports) = (
        old.source.clone(),
        old.status.clone(),
        old.evidence_ids.clone(),
    );
    let mut next = Record {
        memory_type: input.memory_type.unwrap_or_else(|| old.memory_type.clone()),
        category: input.category.unwrap_or_else(|| old.category.clone()),
        title: input.title.unwrap_or(old.title),
        content: input.content.unwrap_or_else(|| old.content.clone()),
        context: input.context.unwrap_or(old.context),
        // The user's stated reason stays through a correction, as the
        // evidence does, unless the update gives a new one ("" clears it).
        reason: input.reason.unwrap_or(old.reason),
        scope: input.scope.unwrap_or(if old.scope == "project" {
            Scope::Project
        } else {
            Scope::Global
        }),
        project: input.project.unwrap_or_else(|| old.project.clone()),
        source: Source::Explicit,
        // The evidence is often the user's own words or the observation the
        // memory came from: a correction keeps it unless it gives new evidence.
        evidence: input.evidence.unwrap_or(old.evidence),
        // The user's own statement rests on no other memory, and only
        // inferences cite supports.
        evidence_ids: vec![],
        origin: input.origin.unwrap_or_default(),
        supersedes: vec![],
        merge: false,
        confirms: None,
        distinct_from: vec![],
        user_confirmed: true,
    };
    if let Some(reason) = normalize(&mut next) {
        return Ok(json!({"updated":false,"reason":reason}));
    }
    // A scope change always changes the project too: a global memory names
    // none, and a project memory must name one.
    let changed = next.memory_type != old.memory_type
        || next.category != old.category
        || next.content != old.content
        || next.project != old.project;
    let status = if changed || !wording {
        // An observation or inference carries the agent's explanation, which
        // is not the user's reason once the memory becomes their statement.
        if old_source != "explicit" && !new_reason {
            next.reason.clear();
        }
        "confirmed".to_owned()
    } else {
        next.source = source_named(&old_source);
        next.evidence_ids = old_supports.into_iter().map(MemoryRef::from).collect();
        if let Some(reason) = normalize(&mut next) {
            return Ok(json!({"updated":false,"reason":reason}));
        }
        match (revived, next.source) {
            (false, _) => old_status,
            (true, Source::Explicit) => "confirmed".to_owned(),
            (true, _) => "tentative".to_owned(),
        }
    };
    if input.replace_origins && next.origin.is_empty() {
        return Ok(
            json!({"updated":false,"reason":"replace_origins needs the origin that replaces them"}),
        );
    }
    tx.execute("UPDATE memories SET memory_type=?1,category=?2,context=?3,title=?4,content=?5,reason=?6,scope=?7,project=?8,source=?9,status=?10,evidence=?11,evidence_ids=?12,superseded_by=NULL,updated_at=CURRENT_TIMESTAMP WHERE id=?13",params![next.memory_type,next.category,next.context,next.title,next.content,next.reason,next.scope.name(),next.project,next.source.name(),status,next.evidence,serde_json::to_string(&next.evidence_ids)?,memory_id.to_string()])?;
    // The memory keeps the conversations it came from; a full restatement
    // in a new conversation replaces them with that one.
    if input.replace_origins {
        tx.execute(
            "DELETE FROM memory_origins WHERE memory_id=?1",
            [memory_id.to_string()],
        )?;
    }
    add_origin(&tx, memory_id, &next.origin)?;
    tx.commit()?;
    let mut result = json!({"updated":true,"id":memory_id,"status":status});
    if revived {
        result["revived"] = json!(true);
    }
    Ok(result)
}

/// The source a stored memory names.
fn source_named(name: &str) -> Source {
    match name {
        "observed" => Source::Observed,
        "inferred" => Source::Inferred,
        _ => Source::Explicit,
    }
}

/// Records that the user means the memory differently from each of
/// `distinct_from`. Only the pairs are written: no memory changes source,
/// status, origins or timestamps, so nothing becomes stale. Superseded and
/// stale memories may be named; a pair outlives neither memory.
fn mark_distinct(db: &Path, mut input: Update) -> Result<Value> {
    let refused = |reason: &str| Ok(json!({"updated":false,"reason":reason}));
    let changes = input.memory_type.is_some()
        || input.category.is_some()
        || input.title.is_some()
        || input.content.is_some()
        || input.context.is_some()
        || input.reason.is_some()
        || input.evidence.is_some()
        || input.scope.is_some()
        || input.project.is_some()
        || input.origin.is_some()
        || input.replace_origins;
    if changes {
        return refused("Mark memories distinct separately from changing one");
    }
    let conn = connect(db)?;
    let tx = write(&conn)?;
    let memory_id = match full_id(&tx, &input.memory_id)? {
        Ok(id) => id,
        Err(reason) => return refused(&reason),
    };
    if let Some(reason) = resolve_all(&tx, &mut input.distinct_from)? {
        return refused(&reason);
    }
    let others = uuids(&input.distinct_from);
    if others.contains(&memory_id) {
        return refused("A memory cannot be distinct from itself");
    }
    for id in std::iter::once(memory_id).chain(others.iter().copied()) {
        if get(&tx, id)?.is_none() {
            return refused("Memory does not exist");
        }
    }
    let mut marked = 0;
    for other in others {
        marked += mark_pair(&tx, memory_id, other)?;
    }
    tx.commit()?;
    Ok(json!({"updated":false,"distinct_marked":marked,"id":memory_id}))
}

pub fn forget(db: &Path, input: Forget) -> Result<Value> {
    if !input.user_confirmed {
        return Ok(
            json!({"deleted":false,"reason":"Forgetting memory requires an explicit user request"}),
        );
    }
    let origin = input.origin.map(|origin| clean(&origin, usize::MAX));
    let conn = connect(db)?;
    let tx = write(&conn)?;
    let active = ids_where(&tx, ACTIVE)?;
    let orphaned = ids_where(&tx, ORPHANED)?;
    let mut result = match (input.memory_id, origin) {
        (Some(named), None) => {
            let id = match full_id(&tx, &named)? {
                Ok(id) => id,
                Err(reason) => return Ok(json!({"deleted":false,"reason":reason})),
            };
            let count = tx.execute("DELETE FROM memories WHERE id=?1", [id.to_string()])?;
            json!({"deleted":count>0,"id":id})
        }
        (None, Some(origin)) if !origin.is_empty() => {
            // Removing the origin stales dependents of every memory that had it;
            // a memory is deleted only when no other conversation stated it.
            let touched = tx
                .prepare("DELETE FROM memory_origins WHERE origin=?1 RETURNING memory_id")?
                .query_map([&origin], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<BTreeSet<_>>>()?;
            let (mut ids, mut kept) = (BTreeSet::new(), BTreeSet::new());
            for id in touched {
                let deleted = tx.execute(
                    "DELETE FROM memories WHERE id=?1 AND NOT EXISTS (SELECT 1 FROM memory_origins WHERE memory_id=?1)",
                    [&id],
                )?;
                if deleted > 0 {
                    ids.insert(id);
                } else {
                    kept.insert(id);
                }
            }
            // The conversation's links go with it.
            let unlinked = tx.execute(
                "DELETE FROM conversation_projects WHERE origin=?1",
                [&origin],
            )?;
            json!({"deleted":!ids.is_empty(),"count":ids.len(),"ids":ids,"kept":kept,"unlinked":unlinked})
        }
        _ => {
            return Ok(
                json!({"deleted":false,"reason":"Give exactly one of memory_id or a non-empty origin"}),
            );
        }
    };
    // What the deletion did beyond itself, for the user to hear now rather
    // than at the next review: patterns that lost a support, and memories
    // whose replacement is gone, which update can make active again.
    let stale = ids_where(&tx, "status='stale'")?;
    result["staled"] = json!(active.intersection(&stale).collect::<Vec<_>>());
    result["left_superseded"] = json!(
        ids_where(&tx, ORPHANED)?
            .difference(&orphaned)
            .collect::<Vec<_>>()
    );
    tx.commit()?;
    Ok(result)
}

/// The ids of the memories matching `condition`, in id order.
fn ids_where(conn: &Connection, condition: &str) -> Result<BTreeSet<String>> {
    Ok(conn
        .prepare(&format!("SELECT id FROM memories WHERE {condition}"))?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listed_memory_that_cannot_be_read_back_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::memory::schema::connect(&dir.path().join("m.sqlite3")).unwrap();
        let err = listed(&conn, Uuid::new_v4()).unwrap_err();
        assert!(
            err.to_string()
                .contains("may be damaged; run user-memory doctor"),
            "{err}"
        );
    }
    use tempfile::tempdir;

    /// A record from `value`, titled by its content unless it names a title.
    fn draft(mut value: Value) -> Record {
        if value.get("title").is_none() {
            value["title"] = value["content"].clone();
        }
        if value.get("scope").is_none() {
            value["scope"] = json!("global");
        }
        serde_json::from_value(value).unwrap()
    }

    fn stored(db: &Path, value: Value) -> Uuid {
        let mut input = draft(value);
        input.user_confirmed = true;
        let out = record(db, input).unwrap();
        assert_eq!(out["stored"], true, "{out}");
        serde_json::from_value(out["id"].clone()).unwrap()
    }

    #[test]
    fn marking_distinct_needs_confirmation_existing_ids_and_nothing_else() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("memory.sqlite3");
        let a = stored(
            &db,
            json!({"type":"preference","category":"x","content":"A"}),
        );
        let b = stored(
            &db,
            json!({"type":"preference","category":"y","content":"B"}),
        );
        let c = stored(
            &db,
            json!({"type":"preference","category":"y","content":"C","supersedes":[b]}),
        );
        let mark = |extra: Value| {
            let mut value = json!({"memory_id":a,"user_confirmed":true});
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            update(&db, serde_json::from_value(value).unwrap()).unwrap()
        };
        assert_eq!(
            mark(json!({"distinct_from":[b],"user_confirmed":false}))["reason"],
            "Updating memory requires an explicit user request"
        );
        for field in [
            json!({"type":"fact"}),
            json!({"category":"z"}),
            json!({"content":"A2"}),
            json!({"context":"ctx"}),
            json!({"reason":"why"}),
            json!({"scope":"global"}),
            json!({"project":"repo"}),
            json!({"origin":"chat/x"}),
        ] {
            let mut extra = field.clone();
            extra["distinct_from"] = json!([b]);
            assert_eq!(
                mark(extra)["reason"],
                "Mark memories distinct separately from changing one",
                "{field}"
            );
        }
        assert_eq!(
            mark(json!({"distinct_from":[b, a]}))["reason"],
            "A memory cannot be distinct from itself"
        );
        assert_eq!(
            mark(json!({"distinct_from":[b, Uuid::new_v4()]}))["reason"],
            "Memory does not exist"
        );
        let pairs = || {
            connect(&db)
                .unwrap()
                .query_row("SELECT count(*) FROM memory_distinct", [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap()
        };
        assert_eq!(pairs(), 0, "refusals write nothing");
        // A superseded memory may be named; listing it twice marks it once.
        assert_eq!(
            mark(json!({"distinct_from":[b, c, b]})),
            json!({"updated":false,"distinct_marked":2,"id":a})
        );
        assert_eq!(mark(json!({"distinct_from":[c]}))["distinct_marked"], 0);
        let gone = json!({"memory_id":Uuid::new_v4(),"distinct_from":[a],"user_confirmed":true});
        assert_eq!(
            update(&db, serde_json::from_value(gone).unwrap()).unwrap()["reason"],
            "Memory does not exist"
        );
        assert_eq!(pairs(), 2);
    }

    #[test]
    fn support_checks_cover_existence_inferences_and_scope() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("memory.sqlite3");
        let a = stored(
            &db,
            json!({"type":"preference","category":"x","content":"A"}),
        );
        let b = stored(
            &db,
            json!({"type":"preference","category":"x","content":"B"}),
        );
        let local = stored(
            &db,
            json!({"type":"preference","category":"x","content":"C","scope":"project","project":"repo"}),
        );
        let pattern = stored(
            &db,
            json!({"type":"pattern","category":"x","content":"P","source":"inferred","evidence_ids":[a,b]}),
        );
        let conn = connect(&db).unwrap();
        let gone = Uuid::new_v4();
        let check = |ids: Vec<Uuid>, scope: &str, extra: &HashSet<Uuid>, missing: Missing| {
            let mut input = draft(
                json!({"type":"pattern","category":"x","content":"Q","source":"inferred","scope":scope,"project":if scope == "project" {"repo"} else {""}}),
            );
            input.evidence_ids = ids.into_iter().map(MemoryRef::from).collect();
            check_support(&conn, &input, extra, missing).unwrap()
        };
        let none = HashSet::new();
        assert_eq!(check(vec![a, b], "global", &none, Missing::Refuse), None);
        assert_eq!(
            check(vec![a, gone], "global", &none, Missing::Refuse),
            Some("Supporting memory does not exist")
        );
        assert_eq!(check(vec![a, gone], "global", &none, Missing::Allow), None);
        assert_eq!(
            check(
                vec![a, gone],
                "global",
                &HashSet::from([gone]),
                Missing::Refuse
            ),
            None
        );
        assert_eq!(
            check(vec![a, pattern], "global", &none, Missing::Allow),
            Some("Evidence must be active explicit or observed memory, not another inference")
        );
        assert_eq!(
            check(vec![a, local], "global", &none, Missing::Refuse),
            Some("Inferences cannot broaden or cross the scope of their evidence")
        );
        assert_eq!(
            check(vec![a, local], "project", &none, Missing::Refuse),
            None
        );
    }
}
