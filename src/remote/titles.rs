//! Titles for a push on a memory server's store (#292). The delivery files
//! hold no title, so a push there named the id alone. Now the running
//! connector asks the server for each scope's titles (`POST /titles`), keeps
//! them in memory only, current to the position its polls found, and one
//! connector on the machine answers the hooks over a loopback port that
//! only this user's key opens ([`channel`], [`owner`]). The hook never asks
//! the server, and nothing of a title is written on this machine.

pub(crate) mod channel;
pub(super) mod owner;

use super::Connection;
use crate::memory::{delivery_files, served_titles};
use anyhow::{Context, Result};
use axum::{
    Json, Router,
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The route's name, beside `/delivery`.
const ROUTE: &str = "titles";

/// Makes a debug build's server leave the route out, as a server released
/// before it answers: 404.
#[cfg(debug_assertions)]
const NO_ROUTE: &str = "USER_MEMORY_TEST_NO_TITLES_ROUTE";

/// What a connector asks: a scope's projects and, when it holds the scope's
/// titles at a position, that position, for the changes since.
#[derive(Deserialize, Serialize)]
pub(crate) struct Asked {
    pub(crate) projects: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) since: Option<String>,
}

/// The titles route over the store `db`, behind the host checks of the
/// other transfers.
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

/// Answers a scope's titles. A read-only token may ask: the answer holds
/// only what `index` and `read` give it, and only of the scope asked.
async fn serve(
    axum::extract::State(db): axum::extract::State<PathBuf>,
    Json(asked): Json<Asked>,
) -> Response {
    let scope = delivery_files::scope(&asked.projects);
    let answer = tokio::task::spawn_blocking(move || {
        served_titles::answer(&db, &scope, asked.since.as_deref())
    })
    .await;
    match answer {
        Ok(Ok(answer)) => Json(answer).into_response(),
        Ok(Err(_)) | Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The memory server's store can't be read for titles now",
        )
            .into_response(),
    }
}

/// What a server answered a connector's ask.
pub(crate) enum Fetched {
    /// The scope's titles.
    Titles(served_titles::Answer),
    /// The server has no titles route: one released before it (404), or a
    /// proxy in front of it that lets only the routes it knows through.
    Unsupported,
}

impl Connection {
    /// Whether the server lets this machine reach its titles route: anything
    /// but 404. It asks with no scope, which the route refuses as
    /// unreadable, so the check costs no titles.
    pub(crate) async fn reaches_titles(&self) -> Result<bool> {
        let response = self.send(ROUTE, "application/json", b"{}".to_vec()).await?;
        Ok(response.status() != reqwest::StatusCode::NOT_FOUND)
    }

    /// Asks the server for the titles of `projects`: the changes since the
    /// position `since`, or whole.
    pub(crate) async fn titles(&self, projects: &[String], since: Option<&str>) -> Result<Fetched> {
        let asked = Asked {
            projects: projects.to_vec(),
            since: since.map(str::to_owned),
        };
        let response = self
            .send(ROUTE, "application/json", serde_json::to_vec(&asked)?)
            .await?;
        match response.status() {
            status if status.is_success() => {
                Ok(Fetched::Titles(response.json().await.context(
                    "The titles from the memory server could not be read",
                )?))
            }
            reqwest::StatusCode::NOT_FOUND => Ok(Fetched::Unsupported),
            status => anyhow::bail!("The memory server answered {status}"),
        }
    }
}
