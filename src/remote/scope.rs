//! What a read-only client token may do: read and search memories, never
//! change them. Each tool is classified here, with the inputs that make a
//! call of it change memories; a tool or input added later fails the tests
//! until it is classified.

use super::gate::Access;
use crate::registry::Scope;
use axum::http::{HeaderMap, header};

/// Whether a tool needs a token with write access.
#[derive(Clone, Copy, Debug)]
pub enum Need {
    /// It reads or searches only, whatever its inputs. A search may save
    /// vectors for the server's own index, and `link` records which
    /// projects a conversation loaded, which the session hooks need: neither
    /// changes a memory.
    Read,
    /// It changes memories, whatever its inputs.
    Write,
    /// It changes memories when one of these inputs is set (present and not
    /// null, false, "" or []).
    WriteWith(&'static [&'static str]),
}

/// Every tool the server offers, with what it needs. `import` is refused
/// even as a preview, which holds the store's write lock while it tries
/// replacements.
pub const TOOLS: &[(&str, Need)] = &[
    ("record", Need::Write),
    ("update", Need::Write),
    ("forget", Need::Write),
    ("import", Need::Write),
    ("review", Need::WriteWith(&["keep"])),
    ("index", Need::Read),
    ("read", Need::Read),
    ("search", Need::Read),
    // A new export file is written beside the store; replacing one, which
    // may be someone's backup, needs write access.
    ("export", Need::WriteWith(&["overwrite"])),
    ("link", Need::Read),
    // Test builds only: reports the build's version.
    ("debug_version", Need::Read),
];

/// The sentence a read-only token's refused call gets, naming the server
/// as the client reached it.
pub fn read_only(url: &str) -> String {
    format!(
        "This machine's token for {url} is read-only; this needs a token with write access from the server's operator."
    )
}

/// The endpoint as the client named it, from the request's Host header:
/// a client reaches a server over HTTPS, or over HTTP on loopback only
/// (`Connection::validate`), so the scheme follows from the host.
pub fn client_url(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    let name = match host.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    };
    let loopback = name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    let scheme = if loopback { "http" } else { "https" };
    format!("{scheme}://{host}/mcp")
}

/// Whether calling `tool` with `arguments` changes memories. An unknown
/// tool counts as a write, so a tool missing from [`TOOLS`] is refused
/// rather than let through.
pub fn writes(tool: &str, arguments: Option<&serde_json::Map<String, serde_json::Value>>) -> bool {
    match TOOLS.iter().find(|(name, _)| *name == tool) {
        Some((_, Need::Read)) => false,
        Some((_, Need::WriteWith(inputs))) => inputs.iter().any(|input| {
            crate::setup::versions::sets(arguments.and_then(|arguments| arguments.get(*input)))
        }),
        Some((_, Need::Write)) | None => true,
    }
}

/// What a hosted call is told when it arrives without the gate's [`Access`],
/// which only a change in how rmcp hands over the request could cause.
pub const UNCHECKED: &str = "This call reached the memory server without its token check, so it was not run; the server's operator should update user-memory or report it.";

/// The refusal for a tool call on the `hosted` server made with a read-only
/// token, from the request parts rmcp hands each call; `None` when the call
/// may run. A hosted call without the gate's [`Access`] is refused whatever
/// it asks, so a read-only token can never write through it. A local stdio
/// `serve` has no token, so it is never scoped.
pub fn refusal(
    tool: &str,
    arguments: Option<&serde_json::Map<String, serde_json::Value>>,
    extensions: &rmcp::model::Extensions,
    hosted: bool,
) -> Option<String> {
    if !hosted {
        return None;
    }
    let parts = extensions.get::<axum::http::request::Parts>();
    let Some((parts, access)) =
        parts.and_then(|parts| Some((parts, parts.extensions.get::<Access>()?)))
    else {
        return Some(UNCHECKED.to_owned());
    };
    (access.scope == Scope::Read && writes(tool, arguments))
        .then(|| read_only(&client_url(&parts.headers)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_inputs::{inputs, memory_structs, tool_structs};
    use serde_json::json;
    use std::collections::BTreeSet;

    /// The inputs of each tool a read-only token may call, checked to change
    /// no memory: a new input of such a tool fails the tests until it is
    /// listed here or makes the tool `WriteWith` it.
    const READ_INPUTS: &[(&str, &[&str])] = &[
        (
            "index",
            &[
                "category",
                "cursor",
                "fit_most_used_room",
                "fit_room",
                "fit_rules_room",
                "fit_unit",
                "fit_whole_projects",
                "instructions",
                "instructions_max_bytes",
                "limit",
                "origin",
                "origin_links",
                "page_size",
                "projects",
                "rule_prints",
                "scope",
                "source",
                "stamps",
                "stamps_since",
                "status",
                "topic",
                "type",
                "without_global_rules",
            ],
        ),
        (
            "read",
            &[
                "category",
                "cursor",
                "history",
                "ids",
                "limit",
                "origin",
                "page_size",
                "projects",
                "scope",
                "source",
                "status",
                "type",
            ],
        ),
        (
            "search",
            &[
                "category",
                "cursor",
                "limit",
                "origin",
                "page_size",
                "projects",
                "query",
                "scope",
                "source",
                "status",
                "topic",
                "type",
            ],
        ),
        (
            "export",
            &[
                "category", "name", "origin", "projects", "scope", "source", "status", "type",
            ],
        ),
        (
            "link",
            &[
                "folder_projects",
                "included_projects",
                "origin",
                "projects",
                "unlink",
            ],
        ),
        ("review", &["max_groups", "projects", "user_confirmed"]),
    ];

    /// Every tool the server can offer is in [`TOOLS`], and every input of
    /// one a read-only token may call is in [`READ_INPUTS`] or makes it a
    /// write: read from the routers and from the source, so a new tool or
    /// a new input, hidden from the schema or not, fails here until it is
    /// classified.
    #[test]
    fn every_tool_and_every_input_a_read_only_token_may_send_is_classified() {
        #[cfg_attr(not(debug_assertions), expect(unused_mut))]
        let mut routed = crate::UserMemoryServer::tool_router().list_all();
        #[cfg(debug_assertions)]
        routed.extend(crate::UserMemoryServer::debug_router().list_all());
        let offered: BTreeSet<String> = routed.iter().map(|tool| tool.name.to_string()).collect();
        // The test-only tool is offered by debug builds alone.
        let classified: BTreeSet<String> = TOOLS
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .filter(|name| cfg!(debug_assertions) || name != "debug_version")
            .collect();
        assert_eq!(
            offered, classified,
            "a tool is offered but not classified, or the reverse"
        );
        let structs = memory_structs();
        let tools = tool_structs();
        let mut wrong = Vec::new();
        for (tool, need) in TOOLS {
            let triggers: &[&str] = match need {
                Need::Write => continue,
                Need::Read => &[],
                Need::WriteWith(inputs) => inputs,
            };
            let Some(name) = tools.get(*tool) else {
                // A tool without parameters has no input to classify.
                assert!(
                    !READ_INPUTS.iter().any(|(listed, _)| listed == tool),
                    "{tool} takes no input"
                );
                continue;
            };
            let read_inputs: BTreeSet<&str> = READ_INPUTS
                .iter()
                .find(|(listed, _)| listed == tool)
                .map(|(_, inputs)| inputs.iter().copied().collect())
                .unwrap_or_default();
            let found = inputs(&structs, name);
            let found: BTreeSet<&str> = found.iter().map(String::as_str).collect();
            let listed: BTreeSet<&str> = read_inputs.iter().chain(triggers).copied().collect();
            if found != listed {
                wrong.push(format!("{tool}: has {found:?}, classified {listed:?}"));
            }
        }
        assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
    }

    #[test]
    fn the_url_is_the_one_the_client_used() {
        for (host, url) in [
            ("memory.example.com", "https://memory.example.com/mcp"),
            (
                "memory.example.com:8443",
                "https://memory.example.com:8443/mcp",
            ),
            ("127.0.0.1:8765", "http://127.0.0.1:8765/mcp"),
            ("localhost:8765", "http://localhost:8765/mcp"),
            ("[::1]:8765", "http://[::1]:8765/mcp"),
            ("10.0.0.5:8765", "https://10.0.0.5:8765/mcp"),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, host.parse().unwrap());
            assert_eq!(client_url(&headers), url, "{host}");
        }
    }

    /// A hosted call is refused without the request parts or the gate's
    /// [`Access`], even a read; a local `serve`'s calls are never scoped.
    #[test]
    fn a_hosted_call_without_its_token_check_is_refused() {
        let args = json!({}).as_object().cloned();
        let empty = rmcp::model::Extensions::new();
        for tool in ["record", "index"] {
            assert_eq!(
                refusal(tool, args.as_ref(), &empty, true).as_deref(),
                Some(UNCHECKED),
                "{tool}"
            );
            assert_eq!(refusal(tool, args.as_ref(), &empty, false), None, "{tool}");
        }
        let (parts, ()) = axum::http::Request::new(()).into_parts();
        let mut without_access = rmcp::model::Extensions::new();
        without_access.insert(parts);
        assert_eq!(
            refusal("record", args.as_ref(), &without_access, true).as_deref(),
            Some(UNCHECKED)
        );
        let (mut parts, ()) = axum::http::Request::new(()).into_parts();
        parts.extensions.insert(Access {
            scope: Scope::Read,
            hash: None,
        });
        let mut read = rmcp::model::Extensions::new();
        read.insert(parts);
        assert!(
            refusal("record", args.as_ref(), &read, true)
                .is_some_and(|said| said.contains("read-only"))
        );
        assert_eq!(refusal("index", args.as_ref(), &read, true), None);
    }

    #[test]
    fn writes_follow_the_table() {
        let args = |value: serde_json::Value| value.as_object().cloned();
        for tool in ["record", "update", "forget", "import", "no_such_tool"] {
            assert!(writes(tool, None), "{tool}");
        }
        for tool in ["index", "read", "search", "export", "link", "debug_version"] {
            assert!(
                !writes(tool, args(json!({"keep": ["x"]})).as_ref()),
                "{tool}"
            );
        }
        assert!(writes(
            "export",
            args(json!({"name": "b.json", "overwrite": true})).as_ref()
        ));
        assert!(writes(
            "review",
            args(json!({"keep": ["0123abcd"]})).as_ref()
        ));
        for unset in [json!({}), json!({"keep": []}), json!({"keep": null})] {
            assert!(!writes("review", args(unset).as_ref()));
        }
    }
}
