//! The client session hooks: at session start the conversation origin, the
//! projects the session sees and the index; before each prompt, what changed
//! in those projects and in memory since the session last looked.
//!
//! Never fail: a broken store, endpoint or hook payload yields at most one
//! short line at session start and nothing before a prompt, so the client
//! session continues normally.

mod live;

use crate::{db_path, memory, project, setup};
use anyhow::Context;
use clap::ValueEnum;
use live::{LINKED, LISTED, PENDING, THIS_FOLDER};
use serde_json::{Value, json};
use std::{
    io::{IsTerminal, Read},
    path::{Path, PathBuf},
    time::Duration,
};

/// Upper bound for the whole hook, well under the client hook timeout.
const LIMIT: Duration = Duration::from_secs(5);
/// Hook output enters model context; clients cap it near 10,000 characters.
const CHARS: usize = 9500;
/// The projects line's and the index's share of [`CHARS`]: above it, whole
/// groups collapse into counted lines, leaving room for the origin line.
const INDEX_CHARS: usize = 9000;
/// The instruction-file sync's share of [`LIMIT`]: a slow one is cut off and
/// the hook shows the whole index, with time left to fetch it.
const SYNC_SHARE: Duration = Duration::from_secs(2);
/// The largest index page, so the hook reads the whole index in few requests.
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

/// Said in place of the projects line when the session sees no project, so
/// the agent links one or proposes a key rather than recording globally:
/// this opening, then [`NO_PROJECT_SEVERAL`] or the repositories found
/// inside with [`NO_PROJECT_LISTED`], then [`NO_PROJECT_NEW`].
const NO_PROJECT: &str = "No project: this folder has no git remote and no .user-memory-key.";
/// For a folder whose repositories session start did not find.
const NO_PROJECT_SEVERAL: &str = "If it holds several repositories, link each one you work in; to load some in every session here, run `user-memory project-id --include <folder>` when the user asks for that, or propose it first.";
/// After the repositories session start found inside the folder.
const NO_PROJECT_LISTED: &str = "Link the ones you work in; to load some in every session here, run `user-memory project-id --include <folder>` when the user asks for that, or propose it first.";
const NO_PROJECT_NEW: &str = "If it is one new project, run `user-memory project-id --set <key>` here when the user names its key, or propose one (such as its planned remote, github.com/<owner>/<name>) and run it after their yes. Record nothing about this work as global without the user's yes.";
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
    let (_, index) = tokio::join!(
        tokio::time::timeout_at(deadline, snapshot),
        tokio::time::timeout_at(deadline, index(path, client, origin.clone()))
    );
    let index = match index {
        Ok(Ok(text)) => capped(text, CHARS),
        Ok(Err(err)) => {
            let reason = err.to_string();
            let reason = reason.lines().next().unwrap_or_default();
            format!("User memory unavailable: {}", memory::clean(reason, 600))
        }
        Err(_) => "User memory unavailable: timed out".into(),
    };
    match origin {
        Some(origin) => format!(
            "Conversation origin: {origin} (use it as origin when recording memories from this conversation)\n{index}"
        ),
        None => index,
    }
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
    let path = live::snapshot_path(client.name(), payload["session_id"].as_str()?)?;
    let snapshot = snapshot(checkout, origin).await.ok()?;
    live::save(&path, &snapshot).ok()
}

/// Hook output before a prompt: a notice of what changed in memory since the
/// session last looked, or nothing, which is also the answer to any failure
/// or to taking longer than [`PROMPT_LIMIT`].
pub async fn prompt_submit(client: HookClient) -> String {
    let notice = match tokio::time::timeout(hook_time(PROMPT_LIMIT), changes(client)).await {
        Ok(notice) => notice,
        Err(_) => {
            // Debug builds say so, so tests can tell this cut-off from the
            // other ways the hook stays silent.
            #[cfg(debug_assertions)]
            eprintln!("user-memory: the prompt hook stopped at its time limit");
            None
        }
    };
    notice
        .map(|text| {
            json!({"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":text}})
                .to_string()
        })
        .unwrap_or_default()
}

/// What changed since the session in the hook payload last looked, saving
/// what it sees now. A session without a snapshot gets one and no notice.
async fn changes(client: HookClient) -> Option<String> {
    let bytes = payload().await?;
    let origin = origin(client, &bytes);
    let payload: Value = serde_json::from_slice(&bytes).ok()?;
    let path = live::snapshot_path(client.name(), payload["session_id"].as_str()?)?;
    let checkout = payload["cwd"].as_str().unwrap_or(".");
    let now = snapshot(PathBuf::from(checkout), origin).await.ok()?;
    let before = live::load(&path);
    let _ = live::save(&path, &now);
    live::notice(&before?, &now)
}

/// What the session in `checkout` sees, read fresh: its projects and, from
/// the shared server or the local store, every active global memory and
/// those of its projects.
async fn snapshot(checkout: PathBuf, origin: Option<String>) -> anyhow::Result<live::Snapshot> {
    let remote = setup::configured_remote()?;
    let seen = look(checkout, origin.as_deref(), remote.as_ref()).await?;
    let input = json!({
        "projects": seen.keys(),
        "origin_links": origin,
        "stamps": true,
    });
    let reply = match &remote {
        Some(connection) => connection.call("index", input).await?,
        None => {
            let input: memory::Index = serde_json::from_value(input)?;
            let db = db_path();
            tokio::task::spawn_blocking(move || memory::index(&db, &input)).await??
        }
    };
    let stamps =
        memory::parse_stamps(&reply).context("The memory server sent no stamps; update it")?;
    Ok(live::Snapshot {
        projects: Some(seen.projects),
        stamps,
    })
}

/// The lines naming the projects the session in `path` sees, then the index
/// of them, from the shared server or the local store: every page of it, so
/// shortening it to the hook's share of the output still lists or counts
/// each memory. When the client's instruction file holds the global rules,
/// they are left out rather than repeated.
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
    let remote = remote.as_ref();
    let whole = whole_of(
        remote,
        json!({
            "projects": seen.keys(),
            "origin_links": origin,
            "without_global_rules": rules_elsewhere,
        }),
    )
    .await?;
    // Only a session with no project at all needs the folder's checkouts
    // named, so it can link one without asking the user for its remote.
    let repositories = if seen.projects.is_empty() {
        tokio::task::spawn_blocking(move || repositories_here(&here)).await?
    } else {
        Repositories::default()
    };
    let line = projects_lines(&seen, &repositories);
    let room = INDEX_CHARS.saturating_sub(line.chars().count() + 1);
    let others: Vec<&live::Project> = seen
        .projects
        .iter()
        .filter(|p| !seen.core.contains(&p.key))
        .collect();
    let index = if whole.chars().count() <= room || others.is_empty() {
        memory::shorten(&whole, room)
    } else {
        // Too large with every project: global memories and the folder's
        // own projects first, then a share of the room for each other one.
        let core = whole_of(
            remote,
            json!({"projects": seen.core, "without_global_rules": rules_elsewhere}),
        )
        .await?;
        let mut projects = Vec::new();
        for p in others {
            let index = whole_of(remote, json!({"projects": [&p.key], "scope": "project"})).await?;
            projects.push(memory::ProjectIndex {
                key: p.key.clone(),
                // The Projects line names where each comes from.
                label: p.key.clone(),
                index,
            });
        }
        memory::tiered(&whole, &core, &projects, room)
    };
    Ok(format!("{line}\n{index}"))
}

/// Every page of the index `input` asks for, merged, from the shared
/// server or the local store.
async fn whole_of(
    remote: Option<&crate::remote::Connection>,
    input: Value,
) -> anyhow::Result<String> {
    let mut fetch = async |cursor: Option<String>| -> anyhow::Result<String> {
        let mut input = input.clone();
        input["page_size"] = json!(INDEX_PAGE);
        input["cursor"] = json!(cursor);
        if let Some(connection) = remote {
            connection.call("index", input).await
        } else {
            let input: memory::Index = serde_json::from_value(input)?;
            let db = db_path();
            tokio::task::spawn_blocking(move || memory::index(&db, &input)).await?
        }
    };
    Ok(memory::merge(&whole_index(&mut fetch).await?))
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
            NO_PROJECT_SEVERAL.to_owned()
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
        format!("{NO_PROJECT} {found} {NO_PROJECT_NEW}")
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

/// Keeps whole lines within `limit` characters and says when it cut: the last
/// resort when even a shortened index does not fit.
fn capped(text: String, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text;
    }
    let mut kept = String::new();
    for line in text.lines() {
        if kept.chars().count() + line.chars().count() + 1 > limit {
            break;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    kept.push_str("(index cut to fit; index lists every line)");
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let note = "(index cut to fit; index lists every line)";
        assert_eq!(capped("abc\ndef".into(), 7), "abc\ndef");
        assert_eq!(capped("abc\ndef\ng".into(), 8), format!("abc\ndef\n{note}"));
        assert_eq!(capped("abc\ndef\nx".into(), 7), format!("abc\n{note}"));
        assert_eq!(capped("ab\nc\nd".into(), 4), format!("ab\n{note}"));
        assert_eq!(capped("abcdef".into(), 3), note);
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
