//! The delivery index of a memory server's store, for its clients (#180):
//! `POST /delivery` sends a scope's base, or the overlay of the changes
//! since the client's base, as the file itself. The client's connector and
//! hooks keep it in their own state folder, as #175 keeps a local store's.

use super::Connection;
use crate::memory::delivery_files::{self, served_base, served_overlay, served_position};
use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
    time::Instant,
};

/// The route's name, beside `/export`.
const ROUTE: &str = "delivery";

/// Makes a debug build's server cut short each delivery file it sends a
/// connector (asked with `since`), so tests see a download that breaks off
/// while the client has older files.
#[cfg(debug_assertions)]
const CUT: &str = "USER_MEMORY_TEST_DELIVERY_CUT";

/// Makes a debug build's server leave the route out, as a server released
/// before it answers: 404.
#[cfg(debug_assertions)]
const NO_ROUTE: &str = "USER_MEMORY_TEST_NO_DELIVERY_ROUTE";

/// Scopes whose base a server keeps built, one each at the store's latest
/// position: a base at 10,000 memories is 2.9 MB, so 32 scopes hold about
/// 90 MB at most, and a team's sessions rarely see more scopes at once.
/// The scope asked for longest ago goes first.
const DELIVERY_CACHE_SCOPES: usize = 32;

/// What a client asks for: its scope's projects, the position of the base
/// it has, if any, and the file format and stems it reads, so a server of
/// another version refuses rather than send a file it can't use.
#[derive(Deserialize, Serialize)]
pub(crate) struct Asked {
    pub(crate) projects: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) since: Option<String>,
    pub(crate) format: String,
    pub(crate) stems: String,
}

/// What the refusal of another version says.
const OTHER_VERSION: &str =
    "This memory server writes its delivery index for another version of user-memory";

/// A base being built or built, for one scope at one position.
struct Entry {
    scope: Vec<String>,
    position: String,
    asked: Instant,
    built: Arc<tokio::sync::OnceCell<Option<Arc<Vec<u8>>>>>,
}

/// The route's state: the store and the bases built for clients.
#[derive(Clone)]
struct Served {
    db: PathBuf,
    bases: Arc<Mutex<Vec<Entry>>>,
}

impl Served {
    /// The base of `scope` at the store's position `position`: one build
    /// however many ask for it, those that ask while it builds waiting for
    /// it. None when the store's keyword index isn't current.
    async fn base(&self, scope: Vec<String>, position: String) -> Option<Arc<Vec<u8>>> {
        let built = {
            let mut bases = self.bases.lock().unwrap_or_else(PoisonError::into_inner);
            match bases.iter_mut().find(|entry| entry.scope == scope) {
                Some(entry) if entry.position == position => {
                    entry.asked = Instant::now();
                    entry.built.clone()
                }
                Some(entry) => {
                    entry.position = position;
                    entry.asked = Instant::now();
                    entry.built = Arc::default();
                    entry.built.clone()
                }
                None => {
                    if bases.len() == DELIVERY_CACHE_SCOPES
                        && let Some(oldest) = bases
                            .iter()
                            .enumerate()
                            .min_by_key(|(_, entry)| entry.asked)
                            .map(|(at, _)| at)
                    {
                        bases.swap_remove(oldest);
                    }
                    let built: Arc<tokio::sync::OnceCell<_>> = Arc::default();
                    bases.push(Entry {
                        scope: scope.clone(),
                        position,
                        asked: Instant::now(),
                        built: built.clone(),
                    });
                    built
                }
            }
        };
        let db = self.db.clone();
        built
            .get_or_init(|| async move {
                // Says, in debug builds, that a base was built, so tests
                // can count the builds many clients cost.
                #[cfg(debug_assertions)]
                eprintln!("user-memory: built a delivery base for clients");
                tokio::task::spawn_blocking(move || served_base(&db, &scope))
                    .await
                    .ok()
                    .flatten()
                    .map(Arc::new)
            })
            .await
            .clone()
    }
}

/// The delivery route over the store `db`, behind the host checks of the
/// other transfers.
pub(super) fn routes(db: PathBuf, allowed: super::transfer::Allowed) -> Router {
    let served = Served {
        db,
        bases: Arc::default(),
    };
    #[cfg(debug_assertions)]
    if std::env::var_os(NO_ROUTE).is_some() {
        return Router::new();
    }
    Router::new()
        .route(&format!("/{ROUTE}"), post(serve))
        .with_state(served)
        .layer(middleware::from_fn_with_state(
            allowed,
            super::transfer::admit,
        ))
}

/// Sends the scope's overlay since the client's base when the store can
/// make it, else the scope's base. A read-only token may ask: the file
/// holds only what `read` and `search` give it.
async fn serve(State(served): State<Served>, Json(asked): Json<Asked>) -> Response {
    if asked.format != crate::memory::delivery_format()
        || asked.stems != crate::memory::delivery_stems()
    {
        return (StatusCode::CONFLICT, OTHER_VERSION).into_response();
    }
    let scope = delivery_files::scope(&asked.projects);
    let asked_since = asked.since.is_some();
    if let Some(since) = asked.since {
        let (db, projects) = (served.db.clone(), scope.clone());
        let overlay =
            tokio::task::spawn_blocking(move || served_overlay(&db, &projects, &since)).await;
        match overlay {
            Ok(Ok(Some(overlay))) => return file(overlay, true),
            Ok(Ok(None)) => {}
            Ok(Err(_)) | Err(_) => return unavailable(),
        }
    }
    let db = served.db.clone();
    let Ok(Some(position)) = tokio::task::spawn_blocking(move || served_position(&db)).await else {
        return unavailable();
    };
    match served.base(scope, position).await {
        Some(base) => file(base.as_ref().clone(), asked_since),
        None => unavailable(),
    }
}

/// The response carrying a delivery file to a client that asked with a
/// base (`since`) or without one.
fn file(bytes: Vec<u8>, since: bool) -> Response {
    #[cfg(debug_assertions)]
    if since && std::env::var_os(CUT).is_some() {
        // Says the whole length and sends half, then the connection ends:
        // a download cut short.
        let length = bytes.len();
        let mut response = Response::new(bytes[..length / 2].to_vec().into());
        response
            .headers_mut()
            .insert(header::CONTENT_LENGTH, HeaderValue::from(length));
        return response;
    }
    let _ = since;
    let mut response = Response::new(bytes.into());
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response
}

/// The store's keyword index isn't current yet, or the store can't be read:
/// the client asks again at its next poll.
fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "The memory server's delivery index is not ready",
    )
        .into_response()
}

/// What a server answered a client's ask.
pub(crate) enum Fetched {
    /// A delivery file, not yet checked.
    File(Vec<u8>),
    /// The server keeps no delivery index this client reads: an older one
    /// (404) or another version (409).
    Unsupported,
}

impl Connection {
    /// Asks the server for the delivery index of `projects`: the overlay
    /// since the client's base at `since`, or a whole base.
    pub(crate) async fn delivery(
        &self,
        projects: &[String],
        since: Option<&str>,
    ) -> Result<Fetched> {
        let asked = Asked {
            projects: projects.to_vec(),
            since: since.map(str::to_owned),
            format: crate::memory::delivery_format().to_owned(),
            stems: crate::memory::delivery_stems(),
        };
        let response = self
            .send(ROUTE, "application/json", serde_json::to_vec(&asked)?)
            .await?;
        match response.status() {
            status if status.is_success() => Ok(Fetched::File(
                response
                    .bytes()
                    .await
                    .context("The delivery index from the memory server broke off")?
                    .to_vec(),
            )),
            reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::CONFLICT => {
                Ok(Fetched::Unsupported)
            }
            status => anyhow::bail!("The memory server answered {status}"),
        }
    }
}

/// The deadline `until` as a clock time the delivery files' lock waits to.
fn lock_deadline(until: tokio::time::Instant) -> Instant {
    Instant::now() + until.saturating_duration_since(tokio::time::Instant::now())
}

/// Asks the server `connection` names for the base of `projects` and
/// writes it, by `until`; a base that doesn't arrive in time is dropped,
/// never renamed into place, and marked missed (see
/// [`delivery_files::served_missed`]). Also marked: a server that keeps no
/// delivery index this version reads, so no prompt asks it again before
/// the next session start.
async fn download(connection: &Connection, projects: &[String], until: tokio::time::Instant) {
    let fetched = tokio::time::timeout_at(until, connection.delivery(projects, None)).await;
    let taken = match fetched {
        Ok(Ok(Fetched::File(bytes))) => {
            let (url, projects) = (connection.url.clone(), projects.to_vec());
            let deadline = lock_deadline(until);
            tokio::task::spawn_blocking(move || {
                delivery_files::take_served(&url, &projects, bytes, deadline)
            })
            .await
            .ok()
            .and_then(Result::ok)
        }
        _ => None,
    };
    if !matches!(
        taken,
        Some(delivery_files::Taken::Written | delivery_files::Taken::Folded)
    ) {
        let (url, projects) = (connection.url.clone(), projects.to_vec());
        let _ = tokio::task::spawn_blocking(move || delivery_files::served_missed(&url, &projects))
            .await;
    }
}

/// Session start's part for a memory server's client (#180), by `until`:
/// folds the scope's overlay into its base on this machine, or, when there
/// is no base, downloads one.
pub(crate) async fn at_session_start(
    connection: &Connection,
    projects: &[String],
    until: tokio::time::Instant,
) {
    let (url, scope) = (connection.url.clone(), projects.to_vec());
    let deadline = lock_deadline(until);
    let found = tokio::task::spawn_blocking(move || {
        delivery_files::served_at_session_start(&url, &scope, deadline)
    })
    .await;
    if matches!(found, Ok(Ok(delivery_files::ServedStart::Missing))) {
        download(connection, projects, until).await;
    }
}

/// The prompt hook's part for a memory server's client (#180), by `until`:
/// downloads the scope's base when there is none and no download of it
/// missed since the last session start.
pub(crate) async fn at_prompt(
    connection: &Connection,
    projects: &[String],
    until: tokio::time::Instant,
) {
    let (url, scope) = (connection.url.clone(), projects.to_vec());
    let wanted =
        tokio::task::spawn_blocking(move || delivery_files::served_wanted(&url, &scope)).await;
    if wanted.unwrap_or(false) {
        download(connection, projects, until).await;
    }
}

/// The connector's part (#180), when its poll found the server at `now`:
/// asks the changes since each of this machine's bases of the server that
/// is behind `now`, and takes each, folding a base whose words a change
/// took out. A server that keeps no delivery index this version reads has
/// every file of its store deleted here. A request that fails leaves its
/// files behind the marker, and the hooks read none of them. Whether every
/// base is at `now` after it, so the connector asks again at its next poll
/// while one is behind.
pub(super) async fn upkeep(connection: &Connection, now: &crate::memory::Position) -> bool {
    let url = connection.url.clone();
    let Ok(bases) = tokio::task::spawn_blocking(move || delivery_files::served_bases(&url)).await
    else {
        return false;
    };
    let mut current = true;
    for (projects, position) in bases {
        if crate::memory::Position::kept(position.clone()).same_change(now) {
            continue;
        }
        match connection.delivery(&projects, Some(&position)).await {
            Ok(Fetched::File(bytes)) => {
                let url = connection.url.clone();
                let deadline = Instant::now() + delivery_files::DELIVERY_LOCK_WAIT;
                let taken = tokio::task::spawn_blocking(move || {
                    delivery_files::take_served(&url, &projects, bytes, deadline)
                })
                .await;
                current &= matches!(
                    taken,
                    Ok(Ok(
                        delivery_files::Taken::Written | delivery_files::Taken::Folded
                    ))
                );
            }
            Ok(Fetched::Unsupported) => {
                let url = connection.url.clone();
                let deadline = Instant::now() + delivery_files::DELIVERY_LOCK_WAIT;
                let _ = tokio::task::spawn_blocking(move || {
                    delivery_files::drop_served(&url, deadline)
                })
                .await;
                return true;
            }
            Err(_) => current = false,
        }
    }
    current
}
