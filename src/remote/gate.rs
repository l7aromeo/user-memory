//! Who may use the memory server: each request's bearer token, checked
//! against the shared token file and the client tokens in the registry.
//! Revocation is live: the registry is read again only when another
//! process has committed to it, which `PRAGMA data_version` tells.

use crate::registry::{self, ClientToken, SecondsI64};
use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use rusqlite::Connection;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
};
use subtle::ConstantTimeEq;

/// What a request's token lets it do, which the routes behind the gate
/// read from the request's extensions.
#[derive(Clone, Copy, Debug)]
pub struct Access {
    /// The client token's SHA-256; `None` for the shared token file.
    pub hash: Option<[u8; 32]>,
}

/// Why a token is refused, as the 401's `error_description` says it, so the
/// client can name the fix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Not a token of this server.
    Unknown,
    Revoked,
    /// Unused for [`registry::INACTIVITY_EXPIRY`].
    Unused,
    /// Past the date set with `--expires`.
    Expired,
}

#[derive(Clone)]
pub struct Gate(Arc<Inner>);

struct Inner {
    shared: String,
    path: PathBuf,
    state: Mutex<Loaded>,
}

/// The registry as last read, with the connection that read it.
#[derive(Default)]
struct Loaded {
    conn: Option<Connection>,
    data_version: i64,
    tokens: HashMap<String, ClientToken>,
    shared_retired: bool,
    /// The last refusal printed for a damaged registry, so the log gets each
    /// once.
    damaged: Option<String>,
}

impl Gate {
    /// The gate for the shared token `shared` and the registry at `path`.
    pub fn new(shared: String, path: PathBuf) -> Self {
        Self(Arc::new(Inner {
            shared,
            path,
            state: Mutex::new(Loaded::default()),
        }))
    }

    /// Checks `provided` now: the access it gives, or why it is refused, or
    /// the registry's own refusal when it cannot be read.
    fn check(&self, provided: &str) -> Result<Result<Access, Refusal>, String> {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(refusal) = state.refresh(&self.0.path) {
            if state.damaged.as_ref() != Some(&refusal) {
                eprintln!("{refusal}");
                state.damaged = Some(refusal.clone());
            }
            return Err(refusal);
        }
        state.damaged = None;
        if bool::from(provided.as_bytes().ct_eq(self.0.shared.as_bytes())) {
            return Ok(if state.shared_retired {
                Err(Refusal::Revoked)
            } else {
                Ok(Access { hash: None })
            });
        }
        let hash = registry::hash(provided);
        let now = registry::now();
        let Some(token) = state.tokens.get(&hash).cloned() else {
            return Ok(Err(Refusal::Unknown));
        };
        if token.revoked_at.is_some() {
            return Ok(Err(Refusal::Revoked));
        }
        match token.expired(now) {
            Some(registry::Expired::Unused) => return Ok(Err(Refusal::Unused)),
            Some(registry::Expired::OnItsDate) => return Ok(Err(Refusal::Expired)),
            None => {}
        }
        if token
            .last_used_at
            .is_none_or(|last| now - last >= registry::LAST_USE_RESOLUTION.as_secs_i64())
        {
            state.used(&hash, now);
        }
        Ok(Ok(Access {
            hash: Some(digest(&hash)),
        }))
    }

    /// Revokes the client token `access` was given for: what
    /// `DELETE /auth/token` does.
    fn revoke(&self, access: Access) -> Result<bool, String> {
        let Some(hash) = access.hash else {
            return Ok(false);
        };
        let hash: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.refresh(&self.0.path)?;
        let Some(conn) = &state.conn else {
            return Ok(false);
        };
        let revoked = registry::revoke_hash(conn, &hash).map_err(|err| format!("{err:#}"))?;
        if let Some(token) = state.tokens.get_mut(&hash) {
            token.revoked_at = Some(registry::now());
        }
        Ok(revoked)
    }
}

/// A SHA-256 in hex as its 32 bytes, so [`Access`] can be `Copy`.
fn digest(hex: &str) -> [u8; 32] {
    let mut bytes = [0; 32];
    for (at, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[at * 2..at * 2 + 2], 16).unwrap_or(0);
    }
    bytes
}

impl Loaded {
    /// Reads the registry again when it has changed: opened when it first
    /// appears, reloaded when another connection has committed since.
    fn refresh(&mut self, path: &std::path::Path) -> Result<(), String> {
        if self.conn.is_none() {
            // One stat per request until the first `host token add`.
            match registry::open(path) {
                Ok(Some(conn)) => self.conn = Some(conn),
                Ok(None) => return Ok(()),
                Err(err) => return Err(format!("{err:#}")),
            }
            self.data_version = -1;
        }
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        let damaged = |err: rusqlite::Error| registry::unreadable(path, &err);
        let version: i64 = conn
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .map_err(damaged)?;
        if version == self.data_version {
            return Ok(());
        }
        let loaded = registry::still_current(conn, path).and_then(|()| {
            Ok((
                registry::tokens(conn)?,
                registry::shared_file_retired(conn)?,
            ))
        });
        match loaded {
            Ok((tokens, retired)) => {
                self.tokens = tokens
                    .into_iter()
                    .map(|token| (token.hash.clone(), token))
                    .collect();
                self.shared_retired = retired;
                self.data_version = version;
                #[cfg(debug_assertions)]
                if std::env::var_os(crate::handover::TEST_TRACE).is_some() {
                    eprintln!("registry loaded: {} tokens", self.tokens.len());
                }
                Ok(())
            }
            Err(err) => {
                self.conn = None;
                Err(format!("{err:#}"))
            }
        }
    }

    /// Writes the token's last use, on the connection the gate reads with:
    /// SQLite moves `data_version` only for other connections' commits, so
    /// this causes no reload. A failed write is left for the next request.
    fn used(&mut self, hash: &str, now: i64) {
        if let Some(conn) = &self.conn
            && registry::used(conn, hash, now).is_ok()
            && let Some(token) = self.tokens.get_mut(hash)
        {
            token.last_used_at = Some(now);
        }
    }
}

/// The `WWW-Authenticate` value for `refusal`: RFC 6750's `invalid_token`
/// with a reason the client turns into a message naming the fix.
fn challenge(refusal: Refusal) -> &'static str {
    match refusal {
        Refusal::Unknown => "Bearer",
        Refusal::Revoked => r#"Bearer error="invalid_token", error_description="revoked""#,
        Refusal::Unused => r#"Bearer error="invalid_token", error_description="unused""#,
        Refusal::Expired => r#"Bearer error="invalid_token", error_description="expired""#,
    }
}

/// Lets a request through with its [`Access`], or answers 401 saying why,
/// or 503 with the registry's refusal when it cannot be read.
pub async fn authenticate(State(gate): State<Gate>, mut request: Request, next: Next) -> Response {
    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_owned();
    match gate.check(&provided) {
        Ok(Ok(access)) => {
            request.extensions_mut().insert(access);
            next.run(request).await
        }
        Ok(Err(refusal)) => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, challenge(refusal))],
            "Authentication required",
        )
            .into_response(),
        Err(damaged) => (StatusCode::SERVICE_UNAVAILABLE, damaged).into_response(),
    }
}

/// `DELETE /auth/token`: the calling client token revokes itself, as a
/// machine does when it leaves this server.
pub async fn revoke_own(
    State(gate): State<Gate>,
    axum::Extension(access): axum::Extension<Access>,
) -> Response {
    if access.hash.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "The shared token file is not revoked this way; retire it with `user-memory host token retire-shared` on the server.",
        )
            .into_response();
    }
    match gate.revoke(access) {
        Ok(_) => (StatusCode::OK, "This token is revoked.").into_response(),
        Err(damaged) => (StatusCode::SERVICE_UNAVAILABLE, damaged).into_response(),
    }
}
