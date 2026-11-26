//! A session's use of memories, for a memory server's store (#123):
//! `POST /use` takes one session's rows, which the hooks send at the next
//! prompt or session start. No agent sees it, and no MCP tool changes.

use super::Connection;
use crate::memory::{self, USE_REQUEST_BYTES, USES_PER_REQUEST, Use, Uses, Written};
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::post,
};
use std::path::PathBuf;

/// The route's name, beside `/delivery`.
const ROUTE: &str = "use";
/// What a request over [`USE_REQUEST_BYTES`] is told.
const TOO_LARGE: &str = "A use request is limited to 256 KiB";

/// Makes a debug build's server leave the route out, as a server released
/// before it answers: 404.
#[cfg(debug_assertions)]
const NO_ROUTE: &str = "USER_MEMORY_TEST_NO_USE_ROUTE";

/// The use route over the store `db`, behind the host checks of the other
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

/// Writes the posted session's use. A read-only token is refused before
/// the body is read: its sessions never count toward use.
async fn serve(State(db): State<PathBuf>, request: Request) -> Response {
    if request
        .extensions()
        .get::<super::gate::Access>()
        .is_some_and(|access| access.scope == crate::registry::Scope::Read)
    {
        let url = super::scope::client_url(request.headers());
        return (StatusCode::FORBIDDEN, super::scope::read_only(&url)).into_response();
    }
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|length| length.to_str().ok()?.parse::<u64>().ok());
    if declared.is_some_and(|length| length > USE_REQUEST_BYTES as u64) {
        return (StatusCode::PAYLOAD_TOO_LARGE, TOO_LARGE).into_response();
    }
    let Ok(body) = to_bytes(request.into_body(), USE_REQUEST_BYTES).await else {
        return (StatusCode::PAYLOAD_TOO_LARGE, TOO_LARGE).into_response();
    };
    let uses: Uses = match serde_json::from_slice(&body) {
        Ok(uses) => uses,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("Malformed use request: {err}"),
            )
                .into_response();
        }
    };
    match tokio::task::spawn_blocking(move || memory::write_uses(&db, &uses)).await {
        Ok(Ok(Written::Done(reply))) => Json(reply).into_response(),
        Ok(Ok(Written::Refused(reason))) => (StatusCode::BAD_REQUEST, reason).into_response(),
        Ok(Err(err)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("The memory server can't record use now: {err:#}"),
        )
            .into_response(),
        Err(err) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("The memory server can't record use now: {err}"),
        )
            .into_response(),
    }
}

/// What became of a session's use sent to a memory server.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Sent {
    /// Every request was written.
    Written,
    /// The server doesn't take use from this machine: a read-only token
    /// (403), a server released before the route (404), or a request it
    /// refused (400). Use is best effort, so the rows are dropped, and the
    /// session doesn't ask that server again.
    Dropped,
    /// The store can't be written now (503), or the server can't be
    /// reached: the rows are kept and sent at the next prompt or session
    /// start.
    Kept,
}

impl Connection {
    /// Sends one session's use, in requests of at most
    /// [`USES_PER_REQUEST`] rows. Rows a request wrote before a later one
    /// fails are written again with the rest, which only adds to them.
    pub(crate) async fn send_uses(&self, origin: &str, uses: &[Use]) -> Sent {
        for chunk in uses.chunks(USES_PER_REQUEST) {
            let request = Uses {
                origin: origin.to_owned(),
                uses: chunk.to_vec(),
            };
            let Ok(body) = serde_json::to_vec(&request) else {
                return Sent::Dropped;
            };
            let response = match self.send(ROUTE, "application/json", body).await {
                Ok(response) => response,
                Err(_) => return Sent::Kept,
            };
            match response.status() {
                status if status.is_success() => {}
                reqwest::StatusCode::FORBIDDEN
                | reqwest::StatusCode::NOT_FOUND
                | reqwest::StatusCode::BAD_REQUEST
                | reqwest::StatusCode::PAYLOAD_TOO_LARGE => return Sent::Dropped,
                _ => return Sent::Kept,
            }
        }
        Sent::Written
    }
}
