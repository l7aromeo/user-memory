//! Authenticated HTTP host and protocol connector. Memory stays on the host.

use crate::private;
use anyhow::{Context, Result, bail};
use axum::{Router, http::StatusCode, middleware, routing::get};
use clap::{Args, Subcommand};
use rmcp::{
    RoleClient, RoleServer, ServiceExt,
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::{
        StreamableHttpClientTransport, Transport, stdio,
        streamable_http_client::{AuthRequiredError, StreamableHttpClientTransportConfig},
        streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService},
    },
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

pub(crate) mod delivery;
mod gate;
pub(crate) mod marker;
pub(crate) mod refused;
pub(crate) mod scope;
mod sessions;
mod tokens;
mod transfer;
pub(crate) mod usage;

#[derive(Clone, Args, Serialize, Deserialize)]
pub struct Connection {
    /// Remote MCP endpoint. HTTPS required except for loopback development.
    #[arg(long)]
    pub url: String,
    /// File containing the server access token; never stored in client config.
    #[arg(long)]
    pub token_file: PathBuf,
}

/// `host`: serve, with its options, or a `host token` command. Without a
/// subcommand it is the server as it always was.
#[derive(Args)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
pub struct HostArgs {
    #[command(subcommand)]
    command: Option<HostCommand>,
    #[command(flatten)]
    options: HostOptions,
}

#[derive(Subcommand)]
pub enum HostCommand {
    /// Issue, list and revoke this server's client tokens: one per client machine, revocable alone and optionally read-only.
    #[command(subcommand)]
    Token(tokens::TokenCommand),
}

/// Runs `host`: the server, or a `host token` command.
pub async fn run_host(args: HostArgs) -> Result<()> {
    match args.command {
        Some(HostCommand::Token(command)) => tokens::run(command),
        None => host(args.options).await,
    }
}

#[derive(Args)]
pub struct HostOptions {
    /// HTTP bind address; use a TLS reverse proxy for off-machine access.
    #[arg(long, default_value = "127.0.0.1:8765")]
    listen: SocketAddr,
    /// SQLite path on this server; defaults to its configured memory location.
    #[arg(long)]
    db: Option<PathBuf>,
    /// Server token file. A private random token is created if absent.
    // Required to serve; an Option so `host token …`, which negates it, can
    // be parsed without one.
    #[arg(long, required = true)]
    token_file: Option<PathBuf>,
    /// Additional hostnames accepted through a reverse proxy.
    #[arg(long)]
    allowed_host: Vec<String>,
}

impl Connection {
    pub fn validate(&self) -> Result<()> {
        let url = reqwest::Url::parse(&self.url).context("Invalid MCP endpoint URL")?;
        let local = url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
                || host == "[::1]"
        });
        if url.scheme() != "https" && !(url.scheme() == "http" && local) {
            bail!(
                "Remote MCP endpoints require HTTPS; HTTP is allowed only on loopback for development"
            );
        }
        if !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.query().is_some()
        {
            bail!(
                "Put credentials in --token-file; endpoint URLs cannot contain credentials, query strings, or fragments"
            );
        }
        if !self.token_file.is_absolute() {
            bail!("Token file must use an absolute path");
        }
        read_token(&self.token_file)?;
        Ok(())
    }
    fn transport(&self) -> Result<StreamableHttpClientTransport<reqwest::Client>> {
        self.validate()?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        // A session the server no longer knows (it restarted, or ended the
        // session as idle) is answered 404: the transport then makes a new
        // session and sends the refused request once more, which keeps the
        // marker poller's kept session working (marker::poll). The default,
        // set here so the poller does not depend on it.
        let config = StreamableHttpClientTransportConfig::with_uri(self.url.clone())
            .auth_header(read_token(&self.token_file)?)
            .reinit_on_expired_session(true);
        Ok(StreamableHttpClientTransport::with_client(client, config))
    }
    pub async fn probe(&self) -> Result<()> {
        let service = tokio::time::timeout(Duration::from_secs(10), ().serve(self.transport()?))
            .await
            .context("Memory server connection timed out")?
            .map_err(|err| {
                refused::told(
                    err,
                    &self.url,
                    Some("Cannot connect to memory server; check endpoint and access token"),
                )
            })?;
        let result = tokio::time::timeout(Duration::from_secs(10), service.list_all_tools()).await;
        let _ = service.cancel().await;
        let tools = result.context("Memory server tool check timed out")??;
        let schemas: Vec<(String, serde_json::Value)> = tools
            .iter()
            .map(|tool| {
                (
                    tool.name.to_string(),
                    serde_json::Value::Object((*tool.input_schema).clone()),
                )
            })
            .collect();
        check_tools(&schemas)
    }

    /// What the server says in its handshake: its `initialize` result. A
    /// refused token's reason stays in the error's chain for
    /// [`refused::Reason::of`].
    pub async fn handshake(&self) -> Result<serde_json::Value> {
        let service = tokio::time::timeout(Duration::from_secs(10), ().serve(self.transport()?))
            .await
            .context("Memory server connection timed out")?
            .context("Cannot connect to memory server")?;
        let info = service.peer_info().map(serde_json::to_value);
        let _ = service.cancel().await;
        Ok(info.context("Memory server sent no handshake")??)
    }

    /// Whether the server says this machine's token is read-only, from
    /// `GET /auth/token` beside the MCP endpoint. Any other answer, as from
    /// a server too old to have the route, counts as a token that writes.
    pub async fn read_only(&self) -> bool {
        let asked = async {
            self.validate().ok()?;
            let url = reqwest::Url::parse(&self.url)
                .ok()?
                .join("auth/token")
                .ok()?;
            let response = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .ok()?
                .get(url)
                .bearer_auth(read_token(&self.token_file).ok()?)
                .send()
                .await
                .ok()?;
            if response.status() != reqwest::StatusCode::OK {
                return None;
            }
            let answer: serde_json::Value = response.json().await.ok()?;
            Some(answer["scope"] == "read")
        };
        asked.await.unwrap_or(false)
    }

    /// Calls the tool `name` with `arguments` in a session of its own,
    /// opened for the call and ended after it.
    pub async fn call(&self, name: &'static str, arguments: serde_json::Value) -> Result<String> {
        let session = self.open().await?;
        let result = self.call_in(&session, name, arguments).await;
        let _ = session.cancel().await;
        result
    }

    /// Opens an MCP session with the server: its handshake, which names the
    /// server's version for [`Connection::call_in`].
    pub(crate) async fn open(&self) -> Result<Session> {
        tokio::time::timeout(Duration::from_secs(10), ().serve(self.transport()?))
            .await
            .context("Memory server connection timed out")?
            .map_err(|err| refused::told(err, &self.url, Some("Cannot connect to memory server")))
    }

    /// Calls the tool `name` with `arguments` in `session`, which stays
    /// open. An input the server is too old for is refused, not sent: it
    /// would be ignored and the reply would look complete. The version is
    /// the one the session's first handshake named. A session the transport
    /// made again after the server dropped it keeps that version, which is
    /// stale when the server was upgraded or rolled back meanwhile; only
    /// the marker poll keeps a session, and no server version refuses its
    /// input, so the stale version decides nothing there.
    pub(crate) async fn call_in(
        &self,
        session: &Session,
        name: &'static str,
        arguments: serde_json::Value,
    ) -> Result<String> {
        let handshake = session.peer_info().map(serde_json::to_value).transpose()?;
        if let Some(server) = handshake
            .as_ref()
            .and_then(crate::setup::versions::server_version)
            .and_then(serde_json::Value::as_str)
            && let Some((refusal, _)) =
                crate::setup::versions::refused_option(&self.url, server, name, &arguments)
        {
            bail!("{refusal}");
        }
        let request = rmcp::model::CallToolRequestParams::new(name)
            .with_arguments(arguments.as_object().cloned().unwrap_or_default());
        let result =
            tokio::time::timeout(Duration::from_secs(10), session.call_tool(request)).await;
        let result = serde_json::to_value(
            result
                .context("Memory server request timed out")?
                .map_err(|err| refused::told(err, &self.url, None))?,
        )?;
        let content = result["content"]
            .as_array()
            .context("Memory server returned no content")?;
        let text = content
            .iter()
            .filter_map(|c| c["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if result["isError"] == true {
            bail!("{}", failure(name, &text));
        }
        Ok(text)
    }
}

/// An MCP session with a memory server, as this machine's client.
pub(crate) type Session = rmcp::service::RunningService<RoleClient, ()>;

/// Why the server's `name` call failed, from the text it sent with
/// `isError`: the `error` of a JSON reply, else the text itself without
/// index's own "Memory unavailable" prefix, so the CLI and the hooks say what
/// the server said.
fn failure(name: &str, text: &str) -> String {
    let said = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|reply| reply["error"].as_str().map(str::to_owned))
        .unwrap_or_else(|| {
            let text = text.trim();
            // index says so itself; the caller adds its own word for it.
            text.strip_prefix(crate::memory::INDEX_UNAVAILABLE)
                .unwrap_or(text)
                .to_owned()
        });
    if said.is_empty() {
        format!("Memory server could not complete {name}")
    } else {
        said
    }
}

/// Checks that an endpoint's tools are this memory's: the tool set, UUID
/// memory IDs, forgetting by origin, and similar-memory resolution. An older
/// server would otherwise silently ignore fields and store duplicates.
fn check_tools(tools: &[(String, serde_json::Value)]) -> Result<()> {
    for required in ["record", "search", "index", "read", "update", "forget"] {
        if !tools.iter().any(|(name, _)| name == required) {
            bail!("Endpoint does not provide the user-memory tool set");
        }
    }
    let property = |tool: &str, field: &str| {
        tools
            .iter()
            .find(|(name, _)| name == tool)
            .and_then(|(_, schema)| schema.get("properties")?.get(field).cloned())
    };
    // A UUID string, optionally nullable when the field may be omitted.
    let uuid = |schema: Option<serde_json::Value>| {
        schema.is_some_and(|id| {
            let string = match id.get("type") {
                Some(serde_json::Value::String(kind)) => kind == "string",
                Some(serde_json::Value::Array(kinds)) => {
                    kinds.iter().any(|kind| kind == "string")
                        && kinds.iter().all(|kind| kind == "string" || kind == "null")
                }
                _ => false,
            };
            string && id.get("format").and_then(|v| v.as_str()) == Some("uuid")
        })
    };
    if !uuid(property("update", "memory_id")) || !uuid(property("forget", "memory_id")) {
        bail!("Endpoint must expose UUID memory IDs");
    }
    if property("forget", "origin").is_none() {
        bail!("Endpoint cannot forget by conversation origin; update the memory server");
    }
    let uuid_list = |tool: &str, field: &str| {
        property(tool, field)
            .is_some_and(|list| list["type"] == "array" && uuid(Some(list["items"].clone())))
    };
    // An older server would read update's distinct_from as a plain
    // confirmation and rewrite the memory as explicit.
    if !uuid(property("record", "confirms"))
        || !uuid_list("record", "supersedes")
        || !uuid_list("record", "distinct_from")
        || !uuid_list("update", "distinct_from")
    {
        bail!("Endpoint cannot resolve similar memories; update the memory server");
    }
    Ok(())
}

fn read_token(path: &Path) -> Result<String> {
    let token = fs::read_to_string(path).context("Cannot read access-token file")?;
    let token = token.trim();
    if token.len() < 32
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))
    {
        bail!("Access-token file must contain a token of at least 32 ASCII characters");
    }
    Ok(token.to_owned())
}

/// A new value for a shared token file: 32 random bytes in hex, which
/// every released server and client reads as a token.
fn new_shared_token() -> String {
    let bytes: [u8; 32] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn host_token(path: &Path) -> Result<String> {
    if !path.is_absolute() {
        bail!("Token file must use an absolute path");
    }
    if path.exists() {
        return read_token(path);
    }
    fs::create_dir_all(path.parent().context("Invalid token-file path")?)?;
    let token = new_shared_token();
    let mut file = private::create_file(fs::OpenOptions::new().write(true).create_new(true), path)
        .context("Cannot create server access-token file")?;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    Ok(token)
}

/// What `/` answers: the service's name while the store opens, read-only
/// and at this binary's schema version, else 503, so the Docker
/// healthcheck needs no token; it never creates or migrates a store. A
/// store that opened within [`HEALTH_FRESH`] is not opened again, so
/// callers of this unauthenticated
/// path cannot make a healthy server open its store at will; a failure is
/// checked again on the next request, so a repaired store answers at once.
#[derive(Clone)]
struct Health {
    db: PathBuf,
    last: Arc<Mutex<Option<Checked>>>,
}

/// When the store was last checked, and whether it opened.
type Checked = (Instant, Result<(), String>);

/// How long a store check answers `/`: far below the healthcheck's
/// 30-second interval, and long enough that a flood opens the store a few
/// times a minute at most.
const HEALTH_FRESH: Duration = Duration::from_secs(5);

impl Health {
    fn new(db: PathBuf) -> Self {
        Self {
            db,
            last: Arc::default(),
        }
    }

    async fn answer(self) -> axum::response::Response {
        use axum::response::IntoResponse;
        let cached = {
            let last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
            last.as_ref()
                .filter(|(at, found)| found.is_ok() && at.elapsed() < HEALTH_FRESH)
                .map(|(_, found)| found.clone())
        };
        let found = match cached {
            Some(found) => found,
            None => {
                let db = self.db.clone();
                let found = tokio::task::spawn_blocking(move || {
                    crate::memory::servable(&db).map_err(|err| format!("{err:#}"))
                })
                .await
                .unwrap_or_else(|err| Err(err.to_string()));
                let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
                // The server's log gets each new failure once.
                if let Err(err) = &found
                    && last.as_ref().map(|(_, was)| was) != Some(&found)
                {
                    eprintln!("The memory store cannot be opened: {err}");
                }
                *last = Some((Instant::now(), found.clone()));
                found
            }
        };
        match found {
            Ok(()) => "user-memory MCP server. Endpoint: /mcp\n".into_response(),
            // The reason stays in the log: this path needs no token.
            Err(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "The memory store cannot be opened; the server's log says why.\n",
            )
                .into_response(),
        }
    }
}

/// Resolves when the process is asked to stop: Ctrl-C (SIGINT, which the
/// Docker server's compose file sends) everywhere, and SIGTERM on Unix.
pub async fn stopped() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

pub async fn host(options: HostOptions) -> Result<()> {
    let db = options.db.unwrap_or_else(crate::db_path);
    if !db.is_absolute() {
        bail!("Server database must use an absolute path");
    }
    let backups = crate::memory::backups::watch(&db);
    let held = crate::memory::hold(&db);
    let token_file = options
        .token_file
        .context("the following required argument was not provided: --token-file")?;
    let token = host_token(&token_file)?;
    crate::connect(&db)?;
    if let Some(folder) = db.parent() {
        crate::server::tokens::sweep(folder);
    }
    let listener = tokio::net::TcpListener::bind(options.listen).await?;
    let address = listener.local_addr()?;
    let mut allowed_hosts = vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "[::1]".to_owned(),
        address.to_string(),
    ];
    allowed_hosts.extend(options.allowed_host);
    let origins: Vec<_> = allowed_hosts
        .iter()
        .flat_map(|h| [format!("http://{h}"), format!("https://{h}")])
        .collect();
    let allowed = transfer::Allowed::new(&allowed_hosts, &origins);
    let config = StreamableHttpServerConfig::default()
        .with_json_response(true)
        .with_allowed_hosts(allowed_hosts)
        .with_allowed_origins(origins);
    let shutdown = config.cancellation_token.clone();
    let gate = gate::Gate::new(token, crate::registry::path(&db));
    let own_token = Router::new()
        .route("/auth/token", get(gate::own_scope).delete(gate::revoke_own))
        .with_state(gate.clone())
        .layer(middleware::from_fn_with_state(
            allowed.clone(),
            transfer::admit,
        ));
    let transfers = transfer::routes(db.clone(), allowed.clone());
    let delivery = delivery::routes(db.clone(), allowed.clone());
    let usage = usage::routes(db.clone(), allowed);
    let health = Health::new(db.clone());
    let service: StreamableHttpService<crate::UserMemoryServer, sessions::Sessions> =
        StreamableHttpService::new(
            move || {
                Ok(crate::UserMemoryServer {
                    db: db.clone(),
                    in_client_folder: false,
                })
            },
            sessions::Sessions::start(),
            config,
        );
    // /mcp, the export, import, delivery and use endpoints and a token's own revocation
    // need a token (src/remote/gate.rs). `/` names the service for health
    // checks and reverse-proxy smoke tests, reads no memory and needs no
    // token; other paths are 404.
    let router = Router::new()
        .nest_service("/mcp", service)
        .merge(transfers)
        .merge(delivery)
        .merge(usage)
        .merge(own_token)
        .layer(middleware::from_fn_with_state(gate, gate::authenticate))
        .route("/", get(move || health.clone().answer()))
        .fallback(|| async { StatusCode::NOT_FOUND });
    println!("Listening: http://{address}/mcp");
    eprintln!("Access-token file: {}", token_file.display());
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            stopped().await;
            shutdown.cancel();
        })
        .await?;
    // As `serve` does: the log goes into the store before the last check.
    drop(held);
    drop(backups);
    Ok(())
}

pub async fn connect(connection: Connection) -> Result<()> {
    let (input, output) = stdio();
    connect_over(connection, input, output, false).await
}

/// The bridge to the memory server for a client on `input` and `output`:
/// `serve`'s, behind its hand-over front, and `connect`'s on stdio, with
/// no front.
pub async fn connect_over(
    connection: Connection,
    input: impl tokio::io::AsyncRead + Send + Unpin + 'static,
    output: impl tokio::io::AsyncWrite + Send + Unpin + 'static,
    behind_front: bool,
) -> Result<()> {
    let remote = connection.transport()?;
    let local = rmcp::transport::async_rw::AsyncRwTransport::new_server(input, output);
    let endpoint = connection.url.clone();
    let token_file = connection.token_file.clone();
    // Beside the relay, never in it: the hooks on this machine read the
    // server's position from the marker it keeps.
    let (stop, stopping) = tokio::sync::watch::channel(false);
    let mut poller = tokio::spawn(marker::poll(connection.clone(), stopping));
    let relayed = forward(
        local,
        remote,
        || connection.transport(),
        &endpoint,
        &token_file,
        behind_front,
        forwarded_call_limit(),
    )
    .await;
    // The poller ends its kept session on its own time, so a hand-over or
    // the session's end waits for none of it; one still running past its
    // wait is stopped.
    let _ = stop.send(true);
    tokio::spawn(async move {
        if tokio::time::timeout(marker::POLLER_END_WAIT, &mut poller)
            .await
            .is_err()
        {
            poller.abort();
        }
    });
    relayed
}

/// Starts a send before the next message is read, then lets it finish
/// concurrently. rmcp's transports claim a message's place in their outgoing
/// queue (or a fair, first-come wait for one) when the send is first polled,
/// so polling here keeps delivery in arrival order. Spawning sends unpolled
/// lets the runtime start them in any order: a request could then overtake
/// `notifications/initialized`, which the HTTP client must send right after
/// `initialize`, and its worker quit.
fn start_in_order(
    sends: &mut tokio::task::JoinSet<Result<()>>,
    send: impl Future<Output = Result<()>> + Send + 'static,
) {
    let mut send = Box::pin(send);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    match send.as_mut().poll(&mut context) {
        std::task::Poll::Ready(result) => sends.spawn(async move { result }),
        std::task::Poll::Pending => sends.spawn(send),
    };
}

/// Tools that change memories, after which the instruction files are
/// brought in line with the store.
pub const WRITE_TOOLS: [&str; 4] = ["record", "update", "forget", "import"];

/// The id of a `tools/call` request for one of [`WRITE_TOOLS`].
fn write_call(message: &impl serde::Serialize) -> Option<serde_json::Value> {
    let message = serde_json::to_value(message).ok()?;
    let name = message.get("params")?.get("name")?.as_str()?;
    (message.get("method")? == "tools/call" && WRITE_TOOLS.contains(&name))
        .then(|| message.get("id").cloned())
        .flatten()
}

/// `message` with the projects that load in this folder anyway added to a
/// `link` call that names none, so the server, which cannot see the folder,
/// never stores or removes one as a link. Any other message passes
/// unchanged.
fn with_folder_projects<M: serde::Serialize + serde::de::DeserializeOwned>(message: M) -> M {
    let Ok(mut value) = serde_json::to_value(&message) else {
        return message;
    };
    let is_link = value.get("method").and_then(|m| m.as_str()) == Some("tools/call")
        && value.pointer("/params/name").and_then(|n| n.as_str()) == Some("link");
    let Some(arguments) = value
        .pointer_mut("/params/arguments")
        .and_then(|a| a.as_object_mut())
        .filter(|_| is_link)
    else {
        return message;
    };
    let named = |field: &str| {
        arguments
            .get(field)
            .and_then(|v| v.as_array())
            .is_some_and(|list| !list.is_empty())
    };
    if named("folder_projects") || named("included_projects") {
        return message;
    }
    let Ok(folder) = std::env::current_dir() else {
        return message;
    };
    let (own, listed) = crate::session::loaded_here(&folder);
    arguments.insert("folder_projects".into(), own.into());
    arguments.insert("included_projects".into(), listed.into());
    serde_json::from_value(value).unwrap_or(message)
}

/// The tool reply `reply` with `notices` added as text items after its own
/// content, which they leave as it was; `None`, keeping the notices for a
/// later reply, when it is not a tool result.
fn with_notices<M: serde::de::DeserializeOwned>(
    reply: &serde_json::Value,
    notices: &mut Vec<String>,
) -> Option<M> {
    let mut reply = reply.clone();
    let content = reply.pointer_mut("/result/content")?.as_array_mut()?;
    for notice in notices.drain(..) {
        content.push(serde_json::json!({"type": "text", "text": notice}));
    }
    serde_json::from_value(reply).ok()
}

/// The id a successful response answers, if `message` is one.
fn answered(message: &impl serde::Serialize) -> Option<serde_json::Value> {
    let message = serde_json::to_value(message).ok()?;
    message.get("result")?;
    message.get("id").cloned()
}

/// How long the relay waits to reconnect before answering the call that
/// asked for it with an error: the 10 seconds `probe` and `call` give a
/// handshake, so a reconnect waits no longer than a hook would.
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the bridge waits for its server to answer a session's first
/// request before it answers the handshake and the tool list itself
/// ([`StandIn`]). Both apps give a server 30 seconds to start (Codex's
/// `DEFAULT_STARTUP_TIMEOUT`, Claude Code's `MCP_TIMEOUT`), but neither
/// waits for it before the first turn; only the session-start hook, which
/// gives up after 5 seconds, holds Claude Code's first turn back. Measured
/// with Claude Code 2.1.325 and Codex 0.163.0 against a fake model: at 3
/// seconds Claude Code has the tools in its first turn whether the server
/// refuses, never answers, or answers after 4 or 6 seconds, and Codex,
/// whose first turn starts after about a second, from its next one. A
/// shorter wait would stand in for reachable servers that are far away.
const FIRST_ANSWER: Duration = Duration::from_secs(3);

/// How long the first real handshake may take, counted from the session's
/// first request: the 30 seconds both apps give a server to start (Codex's
/// `DEFAULT_STARTUP_TIMEOUT`, Claude Code's `MCP_TIMEOUT`). A slow server
/// keeps what it had before the bridge answered handshakes itself; a later
/// reconnect has [`RECONNECT_TIMEOUT`].
const APP_START_LIMIT: Duration = Duration::from_secs(30);

/// How long a call the relay forwarded may wait for the server's answer
/// before the relay answers it with an error and drops the connection: the
/// call's waits, which the server bounds at 90 s in all (`TOOL_CALL_BUDGET`
/// in src/memory/budget.rs), its own work, and 10 s for the network. The
/// heaviest work measured on a synthetic store of 10,000 memories with no
/// other writer, release builds on GitHub's runners, is `review`: 46 s on
/// macOS (3 cpus), 39 s on Windows x64 and 37 s on Ubuntu. An import into an
/// empty store takes 3.7 s, an export of everything 0.2 s and a search that
/// saves 32 vectors 0.04 s. Debug builds, an upper bound, take review past
/// 300 s. Review's cost grows with the square of the store's size, so a
/// review of a much larger store can still pass this limit. Under the 300 s
/// Codex gives a tool call (`DEFAULT_TOOL_TIMEOUT` in its
/// codex-rs/codex-mcp/src/rmcp_client.rs; its docs say 60 s), so the agent
/// gets this answer, not the app's timeout. Claude Code moves a call past two
/// minutes to a background task, where the reply still arrives, and its own
/// limits are far longer (code.claude.com/docs/en/mcp, "Time limits on MCP
/// tool calls").
const FORWARDED_CALL_LIMIT: Duration = Duration::from_secs(150);

/// [`FORWARDED_CALL_LIMIT`], which debug builds take in milliseconds from
/// `USER_MEMORY_FORWARDED_CALL_LIMIT_MS` so tests can see a stalled server's
/// call answered without waiting minutes.
fn forwarded_call_limit() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("USER_MEMORY_FORWARDED_CALL_LIMIT_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
    {
        return Duration::from_millis(ms);
    }
    FORWARDED_CALL_LIMIT
}

/// How long the bridge waits for the stand-in's answers to the requests it
/// still holds when the server takes over. It answers from memory, in the
/// same process, within microseconds; the bound only keeps a stand-in that
/// stopped from holding the session.
const STAND_IN_WAIT: Duration = Duration::from_secs(1);

/// The messages the [`StandIn`] answers: the handshake, the tool list and
/// `ping`, which is all an app asks before its first call.
fn stands_in_for(method: &str) -> bool {
    matches!(
        method,
        "initialize" | "server/discover" | "notifications/initialized" | "tools/list" | "ping"
    )
}

/// This build's own server without a store, which answers a session's
/// handshake, tool list and `ping` while its memory server has not been
/// reached, as `serve` on this machine would. Client and server are the
/// same program, and their contract only adds, so a server of another
/// version accepts what this one describes. It is never given a call.
struct StandIn {
    transport: rmcp::transport::async_rw::AsyncRwTransport<
        RoleClient,
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    >,
}

impl StandIn {
    fn start() -> Self {
        let (bridge, server_side) = tokio::io::duplex(crate::handover::PIPE_BUFFER);
        let server = crate::UserMemoryServer {
            db: PathBuf::new(),
            in_client_folder: true,
        };
        tokio::spawn(async move {
            // It ends when the bridge drops its side.
            if let Ok(service) = server.serve(tokio::io::split(server_side)).await {
                let _ = service.waiting().await;
            }
        });
        let (read, write) = tokio::io::split(bridge);
        Self {
            transport: rmcp::transport::async_rw::AsyncRwTransport::new_client(read, write),
        }
    }
}

/// The stand-in's next message; none while there is no stand-in.
async fn from_stand_in(stand_in: &mut Option<StandIn>) -> Option<RxJsonRpcMessage<RoleClient>> {
    match stand_in {
        Some(stand_in) => Transport::<RoleClient>::receive(&mut stand_in.transport).await,
        None => std::future::pending().await,
    }
}

/// A server reached again: its transport, what its handshake said, and the
/// tools it lists when the bridge asked.
struct Reconnected<R> {
    remote: R,
    handshake: Option<serde_json::Value>,
    tools: Option<serde_json::Value>,
}

/// Reestablish the upstream session without repeating a client request.
/// A 2026-07-28 session that sent no handshake has none to replay: each of
/// its requests names its version in `_meta`, so the new transport alone
/// carries it on. With `tools_params`, the server is also asked for its
/// tools, before any call of the app's reaches it, so a change is told
/// before that call's reply.
async fn reconnect<R: Transport<RoleClient>>(
    mut remote: R,
    initialize: Option<RxJsonRpcMessage<RoleServer>>,
    initialized: Option<RxJsonRpcMessage<RoleServer>>,
    tools_params: Option<&serde_json::Value>,
) -> Result<Reconnected<R>> {
    let handshake = match initialize {
        Some(init) => Some(replay_handshake(&mut remote, init, initialized).await?),
        None => None,
    };
    let tools = match tools_params {
        Some(params) => {
            Transport::<RoleClient>::send(&mut remote, tools_check(params)?).await?;
            let reply = answer(&mut remote, &serde_json::json!(TOOLS_CHECK_ID)).await?;
            reply.pointer("/result/tools").cloned()
        }
        None => None,
    };
    Ok(Reconnected {
        remote,
        handshake,
        tools,
    })
}

/// The reply to request `id` on `remote`, skipping anything else it sends
/// meanwhile.
async fn answer<R: Transport<RoleClient>>(
    remote: &mut R,
    id: &serde_json::Value,
) -> Result<serde_json::Value> {
    loop {
        let reply = Transport::<RoleClient>::receive(remote)
            .await
            .context("Memory server connection closed")?;
        let reply = serde_json::to_value(reply)?;
        if reply.get("id") == Some(id)
            && (reply.get("result").is_some() || reply.get("error").is_some())
        {
            return Ok(reply);
        }
    }
}

/// Replays the app's handshake `init` and `initialized` to a new server
/// connection; the server's handshake result.
async fn replay_handshake<R: Transport<RoleClient>>(
    remote: &mut R,
    init: RxJsonRpcMessage<RoleServer>,
    initialized: Option<RxJsonRpcMessage<RoleServer>>,
) -> Result<serde_json::Value> {
    let init_id = serde_json::to_value(&init)?
        .get("id")
        .cloned()
        .context("Client initialization has no request ID")?;
    Transport::<RoleClient>::send(remote, init).await?;
    let reply = answer(remote, &init_id).await?;
    let Some(result) = reply.get("result").cloned() else {
        bail!("Memory server rejected initialization");
    };
    if let Some(notice) = initialized {
        Transport::<RoleClient>::send(remote, notice).await?;
    }
    Ok(result)
}

/// Relays messages between a local MCP client and the memory server. After
/// the server answers a write, the instruction files on this machine are
/// brought in line with it, beside the relay. An upstream failure never
/// ends the relay: its calls are answered and the next one reconnects.
/// Ending would not even free the client, since the process can't exit
/// while tokio's stdin reader is blocked in a read that can't be cancelled,
/// so it would stay up, silent, until the client's next message. A call
/// forwarded to the server waits at most `call_limit` for its answer
/// ([`FORWARDED_CALL_LIMIT`]).
async fn forward<R: Transport<RoleClient>>(
    mut local: impl Transport<RoleServer>,
    mut remote: R,
    mut new_remote: impl FnMut() -> Result<R>,
    endpoint: &str,
    token_file: &Path,
    behind_front: bool,
    call_limit: Duration,
) -> Result<()> {
    let mut sends = tokio::task::JoinSet::<Result<()>>::new();
    let mut outgoing = tokio::task::JoinSet::<Result<()>>::new();
    let mut syncs = tokio::task::JoinSet::new();
    let mut writes = Vec::new();
    // The app's requests the server has not answered, with their methods.
    let mut pending: Vec<(serde_json::Value, String)> = Vec::new();
    // When each request forwarded to the server is answered with an error
    // instead ([`FORWARDED_CALL_LIMIT`]), oldest first; only those still
    // pending count.
    let mut due: Vec<(serde_json::Value, tokio::time::Instant)> = Vec::new();
    let mut initialize: Option<RxJsonRpcMessage<RoleServer>> = None;
    let mut initialized: Option<RxJsonRpcMessage<RoleServer>> = None;
    let mut initial_version: Option<serde_json::Value> = None;
    // The version the server's latest handshake named, which decides what
    // the bridge refuses ([`refusal`]).
    let mut server_version: Option<String> = None;
    // What the first handshake said about versions, which session start
    // told the user; a re-handshake that says something new is told in the
    // next tool reply.
    let mut version_line: Option<String> = None;
    let mut notices: Vec<String> = Vec::new();
    // The tools the app was last given, and the parameters of its last
    // `tools/list`, to ask a server reached again for its own.
    let mut given_tools: Option<serde_json::Value> = None;
    let mut tools_params: Option<serde_json::Value> = None;
    // Whether the server has answered this session. Until it has, the
    // messages the stand-in answers are kept for it, in order, and the
    // first request waits for the server until `first_answer`.
    let mut reached = false;
    let mut held: Vec<RxJsonRpcMessage<RoleServer>> = Vec::new();
    let mut opened: Option<tokio::time::Instant> = None;
    let mut first_answer: Option<tokio::time::Instant> = None;
    // The app's `tools/list` requests for a later page: their replies are
    // not the whole list.
    let mut pages: Vec<serde_json::Value> = Vec::new();
    let mut stand_in: Option<StandIn> = None;
    // The requests the stand-in has not answered, with their methods; never
    // in `pending`, so a failed call leaves them be.
    let mut standing: Vec<(serde_json::Value, String)> = Vec::new();
    let mut to_stand_in = tokio::task::JoinSet::<Result<()>>::new();
    let mut disconnected = false;
    let mut error = None;
    loop {
        due.retain(|(id, _)| pending.iter().any(|(request_id, _)| request_id == id));
        tokio::select! {
            message = Transport::<RoleServer>::receive(&mut local) => {
                let Some(message) = message else { break; };
                let message = with_folder_projects(message);
                let value = serde_json::to_value(&message)?;
                let method = value.get("method").and_then(|v| v.as_str()).unwrap_or("");
                if method == "initialize" || method == "server/discover" {
                    initialize = Some(message.clone());
                } else if method == "notifications/initialized" {
                    initialized = Some(message.clone());
                } else if method == "tools/list" {
                    tools_params = value.get("params").cloned();
                    if value.pointer("/params/cursor").is_some() && let Some(id) = value.get("id") {
                        pages.push(id.clone());
                    }
                }
                if let Some(id) = value.get("id") && value.get("method").is_some() {
                    pending.push((id.clone(), method.to_owned()));
                    if !reached && first_answer.is_none() {
                        let now = tokio::time::Instant::now();
                        opened = Some(now);
                        first_answer = Some(now + FIRST_ANSWER);
                    }
                }
                if !reached && stands_in_for(method) {
                    if let Some(stand_in) = &mut stand_in {
                        if let Some(id) = value.get("id") {
                            pending.retain(|(request_id, _)| request_id != id);
                            standing.push((id.clone(), method.to_owned()));
                        }
                        let send = Transport::<RoleClient>::send(&mut stand_in.transport, message);
                        start_in_order(&mut to_stand_in, async move { send.await.context("The stand-in stopped") });
                        continue;
                    }
                    held.push(message.clone());
                }
                if disconnected && !reached && value.get("id").is_none() {
                    // Nothing to say to a server not reached yet.
                    continue;
                }
                // A call the server seems too old for is checked against the
                // server as it is now: a refused call never reaches it, so
                // without a fresh handshake an upgraded server would go
                // unseen. Only with nothing else in flight, which the new
                // connection would lose.
                if !disconnected && pending.len() == 1 && refusal(&value, server_version.as_deref(), endpoint).is_some() {
                    disconnected = true;
                }
                if disconnected {
                    if method == "initialize" || method == "server/discover" {
                        match new_remote() {
                            Ok(next) => { remote = next; disconnected = false; }
                            Err(err) => {
                                let message = Failure::of(&err).message(endpoint, token_file);
                                fail_pending(&mut local, &mut sends, &mut pending, &message)?;
                                continue;
                            }
                        }
                    } else {
                        if let Some(stand_in) = &mut stand_in {
                            // What it still holds is answered before the
                            // server can take over.
                            finish_standing(stand_in, &mut standing, &mut local, &mut sends, &mut given_tools).await?;
                        }
                        // The server may have changed while it was away: its
                        // own tool list tells, when the app was given one.
                        let asked = given_tools.as_ref().map(|_| tools_params.clone().unwrap_or_else(|| serde_json::json!({})));
                        let recovered = async {
                            reconnect(new_remote()?, initialize.clone(), initialized.clone(), asked.as_ref()).await
                        };
                        // The first real handshake has the time the app
                        // would have given it.
                        let limit = match opened {
                            Some(opened) if !reached => RECONNECT_TIMEOUT.max((opened + APP_START_LIMIT).saturating_duration_since(tokio::time::Instant::now())),
                            _ => RECONNECT_TIMEOUT,
                        };
                        match tokio::time::timeout(limit, recovered).await {
                            Ok(Ok(Reconnected { remote: next, handshake, tools })) => {
                                remote = next;
                                disconnected = false;
                                if !reached {
                                    // The first real handshake: the stand-in's
                                    // part is over.
                                    reached = true;
                                    held.clear();
                                    stand_in = None;
                                }
                                let version = handshake.as_ref().and_then(crate::setup::versions::server_version).cloned();
                                if let Some(named) = version.as_ref().and_then(serde_json::Value::as_str) {
                                    server_version = Some(named.to_owned());
                                }
                                if initial_version.is_none() {
                                    initial_version.clone_from(&version);
                                }
                                let line = handshake.as_ref().and_then(|result| crate::setup::versions::server_line(crate::setup::versions::running(), endpoint, result));
                                if line.is_some() && line != version_line {
                                    notices.extend(line.clone());
                                }
                                if behind_front && handshake.is_some() && line != version_line {
                                    tell_front_the_line(&mut local, &mut sends, line.as_deref())?;
                                }
                                version_line = line;
                                if given_tools.is_some() {
                                    if let Some(tools) = tools
                                        && given_tools.as_ref() != Some(&tools)
                                    {
                                        given_tools = Some(tools);
                                        tell_tools_changed(&mut local, &mut sends)?;
                                    }
                                } else if let (Some(first), Some(current)) = (&initial_version, &version)
                                    && first != current
                                {
                                    tell_tools_changed(&mut local, &mut sends)?;
                                }
                            }
                            failed => {
                                let failure = match &failed {
                                    Ok(Err(err)) => Failure::of(err),
                                    _ => Failure::Unanswered,
                                };
                                let message = failure.message(endpoint, token_file);
                                fail_pending(&mut local, &mut sends, &mut pending, &message)?;
                                writes.clear();
                                continue;
                            }
                        }
                    }
                }
                if let Some(refused) = refusal(&value, server_version.as_deref(), endpoint)
                    && let Some(id) = value.get("id")
                {
                    pending.retain(|(request_id, _)| request_id != id);
                    fail_request(&mut local, &mut sends, id, method, &refused)?;
                    continue;
                }
                writes.extend(write_call(&message));
                if let Some(id) = value.get("id") && value.get("method").is_some() {
                    due.push((id.clone(), tokio::time::Instant::now() + call_limit));
                }
                let send = Transport::<RoleClient>::send(&mut remote, message);
                start_in_order(&mut outgoing, async move { send.await.context("Memory server request failed") });
            }
            message = Transport::<RoleClient>::receive(&mut remote), if !disconnected => {
                let Some(message) = message else {
                    outgoing.shutdown().await;
                    let message = Failure::Unanswered.message(endpoint, token_file);
                    if !reached {
                        stand_in = Some(start_standing(&mut held, &mut pending, &mut standing, &mut to_stand_in));
                    }
                    fail_pending(&mut local, &mut sends, &mut pending, &message)?;
                    writes.clear();
                    disconnected = true;
                    continue;
                };
                reached = true;
                held.clear();
                let value = serde_json::to_value(&message)?;
                if let Some(named) = value.get("result").and_then(crate::setup::versions::server_version).and_then(serde_json::Value::as_str) {
                    server_version = Some(named.to_owned());
                }
                if initial_version.is_none() && let Some(version) = value.get("result").and_then(crate::setup::versions::server_version) {
                    initial_version = Some(version.clone());
                    version_line = value.get("result").and_then(|result| crate::setup::versions::server_line(crate::setup::versions::running(), endpoint, result));
                }
                let mut message = message;
                if let Some(id) = value.get("id") && (value.get("result").is_some() || value.get("error").is_some()) {
                    let asked = pending.iter().find(|(request_id, _)| request_id == id).map(|(_, method)| method.clone());
                    pending.retain(|(request_id, _)| request_id != id);
                    let page = pages.iter().position(|request| request == id).map(|at| pages.swap_remove(at)).is_some();
                    if asked.as_deref() == Some("tools/list") && !page && let Some(tools) = value.pointer("/result/tools") {
                        given_tools = Some(tools.clone());
                    }
                    let tool = asked.as_deref() == Some("tools/call");
                    if tool && !notices.is_empty() && let Some(with) = with_notices(&value, &mut notices) {
                        message = with;
                    }
                }
                if let Some(id) = answered(&message)
                    && let Some(at) = writes.iter().position(|write| *write == id)
                {
                    writes.swap_remove(at);
                    syncs.spawn(crate::setup::sync_bounded());
                    marker::moved(endpoint);
                }
                let send = Transport::<RoleServer>::send(&mut local, message);
                start_in_order(&mut sends, async move { send.await.context("Cannot send memory server response to client") });
            }
            result = outgoing.join_next(), if !outgoing.is_empty() => {
                if let Err(err) = result.context("Connector send task disappeared").and_then(|r| r.context("Connector send task failed")).and_then(|r| r) {
                    outgoing.shutdown().await;
                    let message = Failure::of(&err).message(endpoint, token_file);
                    if !reached && stand_in.is_none() {
                        stand_in = Some(start_standing(&mut held, &mut pending, &mut standing, &mut to_stand_in));
                    }
                    fail_pending(&mut local, &mut sends, &mut pending, &message)?;
                    writes.clear();
                    disconnected = true;
                }
            }
            () = tokio::time::sleep_until(first_answer.unwrap_or_else(tokio::time::Instant::now)), if !reached && !disconnected && first_answer.is_some() => {
                // The server has not answered in time: the session goes on
                // with the stand-in, and the next call tries again.
                outgoing.shutdown().await;
                stand_in = Some(start_standing(&mut held, &mut pending, &mut standing, &mut to_stand_in));
                let message = Failure::Unanswered.message(endpoint, token_file);
                fail_pending(&mut local, &mut sends, &mut pending, &message)?;
                writes.clear();
                disconnected = true;
            }
            () = tokio::time::sleep_until(due.first().map_or_else(tokio::time::Instant::now, |(_, at)| *at)), if !disconnected && !due.is_empty() => {
                // The server took a forwarded call and has not answered it in
                // time. Every call waiting on it is answered. Cancelling the
                // sends drops their requests, so the server's late answers
                // have nowhere to arrive; the connection is not read while
                // disconnected, and the next call replaces it.
                outgoing.shutdown().await;
                let message = Failure::Unanswered.message(endpoint, token_file);
                fail_pending(&mut local, &mut sends, &mut pending, &message)?;
                writes.clear();
                due.clear();
                disconnected = true;
            }
            message = from_stand_in(&mut stand_in) => {
                let Some(message) = message else {
                    stand_in = None;
                    continue;
                };
                pass_stand_in_reply(message, &mut standing, &mut local, &mut sends, &mut given_tools)?;
            }
            result = to_stand_in.join_next(), if !to_stand_in.is_empty() => {
                if let Some(Ok(Err(err)) | Err(err)) = result.map(|r| r.map_err(anyhow::Error::from)) {
                    error = Some(err);
                    break;
                }
            }
            result = sends.join_next(), if !sends.is_empty() => {
                match result.context("Connector send task disappeared").and_then(|r| r.context("Connector send task failed")).and_then(|r| r) {
                    Ok(()) => {},
                    Err(err) => { error = Some(err); break; }
                }
            }
        }
    }
    outgoing.shutdown().await;
    to_stand_in.shutdown().await;
    sends.shutdown().await;
    let _ = tokio::time::timeout(crate::session::hook_time(crate::setup::SYNC_LIMIT), async {
        while syncs.join_next().await.is_some() {}
    })
    .await;
    let _ = tokio::time::timeout(
        Duration::from_secs(1),
        Transport::<RoleClient>::close(&mut remote),
    )
    .await;
    let _ = Transport::<RoleServer>::close(&mut local).await;
    if let Some(err) = error {
        Err(err)
    } else {
        Ok(())
    }
}

/// Why the relay couldn't get a client's call answered.
#[derive(Debug, PartialEq)]
enum Failure {
    /// The server couldn't be reached, or the connection ended.
    Unanswered,
    /// The server answered 401: it refused this machine's access token,
    /// saying why when it is revoked or expired.
    Refused(Option<refused::Reason>),
    /// The server answered 503: it is up but cannot serve requests, as
    /// while its token registry cannot be read.
    Unavailable,
}

impl Failure {
    /// What a failed send or reconnect `err` means: rmcp reports the
    /// server's 401 as an [`AuthRequiredError`] in the chain.
    fn of(err: &anyhow::Error) -> Self {
        if err.chain().any(|cause| cause.is::<AuthRequiredError>()) {
            Self::Refused(refused::Reason::of(err))
        } else if refused::unavailable(err) {
            Self::Unavailable
        } else {
            Self::Unanswered
        }
    }

    /// The text the client gets for each call this failure left waiting: what
    /// the agent does next. It names the token's file, never the token, and
    /// tells the agent not to open it. A write that reached the server
    /// before it stopped is stored, so a repeat is searched for first.
    fn message(&self, endpoint: &str, token_file: &Path) -> String {
        match self {
            Self::Unanswered => format!(
                "The memory server at {endpoint} did not answer; the call may not have completed. Before repeating a write, search for it. Memory reconnects on the next call."
            ),
            Self::Refused(Some(reason)) => format!(
                "{} The call was not run. Tell the user; don't open the token file ({}).",
                reason.sentence(endpoint),
                token_file.display()
            ),
            Self::Unavailable => format!(
                "{} The call was not run. Tell the user; memory tries again on the next call.",
                refused::unavailable_sentence(endpoint)
            ),
            Self::Refused(None) => format!(
                "The memory server at {endpoint} refused this machine's access token ({}); the call was not run. Tell the user the token in that file needs replacing (user-memory doctor shows it); don't open the file. Memory tries again on the next call.",
                token_file.display()
            ),
        }
    }
}

/// Starts the stand-in for a session whose server was not reached, and
/// gives it the messages held for it, in order. The requests among them
/// leave `pending` for `standing`: the stand-in answers them.
fn start_standing(
    held: &mut Vec<RxJsonRpcMessage<RoleServer>>,
    pending: &mut Vec<(serde_json::Value, String)>,
    standing: &mut Vec<(serde_json::Value, String)>,
    to_stand_in: &mut tokio::task::JoinSet<Result<()>>,
) -> StandIn {
    let mut stand_in = StandIn::start();
    for message in held.drain(..) {
        if let Ok(value) = serde_json::to_value(&message)
            && let Some(id) = value.get("id")
            && let Some(at) = pending.iter().position(|(request_id, _)| request_id == id)
        {
            standing.push(pending.remove(at));
        }
        let send = Transport::<RoleClient>::send(&mut stand_in.transport, message);
        start_in_order(to_stand_in, async move {
            send.await.context("The stand-in stopped")
        });
    }
    stand_in
}

/// Passes a reply of the stand-in's to the app, keeping the tool list it
/// gave as the one the app has.
fn pass_stand_in_reply(
    message: RxJsonRpcMessage<RoleClient>,
    standing: &mut Vec<(serde_json::Value, String)>,
    local: &mut impl Transport<RoleServer>,
    sends: &mut tokio::task::JoinSet<Result<()>>,
    given_tools: &mut Option<serde_json::Value>,
) -> Result<()> {
    let value = serde_json::to_value(&message)?;
    if let Some(id) = value.get("id")
        && let Some(at) = standing.iter().position(|(request_id, _)| request_id == id)
    {
        let (_, method) = standing.swap_remove(at);
        if method == "tools/list"
            && let Some(tools) = value.pointer("/result/tools")
        {
            *given_tools = Some(tools.clone());
        }
    }
    let send = Transport::<RoleServer>::send(local, message);
    start_in_order(sends, async move {
        send.await
            .context("Cannot send memory server response to client")
    });
    Ok(())
}

/// Lets the stand-in answer every request it still holds, before the
/// server takes the session over; one it can't answer within
/// [`STAND_IN_WAIT`] gets the relay's error.
async fn finish_standing(
    stand_in: &mut StandIn,
    standing: &mut Vec<(serde_json::Value, String)>,
    local: &mut impl Transport<RoleServer>,
    sends: &mut tokio::task::JoinSet<Result<()>>,
    given_tools: &mut Option<serde_json::Value>,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + STAND_IN_WAIT;
    while !standing.is_empty() {
        let next = tokio::time::timeout_at(
            deadline,
            Transport::<RoleClient>::receive(&mut stand_in.transport),
        )
        .await;
        let Ok(Some(message)) = next else {
            break;
        };
        pass_stand_in_reply(message, standing, local, sends, given_tools)?;
    }
    fail_pending(
        local,
        sends,
        standing,
        "The memory relay stopped answering this request",
    )
}

/// The method of the private notice that gives the front the version line a
/// re-handshake left the bridge with, so the front never says it again
/// after a hand-over. The front keeps it from the app.
pub(crate) const SERVER_LINE_NOTICE: &str = "notifications/user-memory/server_line";

/// Gives the front the bridge's version line, `None` once there is none.
fn tell_front_the_line(
    local: &mut impl Transport<RoleServer>,
    sends: &mut tokio::task::JoinSet<Result<()>>,
    line: Option<&str>,
) -> Result<()> {
    let notice: TxJsonRpcMessage<RoleServer> = serde_json::from_value(
        serde_json::json!({"jsonrpc":"2.0","method":SERVER_LINE_NOTICE,"params":{"line":line}}),
    )?;
    let send = Transport::<RoleServer>::send(local, notice);
    start_in_order(sends, async move {
        send.await
            .context("Cannot send memory server response to client")
    });
    Ok(())
}

/// The id of the `tools/list` the bridge sends a server it reached again;
/// its reply never reaches the app.
const TOOLS_CHECK_ID: &str = "user-memory-bridge-tools";

/// The bridge's own `tools/list`, with the parameters of the app's last one
/// (a 2026-07-28 request names its version and client in `_meta`), less
/// its cursor, so the reply is the whole list, and its progress token: the
/// reply is the bridge's, so no progress is shown.
fn tools_check(params: &serde_json::Value) -> Result<TxJsonRpcMessage<RoleClient>> {
    let mut params = params.clone();
    // The whole list, not the page the app last asked for.
    if let Some(fields) = params.as_object_mut() {
        fields.remove("cursor");
    }
    if let Some(meta) = params
        .get_mut("_meta")
        .and_then(serde_json::Value::as_object_mut)
    {
        meta.remove("progressToken");
    }
    Ok(serde_json::from_value(
        serde_json::json!({"jsonrpc":"2.0","id":TOOLS_CHECK_ID,"method":"tools/list","params":params}),
    )?)
}

/// Tells the app its tools changed. The front carries it to a 2026-07-28
/// app's listen streams.
fn tell_tools_changed(
    local: &mut impl Transport<RoleServer>,
    sends: &mut tokio::task::JoinSet<Result<()>>,
) -> Result<()> {
    let notice: TxJsonRpcMessage<RoleServer> = serde_json::from_value(
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}),
    )?;
    let send = Transport::<RoleServer>::send(local, notice);
    start_in_order(sends, async move {
        send.await
            .context("Cannot send memory server response to client")
    });
    Ok(())
}

/// Answers every call still waiting with `message`.
fn fail_pending(
    local: &mut impl Transport<RoleServer>,
    sends: &mut tokio::task::JoinSet<Result<()>>,
    pending: &mut Vec<(serde_json::Value, String)>,
    message: &str,
) -> Result<()> {
    for (id, method) in pending.drain(..) {
        fail_request(local, sends, &id, &method, message)?;
    }
    Ok(())
}

/// Answers the app's request `id` for `method` with `message`, as the relay
/// and not the server.
fn fail_request(
    local: &mut impl Transport<RoleServer>,
    sends: &mut tokio::task::JoinSet<Result<()>>,
    id: &serde_json::Value,
    method: &str,
    message: &str,
) -> Result<()> {
    // A tool call gets a tool error, which the agent reads. Any other
    // request gets JSON-RPC's internal error: the relay failed it, not the
    // server, which may never have seen it.
    let response = if method == "tools/call" {
        serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"isError":true,"content":[{"type":"text","text":message}]}})
    } else {
        serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":rmcp::model::ErrorCode::INTERNAL_ERROR.0,"message":message}})
    };
    let response: TxJsonRpcMessage<RoleServer> = serde_json::from_value(response)?;
    let send = Transport::<RoleServer>::send(local, response);
    start_in_order(sends, async move {
        send.await
            .context("Cannot send memory server response to client")
    });
    Ok(())
}

/// What the bridge answers, in place of the server at `endpoint` whose
/// handshake named `server`, to the app's message `value` when it is a tool
/// call with an input that server is too old for: the refusal and what the
/// agent should do. `None` for anything else.
fn refusal(value: &serde_json::Value, server: Option<&str>, endpoint: &str) -> Option<String> {
    if value.get("method")? != "tools/call" {
        return None;
    }
    let params = value.get("params")?;
    let tool = params.get("name")?.as_str()?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let (sentence, floor) =
        crate::setup::versions::refused_option(endpoint, server?, tool, &arguments)?;
    Some(format!("{sentence} {}", floor.for_agent))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bridge asks for the whole tool list with the app's `_meta`,
    /// whatever page the app last asked for, and reports no progress.
    #[test]
    fn the_tools_check_asks_for_the_whole_list() {
        let params = serde_json::json!({
            "cursor": "page-2",
            "_meta": {"progressToken": 7, "io.modelcontextprotocol/protocolVersion": "2026-07-28"}
        });
        let asked = serde_json::to_value(tools_check(&params).unwrap()).unwrap();
        assert_eq!(
            asked,
            serde_json::json!({"jsonrpc": "2.0", "id": TOOLS_CHECK_ID, "method": "tools/list", "params": {
                "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}
            }})
        );
    }

    /// The CLI and the hooks get the server's own words for a failed call.
    #[test]
    fn a_failed_call_says_what_the_server_said() {
        assert_eq!(
            failure("read", r#"{"error":"dead is not a memory id"}"#),
            "dead is not a memory id"
        );
        assert_eq!(
            failure("record", r#"{"stored":false,"error":"store is locked"}"#),
            "store is locked"
        );
        assert_eq!(
            failure("index", "Memory unavailable: store is locked\n"),
            "store is locked"
        );
        assert_eq!(
            failure("index", " "),
            "Memory server could not complete index"
        );
    }

    /// The tool schemas this server publishes, with `edit` applied to one field.
    fn tools(edit: impl Fn(&mut serde_json::Value)) -> Vec<(String, serde_json::Value)> {
        let uuid = serde_json::json!({"type":"string","format":"uuid"});
        let nullable = serde_json::json!({"type":["string","null"],"format":"uuid"});
        let list = serde_json::json!({"type":"array","items":uuid.clone()});
        let mut schemas = serde_json::json!({
            "record": {"properties": {"confirms": nullable, "supersedes": list, "distinct_from": list}},
            "search": {"properties": {}},
            "index": {"properties": {}},
            "read": {"properties": {}},
            "update": {"properties": {"memory_id": uuid, "distinct_from": list}},
            "forget": {"properties": {"memory_id": nullable, "origin": {"type":["string","null"]}}}
        });
        edit(&mut schemas);
        schemas
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, schema)| (name.clone(), schema.clone()))
            .collect()
    }

    #[test]
    fn only_answered_write_calls_bring_instruction_files_up_to_date() {
        let call = |name: &str| serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":name,"arguments":{}}});
        for name in WRITE_TOOLS {
            assert_eq!(
                write_call(&call(name)),
                Some(serde_json::json!(7)),
                "{name}"
            );
        }
        for other in [
            call("search"),
            call("index"),
            serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/list","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        ] {
            assert_eq!(write_call(&other), None, "{other}");
        }
        assert_eq!(
            answered(&serde_json::json!({"jsonrpc":"2.0","id":7,"result":{}})),
            Some(serde_json::json!(7))
        );
        assert_eq!(
            answered(
                &serde_json::json!({"jsonrpc":"2.0","id":7,"error":{"code":-1,"message":"x"}})
            ),
            None
        );
    }

    #[test]
    fn this_servers_tools_pass_and_each_missing_capability_is_refused() {
        assert!(check_tools(&tools(|_| {})).is_ok());
        let set = |path: &[&str], value: serde_json::Value| {
            let path: Vec<String> = path.iter().map(|p| (*p).to_owned()).collect();
            move |schemas: &mut serde_json::Value| {
                let mut target = &mut *schemas;
                for key in &path[..path.len() - 1] {
                    target = &mut target[key.as_str()];
                }
                target[path.last().unwrap().as_str()] = value.clone();
            }
        };
        let ids = "must expose UUID memory IDs";
        let resolve = "cannot resolve similar memories";
        let uuid = serde_json::json!({"type":"string","format":"uuid"});
        for (path, value, message) in [
            (
                &["update", "properties", "memory_id"][..],
                serde_json::json!({"type":"string"}),
                ids,
            ),
            (
                &["forget", "properties", "memory_id"][..],
                serde_json::json!({"type":"integer","format":"uuid"}),
                ids,
            ),
            (
                &["forget", "properties", "memory_id"][..],
                serde_json::json!({"type":["integer","null"],"format":"uuid"}),
                ids,
            ),
            (
                &["forget", "properties", "memory_id"][..],
                serde_json::json!({"type":["string","integer"],"format":"uuid"}),
                ids,
            ),
            (
                &["forget", "properties", "memory_id"][..],
                serde_json::json!({"type":["null"],"format":"uuid"}),
                ids,
            ),
            (
                &["forget", "properties", "origin"][..],
                serde_json::Value::Null,
                "cannot forget by conversation origin",
            ),
            (
                &["record", "properties", "confirms"][..],
                serde_json::Value::Null,
                resolve,
            ),
            (
                &["record", "properties", "supersedes"][..],
                uuid.clone(),
                resolve,
            ),
            (
                &["record", "properties", "distinct_from"][..],
                serde_json::json!({"type":"array","items":{"type":"string"}}),
                resolve,
            ),
            (
                &["update", "properties", "distinct_from"][..],
                serde_json::Value::Null,
                resolve,
            ),
        ] {
            let schemas = tools(set(path, value.clone()));
            let schemas: Vec<_> = schemas
                .into_iter()
                .map(|(name, mut schema)| {
                    if let Some(props) = schema["properties"].as_object_mut() {
                        props.retain(|_, v| !v.is_null());
                    }
                    (name, schema)
                })
                .collect();
            let err = check_tools(&schemas).unwrap_err().to_string();
            assert!(err.contains(message), "{path:?}={value}: {err}");
        }
        let mut partial = tools(|_| {});
        partial.retain(|(name, _)| name != "search");
        assert!(
            check_tools(&partial)
                .unwrap_err()
                .to_string()
                .contains("does not provide the user-memory tool set")
        );
    }
    use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
    use std::{
        io,
        sync::{Arc, Mutex},
    };
    use tokio::sync::mpsc;

    type Log = Arc<Mutex<Vec<String>>>;

    fn label(message: &impl Serialize) -> String {
        let value = serde_json::to_value(message).unwrap();
        value["method"]
            .as_str()
            .map_or_else(|| format!("response {}", value["id"]), str::to_owned)
    }

    /// Like rmcp's transports, a send claims its place in the outgoing queue
    /// when it is first polled, then completes later.
    fn queued(log: &Log, label: String) -> impl Future<Output = io::Result<()>> + Send + 'static {
        let log = log.clone();
        async move {
            log.lock().unwrap().push(label);
            tokio::task::yield_now().await;
            Ok(())
        }
    }

    /// Stands in for the stdio client: yields queued messages, then waits
    /// until the test closes it.
    struct Local {
        incoming: mpsc::UnboundedReceiver<RxJsonRpcMessage<RoleServer>>,
        written: Log,
    }

    impl Transport<RoleServer> for Local {
        type Error = io::Error;
        fn send(
            &mut self,
            item: TxJsonRpcMessage<RoleServer>,
        ) -> impl Future<Output = io::Result<()>> + Send + 'static {
            queued(&self.written, label(&item))
        }
        async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
            self.incoming.recv().await
        }
        async fn close(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Stands in for the memory server: records the order messages reach its
    /// queue and replies with what the test pushes.
    struct Remote {
        replies: mpsc::UnboundedReceiver<RxJsonRpcMessage<RoleClient>>,
        sent: Log,
    }

    impl Transport<RoleClient> for Remote {
        type Error = io::Error;
        fn send(
            &mut self,
            item: TxJsonRpcMessage<RoleClient>,
        ) -> impl Future<Output = io::Result<()>> + Send + 'static {
            queued(&self.sent, label(&item))
        }
        async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
            self.replies.recv().await
        }
        async fn close(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn message<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
        serde_json::from_value(value).unwrap()
    }

    // rmcp's HTTP client treats the message after initialize as the
    // initialized notification; a reordered request killed its worker.
    // One worker thread makes tokio run the last spawned task first, which
    // reorders independently spawned sends every time.
    #[test]
    fn messages_reach_each_side_in_the_order_they_arrived() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (sent, written) = (Log::default(), Log::default());
        let (to_local, incoming) = mpsc::unbounded_channel();
        let (to_remote, replies) = mpsc::unbounded_channel();
        for value in [
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","id":3,"method":"resources/list","params":{}}),
        ] {
            to_local.send(message(value)).unwrap();
        }
        for id in [2, 3] {
            to_remote
                .send(message(
                    serde_json::json!({"jsonrpc":"2.0","id":id,"result":{}}),
                ))
                .unwrap();
        }
        let local = Local {
            incoming,
            written: written.clone(),
        };
        let remote = Remote {
            replies,
            sent: sent.clone(),
        };
        let task = runtime.spawn(forward(
            local,
            remote,
            || Err(anyhow::anyhow!("not used")),
            "test",
            Path::new("token"),
            false,
            FORWARDED_CALL_LIMIT,
        ));
        runtime.block_on(async {
            while written.lock().unwrap().len() < 2 || sent.lock().unwrap().len() < 3 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        drop(to_local);
        runtime.block_on(task).unwrap().unwrap();
        assert_eq!(
            *sent.lock().unwrap(),
            ["notifications/initialized", "tools/list", "resources/list"]
        );
        assert_eq!(*written.lock().unwrap(), ["response 2", "response 3"]);
    }

    #[test]
    fn reconnect_notifies_only_when_the_server_version_changes() {
        for (next_version, changed) in [("1", false), ("2", true)] {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let (sent, written, resumed) = (Log::default(), Log::default(), Log::default());
                let (to_local, incoming) = mpsc::unbounded_channel();
                let (to_remote, replies) = mpsc::unbounded_channel();
                let (to_next, next_replies) = mpsc::unbounded_channel();
                let local = Local { incoming, written: written.clone() };
                let remote = Remote { replies, sent: sent.clone() };
                let mut next = Some(Remote { replies: next_replies, sent: resumed.clone() });
                let task = tokio::spawn(forward(local, remote, move || next.take().context("no transport"), "test", Path::new("token"), false, FORWARDED_CALL_LIMIT));
                let wait_for = |log: Log, n| async move {
                    tokio::time::timeout(Duration::from_secs(5), async {
                        while log.lock().unwrap().len() < n {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                    }).await.unwrap();
                };
                let init = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}});
                let init_reply = |version| serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"test","version":version}}});
                to_local.send(message(init)).unwrap();
                wait_for(sent.clone(), 1).await;
                to_remote.send(message(init_reply("1"))).unwrap();
                wait_for(written.clone(), 1).await;
                to_local.send(message(serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}))).unwrap();
                wait_for(sent.clone(), 2).await;
                to_local.send(message(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))).unwrap();
                to_local.send(message(serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}))).unwrap();
                wait_for(sent.clone(), 4).await;
                drop(to_remote);
                wait_for(written.clone(), 3).await;
                to_next.send(message(init_reply(next_version))).unwrap();
                to_next.send(message(serde_json::json!({"jsonrpc":"2.0","id":4,"result":{}}))).unwrap();
                to_local.send(message(serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/list","params":{}}))).unwrap();
                wait_for(written.clone(), if changed { 5 } else { 4 }).await;
                let output = written.lock().unwrap().clone();
                for id in [2, 3, 4] {
                    assert_eq!(output.iter().filter(|item| **item == format!("response {id}")).count(), 1);
                }
                assert_eq!(output.iter().filter(|item| *item == "notifications/tools/list_changed").count(), usize::from(changed));
                assert_eq!(*resumed.lock().unwrap(), ["initialize", "notifications/initialized", "tools/list"]);
                drop(to_local);
                task.await.unwrap().unwrap();
            });
        }
    }

    /// Stands in for the stdio client, keeping every message it is sent.
    struct Seen {
        incoming: mpsc::UnboundedReceiver<RxJsonRpcMessage<RoleServer>>,
        written: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    impl Transport<RoleServer> for Seen {
        type Error = io::Error;
        fn send(
            &mut self,
            item: TxJsonRpcMessage<RoleServer>,
        ) -> impl Future<Output = io::Result<()>> + Send + 'static {
            self.written
                .lock()
                .unwrap()
                .push(serde_json::to_value(&item).unwrap());
            std::future::ready(Ok(()))
        }
        async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
            self.incoming.recv().await
        }
        async fn close(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Stands in for a server connection whose send of request `held` waits
    /// until the test lets it go, then fails, as a connection reset after a
    /// stall does.
    struct Stalling {
        replies: mpsc::UnboundedReceiver<RxJsonRpcMessage<RoleClient>>,
        sent: Log,
        held: Option<(serde_json::Value, tokio::sync::oneshot::Receiver<()>)>,
    }

    impl Transport<RoleClient> for Stalling {
        type Error = io::Error;
        fn send(
            &mut self,
            item: TxJsonRpcMessage<RoleClient>,
        ) -> impl Future<Output = io::Result<()>> + Send + 'static {
            let id = serde_json::to_value(&item).unwrap()["id"].clone();
            self.sent.lock().unwrap().push(label(&item));
            let held = match self.held.take() {
                Some((held, release)) if held == id => Some(release),
                other => {
                    self.held = other;
                    None
                }
            };
            async move {
                if let Some(release) = held {
                    let _ = release.await;
                    return Err(io::Error::other("connection reset"));
                }
                Ok(())
            }
        }
        async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
            self.replies.recv().await
        }
        async fn close(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The forwarded-call deadline of the given-up-send test: long enough for
    /// the steps after it, which take milliseconds, short enough to be quick.
    const GIVE_UP_AFTER: Duration = Duration::from_secs(1);

    // A call the server never answered is answered at the deadline. Its send
    // failing later, once the next connection carries a call, must not answer
    // that call too: the relay cancels the sends it gave up on.
    #[test]
    fn a_send_given_up_on_that_fails_later_leaves_the_next_connections_call_be() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let written = Arc::<Mutex<Vec<serde_json::Value>>>::default();
            let (sent, resumed) = (Log::default(), Log::default());
            let (to_local, incoming) = mpsc::unbounded_channel();
            let (to_remote, replies) = mpsc::unbounded_channel();
            let (to_next, next_replies) = mpsc::unbounded_channel();
            let (release, released) = tokio::sync::oneshot::channel();
            let local = Seen { incoming, written: written.clone() };
            let remote = Stalling { replies, sent: sent.clone(), held: Some((serde_json::json!(3), released)) };
            let mut next = Some(Stalling { replies: next_replies, sent: resumed.clone(), held: None });
            let task = tokio::spawn(forward(local, remote, move || next.take().context("no transport"), "test", Path::new("token"), false, GIVE_UP_AFTER));
            let wait_until = |done: Box<dyn Fn() -> bool>| async move {
                while !done() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            };
            let init = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}});
            let init_reply = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"test","version":"1"}}});
            let call = |id| serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"index","arguments":{}}});
            to_local.send(message(init)).unwrap();
            let log = sent.clone();
            wait_until(Box::new(move || !log.lock().unwrap().is_empty())).await;
            to_remote.send(message(init_reply.clone())).unwrap();
            let seen = written.clone();
            wait_until(Box::new(move || !seen.lock().unwrap().is_empty())).await;
            to_local.send(message(serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}))).unwrap();
            to_local.send(message(call(3))).unwrap();
            // At the deadline, call 3 is answered.
            let seen = written.clone();
            wait_until(Box::new(move || seen.lock().unwrap().iter().any(|m| m["id"] == 3))).await;
            to_next.send(message(init_reply)).unwrap();
            to_local.send(message(call(5))).unwrap();
            let log = resumed.clone();
            wait_until(Box::new(move || log.lock().unwrap().len() == 3)).await;
            // Call 5 waits on the new connection; the given-up send fails now.
            let _ = release.send(());
            tokio::time::sleep(Duration::from_millis(50)).await;
            to_next.send(message(serde_json::json!({"jsonrpc":"2.0","id":5,"result":{"content":[],"isError":false}}))).unwrap();
            let seen = written.clone();
            wait_until(Box::new(move || seen.lock().unwrap().iter().any(|m| m["id"] == 5))).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            let answers: Vec<_> = written.lock().unwrap().iter().filter(|m| m["id"] == 5).cloned().collect();
            assert_eq!(answers, [serde_json::json!({"jsonrpc":"2.0","id":5,"result":{"content":[],"isError":false}})]);
            assert_eq!(*resumed.lock().unwrap(), ["initialize", "notifications/initialized", "tools/call"]);
            drop(to_local);
            task.await.unwrap().unwrap();
        });
    }
}
