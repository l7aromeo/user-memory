//! Release numbers, and the Claude Code release user-memory's Windows hooks
//! need: setup and doctor read `claude --version` there and say when it is
//! older, or when its output cannot be read.

use std::fmt;

/// A release number `X.Y.Z`, compared part by part.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    /// The version `text` spells, exactly `X.Y.Z`; anything else, such as a
    /// pre-release or a missing part, is `None`.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.').map(|part| part.parse::<u64>().ok());
        let version = Self {
            major: parts.next()??,
            minor: parts.next()??,
            patch: parts.next()??,
        };
        parts.next().is_none().then_some(version)
    }
}

/// Names, in debug builds only, the version this build reports in place
/// of its own: the tests' "newer build" is this binary run with it set.
#[cfg(debug_assertions)]
pub(crate) const TEST_VERSION: &str = "USER_MEMORY_TEST_VERSION";

/// The version this build reports: its own, or in a debug build the one
/// [`TEST_VERSION`] names.
pub(crate) fn running() -> &'static str {
    #[cfg(debug_assertions)]
    {
        static OVERRIDE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        if let Some(version) = OVERRIDE
            .get_or_init(|| std::env::var(TEST_VERSION).ok().filter(|v| !v.is_empty()))
            .as_deref()
        {
            return version;
        }
    }
    env!("CARGO_PKG_VERSION")
}

/// Whether this debug build reports a test version in place of its own,
/// which adds the test-only `debug_version` tool.
#[cfg(debug_assertions)]
pub(crate) fn test_build() -> bool {
    running() != env!("CARGO_PKG_VERSION")
}

/// The first release whose `serve` hands itself over to a replaced binary
/// (#98): sessions served by an earlier one keep its code until their
/// client restarts, so setup still tells the user to restart them.
pub(crate) const HANDOVER_SINCE: Version = Version {
    major: 0,
    minor: 10,
    patch: 0,
};

/// The oldest client this server supports, which the handshake names so a
/// client can say when it must be upgraded. Raised only by a release whose
/// server needs something older clients don't send: none since 0.8.0, the
/// first that sends the 8-character memory ids this server reads. Only a
/// client from 0.10.0 on reads it, so the floor first matters for a release
/// after 0.10.0.
pub(crate) const MIN_CLIENT: Version = Version {
    major: 0,
    minor: 8,
    patch: 0,
};

/// The oldest server this client supports, compared with the version the
/// server's handshake names. Raised only by a release whose client needs
/// something older servers lack: none since 0.8.0, the first server that
/// reads 8-character memory ids, which this client sends.
pub(crate) const MIN_SERVER: Version = Version {
    major: 0,
    minor: 8,
    patch: 0,
};

/// Where the handshake carries user-memory's own fields: MCP's extension
/// point, `capabilities.experimental`, under this key. `serverInfo` holds
/// only a name and a version.
pub(crate) const HANDSHAKE_KEY: &str = "user-memory";

/// The `capabilities.experimental` a server sends in its handshake: the
/// oldest client it supports.
pub(crate) fn handshake_capabilities() -> rmcp::model::ExperimentalCapabilities {
    let mut fields = rmcp::model::JsonObject::new();
    fields.insert("min_client".into(), MIN_CLIENT.to_string().into());
    [(HANDSHAKE_KEY.to_owned(), fields)].into_iter().collect()
}

/// What a client says about its server's handshake: one line when either
/// side is below the other's oldest supported version, else nothing.
/// `result` is the server's `initialize` result; a server without the
/// `min_client` field has no minimum.
pub(crate) fn server_line(this: &str, url: &str, result: &serde_json::Value) -> Option<String> {
    let this = Version::parse(this)?;
    let (shown, server, min_client) = said(result);
    if let Some(min) = min_client
        && this < min
    {
        return Some(format!(
            "The memory server at {url} runs user-memory {shown} and needs {min} or newer on this machine ({this}). Upgrade: user-memory upgrade"
        ));
    }
    if let Some(server) = server
        && server < MIN_SERVER
    {
        return Some(format!(
            "This machine runs user-memory {this}, which needs the memory server at {url} to run {MIN_SERVER} or newer (it runs {server}). Upgrade the server on its machine."
        ));
    }
    None
}

/// What doctor says about the versions of a client and its server.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ServerStep {
    /// One side is below the other's oldest supported version.
    Action(String),
    /// The versions differ, and both still work together.
    Note(String),
}

/// doctor's line on this client's version against its server's: the
/// session-start line as a step, else a note where the versions differ.
pub(crate) fn server_step(this: &str, url: &str, result: &serde_json::Value) -> Option<ServerStep> {
    if let Some(line) = server_line(this, url, result) {
        return Some(ServerStep::Action(line));
    }
    let this = Version::parse(this)?;
    let (_, server, _) = said(result);
    let server = server?;
    match server.cmp(&this) {
        std::cmp::Ordering::Greater => Some(ServerStep::Note(format!(
            "Memory server version: the memory server at {url} runs {server}, newer than this machine ({this}); run \"user-memory upgrade\" to match it"
        ))),
        std::cmp::Ordering::Less => Some(ServerStep::Note(format!(
            "Memory server version: the memory server at {url} runs {server}, older than this machine ({this}); it can be upgraded on its own machine"
        ))),
        std::cmp::Ordering::Equal => None,
    }
}

/// Where a `server/discover` result names the server: in `_meta`, as a JSON
/// pointer (specification 2026-07-28, server/discover). An `initialize`
/// result has it as `serverInfo`.
const DISCOVER_SERVER_INFO: &str = "/_meta/io.modelcontextprotocol~1serverInfo";

/// The version a handshake result names its server by: an `initialize`
/// result's, or a `server/discover` result's.
pub(crate) fn server_version(result: &serde_json::Value) -> Option<&serde_json::Value> {
    result
        .pointer("/serverInfo/version")
        .or_else(|| result.pointer(&format!("{DISCOVER_SERVER_INFO}/version")))
}

/// A tool input that a server reads only from the release `since` on. An
/// older server ignores an input it doesn't know and answers as though it
/// had not been given, so a request that sets it is refused instead.
pub(crate) struct OptionFloor {
    pub(crate) tool: &'static str,
    pub(crate) field: &'static str,
    pub(crate) since: Version,
    /// What a server before `since` lacks, as the refusal says it after
    /// "which".
    pub(crate) lacks: &'static str,
    /// What the bridge adds for the agent: that nothing was done, and what
    /// the request without the input would and would not say.
    pub(crate) for_agent: &'static str,
}

/// Every tool input newer than [`MIN_SERVER`] that an older server's
/// silence would make misleading, with the release that adds it. The CLI
/// and the bridge both refuse by this table. A newer input an older server
/// may ignore without misleading anyone goes in `IGNORED_SAFELY` beside the
/// tests instead, with its reason; a test fails for an input in neither.
pub(crate) const OPTION_FLOORS: &[OptionFloor] = &[OptionFloor {
    tool: "read",
    field: "history",
    since: Version {
        major: 0,
        minor: 10,
        patch: 0,
    },
    lacks: "keeps no history",
    for_agent: "Nothing was read. Tell the user this memory server is too old to show history. A read without history still returns the memory, but says nothing about whether it changed.",
}];

/// The floor this build applies for an input from release `since`: that
/// release, or this build's own version when it is lower. A tree reports
/// the release before until the version is bumped for the release that
/// adds the input, and a server of this tree has every input its client
/// sends; a released build's floors are never above its own version.
fn applied_floor(since: Version) -> Version {
    Version::parse(running()).map_or(since, |own| since.min(own))
}

/// Whether a request sets an input: present and not null, false, "" or [].
/// This is the rule for today's row (`history: true`); a future input
/// whose meaningful value is false or empty needs its own test in its row.
fn sets(value: Option<&serde_json::Value>) -> bool {
    match value {
        None | Some(serde_json::Value::Null | serde_json::Value::Bool(false)) => false,
        Some(serde_json::Value::String(text)) => !text.is_empty(),
        Some(serde_json::Value::Array(items)) => !items.is_empty(),
        Some(_) => true,
    }
}

/// Why a call of `tool` with `arguments` is not sent to the server at
/// `url`, whose handshake named `server`: the sentence the CLI prints, and
/// the floor's row, whose `for_agent` the bridge adds. `None` when the
/// server is new enough, or its version is not a release.
pub(crate) fn refused_option(
    url: &str,
    server: &str,
    tool: &str,
    arguments: &serde_json::Value,
) -> Option<(String, &'static OptionFloor)> {
    let version = Version::parse(server)?;
    let floor = OPTION_FLOORS.iter().find(|floor| {
        floor.tool == tool
            && sets(arguments.get(floor.field))
            && version < applied_floor(floor.since)
    })?;
    Some((
        format!(
            "The memory server at {url} runs user-memory {server}, which {} ({} needs {} or newer). Upgrade the server on its machine.",
            floor.lacks, floor.field, floor.since
        ),
        floor,
    ))
}

/// The server's version as it names it, as a release when it is one, and
/// the oldest client it supports, from its `initialize` or
/// `server/discover` result.
fn said(result: &serde_json::Value) -> (String, Option<Version>, Option<Version>) {
    let shown = server_version(result)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("an unknown version")
        .to_owned();
    let server = Version::parse(&shown);
    let min_client = result
        .pointer("/capabilities/experimental")
        .and_then(|fields| fields.get(HANDSHAKE_KEY))
        .and_then(|fields| fields.get("min_client"))
        .and_then(serde_json::Value::as_str)
        .and_then(Version::parse);
    (shown, server, min_client)
}

/// The version in `user-memory --version`'s output (`user-memory X.Y.Z`),
/// or `None` when the output is anything else.
pub(crate) fn of_binary(output: &str) -> Option<Version> {
    let line = output.lines().map(clean).find(|line| !line.is_empty())?;
    Version::parse(line.strip_prefix("user-memory ")?)
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The oldest Claude Code that runs exec-form hooks (a `command` and an
/// `args` array, with no shell), which setup writes on Windows: there the
/// shell form runs under Git Bash, which strips the backslashes from the
/// hook's path.
pub(crate) const MIN_WINDOWS_CLAUDE: Version = Version {
    major: 2,
    minor: 1,
    patch: 139,
};

/// What follows the version on `claude --version`'s line, which tells it
/// from a warning line or another program's output.
const CLAUDE_MARKER: &str = "Claude Code";

/// How many characters of unreadable output the note quotes: enough to
/// recognise it, short enough for one line.
const QUOTED_CHARS: usize = 80;

/// The version in `claude --version`'s output (`2.1.139 (Claude Code)`):
/// the first line whose first word is `X.Y.Z`, with or without a leading
/// `v`, and which names Claude Code. `None` when no line does.
pub(crate) fn claude(output: &str) -> Option<Version> {
    output.lines().map(clean).find_map(|line| {
        let (first, rest) = line.split_once(char::is_whitespace)?;
        let first = first.strip_prefix('v').unwrap_or(first);
        rest.contains(CLAUDE_MARKER)
            .then(|| Version::parse(first))
            .flatten()
    })
}

/// What setup and doctor say about Claude Code's version.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClaudeStep {
    /// Claude Code is too old: a step printed under "Action needed:".
    Action(String),
    /// The version cannot be read, so it may be too old: a line printed as
    /// is, never taken as new enough.
    Note(String),
}

/// What to say about the Claude Code whose `claude --version` printed
/// `output` (empty when it could not run); `None` when it is new enough,
/// and always off Windows, where the hooks need no particular release.
pub(crate) fn claude_step(output: &str, windows: bool) -> Option<ClaudeStep> {
    if !windows {
        return None;
    }
    match claude(output) {
        Some(version) if version >= MIN_WINDOWS_CLAUDE => None,
        Some(version) => Some(ClaudeStep::Action(format!(
            "Claude Code {version} is older than {MIN_WINDOWS_CLAUDE}, which runs user-memory's Windows hooks; run `claude update`"
        ))),
        None => {
            let printed = output
                .lines()
                .map(clean)
                .find(|line| !line.is_empty())
                .map(|line| format!(" (it printed: {})", quote(line)))
                .unwrap_or_default();
            Some(ClaudeStep::Note(format!(
                "Cannot read Claude Code's version from `claude --version`{printed}; user-memory's Windows hooks need Claude Code {MIN_WINDOWS_CLAUDE} or later: run `claude update` if it is older"
            )))
        }
    }
}

/// One line of output without a byte order mark or surrounding spaces.
fn clean(line: &str) -> &str {
    line.trim_start_matches('\u{feff}').trim()
}

/// `line` as the note quotes it: without control characters, which could
/// move the cursor or change colours, and cut at [`QUOTED_CHARS`].
fn quote(line: &str) -> String {
    let mut chars = line.chars().filter(|c| !c.is_control());
    let mut quoted: String = chars.by_ref().take(QUOTED_CHARS).collect();
    if chars.next().is_some() {
        quoted.push('…');
    }
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    /// Every tool's inputs in v0.8.0 ([`MIN_SERVER`]), as its source names
    /// them, flattened parts and aliases included: what every supported
    /// server reads.
    const AT_MIN_SERVER: &[(&str, &[&str])] = &[
        (
            "record",
            &[
                "type",
                "category",
                "title",
                "content",
                "context",
                "reason",
                "scope",
                "project",
                "source",
                "evidence",
                "evidence_ids",
                "origin",
                "supersedes",
                "merge",
                "confirms",
                "distinct_from",
                "user_confirmed",
            ],
        ),
        (
            "search",
            &[
                "topic",
                "query",
                "projects",
                "scope",
                "category",
                "type",
                "source",
                "status",
                "origin",
                "cursor",
                "page_size",
                "limit",
            ],
        ),
        (
            "read",
            &[
                "ids",
                "projects",
                "scope",
                "category",
                "type",
                "source",
                "status",
                "origin",
                "cursor",
                "page_size",
                "limit",
            ],
        ),
        (
            "review",
            &["projects", "max_groups", "keep", "user_confirmed"],
        ),
        (
            "export",
            &[
                "name",
                "overwrite",
                "projects",
                "scope",
                "category",
                "type",
                "source",
                "status",
                "origin",
            ],
        ),
        (
            "import",
            &[
                "name",
                "apply",
                "on_conflict",
                "user_confirmed",
                "titles",
                "projects",
                "scope",
                "category",
                "type",
                "source",
                "status",
                "origin",
            ],
        ),
        (
            "index",
            &[
                "projects",
                "scope",
                "category",
                "type",
                "source",
                "status",
                "origin",
                "topic",
                "cursor",
                "page_size",
                "limit",
                "instructions",
                "instructions_max_bytes",
                "without_global_rules",
                "stamps",
                "origin_links",
            ],
        ),
        (
            "link",
            &[
                "origin",
                "projects",
                "unlink",
                "folder_projects",
                "included_projects",
            ],
        ),
        (
            "update",
            &[
                "memory_id",
                "type",
                "category",
                "title",
                "content",
                "context",
                "reason",
                "evidence",
                "scope",
                "project",
                "origin",
                "replace_origins",
                "distinct_from",
                "user_confirmed",
            ],
        ),
        ("forget", &["memory_id", "origin", "user_confirmed"]),
    ];

    /// Inputs newer than [`MIN_SERVER`] that an older server may ignore
    /// without misleading anyone: the tool, the input and why.
    const IGNORED_SAFELY: &[(&str, &str, &str)] = &[
        (
            "update",
            "drop_history",
            "it asks that the replaced value be kept nowhere; a server from before history keeps no history, and its correction removes the replaced text from the store's files the same way, so the store ends as the input asks (backups made before keep it on either version)",
        ),
        (
            "index",
            "fit_room",
            "only user-memory's own session hook sends it, to have the index fitted to its room; a server from before it sends a page of the index with a cursor, which the hook reads page by page and fits itself, as it did before",
        ),
        (
            "index",
            "fit_unit",
            "it comes with fit_room, naming the unit the room is counted in; a server from before it fits nothing, and the hook fits the pages it sends in that unit itself",
        ),
        (
            "index",
            "fit_whole_projects",
            "it comes with fit_room, naming the projects that come first; a server from before it fits nothing, and the hook fits the pages itself, the global memories and those projects first, as it did before",
        ),
        (
            "index",
            "stamps_since",
            "only user-memory's own session hook sends it, with the position from its last stamps reply; a server from before it sends the v1 stamps reply, which the hook reads as a full list, as it did before",
        ),
    ];

    /// The source of user-memory's own files, parsed.
    fn parsed(path: &str) -> syn::File {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
        let text = std::fs::read_to_string(&path).unwrap();
        syn::parse_file(&text).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
    }

    /// Every struct in src/memory.rs and src/memory/, by name.
    fn memory_structs() -> BTreeMap<String, syn::ItemStruct> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/memory");
        let mut files = vec!["src/memory.rs".to_owned()];
        for entry in std::fs::read_dir(dir).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            if name.ends_with(".rs") {
                files.push(format!("src/memory/{name}"));
            }
        }
        files
            .iter()
            .flat_map(|file| parsed(file).items)
            .filter_map(|item| match item {
                syn::Item::Struct(item) => Some((item.ident.to_string(), item)),
                _ => None,
            })
            .collect()
    }

    /// The name of the last segment of `ty`'s path: `memory::Read` is `Read`.
    fn type_name(ty: &syn::Type) -> Option<String> {
        match ty {
            syn::Type::Path(path) => path.path.segments.last().map(|s| s.ident.to_string()),
            _ => None,
        }
    }

    /// Each `#[tool]` in src/main.rs with the struct of its `Parameters<…>`.
    fn tool_structs() -> BTreeMap<String, String> {
        let mut tools = BTreeMap::new();
        for item in parsed("src/main.rs").items {
            let syn::Item::Impl(block) = item else {
                continue;
            };
            for item in block.items {
                let syn::ImplItem::Fn(function) = item else {
                    continue;
                };
                if !function
                    .attrs
                    .iter()
                    .any(|attr| attr.path().is_ident("tool"))
                {
                    continue;
                }
                for input in &function.sig.inputs {
                    let syn::FnArg::Typed(input) = input else {
                        continue;
                    };
                    let syn::Type::Path(path) = &*input.ty else {
                        continue;
                    };
                    let Some(last) = path.path.segments.last() else {
                        continue;
                    };
                    if last.ident != "Parameters" {
                        continue;
                    }
                    let syn::PathArguments::AngleBracketed(arguments) = &last.arguments else {
                        continue;
                    };
                    if let Some(syn::GenericArgument::Type(ty)) = arguments.args.first()
                        && let Some(name) = type_name(ty)
                    {
                        tools.insert(function.sig.ident.to_string(), name);
                    }
                }
            }
        }
        tools
    }

    /// The input names a server reads for `name`'s fields: each field's
    /// serde name and aliases, with flattened structs' fields in its place
    /// and skipped fields left out. Fields hidden from the schema count.
    fn inputs(structs: &BTreeMap<String, syn::ItemStruct>, name: &str) -> BTreeSet<String> {
        use syn::ext::IdentExt;
        let item = structs
            .get(name)
            .unwrap_or_else(|| panic!("no struct {name} in src/memory"));
        let mut names = BTreeSet::new();
        for field in &item.fields {
            let (mut renamed, mut aliases, mut flatten, mut skip) =
                (None, Vec::new(), false, false);
            for attr in field
                .attrs
                .iter()
                .filter(|attr| attr.path().is_ident("serde"))
            {
                attr.parse_nested_meta(|meta| {
                    if meta.path.is_ident("rename") {
                        renamed = Some(meta.value()?.parse::<syn::LitStr>()?.value());
                    } else if meta.path.is_ident("alias") {
                        aliases.push(meta.value()?.parse::<syn::LitStr>()?.value());
                    } else if meta.path.is_ident("flatten") {
                        flatten = true;
                    } else if meta.path.is_ident("skip") || meta.path.is_ident("skip_deserializing")
                    {
                        skip = true;
                    } else if meta.input.peek(syn::Token![=]) {
                        meta.value()?.parse::<syn::Expr>()?;
                    }
                    Ok(())
                })
                .unwrap();
            }
            if skip {
                continue;
            }
            if flatten {
                let inner = type_name(&field.ty).expect("a flattened field names its struct");
                names.extend(inputs(structs, &inner));
                continue;
            }
            let ident = field.ident.as_ref().expect("tool inputs have named fields");
            names.insert(renamed.unwrap_or_else(|| ident.unraw().to_string()));
            names.extend(aliases);
        }
        names
    }

    /// Every tool input a server reads that v0.8.0 did not has a floor or a
    /// reason it is safe to ignore, read from the source so inputs hidden
    /// from the schema count too; and every floor and reason names a real
    /// input that is newer.
    #[test]
    fn every_input_newer_than_the_oldest_server_has_a_floor_or_a_reason() {
        let structs = memory_structs();
        let tools = tool_structs();
        let old: BTreeMap<&str, BTreeSet<&str>> = AT_MIN_SERVER
            .iter()
            .map(|(tool, fields)| (*tool, fields.iter().copied().collect()))
            .collect();
        let mut newer = BTreeSet::new();
        for (tool, name) in &tools {
            let known = old
                .get(tool.as_str())
                .unwrap_or_else(|| panic!("{tool} is newer than {MIN_SERVER}: give it a floor"));
            for input in inputs(&structs, name) {
                if !known.contains(input.as_str()) {
                    newer.insert((tool.clone(), input));
                }
            }
        }
        let covered: BTreeSet<(String, String)> = OPTION_FLOORS
            .iter()
            .map(|floor| (floor.tool.to_owned(), floor.field.to_owned()))
            .chain(
                IGNORED_SAFELY
                    .iter()
                    .map(|(tool, field, _)| ((*tool).to_owned(), (*field).to_owned())),
            )
            .collect();
        assert_eq!(
            newer, covered,
            "inputs newer than {MIN_SERVER} vs floors and reasons"
        );
        assert_eq!(tools.len(), AT_MIN_SERVER.len(), "{tools:?}");
    }

    /// The floor applied is the input's release, or this build's own version
    /// when that is lower, as in a tree not yet bumped for that release.
    #[test]
    fn a_floor_above_this_build_is_applied_as_this_build() {
        let own = version(running());
        assert_eq!(applied_floor(version("0.8.0")), version("0.8.0").min(own));
        let above = Version {
            major: own.major,
            minor: own.minor + 1,
            patch: 0,
        };
        assert_eq!(applied_floor(above), own);
    }

    /// Only a request that sets a floored input is refused, only by a server
    /// below the floor, with the sentence the CLI prints; one whose version
    /// isn't a release is not.
    #[test]
    fn only_a_set_input_is_refused_by_a_server_below_its_floor() {
        let url = "https://memory.example.com/mcp";
        let floor = &OPTION_FLOORS[0];
        let applied = applied_floor(floor.since);
        let below = Version {
            patch: 0,
            minor: applied.minor.saturating_sub(1),
            major: applied.major,
        }
        .to_string();
        let set = serde_json::json!({"ids": ["c0ffee01"], floor.field: true});
        let (sentence, refused) = refused_option(url, &below, floor.tool, &set).unwrap();
        assert_eq!(
            sentence,
            format!(
                "The memory server at {url} runs user-memory {below}, which {} ({} needs {} or newer). Upgrade the server on its machine.",
                floor.lacks, floor.field, floor.since
            )
        );
        assert_eq!(refused.field, floor.field);
        for unset in [
            serde_json::json!({}),
            serde_json::json!({floor.field: false}),
            serde_json::json!({floor.field: null}),
        ] {
            assert!(
                refused_option(url, &below, floor.tool, &unset).is_none(),
                "{unset}"
            );
        }
        assert!(refused_option(url, &below, "search", &set).is_none());
        assert!(refused_option(url, &applied.to_string(), floor.tool, &set).is_none());
        assert!(refused_option(url, "an unknown version", floor.tool, &set).is_none());
    }

    #[test]
    fn versions_compare_by_number_not_by_text() {
        assert!(version("2.1.99") < version("2.1.139"));
        assert!(version("2.9.200") < version("2.10.0"));
        assert!(version("2.1.139") < version("3.0.0"));
        assert_eq!(version("2.1.139"), MIN_WINDOWS_CLAUDE);
        assert_eq!(MIN_WINDOWS_CLAUDE.to_string(), "2.1.139");
    }

    fn handshake(version: &str, min_client: Option<&str>) -> serde_json::Value {
        let mut result = serde_json::json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"user-memory","version":version}});
        if let Some(min) = min_client {
            result["capabilities"]["experimental"] =
                serde_json::json!({HANDSHAKE_KEY: {"min_client": min}});
        }
        result
    }

    const URL: &str = "https://memory.example.com/mcp";

    /// Each row of #100's table: only a version below the other side's
    /// floor says something at session start; doctor notes any gap.
    #[test]
    fn a_client_says_when_either_side_is_below_the_others_floor() {
        let server = handshake("0.11.2", Some("0.11.0"));
        assert_eq!(
            server_line("0.10.1", URL, &server).as_deref(),
            Some(
                "The memory server at https://memory.example.com/mcp runs user-memory 0.11.2 and needs 0.11.0 or newer on this machine (0.10.1). Upgrade: user-memory upgrade"
            )
        );
        assert_eq!(server_line("0.11.0", URL, &server), None);
        assert_eq!(
            server_step("0.11.0", URL, &server),
            Some(ServerStep::Note("Memory server version: the memory server at https://memory.example.com/mcp runs 0.11.2, newer than this machine (0.11.0); run \"user-memory upgrade\" to match it".into()))
        );
        assert_eq!(server_line("0.12.0", URL, &server), None);
        assert_eq!(
            server_step("0.12.0", URL, &server),
            Some(ServerStep::Note("Memory server version: the memory server at https://memory.example.com/mcp runs 0.11.2, older than this machine (0.12.0); it can be upgraded on its own machine".into()))
        );
        let below = Version {
            patch: MIN_SERVER.patch,
            minor: MIN_SERVER.minor,
            major: MIN_SERVER.major,
        };
        let old = format!("{}.{}.{}", below.major, below.minor.saturating_sub(1), 0);
        assert_eq!(
            server_line("0.12.0", URL, &handshake(&old, None)),
            Some(format!(
                "This machine runs user-memory 0.12.0, which needs the memory server at https://memory.example.com/mcp to run {MIN_SERVER} or newer (it runs {old}). Upgrade the server on its machine."
            ))
        );
        assert!(matches!(
            server_step("0.12.0", URL, &handshake(&old, None)),
            Some(ServerStep::Action(_))
        ));
        assert_eq!(server_step("0.11.2", URL, &server), None);
    }

    /// A server without the field, as every server before it, has no
    /// minimum; one whose version isn't a release is never below a floor.
    #[test]
    fn a_server_without_a_minimum_never_asks_for_a_newer_client() {
        assert_eq!(server_line("0.0.1", URL, &handshake("0.9.2", None)), None);
        assert_eq!(
            server_line("0.0.1", URL, &handshake("0.11.3-dev", Some("bad"))),
            None
        );
        assert_eq!(
            server_line("0.10.0", URL, &handshake("0.10.0", Some("0.10.0"))),
            None
        );
    }

    /// This build's own handshake carries the minimum where clients read it.
    #[test]
    fn the_handshake_carries_the_oldest_client() {
        let fields = handshake_capabilities();
        assert_eq!(
            fields[HANDSHAKE_KEY]["min_client"],
            serde_json::json!(MIN_CLIENT.to_string())
        );
    }

    #[test]
    fn a_binary_version_is_read_from_its_version_line() {
        assert_eq!(of_binary("user-memory 0.10.0\n"), Some(version("0.10.0")));
        assert_eq!(
            of_binary("\u{feff}user-memory 1.2.3\r\n"),
            Some(version("1.2.3"))
        );
        for output in [
            "",
            "user-memory",
            "user-memory 0.10",
            "other 0.10.0",
            "user-memory 0.10.0-dev",
        ] {
            assert_eq!(of_binary(output), None, "{output:?}");
        }
    }

    #[test]
    fn only_three_numbers_make_a_version() {
        for text in [
            "2.1",
            "2.1.139.1",
            "2.1.139-beta.1",
            "2.1.x",
            "",
            "v2.1.139",
        ] {
            assert_eq!(Version::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn claude_code_versions_are_read_from_its_line() {
        for output in [
            "2.1.139 (Claude Code)\n",
            "2.1.139 (Claude Code)\r\n",
            "\u{feff}2.1.139 (Claude Code)",
            "v2.1.139 (Claude Code)",
            "  2.1.139 (Claude Code)  ",
            "Warning: a newer release is available\n2.1.139 (Claude Code)\n",
        ] {
            assert_eq!(claude(output), Some(MIN_WINDOWS_CLAUDE), "{output:?}");
        }
    }

    #[test]
    fn output_without_a_claude_code_version_is_unreadable() {
        for output in [
            "",
            "\n\n",
            "2.1.139",
            "2.1.139 (Other Tool)",
            "Claude Code 2.1.139",
            "2.1 (Claude Code)",
            "2.1.139-beta.1 (Claude Code)",
            "'claude' is not recognized as an internal or external command",
        ] {
            assert_eq!(claude(output), None, "{output:?}");
        }
    }

    #[test]
    fn an_older_claude_code_on_windows_needs_an_update() {
        for (output, shown) in [
            ("2.1.138 (Claude Code)", "2.1.138"),
            ("2.1.99 (Claude Code)", "2.1.99"),
            ("1.0.200 (Claude Code)", "1.0.200"),
        ] {
            assert_eq!(
                claude_step(output, true),
                Some(ClaudeStep::Action(format!(
                    "Claude Code {shown} is older than 2.1.139, which runs user-memory's Windows hooks; run `claude update`"
                )))
            );
        }
    }

    #[test]
    fn the_minimum_or_newer_on_windows_needs_nothing() {
        for output in [
            "2.1.139 (Claude Code)",
            "2.1.140 (Claude Code)",
            "2.2.0 (Claude Code)",
            "3.0.0 (Claude Code)",
        ] {
            assert_eq!(claude_step(output, true), None, "{output:?}");
        }
    }

    #[test]
    fn an_unreadable_version_on_windows_is_a_note_quoting_the_output() {
        assert_eq!(
            claude_step("command not found\n", true),
            Some(ClaudeStep::Note(
                "Cannot read Claude Code's version from `claude --version` (it printed: command not found); user-memory's Windows hooks need Claude Code 2.1.139 or later: run `claude update` if it is older".into()
            ))
        );
    }

    #[test]
    fn an_empty_output_on_windows_is_a_note_without_a_quote() {
        assert_eq!(
            claude_step("\r\n", true),
            Some(ClaudeStep::Note(
                "Cannot read Claude Code's version from `claude --version`; user-memory's Windows hooks need Claude Code 2.1.139 or later: run `claude update` if it is older".into()
            ))
        );
    }

    #[test]
    fn the_note_quotes_one_short_line_without_control_characters() {
        let long = format!("\u{1b}[31m{}", "x".repeat(QUOTED_CHARS + 20));
        let Some(ClaudeStep::Note(note)) = claude_step(&long, true) else {
            panic!("an unreadable version is a note");
        };
        let quoted = format!("(it printed: [31m{}…)", "x".repeat(QUOTED_CHARS - 4));
        assert!(note.contains(&quoted), "{note}");
        assert!(!note.contains('\u{1b}'), "{note}");
    }

    #[test]
    fn other_platforms_are_never_told_about_claude_code_versions() {
        for output in ["2.1.138 (Claude Code)", "", "command not found"] {
            assert_eq!(claude_step(output, false), None, "{output:?}");
        }
    }
}
