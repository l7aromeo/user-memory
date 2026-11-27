//! The working set after a compaction (#118): the `working-set` hook, run
//! on `compact` beside session start, prints what the context worked with
//! in the epoch that ended, from the store as it is now, and counts it as
//! shown in the new epoch: its own handler, since each client caps each
//! handler's text on its own, it takes no room from session start's.

use super::{
    ADDED_ROOM, HookClient, LIMIT, db_path, hook_limit, hook_time, payload,
    recall::{self, Source, State},
    session_file,
};
use crate::{memory::working_set, setup};
use rusqlite::{Connection, OpenFlags};
use std::time::Instant;

/// What `client`'s working-set hook prints for the session-start payload on
/// stdin, within its `context_limit` (the client's default room when
/// `None`): the working set of a compacted context, or nothing for any
/// other start, an error, or no memory in force. It never fails the
/// start.
pub async fn working_set(client: HookClient, context_limit: Option<usize>) -> String {
    let started = Instant::now();
    let deadline = started + hook_time(LIMIT);
    let Some(payload) = payload().await else {
        return String::new();
    };
    if Source::of(&payload) != Some(Source::Compact) {
        return String::new();
    }
    let Some(snapshot) = session_file(client, &payload) else {
        return String::new();
    };
    let (limit, unit) = hook_limit(Some(client), context_limit);
    let room = limit.saturating_sub(ADDED_ROOM);
    let remote = setup::configured_remote().ok().flatten();
    let restored = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let mut state = State::open(&recall::path(&snapshot), deadline)?;
        let Some((_, entries)) = state.working_set()? else {
            return Ok(String::new());
        };
        // A server's store gives the hook no text: pointers by id.
        let conn = match remote {
            None => Some(Connection::open_with_flags(
                db_path(),
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?),
            Some(_) => None,
        };
        let restored = working_set::restore(conn.as_ref(), &entries, room, unit)?;
        if Instant::now() >= deadline {
            return Ok(String::new());
        }
        state.restored(&restored.printed)?;
        Ok(restored.text)
    });
    restored.await.ok().and_then(Result::ok).unwrap_or_default()
}
