//! The tool hooks: the client runs `pre-tool-use` before the tools its
//! matcher names and `post-tool-use` after every tool. Each reads its
//! payload and decides by the tool's name, here in the binary, where a
//! release can change the list with no new review of the client's hooks.
//! For a tool it does not act on, the hook reads its input and exits,
//! printing nothing: in Codex it runs before every tool call.

use super::HookClient;
use serde::Deserialize;
use std::io::{IsTerminal, Read};

/// The tools Claude Code's pre-tool hook acts on: its shells (`Bash`,
/// `PowerShell` on Windows, and `Monitor`, which runs a command) and the
/// tools that start or message a subagent (`Agent`, earlier `Task`, and
/// `SendMessage`). The same names as setup's matcher for Claude Code, which
/// a test holds equal. Read in Claude Code 2.1.325's tool list; `Bash` and
/// `Agent` seen in its hook payloads.
pub(crate) const CLAUDE_TOOLS: [&str; 6] = [
    "Bash",
    "PowerShell",
    "Monitor",
    "Agent",
    "Task",
    "SendMessage",
];

/// The tools Codex's pre-tool hook acts on, as its hook payloads name them
/// (Codex 0.163.0): `Bash`, every shell command
/// (`HookToolName::bash` in codex-rs/core/src/tools/sandboxing.rs);
/// `spawn_agent`, spawning in the default or the `multi_agent_v1`
/// namespace (`function_hook_tool_name` in codex-rs/core/src/tools/registry.rs);
/// any other namespaced tool as namespace and name (`flat_tool_name` in
/// codex-rs/core/src/tools/mod.rs): `multi_agent_v1send_input`, and
/// `collaborationspawn_agent`, `collaborationsend_message` and
/// `collaborationfollowup_task` (the first measured); `send_input`,
/// `send_message` and `followup_task` for those tools in the default
/// namespace.
pub(crate) const CODEX_TOOLS: [&str; 9] = [
    "Bash",
    "spawn_agent",
    "send_input",
    "multi_agent_v1send_input",
    "send_message",
    "followup_task",
    "collaborationspawn_agent",
    "collaborationsend_message",
    "collaborationfollowup_task",
];

/// Whether `client`'s pre-tool hook acts on the tool named `tool`.
pub(crate) fn acts_on(client: HookClient, tool: &str) -> bool {
    match client {
        HookClient::Claude => CLAUDE_TOOLS.contains(&tool),
        HookClient::Codex => CODEX_TOOLS.contains(&tool),
    }
}

/// What `client`'s pre-tool hook prints for the call on stdin, within its
/// `context_limit` (the client's default room when `None`): nothing for a
/// tool it does not act on, and nothing yet for one it does.
pub fn pre_tool_use(client: HookClient, _context_limit: Option<usize>) -> String {
    let payload = payload();
    let Some(_tool) = tool_name(&payload).filter(|tool| acts_on(client, tool)) else {
        return String::new();
    };
    String::new()
}

/// What `client`'s post-tool hook prints for the result on stdin, within
/// its `context_limit` (the client's default room when `None`): nothing
/// yet.
pub fn post_tool_use(_client: HookClient, _context_limit: Option<usize>) -> String {
    drain();
    String::new()
}

/// Reads a hook's payload whole and keeps nothing of it: for a hook with
/// nothing to say, so its client never writes to a closed pipe.
pub fn drain() {
    payload();
}

/// The tool a hook payload names.
fn tool_name(payload: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct Named {
        tool_name: String,
    }
    serde_json::from_slice::<Named>(payload)
        .ok()
        .map(|named| named.tool_name)
}

/// The hook payload on stdin, read until its JSON object is whole or the
/// input ends: a tool's result can be far larger than session start's
/// payloads, and a hook that stopped reading early would close the pipe
/// on a client still writing. Parsed only when what was read ends a JSON
/// object, so a long result is parsed about once.
fn payload() -> Vec<u8> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Vec::new();
    }
    read_whole(stdin.lock())
}

/// How much one read of the payload takes: a pipe's whole buffer on Linux
/// and macOS, so a read takes everything the client has written so far.
const READ_CHUNK: usize = 64 * 1024;

fn read_whole(mut input: impl Read) -> Vec<u8> {
    let mut payload = Vec::new();
    let mut chunk = [0; READ_CHUNK];
    loop {
        match input.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => payload.extend_from_slice(&chunk[..read]),
        }
        if payload.trim_ascii_end().ends_with(b"}")
            && serde_json::from_slice::<serde::de::IgnoredAny>(&payload).is_ok()
        {
            break;
        }
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A payload in pieces, as a pipe delivers a long one, is read whole,
    /// and reading stops at the end of the object even if the input does not
    /// end there.
    #[test]
    fn a_payload_is_read_whole_and_no_further() {
        struct Pieces(Vec<Vec<u8>>);
        impl Read for Pieces {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                assert!(!self.0.is_empty(), "read past the end of the object");
                let piece = self.0.remove(0);
                buf[..piece.len()].copy_from_slice(&piece);
                Ok(piece.len())
            }
        }
        let long = "x".repeat(200_000);
        let whole = format!(r#"{{"tool_name":"Bash","tool_response":"{long}"}}"#);
        let pieces: Vec<Vec<u8>> = whole
            .as_bytes()
            .chunks(50_000)
            .map(<[u8]>::to_vec)
            .collect();
        let read = read_whole(Pieces(pieces));
        assert_eq!(read, whole.as_bytes());
        assert_eq!(tool_name(&read).as_deref(), Some("Bash"));
    }

    #[test]
    fn only_the_listed_tools_are_acted_on() {
        assert!(acts_on(HookClient::Claude, "Bash"));
        assert!(acts_on(HookClient::Codex, "collaborationspawn_agent"));
        for (client, tool) in [
            (HookClient::Claude, "mcp__probe__BashTool"),
            (HookClient::Claude, "BashOutput"),
            (HookClient::Codex, "apply_patch"),
            (HookClient::Codex, "collaborationwait_agent"),
        ] {
            assert!(!acts_on(client, tool), "{tool}");
        }
    }
}
