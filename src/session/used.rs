//! Which memories a session used (#123), as its hooks see it: the post-tool
//! hook notes what user-memory's own `read`, `record`, `update` and
//! `search` returned (#118: `search` only as found, for the working set), a push
//! notes what it showed (`recall::State::record`), and the next prompt or
//! session start sends what is not sent yet to the store, local or a
//! server's, as `memory_use` rows. A `startup` also sends what other
//! sessions of the store left unsent after their last prompt.

use super::{
    live,
    recall::{self, How, State, Unsent},
    tools::{SEARCH_CLAUDE, SEARCH_CODEX},
};
use crate::{
    memory::{self, USES_PER_REQUEST, Use, Uses, Written},
    remote::{Connection, usage::Sent},
};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use uuid::Uuid;

/// The `read`, `record` and `update` tools as each client's hook payload
/// names them (`search` is `tools`'): Claude Code joins the server's name as
/// configured, Codex replaces its `-` with `_`.
const READ_CLAUDE: &str = "mcp__user-memory__read";
const READ_CODEX: &str = "mcp__user_memory__read";
const RECORD_CLAUDE: &str = "mcp__user-memory__record";
const RECORD_CODEX: &str = "mcp__user_memory__record";
const UPDATE_CLAUDE: &str = "mcp__user-memory__update";
const UPDATE_CODEX: &str = "mcp__user_memory__update";

/// The most other sessions' state files a `startup` sends the use of,
/// newest first: those a session left after its last prompt are recent,
/// and an older one was swept by an earlier start. Twenty is as many
/// sessions of one store as a busy machine runs at once; a file takes
/// about a millisecond to open and read.
pub(super) const USE_SWEEP_FILES: usize = 20;

/// The most a hook spends sending use, after its own text is ready: a local
/// write takes a few milliseconds and a server's request about as long on
/// a LAN. Use left unsent is sent at the next prompt or start.
pub(super) const USE_FLUSH_LIMIT: Duration = Duration::from_millis(250);

/// Notes, for the post-tool hook of the session whose snapshot is at
/// `snapshot`, the memories a call of user-memory's `read` or `search`
/// returned, its `record` stored or confirmed, or its `update` changed.
/// Anything else, and a refused record or update, notes nothing. Keeps `origin` as the session's when the file has none,
/// as one an earlier release started has not: a later startup's sweep
/// sends only under a kept origin.
pub(super) fn note(snapshot: &Path, payload: &Value, origin: Option<&str>, deadline: Instant) {
    let Some((how, memories)) = used_by(payload) else {
        return;
    };
    let agent = payload["agent_id"].as_str().unwrap_or(recall::MAIN);
    let _ = State::open(&recall::path(snapshot), deadline).and_then(|mut state| {
        state.used(agent, &memories, how)?;
        match origin {
            Some(origin) if state.origin()?.is_none() => state.set_origin(origin),
            _ => Ok(()),
        }
    });
}

/// How the call in `payload` used memories, and their full ids.
fn used_by(payload: &Value) -> Option<(How, Vec<String>)> {
    // The tool decides before its reply is read: another tool's result
    // can be long, and is never parsed here.
    let (how, done) = match payload["tool_name"].as_str()? {
        READ_CLAUDE | READ_CODEX => (How::Read, None),
        SEARCH_CLAUDE | SEARCH_CODEX => (How::Found, None),
        RECORD_CLAUDE | RECORD_CODEX => (How::Recorded, Some("stored")),
        UPDATE_CLAUDE | UPDATE_CODEX => (How::Recorded, Some("updated")),
        _ => return None,
    };
    let reply = reply_of(&payload["tool_response"])?;
    let ids: Vec<String> = match done {
        None => reply["memories"]
            .as_array()?
            .iter()
            .filter_map(|m| m["id"].as_str())
            .filter_map(full_id)
            .collect(),
        Some(done) => (reply[done] == true)
            .then(|| reply["id"].as_str().and_then(full_id))
            .flatten()
            .into_iter()
            .collect(),
    };
    Some((how, ids))
}

/// `id` when it is a memory's full id.
fn full_id(id: &str) -> Option<String> {
    Uuid::parse_str(id).ok().map(|id| id.to_string())
}

/// The tool's JSON reply in `response`, in each shape a client gives an
/// MCP tool's result to its hook: the reply itself, its text, the content
/// parts of the result (Claude Code 2.1.325's transcript keeps
/// `[{"type":"text","text":…}]`), or the result with its `content`.
fn reply_of(response: &Value) -> Option<Value> {
    match response {
        Value::Object(object) if object.contains_key("content") => reply_of(&object["content"]),
        Value::Object(_) => Some(response.clone()),
        Value::String(text) => serde_json::from_str(text).ok(),
        Value::Array(parts) => parts
            .iter()
            .filter(|part| part["type"] == "text")
            .find_map(|part| serde_json::from_str(part["text"].as_str()?).ok()),
        _ => None,
    }
}

/// Sends the use the session whose snapshot is at `snapshot` has not sent,
/// as `origin` (kept for a later sweep), to the server `remote` or this
/// machine's store at `db`, within `deadline`.
pub(super) async fn flush(
    snapshot: &Path,
    origin: Option<&str>,
    remote: Option<&Connection>,
    db: &Path,
    deadline: tokio::time::Instant,
) {
    let until = std_deadline(deadline);
    let Ok(mut state) = State::open(&recall::path(snapshot), until) else {
        return;
    };
    if let Some(origin) = origin {
        let _ = state.set_origin(origin);
    }
    send(&mut state, snapshot, remote, db, deadline).await;
}

/// At session start: sends the session's own unsent use, and on a
/// `startup` that of up to [`USE_SWEEP_FILES`] other sessions of the same
/// store, newest first, within `deadline`.
pub(super) async fn at_start(
    snapshot: &Path,
    origin: Option<&str>,
    startup: bool,
    remote: Option<&Connection>,
    db: &Path,
    deadline: tokio::time::Instant,
) {
    if recall::path(snapshot).exists() {
        flush(snapshot, origin, remote, db, deadline).await;
    }
    if !startup {
        return;
    }
    for other in others(snapshot) {
        if tokio::time::Instant::now() >= deadline {
            return;
        }
        let Ok(mut state) = State::open(&other, std_deadline(deadline)) else {
            continue;
        };
        send(
            &mut state,
            &other.with_extension("json"),
            remote,
            db,
            deadline,
        )
        .await;
    }
}

/// The state files of the store `snapshot` names other than its own,
/// newest first, at most [`USE_SWEEP_FILES`].
fn others(snapshot: &Path) -> Vec<PathBuf> {
    let own = recall::path(snapshot);
    let (Some(dir), Some(store)) = (
        snapshot.parent(),
        snapshot
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.rsplit_once('-'))
            .map(|(_, store)| format!("-{store}.{}", live::RECALL)),
    ) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(&store))
        .map(|entry| entry.path())
        .filter(|path| *path != own)
        .filter_map(|path| Some((fs::metadata(&path).ok()?.modified().ok()?, path)))
        .collect();
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    found
        .into_iter()
        .take(USE_SWEEP_FILES)
        .map(|(_, path)| path)
        .collect()
}

/// Sends `state`'s unsent use under its kept origin, counted for each
/// project its session sees (from the scope file beside `snapshot`); a
/// project memory's row for another project is skipped by the store. Marks
/// it sent once written, or once dropped: a session without a project, a
/// request the store refuses, and a server that takes no use from this
/// machine, which is then not asked again this session. Kept for the next
/// hook when the store can't be written or reached in time.
async fn send(
    state: &mut State,
    snapshot: &Path,
    remote: Option<&Connection>,
    db: &Path,
    deadline: tokio::time::Instant,
) {
    let Ok((unsent, mark)) = state.unsent() else {
        return;
    };
    if unsent.is_empty() {
        return;
    }
    let Ok(Some(origin)) = state.origin() else {
        return;
    };
    let projects = live::load_scope(&live::scope_path(snapshot)).unwrap_or_default();
    let uses = rows(&unsent, &projects);
    if uses.is_empty() {
        let _ = state.sent(mark);
        return;
    }
    match remote {
        Some(connection) => {
            match tokio::time::timeout_at(deadline, connection.send_uses(&origin, &uses)).await {
                Ok(Sent::Written) => {
                    let _ = state.sent(mark);
                }
                Ok(Sent::Dropped) => {
                    let _ = state.refuse_use();
                    let _ = state.sent(mark);
                }
                Ok(Sent::Kept) | Err(_) => {}
            }
        }
        None => {
            let db = db.to_owned();
            let write = tokio::task::spawn_blocking(move || {
                for chunk in uses.chunks(USES_PER_REQUEST) {
                    let request = Uses {
                        origin: origin.clone(),
                        uses: chunk.to_vec(),
                    };
                    match memory::write_uses(&db, &request) {
                        Ok(Written::Done(_) | Written::Refused(_)) => {}
                        Err(_) => return false,
                    }
                }
                true
            });
            if let Ok(Ok(true)) = tokio::time::timeout_at(deadline, write).await {
                let _ = state.sent(mark);
            }
        }
    }
}

/// One row per unsent memory and project.
fn rows(unsent: &[Unsent], projects: &[String]) -> Vec<Use> {
    unsent
        .iter()
        .flat_map(|used| {
            projects.iter().map(move |project| Use {
                memory: used.memory.clone(),
                project: project.clone(),
                read: used.read,
                recorded: used.recorded,
                shown: used.shown,
                held: used.held.then_some(true),
                overridden: used.overridden,
                passage: used.passage.and_then(|slot| u32::try_from(slot).ok()),
            })
        })
        .collect()
}

/// `deadline` as the standard library's clock.
fn std_deadline(deadline: tokio::time::Instant) -> Instant {
    Instant::now() + deadline.saturating_duration_since(tokio::time::Instant::now())
}
