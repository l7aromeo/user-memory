//! The client session hooks: at session start the conversation origin, the
//! projects the session sees and the index; before each prompt, what changed
//! in those projects and in memory since the session last looked.
//!
//! Never fail: a broken store, endpoint or hook payload yields at most one
//! short line at session start and nothing before a prompt, so the client
//! session continues normally.

mod live;

use crate::{db_path, memory, memory::FitUnit, project, setup};
use anyhow::Context;
use clap::ValueEnum;
use live::{LINKED, LISTED, PENDING, THIS_FOLDER};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{IsTerminal, Read},
    path::{Path, PathBuf},
    time::Duration,
};

/// Upper bound for the whole hook, well under the client hook timeout.
const LIMIT: Duration = Duration::from_secs(5);
/// Codex shows the agent only the head and tail of a hook's text over
/// 2,500 tokens (`DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT` in
/// codex-rs/hooks/src/output_spill.rs), which it counts as UTF-8 bytes over
/// four (`APPROX_BYTES_PER_TOKEN` in codex-rs/utils/string/src/truncate.rs).
const CODEX_LIMIT: usize = 2_500 * 4;
/// Claude Code saves a hook's text over 10,000 characters to a file and
/// shows the agent a preview.
const CLAUDE_LIMIT: usize = 10_000;
/// What Claude Code counts [`CLAUDE_LIMIT`] in: UTF-16 units, as measured
/// with Claude Code 2.1.325 against a fake model. A session-start text of
/// 9,900 emoji (19,784 units) reached the model as a preview, and one of
/// 9,900 CJK characters (9,900 units, 29,668 bytes) whole.
const CLAUDE_UNIT: FitUnit = FitUnit::Utf16Units;
/// Kept free under a client's limit for the lines session start puts before
/// the cut text: the conversation origin (about 350 with its sentence) and a
/// version line.
const ADDED_ROOM: usize = 500;
/// The projects line's and the index's share is this much less again than
/// the cut's: above it, whole groups collapse into counted lines, so the cut
/// stays a last resort.
const INDEX_MARGIN: usize = 500;
/// Said where [`capped`] cut session start's index.
const INDEX_CUT: &str = "(index cut to fit; index lists every line)";
/// Said where [`capped`] cut the prompt hook's notice.
const NOTICE_CUT: &str = "(cut to fit; index lists every memory)";
/// The instruction-file sync's share of [`LIMIT`]: a slow one is cut off and
/// the hook shows the whole index, with time left to fetch it.
const SYNC_SHARE: Duration = Duration::from_secs(2);
/// The largest index page, so the hook reads an older server's whole index,
/// which it lists only page by page, in few requests.
const INDEX_PAGE: usize = 200;
/// Hook payloads are small JSON objects; anything larger is ignored.
const PAYLOAD_BYTES: usize = 64 * 1024;
/// Origins longer than the stored limit would be refused when recording.
const ORIGIN_CHARS: usize = 240;
/// The session's first snapshot's share of [`LIMIT`], so a slow one never
/// delays the origin line.
const SNAPSHOT_SHARE: Duration = Duration::from_secs(2);
/// Upper bound for the hook that runs before every prompt: a slow or
/// unreachable store delays the user's prompt at most this long, and the
/// notice is skipped.
const PROMPT_LIMIT: Duration = Duration::from_secs(1);

/// `limit` for this run. Debug builds multiply every hook limit, and the
/// instruction-file sync's ([`crate::setup::SYNC_LIMIT`]), by
/// `USER_MEMORY_HOOK_TIME_SCALE`, so tests that check what a hook prints or
/// a write leaves give it room on a loaded machine; the tests of the limits
/// themselves leave it unset.
pub(crate) fn hook_time(limit: Duration) -> Duration {
    #[cfg(debug_assertions)]
    if let Some(scale) = std::env::var("USER_MEMORY_HOOK_TIME_SCALE")
        .ok()
        .and_then(|scale| scale.parse::<u32>().ok())
    {
        return limit * scale;
    }
    limit
}

/// The client that runs the hook; it decides the origin prefix, so the prefix
/// never depends on the payload's content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum HookClient {
    Claude,
    Codex,
}

impl HookClient {
    /// The client as setup knows it.
    fn installed(self) -> setup::Client {
        match self {
            Self::Claude => setup::Client::Claude,
            Self::Codex => setup::Client::Codex,
        }
    }
    fn prefix(self) -> &'static str {
        match self {
            Self::Claude => "claude:session/",
            Self::Codex => "codex:thread/",
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// How long a hook's text may be before `client` cuts it, and the unit it
/// counts in; with no client named, the stricter: Codex's bytes.
fn hook_limit(client: Option<HookClient>) -> (usize, FitUnit) {
    match client {
        Some(HookClient::Claude) => (CLAUDE_LIMIT, CLAUDE_UNIT),
        Some(HookClient::Codex) | None => (CODEX_LIMIT, FitUnit::Utf8Bytes),
    }
}

/// Said in place of the projects line when the session sees no project, so
/// the agent links one or proposes a key rather than recording globally:
/// this opening, then the repositories found inside with
/// [`NO_PROJECT_LISTED`], or [`NO_PROJECT_SEVERAL`] and [`NO_PROJECT_NEW`].
/// A folder holding repositories is a workspace, never one new project, so
/// it is never told to set a key.
const NO_PROJECT: &str = "No project: this folder has no git remote and no .user-memory-key.";
/// For a folder whose repositories session start did not find.
const NO_PROJECT_SEVERAL: &str = "If it holds several repositories, link each one you work in; to load some in every session here, run `user-memory project-id --include <folder>` when the user asks for that, or propose it first.";
/// After the repositories session start found inside the folder.
const NO_PROJECT_LISTED: &str = "Link the ones you work in; to load some in every session here, run `user-memory project-id --include <folder>` when the user asks for that, or propose it first.";
const NO_PROJECT_NEW: &str = "If it is one new project, run `user-memory project-id --set <key>` here when the user names its key, or propose one (such as its planned remote, github.com/<owner>/<name>) and run it after their yes. Record nothing about this work as global without the user's yes.";
/// Said after every reason memory is unavailable at session start: the
/// agent can't check the endpoint or the token, and must not open the token
/// file, so it goes on with the task and leaves the checks to the user.
const UNAVAILABLE_NEXT: &str =
    "Work without memory for now; tell the user once, and suggest they run user-memory doctor.";
/// The most repositories the No-project line names; the rest are counted,
/// so a folder of many checkouts keeps the header short.
const REPOSITORIES_LISTED: usize = 20;

/// Hook output for the checkout at `path`.
pub async fn start(path: PathBuf, client: Option<HookClient>) -> String {
    let deadline = tokio::time::Instant::now() + hook_time(LIMIT);
    let payload = match client {
        Some(_) => tokio::time::timeout_at(deadline, payload())
            .await
            .ok()
            .flatten(),
        None => None,
    };
    let origin = client
        .zip(payload.as_deref())
        .and_then(|(c, p)| origin(c, p));
    let checkout = path.clone();
    let snapshot = async {
        if let (Some(client), Some(payload)) = (client, &payload) {
            // The session's first snapshot: its first prompt then reports
            // only what changed after this.
            let _ = tokio::time::timeout(
                hook_time(SNAPSHOT_SHARE),
                remember(client, payload, checkout, origin.clone()),
            )
            .await;
        }
    };
    let (_, index, versions) = tokio::join!(
        tokio::time::timeout_at(deadline, snapshot),
        tokio::time::timeout_at(deadline, index(path, client, origin.clone())),
        tokio::time::timeout_at(deadline, version_line())
    );
    let index = match index {
        Ok(Ok(text)) => {
            let (limit, unit) = hook_limit(client);
            capped(text, limit - ADDED_ROOM, unit, INDEX_CUT)
        }
        Ok(Err(err)) => {
            let reason = err.to_string();
            let reason = reason.lines().next().unwrap_or_default();
            let reason = memory::clean(reason, 600);
            format!(
                "User memory unavailable: {}. {UNAVAILABLE_NEXT}",
                reason.trim_end_matches('.')
            )
        }
        Err(_) => format!("User memory unavailable: timed out. {UNAVAILABLE_NEXT}"),
    };
    let index = match origin {
        Some(origin) => format!(
            "Conversation origin: {origin} (use it as origin when recording, linking, or forgetting what this conversation recorded)\n{index}"
        ),
        None => index,
    };
    match versions.ok().flatten() {
        Some(line) => format!("{line}\n{index}"),
        None => index,
    }
}

/// What a client of a memory server says at session start when it or its
/// server is below the other's oldest supported version; nothing otherwise,
/// for a store on this machine, or when the server doesn't answer.
async fn version_line() -> Option<String> {
    let remote = setup::configured_remote().ok()??;
    let result = remote.handshake().await.ok()?;
    setup::versions::server_line(setup::versions::running(), &remote.url, &result)
}

/// The hook payload on stdin, or `None` when there is none to read.
async fn payload() -> Option<Vec<u8>> {
    if std::io::stdin().is_terminal() {
        return None;
    }
    tokio::task::spawn_blocking(|| read_payload(std::io::stdin().lock()))
        .await
        .ok()
}

/// Saves what the session named in `payload` sees of memory at its start.
async fn remember(
    client: HookClient,
    payload: &[u8],
    checkout: PathBuf,
    origin: Option<String>,
) -> Option<()> {
    let payload: Value = serde_json::from_slice(payload).ok()?;
    let path = live::snapshot_path(
        client.name(),
        payload["session_id"].as_str()?,
        &store_identity()?,
    )?;
    let snapshot = snapshot(checkout, origin, None).await.ok()?;
    // On a blocking thread, so the snapshot's share of the limit bounds the
    // save too.
    tokio::task::spawn_blocking(move || live::save(&path, &snapshot, &live::Landing::default()))
        .await
        .ok()?
        .ok()
}

/// What the session's memories come from, which names its snapshot.
fn store_identity() -> Option<String> {
    let remote = setup::configured_remote().ok()?;
    Some(identity(
        remote.as_ref().map(|remote| remote.url.as_str()),
        &db_path(),
    ))
}

/// The identity of the memories behind `server`'s address, or of the local
/// store at `db` without one: its path spelled the same before the store
/// file is created as after.
fn identity(server: Option<&str>, db: &Path) -> String {
    match server {
        Some(url) => format!("server {url}"),
        None => format!("store {}", crate::paths::resolved(db).to_string_lossy()),
    }
}

/// Hook output before a prompt: a notice of what changed in memory since the
/// session last looked, or nothing, which is also the answer to any failure
/// or to taking longer than [`PROMPT_LIMIT`].
pub async fn prompt_submit(client: HookClient) -> String {
    let deadline = tokio::time::Instant::now() + hook_time(PROMPT_LIMIT);
    let landing = std::sync::Arc::new(live::Landing::default());
    let notice = match tokio::time::timeout_at(deadline, changes(client, landing.clone())).await {
        Ok(Some((notice, saved))) => match tokio::time::timeout_at(deadline, saved).await {
            Ok(_) => notice,
            // Told only with the snapshot that records it: a save abandoned
            // in time leaves the change for the next prompt.
            Err(_) => {
                stopped();
                #[cfg(debug_assertions)]
                live::hold_for_tests(live::HOLD_ABANDON);
                landing.abandon().then_some(notice).flatten()
            }
        },
        Ok(None) => None,
        Err(_) => {
            stopped();
            None
        }
    };
    let (limit, unit) = hook_limit(Some(client));
    notice
        .map(|text| capped(text, limit, unit, NOTICE_CUT))
        .map(|text| {
            json!({"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":text}})
                .to_string()
        })
        .unwrap_or_default()
}

/// Says, in a debug build, that the prompt hook stopped at its time limit, so
/// tests can tell this cut-off from the other ways the hook stays silent.
fn stopped() {
    #[cfg(debug_assertions)]
    eprintln!("user-memory: the prompt hook stopped at its time limit");
}

/// What changed since the session in the hook payload last looked, and the
/// save of what it sees now, under way on a blocking thread so the prompt's
/// limit bounds it and `landing` can abandon it. A session without a
/// snapshot gets one and no notice.
async fn changes(
    client: HookClient,
    landing: std::sync::Arc<live::Landing>,
) -> Option<(Option<String>, tokio::task::JoinHandle<std::io::Result<()>>)> {
    let bytes = payload().await?;
    let origin = origin(client, &bytes);
    let payload: Value = serde_json::from_slice(&bytes).ok()?;
    let path = live::snapshot_path(
        client.name(),
        payload["session_id"].as_str()?,
        &store_identity()?,
    )?;
    let checkout = payload["cwd"].as_str().unwrap_or(".");
    let before = live::load(&path);
    let now = snapshot(PathBuf::from(checkout), origin, before.as_ref())
        .await
        .ok()?;
    let notice = match &before {
        Some(before) => told(before, &now).await,
        None => None,
    };
    let saved = tokio::task::spawn_blocking(move || live::save(&path, &now, &landing));
    Some((notice, saved))
}

/// The notice for a session that last saw `before` and now sees `now`.
async fn told(before: &live::Snapshot, now: &live::Snapshot) -> Option<String> {
    // Memories that left with a project are still active: the index of
    // those projects alone tells them from memories no longer active.
    let gone = live::gone(before, now);
    let left = if gone.is_empty() {
        BTreeSet::new()
    } else {
        let remote = setup::configured_remote().ok()?;
        stamps(remote.as_ref(), &gone, None, None)
            .await
            .ok()?
            .stamps
            .into_keys()
            .collect()
    };
    live::notice(before, now, &left)
}

/// What the session in `checkout` sees: its projects, read fresh and kept
/// from what it saw `before` in other folders, and, from the shared server
/// or the local store, every active global memory and those of its projects.
async fn snapshot(
    checkout: PathBuf,
    origin: Option<String>,
    before: Option<&live::Snapshot>,
) -> anyhow::Result<live::Snapshot> {
    let remote = setup::configured_remote()?;
    // One spelling per folder, however the client or the command line gives
    // it, so a project seen there is matched on the next prompt.
    let folder = dunce::canonicalize(&checkout)
        .unwrap_or_else(|_| checkout.clone())
        .to_string_lossy()
        .into_owned();
    let seen = look(checkout, origin.as_deref(), remote.as_ref()).await?;
    let projects = live::kept(
        seen.projects,
        before.and_then(|b| b.projects.as_deref()),
        &folder,
    );
    let keys: Vec<String> = projects.iter().map(|p| p.key.clone()).collect();
    // A snapshot of the same projects asks only for what changed since.
    let since = before
        .filter(|b| {
            b.projects
                .as_deref()
                .is_some_and(|was| was.iter().map(|p| &p.key).eq(keys.iter()))
        })
        .and_then(|b| b.changes.clone());
    // A store on this machine whose marker still says what the snapshot
    // saw has changed nothing since: no read at all. A write that has not
    // yet rewritten the marker is told at the next prompt.
    let unchanged = remote.is_none()
        && since
            .as_ref()
            .is_some_and(|since| memory::marked(&db_path()).as_ref() == Some(since));
    let reply = if unchanged {
        memory::StampsReply {
            since,
            delta: true,
            ..memory::StampsReply::default()
        }
    } else {
        stamps(remote.as_ref(), &keys, origin.as_deref(), since).await?
    };
    // Says, in debug builds, what the hook read, so tests can tell a read
    // of the changed memories from a read of every one.
    #[cfg(debug_assertions)]
    eprintln!(
        "user-memory: read {} stamps ({})",
        reply.stamps.len() + reply.gone.len(),
        match (unchanged, reply.delta) {
            (true, _) => "none: the marker is unchanged",
            (false, true) => "changed",
            (false, false) => "every",
        }
    );
    let stamps = match (reply.delta, before) {
        (true, Some(before)) => {
            let mut stamps = before.stamps.clone();
            for id in &reply.gone {
                stamps.remove(id);
            }
            stamps.extend(reply.stamps);
            stamps
        }
        _ => reply.stamps,
    };
    Ok(live::Snapshot {
        stamps,
        projects: Some(projects),
        changes: reply.since,
    })
}

/// The stamps of every active global memory and those of `projects` (and
/// of the projects `origin` linked), from the shared server or the local
/// store.
async fn stamps(
    remote: Option<&crate::remote::Connection>,
    projects: &[String],
    origin: Option<&str>,
    since: Option<memory::Since>,
) -> anyhow::Result<memory::StampsReply> {
    let input = json!({
        "projects": projects,
        "origin_links": origin,
        "stamps": true,
        "stamps_since": since.unwrap_or_default(),
    });
    let reply = match remote {
        Some(connection) => connection.call("index", input).await?,
        None => {
            let input: memory::Index = serde_json::from_value(input)?;
            let db = db_path();
            tokio::task::spawn_blocking(move || memory::index(&db, &input)).await??
        }
    };
    memory::parse_stamps_reply(&reply).context("The memory server sent no stamps; update it")
}

/// The lines naming the projects the session in `path` sees, then the index
/// of them, from the shared server or the local store, shortened to the
/// hook's share of the output so that it still lists or counts each memory.
/// When the client's instruction file holds the global rules, they are left
/// out rather than repeated.
async fn index(
    path: PathBuf,
    client: Option<HookClient>,
    origin: Option<String>,
) -> anyhow::Result<String> {
    // Changes made from other machines reach the instruction files here,
    // before the client reads them at its next launch.
    let current = tokio::time::timeout(hook_time(SYNC_SHARE), setup::sync_instructions())
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let rules_elsewhere = client.is_some_and(|client| current.contains(&client.installed()));
    let remote = setup::configured_remote()?;
    let here = path.clone();
    // Links the store cannot list leave the folder's projects to show.
    let seen = match look(path.clone(), origin.as_deref(), remote.as_ref()).await {
        Ok(seen) => seen,
        Err(_) => seen(
            &tokio::task::spawn_blocking(move || project::folder(&path)).await?,
            &[],
        ),
    };
    // Only a session with no project at all needs the folder's checkouts
    // named, so it can link one without asking the user for its remote.
    let repositories = if seen.projects.is_empty() {
        tokio::task::spawn_blocking(move || repositories_here(&here)).await?
    } else {
        Repositories::default()
    };
    let line = projects_lines(&seen, &repositories);
    let (limit, unit) = hook_limit(client);
    let room = (limit - ADDED_ROOM - INDEX_MARGIN).saturating_sub(unit.len(&line) + 1);
    let remote = remote.as_ref();
    let mut fetch = async |input: Value| -> anyhow::Result<String> {
        let page = match remote {
            Some(connection) => connection.call("index", input).await?,
            None => {
                let input: memory::Index = serde_json::from_value(input)?;
                let db = db_path();
                tokio::task::spawn_blocking(move || memory::index(&db, &input)).await??
            }
        };
        // A page that reports the index unavailable is an error, so it is
        // never shown or merged as if it were the index.
        match page.strip_prefix(memory::INDEX_UNAVAILABLE) {
            Some(reason) => anyhow::bail!("{reason}"),
            None => Ok(page),
        }
    };
    let index = fitted(
        &mut fetch,
        &seen,
        origin.as_deref(),
        rules_elsewhere,
        room,
        unit,
    )
    .await?;
    Ok(format!("{line}\n{index}"))
}

/// The index of what the session sees within `room` in `unit`, asked of
/// `fetch` (one index call) in one request: the store builds the whole
/// index once and shortens it. An older server ignores the room and lists
/// one page, with a cursor when more follow, so the index is then read page
/// by page and shortened here, as it was before.
async fn fitted(
    fetch: &mut impl AsyncFnMut(Value) -> anyhow::Result<String>,
    seen: &Seen,
    origin: Option<&str>,
    rules_elsewhere: bool,
    room: usize,
    unit: FitUnit,
) -> anyhow::Result<String> {
    let input = json!({
        "projects": seen.keys(),
        "origin_links": origin,
        "without_global_rules": rules_elsewhere,
    });
    let mut asked = input.clone();
    asked["fit_room"] = json!(room);
    asked["fit_whole_projects"] = json!(seen.core);
    asked["fit_unit"] = json!(unit);
    let reply = fetch(asked).await?;
    if memory::next_cursor(&reply).is_none() && unit.len(&reply) <= room {
        return Ok(reply);
    }
    let whole = whole_of(fetch, input).await?;
    let others: Vec<&live::Project> = seen
        .projects
        .iter()
        .filter(|p| !seen.core.contains(&p.key))
        .collect();
    if unit.len(&whole) <= room || others.is_empty() {
        return Ok(memory::shorten(&whole, room, unit));
    }
    // Too large with every project: global memories and the folder's own
    // projects first, then a share of the room for each other one.
    let core = whole_of(
        fetch,
        json!({"projects": seen.core, "without_global_rules": rules_elsewhere}),
    )
    .await?;
    let mut projects = Vec::new();
    for p in others {
        let index = whole_of(fetch, json!({"projects": [&p.key], "scope": "project"})).await?;
        projects.push(memory::ProjectIndex {
            key: p.key.clone(),
            // The Projects line names where each comes from.
            label: p.key.clone(),
            index,
        });
    }
    Ok(memory::tiered(&whole, &core, &projects, room, unit))
}

/// Every page of the index `input` asks for, merged, from `fetch`.
async fn whole_of(
    fetch: &mut impl AsyncFnMut(Value) -> anyhow::Result<String>,
    input: Value,
) -> anyhow::Result<String> {
    let mut page = async |cursor: Option<String>| -> anyhow::Result<String> {
        let mut input = input.clone();
        input["page_size"] = json!(INDEX_PAGE);
        input["cursor"] = json!(cursor);
        fetch(input).await
    };
    Ok(memory::merge(&whole_index(&mut page).await?))
}

/// The projects a conversation linked, from the shared server or the local
/// store; a server without links answers none.
async fn linked(
    remote: Option<&crate::remote::Connection>,
    origin: &str,
) -> anyhow::Result<Vec<String>> {
    let input = json!({"origin": origin, "projects": []});
    let reply: Value = match remote {
        Some(connection) => serde_json::from_str(&connection.call("link", input).await?)?,
        None => {
            let input: memory::Link = serde_json::from_value(input)?;
            let db = db_path();
            tokio::task::spawn_blocking(move || memory::link(&db, &input)).await??
        }
    };
    Ok(reply["projects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str().map(str::to_owned))
        .collect())
}

/// What a session sees: its projects in load order, without duplicates,
/// and what to raise with the user about them.
struct Seen {
    projects: Vec<live::Project>,
    /// The folder's own keys: its project and a key waiting for a move to
    /// it, which a shortened index lists whole.
    core: Vec<String>,
    notes: Vec<String>,
}

impl Seen {
    fn keys(&self) -> Vec<&str> {
        self.projects.iter().map(|p| p.key.as_str()).collect()
    }
}

/// What the session in `checkout` sees: its folder's projects, read fresh,
/// and those its conversation `origin` linked, from the shared server or the
/// local store.
async fn look(
    checkout: PathBuf,
    origin: Option<&str>,
    remote: Option<&crate::remote::Connection>,
) -> anyhow::Result<Seen> {
    let folder = tokio::task::spawn_blocking(move || project::folder(&checkout)).await?;
    let linked = match origin {
        Some(origin) => linked(remote, origin).await?,
        None => Vec::new(),
    };
    Ok(seen(&folder, &linked))
}

/// The projects of `folder` and of the conversation's `linked` ones, in
/// load order: the folder's own key and a key waiting for a move to it, the
/// folders its `.user-memory` lists, then the links.
fn seen(folder: &project::Folder, linked: &[String]) -> Seen {
    fn add(projects: &mut Vec<live::Project>, key: &str, source: &str) {
        if projects.iter().all(|p| p.key != key) {
            projects.push(live::Project {
                key: key.into(),
                source: source.into(),
                folders: Vec::new(),
            });
        }
    }
    let mut projects = Vec::new();
    let mut notes = folder.warnings.clone();
    let own = |projects: &mut Vec<live::Project>,
               notes: &mut Vec<String>,
               key: &str,
               pending: Option<&String>,
               source: &str| {
        add(projects, key, source);
        if let Some(pending) = pending {
            add(projects, pending, PENDING);
            notes.push(project::move_advice(pending, key));
        }
    };
    if let Ok(key) = &folder.key {
        own(
            &mut projects,
            &mut notes,
            key,
            folder.pending.as_ref(),
            THIS_FOLDER,
        );
    }
    let core = projects.iter().map(|p| p.key.clone()).collect();
    for include in &folder.includes {
        notes.extend(include.warnings.iter().cloned());
        match &include.key {
            Ok(key) => own(
                &mut projects,
                &mut notes,
                key,
                include.pending.as_ref(),
                LISTED,
            ),
            Err(reason) => notes.push(format!("@{}: {reason}", include.path.display())),
        }
    }
    for key in linked {
        add(&mut projects, key, LINKED);
    }
    Seen {
        projects,
        core,
        notes,
    }
}

/// The header lines: every project the session sees, grouped by where it
/// comes from, or what to do when there is none, then what to raise with
/// the user.
fn projects_lines(seen: &Seen, repositories: &Repositories) -> String {
    let mut lines = vec![if seen.projects.is_empty() {
        let found = if repositories.listed.is_empty() {
            format!("{NO_PROJECT_SEVERAL} {NO_PROJECT_NEW}")
        } else {
            let mut named: Vec<String> = repositories
                .listed
                .iter()
                .map(|(name, key)| format!("{name} ({key})"))
                .collect();
            if repositories.more > 0 {
                named.push(format!("and {} more", repositories.more));
            }
            format!(
                "Repositories here: {}. {NO_PROJECT_LISTED}",
                named.join(", ")
            )
        };
        format!("{NO_PROJECT} {found}")
    } else {
        // Grouped by where they come from, so a workspace of many listed
        // repositories names the source once; keys stay as agents pass them.
        let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
        for p in &seen.projects {
            match groups.iter_mut().find(|(source, _)| *source == p.source) {
                Some((_, keys)) => keys.push(&p.key),
                None => groups.push((&p.source, vec![&p.key])),
            }
        }
        let named: Vec<String> = groups
            .iter()
            .map(|(source, keys)| match *source {
                THIS_FOLDER => format!("{} ({THIS_FOLDER})", keys.join(", ")),
                _ => format!("{source}: {}", keys.join(", ")),
            })
            .collect();
        format!(
            "Projects: {}. Use them as project for scoped memories.",
            named.join("; ")
        )
    }];
    if !seen.notes.is_empty() {
        lines.push(format!(
            "Raise with the user once this session: {}",
            seen.notes.join("; ")
        ));
    }
    lines.join("\n")
}

/// The repositories directly inside a folder: the first ones by name, each
/// with the key a session inside it uses, and how many more there are.
#[derive(Default)]
struct Repositories {
    listed: Vec<(String, String)>,
    more: usize,
}

/// The repositories directly inside `path`: its subfolders, hidden ones
/// aside, that hold `.git` or a `.user-memory-key`, each resolved as a
/// session inside it resolves its project, a few at a time in parallel,
/// until [`REPOSITORIES_LISTED`] have a project. One without a project is
/// left out; those not looked at are counted.
fn repositories_here(path: &Path) -> Repositories {
    let Ok(entries) = std::fs::read_dir(path) else {
        return Repositories::default();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .filter(|name| {
            let folder = path.join(name);
            folder.join(".git").exists() || folder.join(project::KEY_FILE).exists()
        })
        .collect();
    names.sort();
    let mut found = Repositories::default();
    let mut checked = 0;
    while checked < names.len() && found.listed.len() < REPOSITORIES_LISTED {
        let batch = &names[checked..names.len().min(checked + REPOSITORIES_LISTED)];
        let keys: Vec<Option<String>> = std::thread::scope(|scope| {
            let lookups: Vec<_> = batch
                .iter()
                .map(|name| scope.spawn(move || project::folder(&path.join(name)).key.ok()))
                .collect();
            lookups
                .into_iter()
                .map(|lookup| lookup.join().ok().flatten())
                .collect()
        });
        for (name, key) in batch.iter().zip(keys) {
            checked += 1;
            if let Some(key) = key {
                found.listed.push((name.clone(), key));
                if found.listed.len() == REPOSITORIES_LISTED {
                    break;
                }
            }
        }
    }
    found.more = names.len() - checked;
    found
}

/// The projects that load in the folder `path` whatever the conversation
/// links: its own keys (with one waiting for a move), and those of the
/// folders its `.user-memory` lists. Links never hold them, so unlinking
/// one says where it comes from.
pub(crate) fn loaded_here(path: &Path) -> (Vec<String>, Vec<String>) {
    let folder = project::folder(path);
    let seen = seen(&folder, &[]);
    let mut own = Vec::new();
    let mut listed = Vec::new();
    for p in seen.projects {
        let mine = p.source == THIS_FOLDER
            || (p.source == PENDING && folder.pending.as_deref() == Some(p.key.as_str()));
        if mine {
            own.push(p.key)
        } else {
            listed.push(p.key)
        }
    }
    (own, listed)
}

/// Every page of the index. When the store changes between two pages, the
/// cursor is refused and the pages read so far no longer fit together, so
/// reading starts again once from the first page; one more pass usually
/// finishes before the next write.
async fn whole_index(
    fetch: &mut impl AsyncFnMut(Option<String>) -> anyhow::Result<String>,
) -> anyhow::Result<Vec<String>> {
    match pages(fetch).await {
        Err(err) if memory::stale_cursor(&format!("{err:#}")) => pages(fetch).await,
        pages => pages,
    }
}

/// Every page of the index, from the first, as `fetch` returns each page for
/// a cursor. A page that reports the index unavailable is an error, so it is
/// never merged as if it were the rest of the index.
async fn pages(
    fetch: &mut impl AsyncFnMut(Option<String>) -> anyhow::Result<String>,
) -> anyhow::Result<Vec<String>> {
    let mut pages = Vec::new();
    let mut cursor = None;
    loop {
        let page = fetch(cursor).await?;
        if let Some(reason) = page.strip_prefix(memory::INDEX_UNAVAILABLE) {
            anyhow::bail!("{reason}");
        }
        cursor = memory::next_cursor(&page).map(str::to_owned);
        pages.push(page);
        if cursor.is_none() {
            return Ok(pages);
        }
    }
}

/// Reads the hook payload until it is a complete JSON value, end of input or
/// the size limit, so a client that keeps stdin open cannot stall the hook.
fn read_payload(mut input: impl Read) -> Vec<u8> {
    let mut payload = Vec::new();
    let mut chunk = [0; 8192];
    while payload.len() < PAYLOAD_BYTES {
        match input.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => payload.extend_from_slice(&chunk[..read]),
        }
        if serde_json::from_slice::<Value>(&payload).is_ok() {
            break;
        }
    }
    payload
}

/// The conversation origin named by a hook payload's `session_id`, with
/// whitespace and control characters removed; `None` when absent or too long.
fn origin(client: HookClient, payload: &[u8]) -> Option<String> {
    let payload: Value = serde_json::from_slice(payload).ok()?;
    let id: String = payload
        .get("session_id")?
        .as_str()?
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
        .collect();
    let origin = format!("{}{id}", client.prefix());
    (!id.is_empty() && origin.chars().count() <= ORIGIN_CHARS).then_some(origin)
}

/// Keeps whole lines within `limit` in `unit`, ending with `note` when it
/// cut, the note included: the last resort when even a shortened index does
/// not fit.
fn capped(text: String, limit: usize, unit: FitUnit, note: &str) -> String {
    if unit.len(&text) <= limit {
        return text;
    }
    let room = limit.saturating_sub(unit.len(note));
    let mut kept = String::new();
    let mut used = 0;
    for line in text.lines() {
        let width = unit.len(line) + 1;
        if used + width > room {
            break;
        }
        kept.push_str(line);
        kept.push('\n');
        used += width;
    }
    kept.push_str(note);
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fills the store at `db` with `per` memories globally and in each of
    /// `projects`, of rule and other types, across a few categories.
    fn fill(db: &Path, per: usize, projects: &[&str]) {
        let conn = memory::connect(db).unwrap();
        let types = [
            "preference",
            "constraint",
            "fact",
            "decision",
            "goal",
            "lesson",
        ];
        let categories = ["workflow", "tooling", "testing"];
        let mut n = 0;
        for project in std::iter::once("").chain(projects.iter().copied()) {
            let scope = if project.is_empty() {
                "global"
            } else {
                "project"
            };
            for i in 0..per {
                n += 1;
                conn.execute(
                    "INSERT INTO memories (id,memory_type,category,title,content,scope,project) VALUES (?1,?2,?3,?4,?4,?5,?6)",
                    rusqlite::params![
                        format!("{n:08x}-0000-4000-8000-{n:012x}"),
                        types[i % types.len()],
                        categories[i % categories.len()],
                        format!("Memory {n} about the work in this place, one line long"),
                        scope,
                        project,
                    ],
                )
                .unwrap();
            }
        }
    }

    // Session start asks for its index in one request, and reads what the
    // walk of every page read: an older server, which knows no fit_room,
    // is walked page by page and the result shortened here as before.
    #[tokio::test]
    async fn the_session_index_is_one_request_and_reads_as_the_page_walk_did() {
        let dir = tempfile::tempdir().unwrap();
        let app = "github.com/acme/app";
        let others = ["github.com/acme/web", "github.com/acme/api"];
        for (case, per, listed) in [
            ("fits", 5, &[][..]),
            ("shortened", 150, &[][..]),
            ("tiered", 40, &others[..]),
        ] {
            let db = dir.path().join(format!("{case}.sqlite3"));
            let mut projects = vec![app];
            projects.extend(listed);
            fill(&db, per, &projects);
            let project = |key: &str, source: &str| live::Project {
                key: key.into(),
                source: source.into(),
                folders: Vec::new(),
            };
            let mut seen_projects = vec![project(app, THIS_FOLDER)];
            seen_projects.extend(listed.iter().map(|key| project(key, LISTED)));
            let seen = Seen {
                projects: seen_projects,
                core: vec![app.to_owned()],
                notes: Vec::new(),
            };
            let calls = std::cell::Cell::new(0);
            let mut store = async |input: Value| -> anyhow::Result<String> {
                calls.set(calls.get() + 1);
                memory::index(&db, &serde_json::from_value(input)?)
            };
            let room = CODEX_LIMIT - ADDED_ROOM - INDEX_MARGIN;
            let unit = FitUnit::Utf8Bytes;
            let one = fitted(&mut store, &seen, None, false, room, unit)
                .await
                .unwrap();
            assert_eq!(calls.get(), 1, "{case}");
            let pages = std::cell::Cell::new(0);
            let mut older = async |mut input: Value| -> anyhow::Result<String> {
                pages.set(pages.get() + 1);
                let fields = input.as_object_mut().unwrap();
                fields.remove("fit_room");
                fields.remove("fit_whole_projects");
                memory::index(&db, &serde_json::from_value(input)?)
            };
            let walked = fitted(&mut older, &seen, None, false, room, unit)
                .await
                .unwrap();
            assert_eq!(one, walked, "{case}");
            assert!(one.len() <= room, "{case}");
            let shortened = one.contains("Shortened to fit the session start");
            let tiered = one.contains(" more (index projects: [");
            assert_eq!(
                (shortened, tiered),
                match case {
                    "fits" => (false, false),
                    "shortened" => (true, false),
                    _ => (true, true),
                },
                "{case}: {one}"
            );
            // An index that fits is one page from any server.
            assert_eq!(pages.get() > 1, case != "fits", "{case}: pages");
        }
    }

    // Each server, and each local store, names snapshots of its own: two
    // servers differ, and a server differs from a store at any path.
    #[test]
    fn every_store_and_server_has_its_own_identity() {
        let db = Path::new("/home/u/.local/share/user-memory/memories.sqlite3");
        let identities: std::collections::BTreeSet<String> = [
            identity(Some("https://a.example.com/mcp"), db),
            identity(Some("https://b.example.com/mcp"), db),
            identity(None, db),
            identity(None, Path::new("/tmp/dev/memories.sqlite3")),
        ]
        .into_iter()
        .collect();
        assert_eq!(identities.len(), 4, "{identities:?}");
        assert_eq!(
            identity(Some("https://a.example.com/mcp"), db),
            "server https://a.example.com/mcp"
        );
        assert_eq!(
            identity(None, db),
            format!("store {}", crate::paths::resolved(db).to_string_lossy())
        );
        // A store named by a bare file name is the one in the current folder.
        let bare = identity(None, Path::new("memories.sqlite3"));
        assert!(
            Path::new(bare.trim_start_matches("store ")).is_absolute(),
            "{bare}"
        );
    }

    /// A fake index of three pages whose second cursor is refused `stale`
    /// times, as when the store changes between page reads; returns the
    /// pages read and the cursors asked for.
    async fn read_with(stale: usize) -> (anyhow::Result<Vec<String>>, Vec<Option<String>>) {
        let mut asked = Vec::new();
        let mut refusals = stale;
        let mut fetch = async |cursor: Option<String>| -> anyhow::Result<String> {
            asked.push(cursor.clone());
            Ok(match cursor.as_deref() {
                None => {
                    "page 1\nnext_cursor: p1.1.aa (index with this cursor lists the rest)".into()
                }
                Some("p1.1.aa") if refusals > 0 => {
                    refusals -= 1;
                    format!(
                        "{}This cursor does not belong to the current result (the memories changed since, or it was mistyped); start again without a cursor",
                        memory::INDEX_UNAVAILABLE
                    )
                }
                Some("p1.1.aa") => {
                    "page 2\nnext_cursor: p1.2.aa (index with this cursor lists the rest)".into()
                }
                Some(_) => "page 3".into(),
            })
        };
        let pages = whole_index(&mut fetch).await;
        (pages, asked)
    }

    #[tokio::test]
    async fn a_store_change_between_pages_restarts_the_index_once() {
        let (pages, asked) = read_with(0).await;
        assert_eq!(pages.unwrap().len(), 3);
        assert_eq!(asked.len(), 3);
        let (pages, asked) = read_with(1).await;
        assert_eq!(
            pages.unwrap(),
            [
                "page 1\nnext_cursor: p1.1.aa (index with this cursor lists the rest)",
                "page 2\nnext_cursor: p1.2.aa (index with this cursor lists the rest)",
                "page 3"
            ]
        );
        assert_eq!(
            asked,
            [
                None,
                Some("p1.1.aa".into()),
                None,
                Some("p1.1.aa".into()),
                Some("p1.2.aa".into())
            ]
        );
        // A second change in a row gives up, as the hook did before.
        let (pages, asked) = read_with(2).await;
        let err = pages.unwrap_err().to_string();
        assert!(memory::stale_cursor(&err), "{err}");
        assert_eq!(asked.len(), 4);
    }

    #[tokio::test]
    async fn an_unavailable_page_is_an_error_not_part_of_the_index() {
        let mut calls = 0;
        let mut fetch = async |cursor: Option<String>| -> anyhow::Result<String> {
            calls += 1;
            Ok(match cursor {
                None => {
                    "page 1\nnext_cursor: p1.1.aa (index with this cursor lists the rest)".into()
                }
                Some(_) => format!("{}store is locked", memory::INDEX_UNAVAILABLE),
            })
        };
        let err = whole_index(&mut fetch).await.unwrap_err();
        assert_eq!(err.to_string(), "store is locked");
        // Only a stale cursor is worth a second pass.
        assert_eq!(calls, 2);
    }

    #[test]
    fn output_keeps_whole_lines_within_the_limit() {
        let note = "(cut)";
        let cut = |text: &str, limit| capped(text.into(), limit, FitUnit::Utf8Bytes, note);
        assert_eq!(cut("abc\ndef", 7), "abc\ndef");
        // The note counts against the limit.
        assert_eq!(cut("ab\ncd\nefghij", 12), "ab\ncd\nefghij");
        assert_eq!(cut("ab\ncd\nefghij", 11), format!("ab\ncd\n{note}"));
        assert_eq!(cut("ab\ncd\nefghij", 10), format!("ab\n{note}"));
        assert_eq!(cut("abcdef", 5), note);
        // In the client's unit: 'é' is two bytes, one UTF-16 unit; '😀'
        // four bytes, two units.
        let text = "éé\n😀\nx";
        assert_eq!(capped(text.into(), 10, FitUnit::Utf16Units, note), text);
        assert_eq!(
            capped(text.into(), 10, FitUnit::Utf8Bytes, note),
            format!("éé\n{note}")
        );
    }

    #[test]
    fn origins_come_from_the_session_id_with_the_clients_prefix() {
        let claude =
            br#"{"session_id":"abc123","hook_event_name":"SessionStart","source":"startup"}"#;
        assert_eq!(
            origin(HookClient::Claude, claude).as_deref(),
            Some("claude:session/abc123")
        );
        let codex = br#"{"session_id":"thr_123","cwd":"/w","hook_event_name":"SessionStart"}"#;
        assert_eq!(
            origin(HookClient::Codex, codex).as_deref(),
            Some("codex:thread/thr_123")
        );
        let messy = "{\"session_id\":\" a b\\t\\u0007c\\n \"}";
        assert_eq!(
            origin(HookClient::Claude, messy.as_bytes()).as_deref(),
            Some("claude:session/abc")
        );
        let longest = format!("{{\"session_id\":\"{}\"}}", "x".repeat(240 - 15));
        assert_eq!(
            origin(HookClient::Claude, longest.as_bytes())
                .unwrap()
                .len(),
            240
        );
        let too_long = format!("{{\"session_id\":\"{}\"}}", "x".repeat(241 - 15));
        for payload in [
            too_long.as_bytes(),
            b"",
            b"not json",
            b"{}",
            br#"{"session_id":7}"#,
            br#"{"session_id":" \u0007 "}"#,
            br#"{"sessionId":"abc"}"#,
        ] {
            assert_eq!(origin(HookClient::Claude, payload), None);
        }
    }

    #[test]
    fn payload_reading_stops_at_a_complete_value_or_the_size_limit() {
        let open = br#"{"session_id":"abc"}"#.chain(std::io::repeat(b' '));
        assert_eq!(read_payload(open), br#"{"session_id":"abc"}"#);
        let endless = read_payload(std::io::repeat(b'['));
        assert!((65_536..65_536 + 8192).contains(&endless.len()));
        assert_eq!(read_payload(&b"{\"a\":"[..]), b"{\"a\":");
    }
}
