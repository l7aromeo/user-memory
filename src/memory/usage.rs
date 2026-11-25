//! Which memories each session used (#123): `memory_use`, one row per
//! memory, project and session origin, with what the session did with it.
//! The index orders by it and session start prints a project's most-used
//! knowledge from it. Rows carry no memory text and move no change marker.

/// One row per memory, the project it counted for and the session (its
/// origin): when the store last wrote it, and whether the session read it,
/// recorded or confirmed it, or was shown a passage of it. `held` and
/// `overridden` are for #118, null until known. No trigger and no change
/// counter: use changes nothing a session reads. A forgotten memory's rows
/// go with it. Part of the stored schema since version 12.
pub(super) const TABLE: &str = "CREATE TABLE IF NOT EXISTS memory_use (
    memory TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    project TEXT NOT NULL CHECK(length(project) BETWEEN 1 AND 240),
    origin TEXT NOT NULL CHECK(length(origin) BETWEEN 1 AND 240),
    at TEXT NOT NULL,
    read INTEGER NOT NULL CHECK(read IN (0,1)),
    recorded INTEGER NOT NULL CHECK(recorded IN (0,1)),
    shown INTEGER NOT NULL CHECK(shown IN (0,1)),
    held INTEGER CHECK(held IN (0,1)),
    overridden INTEGER CHECK(overridden IN (0,1)),
    PRIMARY KEY(memory, project, origin)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS memory_use_by_project ON memory_use(project, origin);";

use super::{clean, project_key, schema::connect, schema::write, selection::refuse_local_paths};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};
use uuid::Uuid;

/// The sessions of a project whose use counts: its newest 40 origins, as
/// #125's benchmark set it. At 10,000 memories windows of 20, 40 and 80
/// sessions listed 0.93, 0.95 and 0.95 of later sessions' needs; 40 is one
/// round of its worlds' tasks. Drift, which would make a long window stale,
/// is not modelled there.
pub(crate) const USE_WINDOW: usize = 40;
/// Most rows one request carries: a session that used more sends several.
/// A thousand rows is about 200 KB of JSON, within [`USE_REQUEST_BYTES`].
pub(crate) const USES_PER_REQUEST: usize = 1000;
/// Largest `POST /use` body, refused before it is read: a full request of
/// [`USES_PER_REQUEST`] rows with long project keys fits.
pub(crate) const USE_REQUEST_BYTES: usize = 256 * 1024;
/// Longest origin: the store's limit for a conversation's origin.
const ORIGIN_CHARS: usize = 240;
/// Longest project key: the store's limit for a project key.
const PROJECT_CHARS: usize = 240;

/// One memory a session used, and the project it counts for.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Use {
    pub memory: String,
    pub project: String,
    pub read: bool,
    pub recorded: bool,
    pub shown: bool,
    /// For #118: whether the memory held a command; absent when not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<bool>,
    /// For #118: whether the agent ran the held command again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overridden: Option<bool>,
}

/// One session's use, as `POST /use` takes it and the hooks write it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Uses {
    pub origin: String,
    pub uses: Vec<Use>,
}

/// A row ready to write: the memory's id and the project's key.
struct Row<'a> {
    memory: Uuid,
    project: String,
    given: &'a Use,
}

/// Why `uses` can't be written, in the words a client prints, or its rows.
fn checked(uses: &Uses) -> std::result::Result<(String, Vec<Row<'_>>), String> {
    let origin = clean(&uses.origin, usize::MAX);
    let length = origin.chars().count();
    if length == 0 || length > ORIGIN_CHARS {
        return Err(format!(
            "origin has {length} characters; it must have 1 to {ORIGIN_CHARS}"
        ));
    }
    if uses.uses.is_empty() || uses.uses.len() > USES_PER_REQUEST {
        return Err(format!(
            "uses has {} rows; send 1 to {USES_PER_REQUEST} per request",
            uses.uses.len()
        ));
    }
    let mut rows = Vec::with_capacity(uses.uses.len());
    for given in &uses.uses {
        let Ok(memory) = Uuid::parse_str(given.memory.trim()) else {
            return Err(format!(
                "memory {:?} is not a memory's full id",
                given.memory
            ));
        };
        refuse_local_paths(std::slice::from_ref(&given.project)).map_err(|err| err.to_string())?;
        let project = project_key(&given.project);
        if project.is_empty() || project.chars().count() > PROJECT_CHARS {
            return Err(format!(
                "project {:?} is not a project key of 1 to {PROJECT_CHARS} characters",
                given.project
            ));
        }
        rows.push(Row {
            memory,
            project,
            given,
        });
    }
    Ok((origin, rows))
}

/// What writing a session's use did, or why it was refused.
pub enum Written {
    /// Rows written and skipped, as the reply's JSON.
    Done(Value),
    /// The request is malformed: the reason, in plain text.
    Refused(String),
}

/// Writes one session's use: each row upserted by memory, project and
/// origin, its flags merged with OR and its time the store's, so sending a
/// row again only adds to it; then each project written to keeps its
/// newest [`USE_WINDOW`] origins. A row whose memory isn't stored, such as
/// one forgotten since, or a project memory's row for another project is
/// skipped: a project memory counts for its own project, a global one for
/// any. No change marker moves and no memory changes.
pub fn write_uses(db: &Path, uses: &Uses) -> Result<Written> {
    let (origin, rows) = match checked(uses) {
        Ok(checked) => checked,
        Err(reason) => return Ok(Written::Refused(reason)),
    };
    let conn = connect(db)?;
    let tx = write(&conn)?;
    let mut place = tx.prepare("SELECT scope,project FROM memories WHERE id=?1")?;
    let mut upsert = tx.prepare(
        "INSERT INTO memory_use (memory,project,origin,at,read,recorded,shown,held,overridden) VALUES (?1,?2,?3,CURRENT_TIMESTAMP,?4,?5,?6,?7,?8)
         ON CONFLICT(memory,project,origin) DO UPDATE SET at=excluded.at,read=max(read,excluded.read),recorded=max(recorded,excluded.recorded),shown=max(shown,excluded.shown),
         held=CASE WHEN excluded.held IS NULL THEN held ELSE max(coalesce(held,0),excluded.held) END,
         overridden=CASE WHEN excluded.overridden IS NULL THEN overridden ELSE max(coalesce(overridden,0),excluded.overridden) END",
    )?;
    let (mut written, mut skipped) = (0, 0);
    let mut projects = BTreeSet::new();
    for row in &rows {
        let found: Option<(String, String)> = place
            .query_row([row.memory.to_string()], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let counts =
            found.is_some_and(|(scope, project)| scope != "project" || project == row.project);
        if !counts {
            skipped += 1;
            continue;
        }
        upsert.execute(params![
            row.memory.to_string(),
            row.project,
            origin,
            row.given.read,
            row.given.recorded,
            row.given.shown,
            row.given.held,
            row.given.overridden,
        ])?;
        written += 1;
        projects.insert(row.project.clone());
    }
    drop((place, upsert));
    let mut prune = tx.prepare(
        "DELETE FROM memory_use WHERE project=?1 AND origin NOT IN (SELECT origin FROM memory_use WHERE project=?1 GROUP BY origin ORDER BY max(at) DESC, origin DESC LIMIT ?2)",
    )?;
    let window = i64::try_from(USE_WINDOW).expect("USE_WINDOW is a small constant");
    for project in &projects {
        prune.execute(params![project, window])?;
    }
    drop(prune);
    tx.commit()?;
    Ok(Written::Done(
        json!({"written": written, "skipped": skipped}),
    ))
}

/// How much a project's recent sessions used a memory: in how many of
/// them, then in how many it was read or recorded. Greater means used
/// more; ordering by it puts a read or a record before a delivery.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Used {
    pub(super) sessions: i64,
    pub(super) worked: i64,
}

/// The use of each memory in `projects`' kept sessions (each project's
/// newest [`USE_WINDOW`], as the write prunes them). A session that
/// counts for two of the projects counts once. Empty without projects, or
/// before any session wrote use.
pub(super) fn used(
    conn: &rusqlite::Connection,
    projects: &[String],
) -> Result<std::collections::HashMap<Uuid, Used>> {
    let mut found = std::collections::HashMap::new();
    if projects.is_empty() {
        return Ok(found);
    }
    let mut statement = conn.prepare(
        "SELECT memory,count(DISTINCT origin),count(DISTINCT CASE WHEN read OR recorded THEN origin END) FROM memory_use WHERE project IN (SELECT value FROM json_each(?1)) GROUP BY memory",
    )?;
    let rows = statement.query_map([serde_json::to_string(projects)?], |r| {
        Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?))
    })?;
    for row in rows {
        let (memory, sessions, worked) = row?;
        if let Ok(memory) = Uuid::parse_str(&memory) {
            found.insert(memory, Used { sessions, worked });
        }
    }
    Ok(found)
}
