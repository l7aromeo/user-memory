//! The tool hooks: the client runs `pre-tool-use` before the tools its
//! matcher names and `post-tool-use` after every tool. Each reads its
//! payload and decides by the tool's name, here in the binary, where a
//! release can change the list with no new review of the client's hooks.
//! For a tool it does not act on, the hook reads its input and exits,
//! printing nothing: in Codex it runs before every tool call. Both keep the
//! session's delivery index current (#175); `post-tool-use` then matches
//! the call against it and pushes what the agent context was not shown in
//! this stretch (#117). Before a shell command, `pre-tool-use` holds it
//! back once with the passages whose anchors it names (#118).

use super::{
    HookClient,
    recall::{self, Call, Offer, State, Step, Use},
};
use crate::memory::{
    delivery_files::{self, Refreshed},
    hold::{self, HOLD_CHARS, HOLD_READ},
    matcher::{Action, Kind, Matched, Terms, Window, terms_of, word_spans},
    push,
};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{IsTerminal, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// The most a tool hook spends keeping the delivery index current (#117,
/// #175), from its start: a refresh past it renames nothing. A refresh
/// costs 5 to 12 ms on macOS at 10,000 memories, process start included;
/// the rest is room for Windows' slower process start and file scanning,
/// and a loaded machine. A hold has its own, [`HOLD_LIMIT`]. The delivery
/// bench's cost check (evals/delivery/run.py) holds to it the time a tool
/// hook adds over a start of the same hook that returns at once, since the
/// limit starts in the hook; the start has a limit of its own there.
pub(crate) const TOOL_HOOK_LIMIT: Duration = Duration::from_millis(100);

/// The most the pre-tool hook spends deciding a hold (#118, #241), from its
/// start, with the fold of the store's changes that a second look needs.
/// Claude Code and Codex wait for this hook before they run the command
/// (setup gives it 5 s), so a hold that comes late costs less than one that
/// never comes; on Windows x64 CI the hook's 95th percentile was 107 ms
/// with the fold before the hold. 250 ms is about twice that, and a wait
/// before a command that is hardly felt. The delivery bench's cost check
/// holds to it the time the hook adds over a start that returns at once, as
/// for [`TOOL_HOOK_LIMIT`].
pub(crate) const HOLD_LIMIT: Duration = Duration::from_millis(250);

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

/// What `client`'s pre-tool hook prints for the call on stdin. For a tool
/// it acts on, it keeps the session's delivery index current, and a call
/// that starts a subagent keeps its task for the subagent's start (#117).
/// A shell command that names a passage's anchor strongly enough, which
/// this agent context was not shown or held in this stretch, is held back
/// once: a deny whose reason shows the passages (#118). Anything else, and
/// any error, missing state or overrun deadline, prints nothing, and the
/// call runs.
///
/// The hold is decided first, from the delivery files as they are, within
/// [`HOLD_LIMIT`], so folding the store's changes into them never takes its
/// time (#241). After a hold the fold keeps the tool hook's own limit. When
/// nothing is held, the fold gets what is left of `HOLD_LIMIT`, and if it
/// took in a change the hold is decided again within it: a rule recorded
/// outside the session's own tool calls, such as through the CLI, then holds
/// the very next command when the fold fits, and the command after it when
/// not.
pub fn pre_tool_use(client: HookClient, _context_limit: Option<usize>) -> String {
    let started = Instant::now();
    let payload = payload();
    let Some(_tool) = tool_name(&payload).filter(|tool| acts_on(client, tool)) else {
        return String::new();
    };
    let Some(session) = Session::of(client, &payload, tool_deadline(started)) else {
        return String::new();
    };
    let Ok(payload) = serde_json::from_slice::<Value>(&payload) else {
        session.keep_current(session.deadline);
        return String::new();
    };
    if let Some((key, task)) = spawned(client, &payload) {
        session.keep_current(session.deadline);
        let _ = State::open(&recall::path(&session.snapshot), session.deadline)
            .and_then(|mut state| state.spawn(&key, &terms_of(&task)));
        return String::new();
    }
    let Some(action) = action(&payload).filter(|action| action.kind == Kind::Command) else {
        session.keep_current(session.deadline);
        return String::new();
    };
    let hold_deadline = started + super::hook_time(HOLD_LIMIT);
    let terms = terms_of(&action);
    let command = command_key(&action.text);
    if let Ok(Some(reason)) = session.hold(&payload, &command, terms.clone(), hold_deadline) {
        session.keep_current(session.deadline);
        return deny(client, &payload, &reason);
    }
    if session
        .keep_current(hold_deadline)
        .is_some_and(Refreshed::took_in)
        && let Ok(Some(reason)) = session.hold(&payload, &command, terms, hold_deadline)
    {
        return deny(client, &payload, &reason);
    }
    String::new()
}

/// The pre-tool hook's deny with `reason`, as both clients read it. Codex
/// follows the reason with ". Command: <command>", so the reason's own
/// last full stop is left off there.
fn deny(client: HookClient, payload: &Value, reason: &str) -> String {
    let reason = match client {
        HookClient::Claude => reason,
        HookClient::Codex => reason.strip_suffix('.').unwrap_or(reason),
    };
    let event = payload["hook_event_name"].as_str().unwrap_or("PreToolUse");
    json!({"hookSpecificOutput": {"hookEventName": event, "permissionDecision": "deny",
                                  "permissionDecisionReason": reason}})
    .to_string()
}

/// The tools Claude Code starts a subagent with, its task in
/// `tool_input.prompt`.
const CLAUDE_SPAWNS: [&str; 2] = ["Agent", "Task"];

/// The tools Codex starts a sub-agent with, its task in
/// `tool_input.message`: `collaborationspawn_agent`, as the hook payload
/// names it in Codex 0.163.0's default setup (namespace and tool joined,
/// probed against the fake model), and `spawn_agent` in the default or
/// `multi_agent_v1` namespace.
const CODEX_SPAWNS: [&str; 2] = ["collaborationspawn_agent", "spawn_agent"];

/// The tools Codex sends a running sub-agent a message or task with, the
/// text in `tool_input.message`.
const CODEX_MESSAGES: [&str; 6] = [
    "send_input",
    "multi_agent_v1send_input",
    "send_message",
    "followup_task",
    "collaborationsend_message",
    "collaborationfollowup_task",
];

/// The task a call that starts a subagent gives it, and the key its start
/// claims it by. Claude Code's start names the turn (`prompt_id`) and the
/// subagent's type, as the call does. Codex's start shares nothing with the
/// call that spawned it but the session, so the key is empty and each
/// start claims the session's oldest task: two spawned at once may pair
/// crosswise, a sibling's task in each.
fn spawned(client: HookClient, payload: &Value) -> Option<(String, Action)> {
    let tool = payload["tool_name"].as_str()?;
    let input = &payload["tool_input"];
    let (key, text) = match client {
        HookClient::Claude if CLAUDE_SPAWNS.contains(&tool) => (
            spawn_key(
                payload["prompt_id"].as_str()?,
                input["subagent_type"]
                    .as_str()
                    .unwrap_or(super::subagent::CLAUDE_DEFAULT_AGENT),
            ),
            input["prompt"].as_str()?,
        ),
        HookClient::Codex if CODEX_SPAWNS.contains(&tool) => {
            (String::new(), input["message"].as_str()?)
        }
        _ => return None,
    };
    Some((
        key,
        Action {
            kind: Kind::Task,
            text: text.to_owned(),
            failed: false,
        },
    ))
}

/// The key a subagent's task is kept and claimed by: the turn and the
/// subagent's type for Claude Code.
pub(super) fn spawn_key(prompt_id: &str, agent_type: &str) -> String {
    format!("{prompt_id}\n{agent_type}")
}

/// What `client`'s post-tool hook prints for the result on stdin, within
/// its `context_limit` (the client's default room when `None`): the
/// memories the call matches that this agent context has not been shown
/// in this stretch (#117), or nothing.
pub fn post_tool_use(client: HookClient, _context_limit: Option<usize>) -> String {
    let started = Instant::now();
    let payload = payload();
    let Some(session) = Session::of(client, &payload, push_deadline(started)) else {
        return String::new();
    };
    session.keep_current(session.deadline);
    let pushed = session.push(client, &payload);
    // Settling the context's last hold (#118) comes after the push, on a
    // deadline of its own: it writes only what the next flush and the
    // working set read, never what this push reads, and on a slow disk its
    // open of the session state would otherwise leave the push no time.
    session.settle(&payload, tool_deadline(Instant::now()));
    // The tag check (#121) runs once the push is chosen, on a deadline of
    // its own, so its git and its request to a server never take the
    // push's time. The prompt hook and session start run theirs within
    // what their own work leaves of their limits; this limit is too short
    // to leave the check anything on a loaded machine, and a tag a command
    // made would then wait for the next prompt.
    session.check_tags(client, &payload, tool_deadline(Instant::now()));
    pushed
}

/// A tool hook's deadline: [`TOOL_HOOK_LIMIT`] after it `started`.
pub(super) fn tool_deadline(started: Instant) -> Instant {
    started + super::hook_time(TOOL_HOOK_LIMIT)
}

/// The post-tool push's deadline: [`TOOL_HOOK_LIMIT`] after the hook
/// `started`, with the room a test gives it ([`super::hook_push_time`]).
pub(super) fn push_deadline(started: Instant) -> Instant {
    started + super::hook_push_time(TOOL_HOOK_LIMIT)
}

/// What a tool hook knows of its session: the store, the session's state
/// files and scope, and the hook's deadline.
pub(super) struct Session {
    /// The server the store is on, or none for a store on this machine.
    remote: Option<String>,
    snapshot: PathBuf,
    projects: Vec<String>,
    deadline: Instant,
}

impl Session {
    /// The session the payload names, its scope from the scope file its
    /// last prompt or start wrote, and the hook's `deadline`. None for a
    /// session with no scope file, as one started by an earlier version, or
    /// when the store's setup can't be read.
    pub(super) fn of(client: HookClient, payload: &[u8], deadline: Instant) -> Option<Self> {
        let remote = crate::setup::configured_remote()
            .ok()?
            .map(|connection| connection.url);
        Self::on(remote, client, payload, deadline)
    }

    /// [`Session::of`] on the store setup names, read already: the server
    /// at `remote`, or this machine's when none.
    pub(super) fn on(
        remote: Option<String>,
        client: HookClient,
        payload: &[u8],
        deadline: Instant,
    ) -> Option<Self> {
        let snapshot = super::session_file(client, payload)?;
        let projects = super::live::load_scope(&super::live::scope_path(&snapshot))?;
        Some(Self {
            remote,
            snapshot,
            projects,
            deadline,
        })
    }

    /// The session's snapshot file, which names its state files.
    pub(super) fn snapshot(&self) -> &Path {
        &self.snapshot
    }

    /// When the hook's time is up.
    pub(super) fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Keeps the session's delivery overlay current (#175), never waiting
    /// for the lock, and writing it only by `deadline`; says what the
    /// refresh did, and none on a server's store, whose files are its
    /// connector's, or when it failed. Prints nothing.
    fn keep_current(&self, deadline: Instant) -> Option<Refreshed> {
        if self.remote.is_some() {
            return None;
        }
        crate::memory::delivery_files::refresh(&super::db_path(), &self.projects, deadline, false)
            .ok()
    }

    /// What the hook prints for the result in `payload`: the push of what
    /// the call matches that its agent context was not shown in this
    /// stretch, or nothing. Notes what the call used first.
    fn push(&self, client: HookClient, payload: &[u8]) -> String {
        let origin = super::origin(client, payload);
        let Ok(payload) = serde_json::from_slice::<Value>(payload) else {
            return String::new();
        };
        super::used::note(&self.snapshot, &payload, origin.as_deref(), self.deadline);
        let Some(mut action) = action(&payload) else {
            return String::new();
        };
        action.failed = failed(client, &payload);
        self.recall(&payload, action.kind, terms_of(&action), action.failed)
            .ok()
            .flatten()
            .map(|text| {
                let event = payload["hook_event_name"].as_str().unwrap_or("PostToolUse");
                json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": text}})
                    .to_string()
            })
            .unwrap_or_default()
    }

    /// The tag check after a tool call (#121), in the payload's folder, by
    /// `deadline`: a look only when a tag is waited for and its refs
    /// changed. Prints nothing.
    fn check_tags(&self, client: HookClient, payload: &[u8], deadline: Instant) {
        let Some(origin) = super::origin(client, payload) else {
            return;
        };
        let Some(folder) = serde_json::from_slice::<Value>(payload)
            .ok()
            .and_then(|payload| payload["cwd"].as_str().map(PathBuf::from))
        else {
            return;
        };
        let remote = match &self.remote {
            Some(_) => match crate::setup::configured_remote() {
                Ok(Some(connection)) => Some(connection),
                _ => return,
            },
            None => None,
        };
        let _ = super::tags::check(
            &self.snapshot,
            &folder,
            &origin,
            remote.as_ref(),
            &super::db_path(),
            super::tags::When::Tool,
            deadline,
        );
    }

    /// Adds an action of `kind` that gave `terms` to its agent context's
    /// window and returns the push of what it matches that the context was
    /// not shown in this stretch; none when nothing passes, or the hook
    /// runs out of time, which then writes nothing. On a server's store
    /// each line names the id alone.
    pub(super) fn recall(
        &self,
        payload: &Value,
        kind: Kind,
        terms: Terms,
        failed: bool,
    ) -> anyhow::Result<Option<String>> {
        let mut state = State::open(&recall::path(&self.snapshot), self.deadline)?;
        let agent = payload["agent_id"].as_str().unwrap_or(recall::MAIN);
        let mut window = Window::default();
        for step in state.window(agent)? {
            window.push_terms(step.kind, step.terms, step.failed);
        }
        let step = Step {
            kind,
            failed,
            terms: window.push_terms(kind, terms, failed).clone(),
        };
        let db = super::db_path();
        let mut lines = Vec::new();
        if let Some(view) = delivery_files::open(self.remote.as_deref(), &db, &self.projects) {
            let found = view.rank(&window)?;
            // What the context had in this stretch takes no read: a passage
            // a hold deferred is among the next ones. A date passed since
            // the index was kept ends its memory with no write: dropped
            // here, from the file alone.
            let handled = state.handled(agent, Use::Push)?;
            let now = crate::memory::until::stored_now(jiff::Timestamp::now());
            let mut passing = Vec::new();
            for matched in found
                .iter()
                .filter(|m| push::passes(m, failed))
                .filter(|m| !view.past_date(m.memory, &now).unwrap_or(true))
            {
                if passing.len() == push::READ_PER_EVENT {
                    break;
                }
                if !handled.contains(view.id(matched.memory)?, matched.slot) {
                    passing.push(matched);
                }
            }
            if !passing.is_empty() {
                // The store is opened only for a match's text, and only here.
                let conn = match self.remote {
                    None => Some(Connection::open_with_flags(
                        &db,
                        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                    )?),
                    Some(_) => None,
                };
                lines = push::lines(conn.as_ref(), &view, &passing, self.deadline)?;
            }
        }
        if Instant::now() >= self.deadline {
            return Ok(None);
        }
        // A memory a tag ended here, whose end the store doesn't have yet,
        // is ended for this session.
        let ended = state.ended_here()?;
        lines.retain(|line| !ended.contains(&line.id));
        let offers: Vec<Offer> = lines
            .iter()
            .map(|line| Offer {
                memory: &line.id,
                passage: line.passage,
                chars: line.text.chars().count() + 1,
            })
            .collect();
        let room = push::RECALL_CHARS - push::header_chars(failed);
        let shown = state.record(agent, &step, &offers, room)?;
        Ok((!shown.is_empty()).then(|| push::text(shown.iter().map(|&i| &lines[i]), failed)))
    }

    /// Settles the last hold of the context whose tool call `payload`
    /// reports ([`State::settle`]) by `deadline`: its shell command's key,
    /// or none for any other tool. Errors are dropped: an unsettled hold
    /// only leaves its outcome unknown.
    fn settle(&self, payload: &[u8], deadline: Instant) {
        let Ok(payload) = serde_json::from_slice::<Value>(payload) else {
            return;
        };
        let payload = &payload;
        let agent = payload["agent_id"].as_str().unwrap_or(recall::MAIN);
        let call = payload["tool_use_id"].as_str().unwrap_or_default();
        let command = action(payload)
            .filter(|action| action.kind == Kind::Command)
            .map(|action| command_key(&action.text));
        let _ = State::open(&recall::path(&self.snapshot), deadline)
            .and_then(|mut state| state.settle(agent, call, command.as_deref()));
    }

    /// The reason to hold back the shell command that gave `terms`, about
    /// to run in its agent context, or none: the passages that hold it
    /// ([`Matched::hold`]) that the context was not shown, held or
    /// deferred in this stretch, whose memory may hold a command
    /// ([`push::Voice::holds`]), strongest first, at most
    /// [`hold::HOLD_PER_EVENT`] within [`HOLD_CHARS`]. The command is not added
    /// to the window: the post-tool hook does that once it runs. A
    /// server's store holds nothing: a hold shows a passage and its label,
    /// which only this machine's store gives the hook. Past the deadline it
    /// holds nothing and writes nothing.
    pub(super) fn hold(
        &self,
        payload: &Value,
        command: &str,
        terms: Terms,
        deadline: Instant,
    ) -> anyhow::Result<Option<String>> {
        if self.remote.is_some() {
            return Ok(None);
        }
        let mut state = State::open(&recall::path(&self.snapshot), deadline)?;
        let agent = payload["agent_id"].as_str().unwrap_or(recall::MAIN);
        let mut window = Window::default();
        for step in state.window(agent)? {
            window.push_terms(step.kind, step.terms, step.failed);
        }
        window.push_terms(Kind::Command, terms, false);
        let db = super::db_path();
        let Some(view) = delivery_files::open(None, &db, &self.projects) else {
            return Ok(None);
        };
        let found = view.rank(&window)?;
        let handled = state.handled(agent, Use::Hold)?;
        // A note agents kept running past holds no more: it is pushed after
        // the command instead (#118).
        let stopped = state.stopped()?;
        let mut holding: Vec<&Matched> = Vec::new();
        for matched in found.iter().filter(|m| m.hold()) {
            let id = view.id(matched.memory)?;
            if !handled.contains(id, matched.slot) && !stopped.iter().any(|s| s == id) {
                holding.push(matched);
            }
        }
        if holding.is_empty() {
            return Ok(None);
        }
        let read = holding.len().min(HOLD_READ);
        let conn = Connection::open_with_flags(
            &db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let held = hold::lines(&conn, &view, &holding[..read], deadline)?;
        let mut unread = Vec::with_capacity(holding.len() - read);
        for matched in &holding[read..] {
            unread.push((view.id(matched.memory)?, matched.slot));
        }
        let offers: Vec<Offer> = held
            .iter()
            .map(|one| Offer {
                memory: &one.id,
                passage: one.passage,
                chars: one.chars(),
            })
            .collect();
        let room = HOLD_CHARS - hold::frame_chars();
        let call = Call {
            id: payload["tool_use_id"].as_str().unwrap_or_default(),
            key: command,
        };
        let shown = state.hold(agent, call, &offers, &unread, room, deadline)?;
        if shown.is_empty() {
            return Ok(None);
        }
        let shown: Vec<&hold::Held> = shown.iter().map(|&i| &held[i]).collect();
        Ok(Some(hold::reason(&shown)))
    }
}

/// The key a held command is known by until its context's next call: the
/// SHA-256 of its line, so the state keeps no command text.
fn command_key(line: &str) -> String {
    crate::setup::upgrade::sha256(line.as_bytes())
}

/// The action a tool call is, from its payload: the command line, the
/// path or the topic the agent gave, never the tool's result, a file's
/// contents or an edit's text. None for a tool whose call names nothing a
/// memory could.
fn action(payload: &Value) -> Option<Action> {
    let input = &payload["tool_input"];
    let (kind, text) = match payload["tool_name"].as_str()? {
        "Bash" | "PowerShell" | "Monitor" => (
            Kind::Command,
            relative_line(payload, &command_line(&input["command"])?),
        ),
        "Read" | "Edit" | "MultiEdit" | "Write" => {
            (Kind::File, relative(payload, input["file_path"].as_str()?))
        }
        "NotebookEdit" => (
            Kind::File,
            relative(payload, input["notebook_path"].as_str()?),
        ),
        "apply_patch" => (Kind::File, patched(payload, input["command"].as_str()?)?),
        // The words a parent gives a subagent are its own; what the
        // subagent returns is the tool's result.
        tool if CLAUDE_SPAWNS.contains(&tool) => {
            (Kind::Message, input["prompt"].as_str()?.to_owned())
        }
        "SendMessage" => (Kind::Message, input["message"].as_str()?.to_owned()),
        tool if CODEX_SPAWNS.contains(&tool) || CODEX_MESSAGES.contains(&tool) => {
            (Kind::Message, input["message"].as_str()?.to_owned())
        }
        SEARCH_CLAUDE | SEARCH_CODEX => (Kind::Search, input["topic"].as_str()?.to_owned()),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(Action {
        kind,
        text,
        failed: false,
    })
}

/// Whether the call in `payload` failed. Claude Code says so with its own
/// event, `PostToolUseFailure`; one the user interrupted is not the
/// command's failure. Codex's payload does not say, so its transcript's
/// end is read for the call's result ([`codex_failed`]).
fn failed(client: HookClient, payload: &Value) -> bool {
    match client {
        HookClient::Claude => {
            payload["hook_event_name"] == "PostToolUseFailure" && payload["is_interrupt"] != true
        }
        HookClient::Codex => payload["transcript_path"]
            .as_str()
            .zip(payload["tool_use_id"].as_str())
            .is_some_and(|(transcript, id)| codex_failed(Path::new(transcript), id)),
    }
}

/// The most of a Codex transcript's end read for a call's result. The
/// result's line carries the command's whole output, so it can end far
/// from where the call's id is; 1 MiB bounds a read at well under 1 ms.
const TRANSCRIPT_TAIL_BYTES: u64 = 1024 * 1024;

/// How much more of the transcript each step reads, backwards from its
/// end: a pipe's buffer, so a short result is found in one read.
const TRANSCRIPT_STEP: u64 = 64 * 1024;

/// Whether the Codex transcript at `path` records the call `id` as failed:
/// its `item_completed` event, as Codex 0.163.0 writes it before the
/// post-tool hook runs, with the item's `status` failed or a nonzero
/// `exit_code`. Read backwards from the end in [`TRANSCRIPT_STEP`]s up to
/// [`TRANSCRIPT_TAIL_BYTES`]. A missing transcript (an ephemeral thread
/// has none), a line not found within the bound, or one not yet written
/// reads as a success: the match is the same, only the failure's header
/// and bars are lost.
fn codex_failed(path: &Path, id: &str) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(length) = file.metadata().map(|m| m.len()) else {
        return false;
    };
    let mut read = 0;
    while read < length.min(TRANSCRIPT_TAIL_BYTES) {
        read = (read + TRANSCRIPT_STEP)
            .min(length)
            .min(TRANSCRIPT_TAIL_BYTES);
        let mut tail = Vec::new();
        if file.seek(SeekFrom::Start(length - read)).is_err()
            || (&mut file).take(read).read_to_end(&mut tail).is_err()
        {
            return false;
        }
        // The first line may be cut by the read's start, unless it is the
        // file's own first line.
        let whole = if read == length {
            &tail[..]
        } else {
            match tail.iter().position(|b| *b == b'\n') {
                Some(at) => &tail[at + 1..],
                None => continue,
            }
        };
        let found = whole
            .split(|b| *b == b'\n')
            .rev()
            .filter_map(|line| std::str::from_utf8(line).ok())
            .filter(|line| line.contains(id) && line.contains("\"item_completed\""))
            .find_map(|line| result(line, id));
        if let Some(failed) = found {
            return failed;
        }
    }
    false
}

/// Whether `line`, a Codex transcript line, is call `id`'s `item_completed`
/// event, and if so whether the call failed. Only the event's and the
/// item's `type`, the item's `id`, `status` and `exit_code` are read: the
/// item's `aggregated_output` is the command's output, which never joins
/// the window.
fn result(line: &str, id: &str) -> Option<bool> {
    #[derive(Deserialize)]
    struct Event {
        #[serde(rename = "type")]
        kind: String,
        payload: Payload,
    }
    #[derive(Deserialize)]
    struct Payload {
        #[serde(rename = "type")]
        kind: String,
        item: Option<Item>,
    }
    #[derive(Deserialize)]
    struct Item {
        id: Option<String>,
        status: Option<String>,
        exit_code: Option<i64>,
    }
    let event: Event = serde_json::from_str(line).ok()?;
    let item = event.payload.item?;
    (event.kind == "event_msg"
        && event.payload.kind == "item_completed"
        && item.id.as_deref() == Some(id))
    .then(|| {
        item.status.as_deref() == Some("failed") || item.exit_code.is_some_and(|code| code != 0)
    })
}

/// The memory search tool as Claude Code names it in a hook payload.
pub(super) const SEARCH_CLAUDE: &str = "mcp__user-memory__search";
/// The memory search tool as Codex names it in a hook payload.
pub(super) const SEARCH_CODEX: &str = "mcp__user_memory__search";

/// A shell call's command line: as given, or a command and its arguments
/// joined, where a shell's `-c` or `-lc` script is the command line.
fn command_line(command: &Value) -> Option<String> {
    match command {
        Value::String(line) => Some(line.clone()),
        Value::Array(words) => {
            let words: Vec<&str> = words.iter().filter_map(Value::as_str).collect();
            match words.as_slice() {
                [_, "-c" | "-lc", script] => Some((*script).to_owned()),
                words if !words.is_empty() => Some(words.join(" ")),
                _ => None,
            }
        }
        _ => None,
    }
}

/// `path` as the session's folder (the payload's `cwd`) sees it: relative
/// when it is inside, as a memory names a project's files, with `/`
/// between its names on Windows ([`Folder`]).
fn relative(payload: &Value, path: &str) -> String {
    match Folder::of(payload) {
        Some(folder) => folder.relative(path),
        None => path.to_owned(),
    }
}

/// A command line with the session's folder (the payload's `cwd`) taken
/// off each word that starts with it, and on Windows each path's `\`
/// written `/`, so a path names what a read of the same file does. Words
/// are split as the matcher splits the line, so a folder inside a word or
/// inside quoted text stays as written.
fn relative_line(payload: &Value, line: &str) -> String {
    let Some(folder) = Folder::of(payload) else {
        return line.to_owned();
    };
    let mut relative = String::with_capacity(line.len());
    let mut at = 0;
    for span in word_spans(line) {
        relative.push_str(&line[at..span.start]);
        let word = &line[span.clone()];
        if folder.windows && !word.contains(WINDOWS_RESERVED) {
            relative.push_str(&folder.relative(word));
        } else {
            relative.push_str(&word[folder.prefix(word).unwrap_or(0)..]);
        }
        at = span.end;
    }
    relative.push_str(&line[at..]);
    relative
}

/// The characters no Windows file name holds: a word with one is no path,
/// so its `\` is left as written (`findstr "a\|b"`).
const WINDOWS_RESERVED: [char; 6] = ['<', '>', '"', '|', '?', '*'];

/// The session's folder, split as its platform splits a path. One that
/// starts with a drive (`C:\`, `C:/`) or a share (`\\`) is a Windows
/// folder, wherever the hook runs: `\` and `/` both separate, and a name
/// matches in any case, as Windows opens files. Any other separates by `/`
/// alone and matches as written, as `Path` does on Unix.
struct Folder<'a> {
    /// How many separators the folder's path starts with: 1 from the root,
    /// 2 for a share, none after a drive.
    root: usize,
    names: Vec<&'a str>,
    windows: bool,
}

impl<'a> Folder<'a> {
    fn of(payload: &'a Value) -> Option<Self> {
        let cwd = payload["cwd"].as_str().filter(|cwd| !cwd.is_empty())?;
        let windows = matches!(
            cwd.as_bytes(),
            [letter, b':', b'\\' | b'/', ..] if letter.is_ascii_alphabetic()
        ) || cwd.starts_with(r"\\");
        let mut folder = Self {
            root: 0,
            names: Vec::new(),
            windows,
        };
        folder.root = cwd.len() - cwd.trim_start_matches(|c| folder.separator(c)).len();
        folder.names = cwd
            .split(|c| folder.separator(c))
            .filter(|name| !name.is_empty())
            .collect();
        Some(folder)
    }

    fn separator(&self, c: char) -> bool {
        c == '/' || (self.windows && c == '\\')
    }

    /// `path` as a memory names it: relative when inside the folder, with
    /// `/` between its names on Windows.
    fn relative(&self, path: &str) -> String {
        let inside = &path[self.prefix(path).unwrap_or(0)..];
        if self.windows {
            inside.replace('\\', "/")
        } else {
            inside.to_owned()
        }
    }

    /// How many bytes the folder and the separators after it take at the
    /// start of `path`, when `path` names something inside the folder.
    fn prefix(&self, path: &str) -> Option<usize> {
        let mut names = self.names.iter();
        let mut rest = if let Some(rest) = self.git_bash_drive(path) {
            names.next();
            rest
        } else {
            let rest = path.trim_start_matches(|c| self.separator(c));
            if path.len() - rest.len() != self.root {
                return None;
            }
            rest
        };
        for name in names {
            let end = rest.find(|c| self.separator(c))?;
            if !self.same(&rest[..end], name) {
                return None;
            }
            rest = rest[end..].trim_start_matches(|c| self.separator(c));
        }
        (!rest.is_empty()).then(|| path.len() - rest.len())
    }

    /// What follows the drive in `path` when it is Git Bash's spelling of
    /// a path on the folder's drive, `/c/Users` for `C:\Users`: Claude
    /// Code's shell on Windows is Git Bash, and a path there may be either.
    fn git_bash_drive<'p>(&self, path: &'p str) -> Option<&'p str> {
        let drive = self.names.first()?.strip_suffix(':')?;
        if !self.windows || self.root != 0 {
            return None;
        }
        let rest = path.strip_prefix('/')?;
        let after = rest.get(drive.len()..)?;
        (self.same(&rest[..drive.len()], drive) && after.starts_with('/'))
            .then(|| after.trim_start_matches(|c| self.separator(c)))
    }

    /// Whether two names name the same folder.
    fn same(&self, one: &str, other: &str) -> bool {
        if self.windows {
            one.chars()
                .flat_map(char::to_lowercase)
                .eq(other.chars().flat_map(char::to_lowercase))
        } else {
            one == other
        }
    }
}

/// The files a patch adds, updates, deletes or moves, one per line; its
/// body is never read.
fn patched(payload: &Value, patch: &str) -> Option<String> {
    let paths: Vec<String> = patch
        .lines()
        .filter_map(|line| {
            PATCH_FILES
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix))
        })
        .map(|path| relative(payload, path.trim()))
        .collect();
    (!paths.is_empty()).then(|| paths.join("\n"))
}

/// The lines of a Codex patch that name a file, as `apply_patch` writes
/// them.
const PATCH_FILES: [&str; 4] = [
    "*** Add File: ",
    "*** Update File: ",
    "*** Delete File: ",
    "*** Move to: ",
];

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

    /// On a server's store, until its connector keeps delivery files
    /// (#180), a call pushes nothing and calls no server, and its action
    /// still joins the window, so recall starts cleanly once files exist.
    #[test]
    fn a_servers_store_pushes_nothing_and_keeps_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session {
            remote: Some("https://memory.example.com/mcp".to_owned()),
            snapshot: dir.path().join("sessions/claude-s1-key.json"),
            projects: Vec::new(),
            deadline: Instant::now() + Duration::from_secs(30),
        };
        let payload = json!({"tool_name": "Bash", "tool_input": {"command": "cargo publish"}});
        let action = action(&payload).unwrap();
        let terms = terms_of(&action);
        assert!(
            session
                .recall(&payload, action.kind, terms, false)
                .unwrap()
                .is_none()
        );
        let state = State::open(&recall::path(&session.snapshot), session.deadline).unwrap();
        let window = state.window(recall::MAIN).unwrap();
        assert_eq!(window.len(), 1);
        assert!(window[0].terms.anchors.contains("cargo publish"));
    }

    #[test]
    fn a_call_is_the_action_the_agent_gave() {
        let payload = |tool: &str, input: Value| json!({"cwd": "/work/app", "tool_name": tool, "tool_input": input, "tool_response": "`cargo publish`"});
        let of = |tool: &str, input: Value| action(&payload(tool, input)).map(|a| (a.kind, a.text));
        assert_eq!(
            of("Bash", json!({"command": "cargo publish"})),
            Some((Kind::Command, "cargo publish".to_owned()))
        );
        assert_eq!(
            of("Bash", json!({"command": ["bash", "-lc", "git push"]})),
            Some((Kind::Command, "git push".to_owned()))
        );
        assert_eq!(
            of("Read", json!({"file_path": "/work/app/docs/notes.md"})),
            Some((Kind::File, "docs/notes.md".to_owned()))
        );
        assert_eq!(
            of(
                "Edit",
                json!({"file_path": "/elsewhere/notes.md", "new_string": "`cargo publish`"})
            ),
            Some((Kind::File, "/elsewhere/notes.md".to_owned()))
        );
        assert_eq!(
            of(
                "apply_patch",
                json!({"command": "*** Begin Patch\n*** Update File: /work/app/src/a.rs\n+`cargo publish`\n*** Add File: b.md\n*** End Patch"})
            ),
            Some((Kind::File, "src/a.rs\nb.md".to_owned()))
        );
        assert_eq!(
            of(SEARCH_CODEX, json!({"topic": "release steps"})),
            Some((Kind::Search, "release steps".to_owned()))
        );
        for (tool, input) in [
            ("Glob", json!({"pattern": "*.rs"})),
            ("Bash", json!({"command": "  "})),
            ("apply_patch", json!({"command": "+`cargo publish`"})),
            ("WebFetch", json!({"url": "https://example.com"})),
        ] {
            assert_eq!(of(tool, input), None, "{tool}");
        }
    }

    /// On Windows a path in a command is taken off the session's folder as
    /// a read of it is, whatever its separators and case, and every path
    /// names its folders with `/`, as a memory does.
    #[test]
    fn a_windows_path_in_a_command_is_relative_as_a_read_of_it_is() {
        let of = |cwd: &str, tool: &str, input: Value| {
            action(&json!({"cwd": cwd, "tool_name": tool, "tool_input": input})).map(|a| a.text)
        };
        let cwd = r"C:\Users\dev\proj";
        assert_eq!(
            of(
                cwd,
                "Read",
                json!({"file_path": r"C:\Users\dev\proj\src\store\cache.py"})
            )
            .as_deref(),
            Some("src/store/cache.py")
        );
        for (line, relative) in [
            (
                r"Get-Content C:\Users\dev\proj\src\store\cache.py",
                "Get-Content src/store/cache.py",
            ),
            (
                r#"type "c:\users\DEV\proj\src\store\cache.py""#,
                r#"type "src/store/cache.py""#,
            ),
            (
                "cat C:/Users/dev/proj/src/store/cache.py",
                "cat src/store/cache.py",
            ),
            (r"type src\store\cache.py", "type src/store/cache.py"),
            (r"type D:\other\notes.md", "type D:/other/notes.md"),
            (r#"findstr "a\|b" src\a.py"#, r#"findstr "a\|b" src/a.py"#),
        ] {
            assert_eq!(
                of(cwd, "PowerShell", json!({"command": line})).as_deref(),
                Some(relative),
                "{line}"
            );
        }
        // A share's folder starts with two separators, so one does not name it.
        for (line, relative) in [
            (r"type \\srv\share\proj\a.md", "type a.md"),
            (r"type \srv\share\proj\a.md", "type /srv/share/proj/a.md"),
        ] {
            assert_eq!(
                of(r"\\srv\share\proj", "PowerShell", json!({"command": line})).as_deref(),
                Some(relative),
                "{line}"
            );
        }
    }

    /// Git Bash, the shell of Claude Code's Bash tool on Windows, writes the
    /// folder's drive as a folder: `/c/Users/dev` is `C:\Users\dev`.
    #[test]
    fn git_bash_names_the_folders_drive_as_a_folder() {
        let payload = json!({"cwd": r"C:\Users\dev\proj"});
        for (line, relative) in [
            (
                "cat /c/Users/dev/proj/src/store/cache.py",
                "cat src/store/cache.py",
            ),
            ("cat /C/users/dev/proj/src/a.py", "cat src/a.py"),
            ("cat /d/Users/dev/proj/a.py", "cat /d/Users/dev/proj/a.py"),
            ("cat /cd/Users/dev/proj/a.py", "cat /cd/Users/dev/proj/a.py"),
        ] {
            assert_eq!(relative_line(&payload, line), relative, "{line}");
        }
        // A Unix folder has no drive.
        assert_eq!(
            relative_line(&json!({"cwd": "/c/proj"}), "cat /c/proj/a.py"),
            "cat a.py"
        );
    }

    /// Only a word that starts with the session's folder is taken off it:
    /// the folder's path inside another path, a word or quoted text stays.
    #[test]
    fn a_folder_inside_a_word_or_quoted_text_stays() {
        let payload = json!({"cwd": "/home/u/proj"});
        assert_eq!(
            relative_line(
                &payload,
                "cat /home/u/proj/src/a.py && ls '/home/u/proj/docs'"
            ),
            "cat src/a.py && ls 'docs'"
        );
        for line in [
            "cat /mnt/home/u/proj/a.py",
            "cp a.py backup/home/u/proj/a.py",
            "git commit -m \"see /home/u/proj/src/a.py\"",
            "cat /home/u/project/a.py",
            r"cat /home/u/proj\a.py",
        ] {
            assert_eq!(relative_line(&payload, line), line);
        }
    }

    /// A Codex transcript line for call `id`'s result, as Codex 0.163.0
    /// writes it, with `output` as the command's output.
    fn completed(id: &str, status: &str, exit_code: i64, output: &str) -> String {
        json!({"timestamp": "2026-11-21T10:00:00.000Z", "type": "event_msg", "payload": {
            "type": "item_completed", "item": {"type": "CommandExecution", "id": id,
            "command": ["/bin/zsh", "-lc", "./scripts/release.sh"], "status": status,
            "aggregated_output": output, "exit_code": exit_code}}})
        .to_string()
    }

    #[test]
    fn a_codex_calls_failure_is_read_from_its_transcripts_end() {
        const CALL: &str = "exec-e0000001-0000-4000-8000-000000000001";
        const OTHER: &str = "exec-e0000002-0000-4000-8000-000000000002";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let read = |lines: &[String]| {
            std::fs::write(&path, lines.join("\n") + "\n").unwrap();
            codex_failed(&path, CALL)
        };
        let filler = |bytes: usize| {
            json!({"type": "response_item", "payload": {"type": "message", "text": "x".repeat(bytes)}})
                .to_string()
        };
        assert!(read(&[completed(
            CALL,
            "failed",
            128,
            "fatal: not a git repository"
        )]));
        assert!(
            read(&[completed(CALL, "completed", 2, "")]),
            "a nonzero exit code"
        );
        assert!(!read(&[completed(CALL, "completed", 0, "")]));
        // The output is never read: a success whose output says otherwise.
        let says = r#"{"status":"failed","exit_code":1,"type":"item_completed"}"#;
        assert!(!read(&[completed(CALL, "completed", 0, says)]));
        // Another call's failure, even one whose output names this call.
        assert!(!read(&[completed(OTHER, "failed", 1, CALL)]));
        // Found past the first step, the cut line at the read's start
        // skipped.
        let far = TRANSCRIPT_STEP as usize * 3;
        assert!(read(&[
            filler(far),
            completed(CALL, "failed", 1, ""),
            filler(far)
        ]));
        // Past the bound, or no transcript at all: a success.
        let past = TRANSCRIPT_TAIL_BYTES as usize;
        assert!(!read(&[completed(CALL, "failed", 1, ""), filler(past)]));
        assert!(!codex_failed(&dir.path().join("missing.jsonl"), CALL));
    }

    #[test]
    fn a_claude_code_failure_is_its_own_event_unless_interrupted() {
        let event = |name: &str, interrupted: bool| json!({"hook_event_name": name, "is_interrupt": interrupted});
        assert!(failed(
            HookClient::Claude,
            &event("PostToolUseFailure", false)
        ));
        assert!(!failed(
            HookClient::Claude,
            &event("PostToolUseFailure", true)
        ));
        assert!(!failed(HookClient::Claude, &event("PostToolUse", false)));
        assert!(!failed(
            HookClient::Codex,
            &event("PostToolUseFailure", false)
        ));
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
