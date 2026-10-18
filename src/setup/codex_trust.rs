//! Whether Codex will run the user-memory hooks, as Codex itself reports it.
//!
//! Codex runs a user hook only after the user approves it in `/hooks`; it
//! stores the approval as `[hooks.state."<key>"] trusted_hash` in config.toml
//! and compares it with a hash of the hook's command, matcher, timeout and
//! status message. Rather than re-derive that hash, this asks the installed
//! Codex through its app-server `hooks/list` method, which reports the same
//! trust status Codex uses when deciding to run the hook. Read only: nothing
//! here writes Codex configuration or trust state.

use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

/// Long enough for a cold start; the app-server answered in ~0.25 s.
const WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Trust {
    /// Approved by the user, or trusted by managed policy.
    Approved,
    /// Never approved; Codex skips it.
    Unapproved,
    /// Approved before, but the hook changed since (e.g. the binary moved).
    Modified,
    /// Codex's answer could not establish the status.
    Unverifiable(String),
}

/// The exact one-time step that makes Codex run the hooks.
pub(super) const APPROVE: &str = "open Codex and approve them once: at startup Codex says hooks need review, or run /hooks and choose Trust for user-memory's SessionStart and UserPromptSubmit hooks";

/// Asks `codex app-server` for the trust status of the hooks running
/// `commands` from `hooks_file`: approved only when every one is, otherwise
/// the first one's status that is not.
pub(super) fn trust(codex: &Path, hooks_file: &Path, commands: &[String]) -> Trust {
    match hooks_list(codex) {
        Ok(reply) => combined(&reply, hooks_file, commands),
        Err(reason) => Trust::Unverifiable(reason),
    }
}

/// The trust status of the hooks running `commands` in a `hooks/list`
/// reply: approved only when every one is.
fn combined(reply: &Value, hooks_file: &Path, commands: &[String]) -> Trust {
    commands
        .iter()
        .map(|command| status(reply, hooks_file, command))
        .find(|trust| *trust != Trust::Approved)
        .unwrap_or(Trust::Approved)
}

fn hooks_list(codex: &Path) -> Result<Value, String> {
    let mut child = Command::new(codex)
        .arg("app-server")
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("cannot start Codex: {err}"))?;
    let mut input = child.stdin.take().ok_or("no Codex input")?;
    let output = child.stdout.take().ok_or("no Codex output")?;
    let (lines, replies) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let cwd = std::env::temp_dir();
    let requests = [
        json!({"id":0,"method":"initialize","params":{"clientInfo":{"name":"user-memory","version":env!("CARGO_PKG_VERSION")}}}),
        json!({"method":"initialized"}),
        json!({"id":1,"method":"hooks/list","params":{"cwds":[cwd]}}),
    ];
    let sent = requests
        .iter()
        .try_for_each(|request| writeln!(input, "{request}"))
        .and_then(|()| input.flush());
    // Test builds give a loaded machine room, as for the hooks.
    let deadline = std::time::Instant::now() + crate::session::hook_time(WAIT);
    let reply = sent
        .map_err(|err| format!("cannot talk to Codex: {err}"))
        .and_then(|()| {
            loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                let line = replies
                    .recv_timeout(left)
                    .map_err(|_| "Codex did not answer hooks/list".to_owned())?;
                if let Ok(message) = serde_json::from_str::<Value>(&line)
                    && message["id"] == 1
                {
                    break Ok(message);
                }
            }
        });
    drop(input);
    let _ = child.kill();
    let _ = child.wait();
    reply
}

/// Trust status of the matching hook in a `hooks/list` reply.
fn status(reply: &Value, hooks_file: &Path, command: &str) -> Trust {
    let Some(entries) = reply["result"]["data"].as_array() else {
        return Trust::Unverifiable(match reply.get("error") {
            Some(error) => format!("Codex refused hooks/list: {error}"),
            None => "Codex sent no hook list".into(),
        });
    };
    let same_file = |path: &str| {
        Path::new(path) == hooks_file
            || std::fs::canonicalize(path)
                .is_ok_and(|path| std::fs::canonicalize(hooks_file).is_ok_and(|file| path == file))
    };
    let found = entries
        .iter()
        .filter_map(|entry| entry["hooks"].as_array())
        .flatten()
        .find(|hook| {
            hook["command"] == command && hook["sourcePath"].as_str().is_some_and(same_file)
        });
    let Some(hook) = found else {
        return Trust::Unverifiable(
            "Codex does not list the hook; check that hooks are enabled in Codex".into(),
        );
    };
    match hook["trustStatus"].as_str() {
        Some("trusted" | "managed") => Trust::Approved,
        Some("untrusted") => Trust::Unapproved,
        Some("modified") => Trust::Modified,
        other => Trust::Unverifiable(format!("unknown Codex trust status {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMAND: &str = "'/opt/um/user-memory' session-start --client codex";

    fn reply(hooks: Value) -> Value {
        json!({"id":1,"result":{"data":[{"cwd":"/tmp","hooks":hooks,"warnings":[],"errors":[]}]}})
    }

    fn hook(command: &str, source: &str, trust: &str) -> Value {
        json!({"key":format!("{source}:session_start:0:0"),"eventName":"sessionStart","handlerType":"command",
            "command":command,"sourcePath":source,"source":"user","trustStatus":trust,
            "currentHash":"sha256:00","enabled":true,"isManaged":false,"displayOrder":0,"timeoutSec":10})
    }

    #[test]
    fn statuses_map_to_what_codex_will_do() {
        let file = Path::new("/home/u/.codex/hooks.json");
        for (status, expected) in [
            ("trusted", Trust::Approved),
            ("managed", Trust::Approved),
            ("untrusted", Trust::Unapproved),
            ("modified", Trust::Modified),
        ] {
            let listed = reply(json!([
                hook(
                    "other session-start",
                    "/home/u/.codex/hooks.json",
                    "untrusted"
                ),
                hook(COMMAND, "/repo/.codex/hooks.json", "untrusted"),
                hook(COMMAND, "/home/u/.codex/hooks.json", status),
            ]));
            assert_eq!(super::status(&listed, file, COMMAND), expected, "{status}");
        }
    }

    #[test]
    fn hooks_count_as_approved_only_when_every_one_is() {
        let file = Path::new("/home/u/.codex/hooks.json");
        let prompt = "'/opt/um/user-memory' prompt-submit --client codex";
        let commands = [COMMAND.to_owned(), prompt.to_owned()];
        let listed = |start: &str, submit: &str| {
            reply(json!([
                hook(COMMAND, "/home/u/.codex/hooks.json", start),
                hook(prompt, "/home/u/.codex/hooks.json", submit),
            ]))
        };
        assert_eq!(
            combined(&listed("trusted", "trusted"), file, &commands),
            Trust::Approved
        );
        assert_eq!(
            combined(&listed("trusted", "untrusted"), file, &commands),
            Trust::Unapproved
        );
        assert_eq!(
            combined(&listed("modified", "trusted"), file, &commands),
            Trust::Modified
        );
        let only_start = reply(json!([hook(
            COMMAND,
            "/home/u/.codex/hooks.json",
            "trusted"
        )]));
        assert!(matches!(
            combined(&only_start, file, &commands),
            Trust::Unverifiable(_)
        ));
    }

    #[test]
    fn unclear_answers_are_unverifiable_not_failures() {
        let file = Path::new("/home/u/.codex/hooks.json");
        for listed in [
            reply(json!([])),
            reply(json!([hook(
                "'/elsewhere/user-memory' session-start --client codex",
                "/home/u/.codex/hooks.json",
                "trusted"
            )])),
            reply(json!([hook(
                COMMAND,
                "/home/u/.codex/hooks.json",
                "pending"
            )])),
            json!({"id":1,"error":{"code":-32601,"message":"Method not found"}}),
            json!({"id":1,"result":{}}),
        ] {
            assert!(
                matches!(
                    super::status(&listed, file, COMMAND),
                    Trust::Unverifiable(_)
                ),
                "{listed}"
            );
        }
    }

    // Unix only: a symlink on Windows needs Developer Mode or an
    // administrator, which CI runners do not give a test.
    #[cfg(unix)]
    #[test]
    fn the_hook_file_is_matched_through_symlinks_but_not_across_files() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("hooks.json"), "{}").unwrap();
        std::fs::write(real.join("other.json"), "{}").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let ours = link.join("hooks.json");
        let listed = |path: &Path| reply(json!([hook(COMMAND, path.to_str().unwrap(), "trusted")]));
        assert_eq!(
            super::status(&listed(&real.join("hooks.json")), &ours, COMMAND),
            Trust::Approved
        );
        assert!(matches!(
            super::status(&listed(&real.join("other.json")), &ours, COMMAND),
            Trust::Unverifiable(_)
        ));
    }

    #[test]
    fn a_missing_codex_is_unverifiable() {
        let trust = trust(
            Path::new("/nonexistent/codex"),
            Path::new("/x/hooks.json"),
            &[COMMAND.to_owned()],
        );
        assert!(
            matches!(trust, Trust::Unverifiable(reason) if reason.starts_with("cannot start Codex"))
        );
    }
}
