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

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap()
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
