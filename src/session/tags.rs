//! The tag check (#121), run only by the hooks on the client: a memory whose
//! condition is a tag of a project ends once a checkout of that project
//! has the tag. Session start always looks; a prompt asks the store again
//! what is pending; a tool hook looks only when a tag is waited for and
//! the refs' stamps changed, which costs no git spawn while they stay as
//! they were. Each hook checks after choosing what it prints. A fork, a
//! vendored copy or another repository never decides: the checkout's own
//! project (from its `origin` remote) must be the condition's. Never the
//! network beyond the store.

use super::recall::{self, State};
use crate::{
    memory::{self, TagEnd},
    project::{self, tags},
    remote::{
        Connection,
        tags::{Answer, Told},
    },
};
use std::{path::Path, time::Instant};

/// When the check runs, which decides how much it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum When {
    /// Session start: asks what is pending and looks up its tags.
    Start,
    /// A prompt: asks what is pending, and looks when the stamps changed.
    Prompt,
    /// A tool hook: looks when the stamps changed.
    Tool,
}

/// The stamp a lookup keeps beside the refs' own: the pending names it
/// looked for, so a condition recorded since is looked up once even when
/// no ref changed.
const NAMES: &str = "names";
/// The flag a server without the tags route (404) leaves: the check is off
/// for the rest of the session, and the conditions stay shown.
const OFF: &str = "off";
/// The flag a read-only token (403) leaves: ends are never sent, and count
/// as ended only in this session.
const READ_ONLY: &str = "read-only";

/// Checks the tag conditions of the checkout at `folder` for the session
/// whose snapshot is at `snapshot`, writing ends to the server `remote` or
/// this machine's store at `db` under `origin`, as `when` says, until
/// `deadline`. Sends first any end an earlier hook could not write.
///
/// With a server, the check runs on a thread of its own with its own
/// runtime: the hooks call it from inside the program's runtime, where
/// another can be neither started nor dropped.
pub(super) fn check(
    snapshot: &Path,
    folder: &Path,
    origin: &str,
    remote: Option<&Connection>,
    db: &Path,
    when: When,
    deadline: Instant,
) -> anyhow::Result<()> {
    if remote.is_none() {
        return check_on(snapshot, folder, origin, None, db, when, deadline);
    }
    std::thread::scope(|scope| {
        scope
            .spawn(|| check_on(snapshot, folder, origin, remote, db, when, deadline))
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("the tag check stopped")))
    })
}

/// [`check`] on the calling thread, outside any runtime when `remote` is
/// given.
fn check_on(
    snapshot: &Path,
    folder: &Path,
    origin: &str,
    remote: Option<&Connection>,
    db: &Path,
    when: When,
    deadline: Instant,
) -> anyhow::Result<()> {
    let mut state = State::open(&recall::path(snapshot), deadline)?;
    if state.tag_flag(OFF)? {
        return Ok(());
    }
    let runtime = remote
        .map(|_| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
        })
        .transpose()?;
    let ask = |project: &str, ended: &[TagEnd]| -> Told {
        match (remote, &runtime) {
            (Some(connection), Some(runtime)) => {
                let until = tokio::time::Instant::now()
                    + deadline.saturating_duration_since(Instant::now());
                runtime
                    .block_on(async {
                        tokio::time::timeout_at(until, connection.tags(origin, project, ended))
                            .await
                    })
                    .unwrap_or(Told::Later)
            }
            _ => local(db, project, origin, ended, deadline),
        }
    };
    send_unwritten(&mut state, &ask)?;
    // A tool hook asks the store nothing, so with no tag waited for it has
    // nothing to look for and spawns no git, in any folder.
    if when == When::Tool && !state.waits_for_tags()? {
        return Ok(());
    }
    let key = folder.to_string_lossy().into_owned();
    let (project, common) = match state.tag_folder(&key)? {
        Some(kept) => kept,
        None => {
            if Instant::now() >= deadline {
                return Ok(());
            }
            let project = project::project_id(folder).unwrap_or_default();
            let common = tags::common_dir(folder)
                .map(|dir| dir.to_string_lossy().into_owned())
                .unwrap_or_default();
            state.keep_tag_folder(&key, &project, &common)?;
            (project, common)
        }
    };
    if project.is_empty() || common.is_empty() {
        return Ok(());
    }
    if when != When::Tool {
        match ask(&project, &[]) {
            Told::Answered(answer) => {
                let pending: Vec<(String, String)> = answer
                    .pending
                    .into_iter()
                    .map(|p| (p.memory, p.tag))
                    .collect();
                state.keep_pending(&project, &pending)?;
            }
            Told::Refused => state.keep_pending(&project, &[])?,
            Told::Missing => return state.keep_tag_flag(OFF),
            Told::ReadOnly | Told::Later => {}
        }
    }
    let pending = state.pending(&project)?;
    if pending.is_empty() {
        return Ok(());
    }
    let mut names: Vec<&str> = pending.iter().map(|(_, tag)| tag.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    let mut stamps = tags::stamps(Path::new(&common), &names);
    stamps.push((NAMES.to_owned(), names.join("\n")));
    if when != When::Start && state.same_stamps(&common, &stamps)? {
        return Ok(());
    }
    if Instant::now() >= deadline {
        return Ok(());
    }
    let Some(found) = tags::found(folder, &names) else {
        return Ok(());
    };
    state.keep_stamps(&common, &stamps)?;
    let ends: Vec<(String, String, String)> = pending
        .iter()
        .filter_map(|(memory, tag)| {
            found
                .iter()
                .find(|f| f.tag == *tag)
                .map(|f| (memory.clone(), tag.clone(), f.commit.clone()))
        })
        .collect();
    if ends.is_empty() {
        return Ok(());
    }
    state.keep_ends(&project, &ends)?;
    send_unwritten(&mut state, &ask)
}

/// Sends every end kept but not yet written, by project: forgotten once
/// the store has it, or once it refused the request; kept, and counted as
/// ended here, while the store can't take it, and for good on a read-only
/// token, which is not sent again.
fn send_unwritten(state: &mut State, ask: &impl Fn(&str, &[TagEnd]) -> Told) -> anyhow::Result<()> {
    if state.tag_flag(READ_ONLY)? {
        return Ok(());
    }
    let unwritten = state.unwritten_ends()?;
    let mut projects: Vec<&str> = unwritten
        .iter()
        .map(|(project, ..)| project.as_str())
        .collect();
    projects.dedup();
    for project in projects {
        let ended: Vec<TagEnd> = unwritten
            .iter()
            .filter(|(p, ..)| p == project)
            .map(|(_, memory, tag, commit)| TagEnd {
                memory: memory.clone(),
                tag: tag.clone(),
                commit: commit.clone(),
            })
            .collect();
        match ask(project, &ended) {
            Told::Answered(Answer { pending, .. }) => {
                state.written_ends(project)?;
                let pending: Vec<(String, String)> =
                    pending.into_iter().map(|p| (p.memory, p.tag)).collect();
                state.keep_pending(project, &pending)?;
            }
            Told::Refused => state.written_ends(project)?,
            Told::Missing => return state.keep_tag_flag(OFF),
            Told::ReadOnly => return state.keep_tag_flag(READ_ONLY),
            Told::Later => {}
        }
    }
    Ok(())
}

/// The tags request answered by this machine's store at `db`, waiting for
/// it no later than `deadline`.
fn local(db: &Path, project: &str, origin: &str, ended: &[TagEnd], deadline: Instant) -> Told {
    let wait = deadline.saturating_duration_since(Instant::now());
    let written = match memory::end_by_tags(db, project, origin, ended, wait) {
        Ok(Ok(written)) => written,
        Ok(Err(_)) => return Told::Refused,
        Err(_) => return Told::Later,
    };
    match memory::pending_tags(db, project) {
        Ok(pending) => Told::Answered(Answer {
            pending,
            written: written.written,
            skipped: written.skipped,
        }),
        Err(_) => Told::Later,
    }
}
