//! Export and import between a client and its memory server: two HTTP
//! endpoints beside `/mcp`, behind the same access token and the same host
//! checks, run by the same transfer code as a local store. The export
//! travels to the client as the file itself, and the import as one; the MCP
//! tools never carry an export's contents, so agents do not see them.

use super::{Connection, read_token};
use crate::memory::{self, ExportFile, ImportRequest, MAX_BYTES, Subset};
use anyhow::{Context, Result, anyhow};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header, uri::Authority},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::Value;
use std::{
    fmt::Write as _,
    io::{self, Write as _},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// The response header carrying an export's summary, as JSON in ASCII.
const SUMMARY: &str = "user-memory-summary";
/// Largest import request: its options line (which may hold a titles map)
/// and the document, each at most the import limit, and the newline
/// between them.
const IMPORT_BYTES: usize = 2 * MAX_BYTES as usize + 1;
/// How long the client waits to reach the server, as the MCP connector does.
const CONNECT: Duration = Duration::from_secs(5);
/// What a request over the size limit is told: a local import's message.
const TOO_LARGE: &str = "An export file is limited to 64 MiB";

/// The hosts and browser origins the server answers, as given to `/mcp`.
#[derive(Clone)]
pub(super) struct Allowed {
    hosts: Arc<Vec<String>>,
    origins: Arc<Vec<String>>,
}

impl Allowed {
    pub(super) fn new(hosts: &[String], origins: &[String]) -> Self {
        Self {
            hosts: Arc::new(hosts.to_vec()),
            origins: Arc::new(origins.to_vec()),
        }
    }

    /// Whether the request's Host, and its Origin when it has one, are among
    /// those allowed: an allowed entry without a port allows any port, as
    /// for `/mcp`. This keeps a page another site serves from reaching the
    /// store through the user's browser (DNS rebinding).
    fn admits(&self, uri: &Uri, headers: &HeaderMap) -> bool {
        let host = headers
            .get(header::HOST)
            .and_then(|host| host.to_str().ok())
            .and_then(|host| host.parse::<Authority>().ok())
            .or_else(|| uri.authority().cloned());
        let Some(host) = host else {
            return false;
        };
        if !self.hosts.iter().any(|allowed| matches(allowed, &host)) {
            return false;
        }
        let Some(origin) = headers.get(header::ORIGIN) else {
            return true;
        };
        let origin = origin.to_str().ok().and_then(|o| o.parse::<Uri>().ok());
        origin.is_some_and(|origin| {
            self.origins.iter().any(|allowed| {
                let allowed = allowed.parse::<Uri>().ok();
                allowed.is_some_and(|allowed| {
                    allowed.scheme_str() == origin.scheme_str()
                        && match (allowed.authority(), origin.authority()) {
                            (Some(a), Some(o)) => matches(a.as_str(), o),
                            _ => false,
                        }
                })
            })
        })
    }
}

/// Whether `authority` is the allowed host `allowed`, ignoring case and
/// IPv6 brackets; `allowed` without a port allows any port.
fn matches(allowed: &str, authority: &Authority) -> bool {
    let name = |host: &str| host.trim_matches(['[', ']']).to_ascii_lowercase();
    match allowed.parse::<Authority>() {
        Ok(allowed) => {
            name(allowed.host()) == name(authority.host())
                && allowed
                    .port_u16()
                    .is_none_or(|port| authority.port_u16() == Some(port))
        }
        Err(_) => name(allowed) == name(authority.host()),
    }
}

async fn admit(State(allowed): State<Allowed>, request: Request, next: Next) -> Response {
    if !allowed.admits(request.uri(), request.headers()) {
        return (
            StatusCode::FORBIDDEN,
            "Forbidden: Host header is not allowed",
        )
            .into_response();
    }
    next.run(request).await
}

/// The export and import endpoints over the store `db`.
pub(super) fn routes(db: PathBuf, allowed: Allowed) -> Router {
    Router::new()
        .route("/export", post(export))
        .route("/import", post(import))
        .with_state(db)
        .layer(middleware::from_fn_with_state(allowed, admit))
}

/// The refusal for `err`, its whole message as the client prints it.
fn refused(err: &anyhow::Error) -> Response {
    (StatusCode::BAD_REQUEST, format!("{err:#}")).into_response()
}

/// Exports the memories the posted filters select: the document as the
/// body, its summary in a header.
async fn export(State(db): State<PathBuf>, Json(subset): Json<Subset>) -> Response {
    let made = tokio::task::spawn_blocking(move || memory::export_document(&db, &subset)).await;
    let (document, summary) = match made {
        Ok(Ok(made)) => made,
        Ok(Err(err)) => return refused(&err),
        Err(err) => return refused(&anyhow!("Export failed: {err}")),
    };
    let Ok(summary) = HeaderValue::from_str(&ascii(&summary)) else {
        return refused(&anyhow!("Export failed: the summary is not a header"));
    };
    let mut response = Response::new(Body::from(document));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(SUMMARY, summary);
    response
}

/// Imports the posted document: one line of [`ImportRequest`] JSON, a
/// newline, then the document. Answers with the import's summary.
async fn import(State(db): State<PathBuf>, request: Request) -> Response {
    // A declared length over the limit is refused before any of the body is
    // read; a body without one stops being read at the limit.
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|length| length.to_str().ok()?.parse::<u64>().ok());
    if declared.is_some_and(|length| length > IMPORT_BYTES as u64) {
        return (StatusCode::PAYLOAD_TOO_LARGE, TOO_LARGE).into_response();
    }
    let Ok(body) = to_bytes(request.into_body(), IMPORT_BYTES).await else {
        return (StatusCode::PAYLOAD_TOO_LARGE, TOO_LARGE).into_response();
    };
    let Some(split) = body.iter().position(|&byte| byte == b'\n') else {
        return refused(&anyhow!("Malformed import request: no options line"));
    };
    let options: ImportRequest = match serde_json::from_slice(&body[..split]) {
        Ok(options) => options,
        Err(err) => return refused(&anyhow!("Malformed import options: {err}")),
    };
    let Ok(document) = String::from_utf8(body[split + 1..].to_vec()) else {
        return refused(&anyhow!("An export file must be UTF-8 JSON"));
    };
    let imported = tokio::task::spawn_blocking(move || {
        memory::import_document(
            &db,
            &document,
            options.apply,
            options.on_conflict,
            options.titles.as_ref(),
            &options.subset,
        )
    })
    .await;
    match imported {
        Ok(Ok(summary)) => Json(summary).into_response(),
        Ok(Err(err)) => refused(&err),
        Err(err) => refused(&anyhow!("Import failed: {err}")),
    }
}

/// `value` as JSON whose non-ASCII characters are `\u` escapes, so it fits
/// an HTTP header.
fn ascii(value: &Value) -> String {
    let mut out = String::new();
    for c in value.to_string().chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                let _ = write!(out, "\\u{unit:04x}");
            }
        }
    }
    out
}

impl Connection {
    /// Posts `body` to the server's endpoint `name`, beside its MCP
    /// endpoint; a refusal becomes an error with the server's message.
    async fn post(
        &self,
        name: &str,
        content: &'static str,
        body: Vec<u8>,
    ) -> Result<reqwest::Response> {
        self.validate()?;
        let url = reqwest::Url::parse(&self.url)?.join(name)?;
        let response = reqwest::Client::builder()
            .connect_timeout(CONNECT)
            .redirect(reqwest::redirect::Policy::none())
            .build()?
            .post(url)
            .bearer_auth(read_token(&self.token_file)?)
            .header(reqwest::header::CONTENT_TYPE, content)
            .body(body)
            .send()
            .await
            .context("Cannot connect to memory server")?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let message = response.text().await.unwrap_or_default();
        Err(match status {
            reqwest::StatusCode::UNAUTHORIZED => {
                anyhow!("The memory server refused the access token")
            }
            reqwest::StatusCode::NOT_FOUND => anyhow!(
                "The memory server does not take {name} from clients; upgrade it to this version"
            ),
            _ => anyhow!("{}", message.trim()),
        })
    }

    /// Exports what `subset` selects from the server to `path` (a private
    /// file, refused if it exists unless `force`) or stdout for `-`,
    /// streaming it; returns the export's summary.
    pub async fn export(&self, subset: &Subset, path: &Path, force: bool) -> Result<Value> {
        let mut file = match path.as_os_str() == "-" {
            true => None,
            false => Some(ExportFile::create(path, force)?),
        };
        let mut response = self
            .post("export", "application/json", serde_json::to_vec(subset)?)
            .await?;
        let summary = response
            .headers()
            .get(SUMMARY)
            .and_then(|summary| summary.to_str().ok())
            .and_then(|summary| serde_json::from_str(summary).ok())
            .context("The memory server sent no export summary")?;
        let mut stdout = io::stdout().lock();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("The export from the memory server broke off")?
        {
            match &mut file {
                Some(file) => file.write(&chunk)?,
                None => stdout.write_all(&chunk)?,
            }
        }
        match file {
            Some(file) => file.finish()?,
            None => stdout.flush()?,
        }
        Ok(summary)
    }

    /// Imports `document` into the server with `request`'s options;
    /// returns the import's summary.
    pub async fn import(&self, request: &ImportRequest, document: &str) -> Result<Value> {
        let mut body = serde_json::to_vec(request)?;
        body.push(b'\n');
        body.extend_from_slice(document.as_bytes());
        let response = self
            .post("import", "application/octet-stream", body)
            .await?;
        response
            .json()
            .await
            .context("The memory server sent an unreadable import summary")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_and_origins_are_matched_as_for_mcp() {
        let allowed = Allowed::new(
            &["localhost".into(), "127.0.0.1:8765".into(), "::1".into()],
            &["http://localhost".into(), "https://127.0.0.1:8765".into()],
        );
        let admits = |host: &str, origin: Option<&str>| {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, HeaderValue::from_str(host).unwrap());
            if let Some(origin) = origin {
                headers.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
            }
            allowed.admits(&Uri::from_static("/export"), &headers)
        };
        assert!(admits("localhost:9000", None), "no port allows any");
        assert!(admits("LOCALHOST", None));
        assert!(admits("127.0.0.1:8765", None));
        assert!(!admits("127.0.0.1:8766", None), "a port must match");
        assert!(admits("[::1]:8765", None), "a bare IPv6 entry");
        assert!(!admits("evil.example", None));
        assert!(admits("localhost", Some("http://localhost:3000")));
        assert!(admits("localhost", Some("https://127.0.0.1:8765")));
        assert!(!admits("localhost", Some("https://localhost")), "scheme");
        assert!(!admits("localhost", Some("http://evil.example")));
        assert!(!admits("localhost", Some("not an origin")));
    }

    #[test]
    fn a_summary_header_is_ascii_json() {
        let summary = serde_json::json!({"selection": {"category": "programación 🦀"}});
        let header = ascii(&summary);
        assert!(header.is_ascii());
        assert_eq!(serde_json::from_str::<Value>(&header).unwrap(), summary);
    }
}
