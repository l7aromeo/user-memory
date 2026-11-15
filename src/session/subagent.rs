//! The subagent hook: a starting subagent gets the session's project rules,
//! which neither client gives it from session start. Claude Code's subagents
//! never carry session start's text. A Codex sub-agent forked from its parent
//! (`fork_turns` all, the default) carries the parent's current history,
//! session start's text included, so the hook prints nothing for one whose
//! transcript holds it; every doubt prints the rules, so the cost of a wrong
//! guess is a duplicate, never a missing rule.

use super::{HookClient, LIMIT, hook_limit, hook_time, index, origin, payload};
use crate::memory;
use serde::Serialize;
use serde_json::Value;
use std::{io::Read, path::PathBuf};

/// How much of a forked sub-agent's transcript the hook reads, from its
/// start. Measured with Codex 0.163.0: the file opens with the sub-agent's
/// and the parent's `session_meta` (about 19 KB each), then the parent's
/// history as it stands: its first context (AGENTS.md is capped at 32 KiB by
/// default), after a compaction the summary and the user messages it keeps
/// (at most 20,000 tokens, about 80 KB), and then session start's text, at
/// 48 KB without and 50 KB after a compaction. About 250 KB at the most; a
/// heading past this bound is not seen, and the rules print again.
const FORK_SCAN_BYTES: u64 = 512 * 1024;

/// What the hook prints for a subagent starting in the current folder:
/// the project rules part of session start, with the line counting those
/// it leaves out, as both clients' JSON; nothing when the projects have no
/// rules, the store does not answer, or a forked Codex sub-agent already
/// carries them.
pub async fn subagent_start(client: HookClient, context_limit: Option<usize>) -> String {
    let deadline = tokio::time::Instant::now() + hook_time(LIMIT);
    let payload = tokio::time::timeout_at(deadline, payload())
        .await
        .ok()
        .flatten();
    let rules = tokio::time::timeout_at(
        deadline,
        subagent_rules(client, context_limit, payload.as_deref()),
    )
    .await
    .ok()
    .flatten()
    .unwrap_or_default();
    if rules.is_empty() {
        return rules;
    }
    serde_json::to_string(&SubagentOutput {
        hook_specific_output: SubagentContext {
            hook_event_name: "SubagentStart",
            additional_context: rules,
        },
    })
    .expect("a struct of strings always serializes")
}

/// The subagent hook's JSON: only the fields both clients read.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SubagentOutput {
    hook_specific_output: SubagentContext,
}

/// What the subagent reads.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SubagentContext {
    hook_event_name: &'static str,
    additional_context: String,
}

/// The project rules a subagent starting with `payload` gets, within
/// `client`'s room for them: the rules part session start would print, and
/// its line counting the rules left out; empty when there are none or a
/// forked Codex sub-agent already carries them. `None` when the store does
/// not answer.
pub async fn subagent_rules(
    client: HookClient,
    context_limit: Option<usize>,
    payload: Option<&[u8]>,
) -> Option<String> {
    let origin = payload.and_then(|payload| origin(client, payload));
    let folder = payload
        .and_then(|payload| serde_json::from_slice::<Value>(payload).ok())
        .and_then(|payload| payload["cwd"].as_str().map(PathBuf::from))
        .or_else(|| std::env::current_dir().ok())?;
    // The global rules are the instruction files'; only the rules part is
    // printed, so only the rules are asked for, without an index.
    let (_, text) = index(folder, Some(client), context_limit, origin, true, 0, true)
        .await
        .ok()?;
    let part = memory::rules_part(&text);
    if part.is_empty() {
        return Some(String::new());
    }
    let headings = memory::rules_headings(part);
    if client == HookClient::Codex
        && let Some(transcript) = payload.and_then(transcript_path)
        && carried(&transcript, &headings)
    {
        return Some(String::new());
    }
    let (limit, unit) = hook_limit(Some(client), context_limit);
    let overflow = super::overflow_line(&memory::rules_left_out(part));
    Some(super::capped(
        format!("{part}{overflow}"),
        limit,
        unit,
        super::NOTICE_CUT,
    ))
}

/// The transcript the payload names.
fn transcript_path(payload: &[u8]) -> Option<PathBuf> {
    serde_json::from_slice::<Value>(payload)
        .ok()?
        .get("transcript_path")?
        .as_str()
        .map(PathBuf::from)
}

/// Whether the transcript at `path` holds every one of `headings` in a
/// message Codex wrote for a hook: a `response_item` message with the
/// developer role and the heading in its input text, within its first
/// [`FORK_SCAN_BYTES`]. A user's message, the model's reply or a tool's
/// output quoting a heading is never taken for one; a developer message
/// that quotes session start's text for another reason (an instruction file
/// pasting it) would be. A file that cannot be read, or does not start as
/// JSON lines (a compressed one), holds none. A key with characters JSON
/// escapes (a Windows path's backslashes) never matches the file's text.
fn carried(path: &std::path::Path, headings: &[&str]) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = Vec::new();
    if file.take(FORK_SCAN_BYTES).read_to_end(&mut head).is_err() || head.first() != Some(&b'{') {
        return false;
    }
    let lines: Vec<&str> = head
        .split(|byte| *byte == b'\n')
        .filter_map(|line| std::str::from_utf8(line).ok())
        .collect();
    headings.iter().all(|heading| {
        lines
            .iter()
            .filter(|line| line.contains(heading))
            .any(|line| hook_message_says(line, heading))
    })
}

/// Whether `line`, one item of a Codex transcript, is a developer message
/// whose input text holds `heading`.
fn hook_message_says(line: &str, heading: &str) -> bool {
    let Ok(item) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    let message = &item["payload"];
    item["type"] == "response_item"
        && message["type"] == "message"
        && message["role"] == "developer"
        && message["content"].as_array().is_some_and(|content| {
            content.iter().any(|part| {
                part["type"] == "input_text"
                    && part["text"]
                        .as_str()
                        .is_some_and(|text| text.contains(heading))
            })
        })
}
