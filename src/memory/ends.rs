//! Tag conditions (#121): which memories in force wait for a tag of a
//! project, and their end once a checkout of that project has it. Only the
//! hooks call these, on this machine's store or through a server's
//! `POST /tags`; no MCP tool ends a condition by tag, so no tool call can
//! end one without the tag.

use super::{
    clean,
    in_force::live,
    project_key,
    schema::{BUSY, connect, write_within},
};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The most pending tag conditions one answer lists, oldest first: a
/// project with more is checked on these, the rest as they come up.
pub const TAGS_PENDING: usize = 1000;
/// How long a server's end of a tag condition waits for the store: as long
/// as any write does.
pub const TAGS_WAIT: std::time::Duration = BUSY;
/// Hex characters of a commit the evidence names, as `git log --oneline`
/// shows one.
const SHORT_COMMIT: usize = 7;
/// Longest origin: the store's limit for a conversation's origin.
const ORIGIN_CHARS: usize = 240;

/// A memory whose tag condition is pending, and its tag.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Pending {
    pub memory: String,
    pub tag: String,
}

/// A tag a checkout of the project has, for the memory waiting for it, and
/// the commit it points at.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct TagEnd {
    pub memory: String,
    pub tag: String,
    pub commit: String,
}

/// The memories in force whose condition is a tag of `project` (a project
/// key or git remote), global or project scope, at most [`TAGS_PENDING`],
/// the oldest condition first; those set in the same second in the order
/// they were stored.
pub fn pending_tags(db: &std::path::Path, project: &str) -> Result<Vec<Pending>> {
    let conn = connect(db)?;
    let mut statement = conn.prepare(concat!(
        "SELECT id,until_value FROM memories WHERE until_kind='tag' AND until_project=?1 AND ",
        live!("memories"),
        " ORDER BY updated_at, rowid LIMIT ?2"
    ))?;
    let rows = statement.query_map(
        params![project_key(project), i64::try_from(TAGS_PENDING)?],
        |r| {
            Ok(Pending {
                memory: r.get(0)?,
                tag: r.get(1)?,
            })
        },
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Why a tag end can't be taken, in the words a client prints.
fn refused(end: &TagEnd) -> Option<String> {
    if Uuid::parse_str(&end.memory).is_err() {
        return Some(format!("memory {:?} is not a memory's full id", end.memory));
    }
    let hex = end.commit.bytes().all(|b| b.is_ascii_hexdigit());
    if !hex || !matches!(end.commit.len(), 40 | 64) {
        return Some(format!(
            "commit {:?} is not a commit's 40 or 64 hex characters",
            end.commit
        ));
    }
    None
}

/// What ending conditions by tag did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Ended {
    pub written: usize,
    pub skipped: usize,
}

/// Ends each of `ended` whose memory is in force with a tag condition of
/// `project` naming exactly its tag, with the end record a tag gives: the
/// tag, its commit and the checkout's project, under `origin`. Any other is
/// skipped: a memory not stored, ended already, or with another condition.
/// The first record stands, as every end does. A malformed entry refuses
/// the whole request, writing nothing. Waits at most `wait` for the store
/// ([`TAGS_WAIT`] on a server; a hook passes what its time limit leaves).
pub fn end_by_tags(
    db: &std::path::Path,
    project: &str,
    origin: &str,
    ended: &[TagEnd],
    wait: std::time::Duration,
) -> Result<std::result::Result<Ended, String>> {
    if let Some(reason) = ended.iter().find_map(refused) {
        return Ok(Err(reason));
    }
    let origin = clean(origin, usize::MAX);
    if origin.chars().count() > ORIGIN_CHARS {
        return Ok(Err(format!(
            "origin has more than {ORIGIN_CHARS} characters"
        )));
    }
    let project = project_key(project);
    let mut result = Ended::default();
    if ended.is_empty() {
        return Ok(Ok(result));
    }
    let conn = connect(db)?;
    let mut tx = write_within(&conn, wait)?;
    for end in ended {
        let waiting: Option<String> = tx
            .query_row(
                concat!(
                    "SELECT id FROM memories WHERE id=?1 AND until_kind='tag' AND until_value=?2 AND until_project=?3 AND ",
                    live!("memories")
                ),
                params![end.memory, end.tag, project],
                |r| r.get(0),
            )
            .optional()?;
        let Some(id) = waiting else {
            result.skipped += 1;
            continue;
        };
        tx.note(Some("update"), Uuid::parse_str(&id).ok(), &origin)?;
        let evidence = format!(
            "tag {} at {} in a checkout of {project}",
            end.tag,
            &end.commit[..SHORT_COMMIT]
        );
        let added = tx.execute(
            "INSERT OR IGNORE INTO memory_ended(memory_id,how,evidence,origin) VALUES (?1,'tag',?2,?3)",
            params![id, evidence, origin],
        )?;
        if added == 1 {
            result.written += 1;
        } else {
            result.skipped += 1;
        }
    }
    tx.commit()?;
    Ok(Ok(result))
}
