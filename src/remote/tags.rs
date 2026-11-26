//! Tag conditions on a memory server's store (#121): `POST /tags` lists the
//! tag conditions pending for a project and writes the ends a client's
//! checkout found, as posted on #121. Only the hooks call it, and no MCP
//! tool reaches it, so no tool call can end a condition without the tag.

use super::Connection;
use crate::memory::{self, Pending, TAGS_PENDING, TAGS_WAIT, TagEnd};
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;

/// The route's name, beside `/delivery`.
const ROUTE: &str = "tags";
/// Largest body, refused before it is read: a full request of
/// [`TAGS_PENDING`] ends with long ids and tags fits.
const TAGS_REQUEST_BYTES: usize = 256 * 1024;
/// What a request over [`TAGS_REQUEST_BYTES`] is told.
const TOO_LARGE: &str = "A tags request is limited to 256 KiB";
/// Longest project key: the store's limit for a project key.
const PROJECT_CHARS: usize = 240;

/// Makes a debug build's server leave the route out, as a server released
/// before it answers: 404.
#[cfg(debug_assertions)]
const NO_ROUTE: &str = "USER_MEMORY_TEST_NO_TAGS_ROUTE";

/// A client's request: the session, the checkout's project and the tags it
/// found; with no `ended`, it only asks what is pending.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Asked {
    pub(crate) origin: String,
    pub(crate) project: String,
    #[serde(default)]
    pub(crate) ended: Vec<TagEnd>,
}

/// The server's answer: what is still pending, and what the request wrote.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Answer {
    pub(crate) pending: Vec<Pending>,
    pub(crate) written: usize,
    pub(crate) skipped: usize,
}

/// The tags route over the store `db`, behind the host checks of the other
/// transfers.
pub(super) fn routes(db: PathBuf, allowed: super::transfer::Allowed) -> Router {
    #[cfg(debug_assertions)]
    if std::env::var_os(NO_ROUTE).is_some() {
        return Router::new();
    }
    Router::new()
        .route(&format!("/{ROUTE}"), post(serve))
        .with_state(db)
        .layer(middleware::from_fn_with_state(
            allowed,
            super::transfer::admit,
        ))
}

/// Writes the posted ends and lists what is pending. A read-only token may
/// ask what is pending, which only reads; one that names an end is refused
/// and nothing is written.
async fn serve(State(db): State<PathBuf>, request: Request) -> Response {
    let read_only = request
        .extensions()
        .get::<super::gate::Access>()
        .is_some_and(|access| access.scope == crate::registry::Scope::Read);
    let url = super::scope::client_url(request.headers());
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|length| length.to_str().ok()?.parse::<u64>().ok());
    if declared.is_some_and(|length| length > TAGS_REQUEST_BYTES as u64) {
        return (StatusCode::PAYLOAD_TOO_LARGE, TOO_LARGE).into_response();
    }
    let Ok(body) = to_bytes(request.into_body(), TAGS_REQUEST_BYTES).await else {
        return (StatusCode::PAYLOAD_TOO_LARGE, TOO_LARGE).into_response();
    };
    let asked: Asked = match serde_json::from_slice(&body) {
        Ok(asked) => asked,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("Malformed tags request: {err}"),
            )
                .into_response();
        }
    };
    if let Some(reason) = malformed(&asked) {
        return (StatusCode::BAD_REQUEST, reason).into_response();
    }
    if read_only && !asked.ended.is_empty() {
        return (StatusCode::FORBIDDEN, super::scope::read_only(&url)).into_response();
    }
    let answered = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let ended =
            memory::end_by_tags(&db, &asked.project, &asked.origin, &asked.ended, TAGS_WAIT)?;
        let pending = memory::pending_tags(&db, &asked.project)?;
        Ok(ended.map(|ended| (ended, pending)))
    })
    .await;
    match answered {
        Ok(Ok(Ok((ended, pending)))) => Json(json!({
            "pending": pending,
            "written": ended.written,
            "skipped": ended.skipped,
        }))
        .into_response(),
        Ok(Ok(Err(reason))) => (StatusCode::BAD_REQUEST, reason).into_response(),
        Ok(Err(err)) => unavailable(&format!("{err:#}")),
        Err(err) => unavailable(&err.to_string()),
    }
}

/// Why `asked` can't be taken, before the store is opened.
fn malformed(asked: &Asked) -> Option<String> {
    let origin = asked.origin.trim().chars().count();
    if origin == 0 {
        return Some("origin is empty".to_owned());
    }
    let project = asked.project.trim().chars().count();
    if project == 0 || project > PROJECT_CHARS {
        return Some(format!(
            "project has {project} characters; it must have 1 to {PROJECT_CHARS}"
        ));
    }
    if memory::local_path(&asked.project) {
        return Some("project is a local path; give a project key or git remote URL".to_owned());
    }
    (asked.ended.len() > TAGS_PENDING).then(|| {
        format!(
            "ended has {} entries; send at most {TAGS_PENDING} per request",
            asked.ended.len()
        )
    })
}

/// The answer when the store can't be read or written now.
fn unavailable(reason: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        format!("The memory server can't check tags now: {reason}"),
    )
        .into_response()
}

/// What became of a tags request to a memory server.
#[derive(Debug)]
pub(crate) enum Told {
    /// The server answered: what is pending, and the ends are written.
    Answered(Answer),
    /// The server takes no ends from this machine's token (403): the ends
    /// count as ended only in this session.
    ReadOnly,
    /// The server is from before the route (404): no tag is checked on it
    /// for the rest of the session, and the conditions stay shown.
    Missing,
    /// The server refused the request (400): its ends are dropped, and the
    /// project counts as having none pending.
    Refused,
    /// The store can't be written now (503), or the server can't be
    /// reached: the ends are kept and sent at the next hook.
    Later,
}

impl Connection {
    /// Asks the server what is pending for `project`, sending `ended`.
    pub(crate) async fn tags(&self, origin: &str, project: &str, ended: &[TagEnd]) -> Told {
        let asked = Asked {
            origin: origin.to_owned(),
            project: project.to_owned(),
            ended: ended.to_vec(),
        };
        let Ok(body) = serde_json::to_vec(&asked) else {
            return Told::Refused;
        };
        let Ok(response) = self.send(ROUTE, "application/json", body).await else {
            return Told::Later;
        };
        match response.status() {
            status if status.is_success() => match response.json::<Answer>().await {
                Ok(answer) => Told::Answered(answer),
                Err(_) => Told::Later,
            },
            reqwest::StatusCode::FORBIDDEN => Told::ReadOnly,
            reqwest::StatusCode::NOT_FOUND => Told::Missing,
            reqwest::StatusCode::BAD_REQUEST => Told::Refused,
            _ => Told::Later,
        }
    }
}
