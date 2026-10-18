//! The projects a conversation linked, so its session hooks load them with
//! the folder's own project. Links are conversation state, not memories:
//! they are never exported, and forgetting the conversation removes them.

use super::{
    Index, clean, project_key,
    schema::{connect, write},
    selection::{ScopeFilter, Selection, local_path},
};
use anyhow::Result;
use rusqlite::{Connection, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

/// The longest origin or project key the store accepts.
const KEY_LIMIT: usize = 240;
/// Said when the conversation has no origin, as without the session hooks.
const NEEDS_ORIGIN: &str = "link needs the conversation origin printed at session start; without the session hooks, pass projects to index in each call instead.";

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct Link {
    /// The conversation origin printed at session start, such as claude:session/<id>.
    pub origin: String,
    /// Project keys or git remote URLs to load in this conversation, or with unlink to remove; none lists its links. For repositories this folder always needs, use .user-memory @ lines (user-memory project-id --include) instead of linking in every conversation: run it when the user asks for that, propose it when you noticed the need.
    #[serde(default)]
    pub projects: Vec<String>,
    /// Remove the listed links instead of adding them.
    #[serde(default)]
    pub unlink: bool,
    /// Used by user-memory's own client to name this folder's own projects, which always load; leave unset.
    #[serde(default)]
    pub folder_projects: Vec<String>,
    /// Used by user-memory's own client to name the projects this folder's .user-memory lists; leave unset.
    #[serde(default)]
    pub included_projects: Vec<String>,
}

/// The projects `origin` linked, in the order it linked them.
pub(super) fn linked(conn: &Connection, origin: &str) -> Result<Vec<String>> {
    Ok(conn
        .prepare("SELECT project FROM conversation_projects WHERE origin=?1 ORDER BY rowid")?
        .query_map([origin], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Links, unlinks or lists the projects of one conversation. The folder's
/// own projects and those its `.user-memory` lists always load there, so
/// none is stored as a link or unlinked. Linking returns
/// the index lines of the projects it added, so the agent has them at once.
pub fn link(db: &Path, input: &Link) -> Result<Value> {
    let origin = clean(&input.origin, usize::MAX);
    if origin.is_empty() {
        return Ok(json!({"linked":false,"reason":NEEDS_ORIGIN}));
    }
    if origin.chars().count() > KEY_LIMIT {
        return Ok(
            json!({"linked":false,"reason":"Origin must be a short reference of at most 240 characters"}),
        );
    }
    let mut keys = Vec::new();
    for raw in &input.projects {
        if local_path(raw) || raw.contains('\\') {
            return Ok(json!({"linked":false,"reason":format!(
                "{} is a local path; link projects by project key or git remote URL (`user-memory project-id` prints the key)",
                raw.trim()
            )}));
        }
        let key = project_key(raw);
        if key.is_empty() || key.chars().count() > KEY_LIMIT {
            return Ok(json!({"linked":false,"reason":format!(
                "{} is not a project key of 1 to 240 characters",
                raw.trim()
            )}));
        }
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    let own: Vec<String> = input
        .folder_projects
        .iter()
        .map(|k| project_key(k))
        .collect();
    let listed: Vec<String> = input
        .included_projects
        .iter()
        .map(|k| project_key(k))
        .collect();
    let conn = connect(db)?;
    let tx = write(&conn)?;
    let (mut changed, mut notes) = (Vec::new(), Vec::new());
    for key in keys {
        if own.contains(&key) {
            notes.push(format!(
                "{key} comes from this folder; it always loads here"
            ));
        } else if listed.contains(&key) {
            notes.push(format!(
                "{key} is listed in .user-memory; it loads here while listed"
            ));
        } else if input.unlink {
            let removed = tx.execute(
                "DELETE FROM conversation_projects WHERE origin=?1 AND project=?2",
                params![origin, key],
            )?;
            if removed > 0 {
                changed.push(key);
            } else {
                notes.push(format!("{key} is not linked"));
            }
        } else {
            let added = tx.execute(
                "INSERT OR IGNORE INTO conversation_projects (origin,project) VALUES (?1,?2)",
                params![origin, key],
            )?;
            if added > 0 {
                changed.push(key);
            } else {
                notes.push(format!("{key} is already linked"));
            }
        }
    }
    let projects = linked(&tx, &origin)?;
    tx.commit()?;
    let mut reply = json!({"linked":true,"projects":projects});
    reply[if input.unlink { "removed" } else { "added" }] = json!(changed);
    if !notes.is_empty() {
        reply["notes"] = json!(notes);
    }
    if !input.unlink && !changed.is_empty() {
        let added = Index {
            selection: Selection {
                projects: changed,
                scope: Some(ScopeFilter::Project),
                ..Selection::default()
            },
            ..Index::default()
        };
        reply["index"] = json!(super::index(db, &added)?);
    }
    Ok(reply)
}
