//! Authenticated HTTP host and protocol connector. Memory stays on the host.

use crate::private;
use anyhow::{Context, Result, bail};
use axum::{
    Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use clap::Args;
use rmcp::{
    RoleClient, RoleServer, ServiceExt,
    transport::{
        StreamableHttpClientTransport, Transport, stdio,
        streamable_http_client::StreamableHttpClientTransportConfig,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    },
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};
use subtle::ConstantTimeEq;

mod transfer;

#[derive(Clone, Args, Serialize, Deserialize)]
pub struct Connection {
    /// Remote MCP endpoint. HTTPS required except for loopback development.
    #[arg(long)]
    pub url: String,
    /// File containing the server access token; never stored in client config.
    #[arg(long)]
    pub token_file: PathBuf,
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
    #[arg(long)]
    token_file: PathBuf,
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
        let config = StreamableHttpClientTransportConfig::with_uri(self.url.clone())
            .auth_header(read_token(&self.token_file)?);
        Ok(StreamableHttpClientTransport::with_client(client, config))
    }
    pub async fn probe(&self) -> Result<()> {
        let service = tokio::time::timeout(Duration::from_secs(10), ().serve(self.transport()?))
            .await
            .context("Memory server connection timed out")?
            .context("Cannot connect to memory server; check endpoint and access token")?;
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

    pub async fn call(&self, name: &'static str, arguments: serde_json::Value) -> Result<String> {
        let service = tokio::time::timeout(Duration::from_secs(10), ().serve(self.transport()?))
            .await
            .context("Memory server connection timed out")?
            .context("Cannot connect to memory server")?;
        let request = rmcp::model::CallToolRequestParams::new(name)
            .with_arguments(arguments.as_object().cloned().unwrap_or_default());
        let result =
            tokio::time::timeout(Duration::from_secs(10), service.call_tool(request)).await;
        let _ = service.cancel().await;
        let result = serde_json::to_value(result.context("Memory server request timed out")??)?;
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

fn host_token(path: &Path) -> Result<String> {
    if !path.is_absolute() {
        bail!("Token file must use an absolute path");
    }
    if path.exists() {
        return read_token(path);
    }
    fs::create_dir_all(path.parent().context("Invalid token-file path")?)?;
    let bytes: [u8; 32] = rand::random();
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let mut file = private::create_file(fs::OpenOptions::new().write(true).create_new(true), path)
        .context("Cannot create server access-token file")?;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    Ok(token)
}

async fn authenticate(State(token): State<String>, request: Request, next: Next) -> Response {
    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !bool::from(provided.as_bytes().ct_eq(token.as_bytes())) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "Authentication required",
        )
            .into_response();
    }
    next.run(request).await
}

pub async fn host(options: HostOptions) -> Result<()> {
    let db = options.db.unwrap_or_else(crate::db_path);
    if !db.is_absolute() {
        bail!("Server database must use an absolute path");
    }
    let token = host_token(&options.token_file)?;
    crate::connect(&db)?;
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
    let transfers = transfer::routes(db.clone(), allowed);
    let service: StreamableHttpService<crate::UserMemoryServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || {
                Ok(crate::UserMemoryServer {
                    db: db.clone(),
                    in_client_folder: false,
                })
            },
            Default::default(),
            config,
        );
    // /mcp and the export and import endpoints need the token. `/` names
    // the service for health checks and reverse-proxy smoke tests and reads
    // nothing; other paths are 404.
    let router = Router::new()
        .nest_service("/mcp", service)
        .merge(transfers)
        .layer(middleware::from_fn_with_state(token, authenticate))
        .route(
            "/",
            get(|| async { "user-memory MCP server. Endpoint: /mcp\n" }),
        )
        .fallback(|| async { StatusCode::NOT_FOUND });
    println!("Listening: http://{address}/mcp");
    eprintln!("Access-token file: {}", options.token_file.display());
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown.cancel();
        })
        .await?;
    Ok(())
}

pub async fn connect(connection: Connection) -> Result<()> {
    let remote = connection.transport()?;
    let (input, output) = stdio();
    let local = rmcp::transport::async_rw::AsyncRwTransport::new_server(input, output);
    forward(local, remote).await
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

/// The id a successful response answers, if `message` is one.
fn answered(message: &impl serde::Serialize) -> Option<serde_json::Value> {
    let message = serde_json::to_value(message).ok()?;
    message.get("result")?;
    message.get("id").cloned()
}

/// Relays messages between a local MCP client and the memory server. After
/// the server answers a write, the instruction files on this machine are
/// brought in line with it, beside the relay.
async fn forward(
    mut local: impl Transport<RoleServer>,
    mut remote: impl Transport<RoleClient>,
) -> Result<()> {
    let mut sends = tokio::task::JoinSet::<Result<()>>::new();
    let mut syncs = tokio::task::JoinSet::new();
    let mut writes = Vec::new();
    let mut error = None;
    loop {
        tokio::select! {
            message = Transport::<RoleServer>::receive(&mut local) => {
                let Some(message) = message else { break; };
                let message = with_folder_projects(message);
                writes.extend(write_call(&message));
                let send = Transport::<RoleClient>::send(&mut remote, message);
                start_in_order(&mut sends, async move { send.await.context("Memory server request failed") });
            }
            message = Transport::<RoleClient>::receive(&mut remote) => {
                let Some(message) = message else { error = Some(anyhow::anyhow!("Memory server connection closed")); break; };
                if let Some(id) = answered(&message)
                    && let Some(at) = writes.iter().position(|write| *write == id)
                {
                    writes.swap_remove(at);
                    syncs.spawn(crate::setup::sync_bounded());
                }
                let send = Transport::<RoleServer>::send(&mut local, message);
                start_in_order(&mut sends, async move { send.await.context("Cannot send memory server response to client") });
            }
            result = sends.join_next(), if !sends.is_empty() => {
                match result.context("Connector send task disappeared").and_then(|r| r.context("Connector send task failed")).and_then(|r| r) {
                    Ok(()) => {},
                    Err(err) => { error = Some(err); break; }
                }
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let task = runtime.spawn(forward(local, remote));
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
}
