//! Client session hooks: one loads the index of user memories when a session
//! starts or resumes, the other tells a running session before each prompt
//! what changed in memory since it last looked.
//!
//! Claude Code reads hooks from its user settings file and Codex from
//! `hooks.json` in its home; both use the same JSON shape. Edits keep every
//! unrelated setting, key order and indentation, touch only this tool's own
//! handler, and leave the file byte-identical when nothing changes.

use crate::{paths, private};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json, ser::PrettyFormatter};
use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

/// A hook this tool installs: the client event that runs it, which sources
/// of that event do, and the subcommand it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Hook {
    pub(super) event: &'static str,
    matcher: Option<&'static str>,
    subcommand: &'static str,
    /// The command bounds itself well under this; it is the client's backstop.
    timeout_seconds: u64,
    status_message: Option<&'static str>,
}

/// Loads the index into every new, cleared, compacted or resumed context: a
/// resumed transcript holds the index as it was, so it gets the current one.
pub(super) const SESSION_START: Hook = Hook {
    event: "SessionStart",
    matcher: Some("startup|resume|clear|compact"),
    subcommand: "session-start",
    timeout_seconds: 10,
    status_message: Some("Loading user memory"),
};

/// Tells a running session what changed in memory before each prompt. It
/// runs on every prompt, so it shows no status; it bounds itself to about a
/// second.
pub(super) const PROMPT_SUBMIT: Hook = Hook {
    event: "UserPromptSubmit",
    matcher: None,
    subcommand: "prompt-submit",
    timeout_seconds: 5,
    status_message: None,
};

/// Every hook this tool installs, in the order setup reports them.
pub(super) const HOOKS: [Hook; 2] = [SESSION_START, PROMPT_SUBMIT];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Change {
    Added,
    Updated,
    Removed,
    Unchanged,
}

impl Change {
    pub(super) fn describe(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Updated => "updated",
            Self::Removed => "removed",
            Self::Unchanged => "unchanged",
        }
    }
}

/// Claude Code user settings: `$CLAUDE_CONFIG_DIR/settings.json` or `~/.claude/settings.json`.
pub(super) fn claude_settings() -> Result<PathBuf> {
    Ok(config_dir("CLAUDE_CONFIG_DIR", ".claude")?.join("settings.json"))
}

/// Codex user hooks: `$CODEX_HOME/hooks.json` or `~/.codex/hooks.json`.
pub(super) fn codex_hooks() -> Result<PathBuf> {
    Ok(config_dir("CODEX_HOME", ".codex")?.join("hooks.json"))
}

/// Codex user configuration, read only to detect inline hooks.
pub(super) fn codex_config() -> Result<PathBuf> {
    Ok(config_dir("CODEX_HOME", ".codex")?.join("config.toml"))
}

pub(super) fn config_dir(variable: &str, default: &str) -> Result<PathBuf> {
    if let Some(directory) = env::var_os(variable).filter(|dir| !dir.is_empty()) {
        return Ok(directory.into());
    }
    let home = paths::home().context("Cannot locate your home directory")?;
    Ok(home.join(default))
}

/// The matcher group this tool owns for `hook` and an installed binary.
/// `client` (`claude` or `codex`) names the hook's runner, so the hook can
/// label the conversation origin and its snapshots without guessing.
pub(super) fn entry(hook: Hook, binary: &Path, client: &str) -> Value {
    entry_for(hook, binary, client, cfg!(windows))
}

/// [`entry`] as written on Windows or elsewhere. Claude Code on Windows
/// runs a hook's command through Git Bash, or PowerShell without it, so
/// there the binary is named in exec form (`command` and `args`, run
/// without any shell), with forward slashes, which every shell and Claude
/// Code itself read alike.
fn entry_for(hook: Hook, binary: &Path, client: &str, windows: bool) -> Value {
    let mut handler = if windows && client == "claude" {
        json!({
            "type": "command",
            "command": binary.to_string_lossy().replace('\\', "/"),
            "args": [hook.subcommand, "--client", client],
        })
    } else {
        json!({
            "type": "command",
            "command": command_for(hook, binary, client, windows),
        })
    };
    handler["timeout"] = json!(hook.timeout_seconds);
    if let Some(message) = hook.status_message {
        handler["statusMessage"] = json!(message);
    }
    let mut group = json!({"hooks": [handler]});
    if let Some(matcher) = hook.matcher {
        group["matcher"] = json!(matcher);
    }
    group
}

/// The command line `hook`'s entry runs, for a client that runs it through
/// a shell.
pub(super) fn command(hook: Hook, binary: &Path, client: &str) -> String {
    command_for(hook, binary, client, cfg!(windows))
}

/// [`command`] as written on Windows or elsewhere: for the POSIX shell, or
/// on Windows for cmd.exe, which Codex runs it through, with the path in
/// double quotes (a Windows path never contains one).
fn command_for(hook: Hook, binary: &Path, client: &str, windows: bool) -> String {
    let binary = if windows {
        format!("\"{}\"", binary.display())
    } else {
        crate::shell::quote(binary)
    };
    format!("{binary} {} --client {client}", hook.subcommand)
}

/// A handler written by this tool for `hook`, for any install location,
/// client or form, including the earlier one without `--client`, so a
/// handler is replaced, never doubled, when its form changes.
fn ours(hook: Hook, handler: &Value) -> bool {
    let Some(command) = handler.get("command").and_then(Value::as_str) else {
        return false;
    };
    let command = command.trim_end();
    if !command.contains("user-memory") {
        return false;
    }
    let exec = handler
        .get("args")
        .and_then(Value::as_array)
        .and_then(|args| args.first())
        .is_some_and(|first| first == hook.subcommand);
    exec || command.ends_with(&format!(" {}", hook.subcommand))
        || command.contains(&format!(" {} --client ", hook.subcommand))
}

fn groups(config: &Value, hook: Hook) -> &[Value] {
    config
        .get("hooks")
        .and_then(|hooks| hooks.get(hook.event))
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn handlers(group: &Value) -> &[Value] {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn owned_count(config: &Value, hook: Hook) -> usize {
    groups(config, hook)
        .iter()
        .flat_map(handlers)
        .filter(|handler| ours(hook, handler))
        .count()
}

/// True when the wanted group is present and is this tool's only handler
/// for `hook`.
fn installed(config: &Value, hook: Hook, wanted: &Value) -> bool {
    owned_count(config, hook) == 1 && groups(config, hook).contains(wanted)
}

/// Makes `wanted` this tool's only handler for `hook`, or removes this
/// tool's handlers for it when `wanted` is `None`. Returns whether anything
/// changed.
fn merge(config: &mut Value, hook: Hook, wanted: Option<&Value>, path: &Path) -> Result<bool> {
    match wanted {
        Some(wanted) if installed(config, hook, wanted) => return Ok(false),
        None if owned_count(config, hook) == 0 => return Ok(false),
        _ => {}
    }
    let shape = || {
        format!(
            "{} has an unexpected hooks layout; it was left unchanged",
            path.display()
        )
    };
    let root = config.as_object_mut().with_context(shape)?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .with_context(shape)?;
    let list = hooks
        .entry(hook.event)
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .with_context(shape)?;
    list.retain_mut(|group| {
        let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = handlers.len();
        handlers.retain(|handler| !ours(hook, handler));
        // Drop a group only when removing this tool's handler emptied it.
        !(handlers.is_empty() && before > 0)
    });
    if let Some(wanted) = wanted {
        list.push(wanted.clone());
    }
    if list.is_empty() {
        hooks.remove(hook.event);
    }
    if hooks.is_empty() {
        root.remove("hooks");
    }
    Ok(true)
}

fn read(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("Cannot read {}", path.display())),
    }
}

fn parse(path: &Path, text: Option<&str>) -> Result<Value> {
    match text {
        Some(text) if !text.trim().is_empty() => {
            let config: Value = serde_json::from_str(text).with_context(|| {
                format!(
                    "{} is not valid JSON; fix it and run setup again. It was left unchanged",
                    path.display()
                )
            })?;
            if !config.is_object() {
                bail!(
                    "{} must contain a JSON object; it was left unchanged",
                    path.display()
                );
            }
            Ok(config)
        }
        _ => Ok(json!({})),
    }
}

/// Installs (`Some`) or removes (`None`) this tool's `hook` in `path`.
pub(super) fn apply(path: &Path, hook: Hook, wanted: Option<&Value>) -> Result<Change> {
    let original = read(path)?;
    if original.is_none() && wanted.is_none() {
        return Ok(Change::Unchanged);
    }
    let mut config = parse(path, original.as_deref())?;
    let had = owned_count(&config, hook) > 0;
    if !merge(&mut config, hook, wanted, path)? {
        return Ok(Change::Unchanged);
    }
    write(path, &config, original.as_deref())?;
    Ok(match (wanted, had) {
        (None, _) => Change::Removed,
        (Some(_), false) => Change::Added,
        (Some(_), true) => Change::Updated,
    })
}

/// Whether `path` holds this tool's `hook` in any form.
pub(super) fn present(path: &Path, hook: Hook) -> Result<bool> {
    let Some(text) = read(path)? else {
        return Ok(false);
    };
    Ok(owned_count(&parse(path, Some(&text))?, hook) > 0)
}

/// True when `path` holds exactly this tool's wanted `hook`.
pub(super) fn verify(path: &Path, hook: Hook, wanted: &Value) -> Result<bool> {
    let original = read(path)?;
    Ok(installed(&parse(path, original.as_deref())?, hook, wanted))
}

/// Serializes with the original indentation and trailing newline, then
/// replaces the file atomically. A symlinked file is updated at its target.
fn write(path: &Path, config: &Value, original: Option<&str>) -> Result<()> {
    let target = if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        fs::canonicalize(path).with_context(|| format!("Cannot resolve {}", path.display()))?
    } else {
        path.to_path_buf()
    };
    let indent = original
        .and_then(|text| {
            text.lines().skip(1).find_map(|line| {
                let width = line.len() - line.trim_start_matches([' ', '\t']).len();
                (width > 0).then(|| line[..width].to_owned())
            })
        })
        .unwrap_or_else(|| "  ".into());
    let mut bytes = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(
        &mut bytes,
        PrettyFormatter::with_indent(indent.as_bytes()),
    );
    config.serialize(&mut serializer)?;
    if original.is_none_or(|text| text.ends_with('\n')) {
        bytes.push(b'\n');
    }
    let parent = target.parent().context("Invalid configuration path")?;
    fs::create_dir_all(parent).with_context(|| format!("Cannot create {}", parent.display()))?;
    let name = target
        .file_name()
        .context("Invalid configuration path")?
        .to_string_lossy();
    let temporary = parent.join(format!(".{name}.user-memory-{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = private::create_file(
            fs::OpenOptions::new().write(true).create_new(true),
            &temporary,
        )?;
        file.write_all(&bytes)?;
        if let Ok(existing) = fs::metadata(&target) {
            file.set_permissions(existing.permissions())?;
        }
        file.sync_all()?;
        fs::rename(&temporary, &target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("Cannot update {}", target.display()))
}

/// Warns when Codex's config.toml also defines hooks inline, which Codex
/// merges with hooks.json. Reads config.toml; never writes it.
pub(super) fn codex_inline_warning(config: &Path) -> Result<Option<String>> {
    let Some(text) = read(config)? else {
        return Ok(None);
    };
    let duplicate = text.lines().any(|line| {
        line.contains("user-memory") && HOOKS.iter().any(|hook| line.contains(hook.subcommand))
    });
    if duplicate {
        return Ok(Some(format!(
            "{} also defines a user-memory hook, so Codex runs it twice; remove that copy",
            config.display()
        )));
    }
    let inline = text.lines().map(str::trim).any(|line| {
        line == "[hooks]"
            || line.starts_with("[[hooks.")
            || (line.starts_with("[hooks.") && !line.starts_with("[hooks.state"))
    });
    Ok(inline.then(|| {
        format!(
            "{} also defines inline hooks; Codex merges them with hooks.json and warns at startup",
            config.display()
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const BINARY: &str = "/opt/user memory/bin/user-memory";

    fn wanted() -> Value {
        entry(SESSION_START, Path::new(BINARY), "claude")
    }

    /// A realistic user settings file: other hooks, permissions, env and
    /// unknown keys in non-alphabetical order with four-space indentation.
    const SETTINGS: &str = r#"{
    "permissions": {
        "allow": ["Bash(cargo test:*)"],
        "deny": ["Read(./.env)"]
    },
    "env": {"RUST_LOG": "info"},
    "model": "opus",
    "hooks": {
        "PreToolUse": [
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "rtk hook claude"}]}
        ],
        "SessionStart": [
            {"matcher": "startup", "hooks": [
                {"type": "command", "command": "echo hello"},
                {"type": "command", "command": "'/old/place/user-memory' session-start", "timeout": 30}
            ]},
            {"hooks": [{"type": "command", "command": "/old/user-memory session-start"}]},
            {"matcher": "resume", "hooks": []}
        ]
    },
    "zzUnknownSetting": {"nested": [3, 1, 2]},
    "aaLater": true
}
"#;

    /// The macOS and Linux form, pinned on every platform; the Windows
    /// forms have their own test.
    #[test]
    fn entries_quote_the_binary_and_cover_new_and_resumed_sessions() {
        assert_eq!(
            entry_for(SESSION_START, Path::new(BINARY), "claude", false),
            json!({"matcher":"startup|resume|clear|compact","hooks":[{"type":"command","command":"'/opt/user memory/bin/user-memory' session-start --client claude","timeout":10,"statusMessage":"Loading user memory"}]})
        );
        assert_eq!(
            entry_for(PROMPT_SUBMIT, Path::new(BINARY), "codex", false),
            json!({"hooks":[{"type":"command","command":"'/opt/user memory/bin/user-memory' prompt-submit --client codex","timeout":5}]})
        );
    }

    /// The example in examples/ is the macOS and Linux form.
    #[test]
    fn the_documented_example_matches_what_setup_writes() {
        let example: Value =
            serde_json::from_str(include_str!("../../examples/claude-hooks.json")).unwrap();
        let binary = Path::new("/ABS/PATH/TO/user-memory");
        assert_eq!(
            example,
            json!({"hooks":{
                "SessionStart":[entry_for(SESSION_START, binary, "claude", false)],
                "UserPromptSubmit":[entry_for(PROMPT_SUBMIT, binary, "claude", false)]
            }})
        );
    }

    #[test]
    fn each_hook_is_installed_verified_and_removed_on_its_own() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let prompt = entry(PROMPT_SUBMIT, Path::new(BINARY), "claude");
        assert_eq!(
            apply(&path, SESSION_START, Some(&wanted())).unwrap(),
            Change::Added
        );
        assert!(!verify(&path, PROMPT_SUBMIT, &prompt).unwrap());
        assert_eq!(
            apply(&path, PROMPT_SUBMIT, Some(&prompt)).unwrap(),
            Change::Added
        );
        assert!(verify(&path, SESSION_START, &wanted()).unwrap());
        assert!(verify(&path, PROMPT_SUBMIT, &prompt).unwrap());
        // A session hook's handler is never taken for the prompt hook's: the
        // prompt hook's handler, in whichever form (command, or command and
        // arguments), placed in the session group.
        let mut misplaced = wanted();
        misplaced["hooks"][0] = prompt["hooks"][0].clone();
        assert!(!ours(SESSION_START, &misplaced["hooks"][0]));
        assert!(ours(PROMPT_SUBMIT, &misplaced["hooks"][0]));
        assert_eq!(apply(&path, PROMPT_SUBMIT, None).unwrap(), Change::Removed);
        let config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(config, json!({"hooks":{"SessionStart":[wanted()]}}));
        assert_eq!(apply(&path, SESSION_START, None).unwrap(), Change::Removed);
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}\n");
    }

    #[test]
    fn merging_replaces_only_earlier_entries_and_keeps_everything_else() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        fs::write(&path, SETTINGS).unwrap();
        assert_eq!(
            apply(&path, SESSION_START, Some(&wanted())).unwrap(),
            Change::Updated
        );
        let text = fs::read_to_string(&path).unwrap();
        let config: Value = serde_json::from_str(&text).unwrap();
        let keys: Vec<_> = config.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            [
                "permissions",
                "env",
                "model",
                "hooks",
                "zzUnknownSetting",
                "aaLater"
            ]
        );
        let mut expected: Value = serde_json::from_str(SETTINGS).unwrap();
        expected["hooks"]["SessionStart"] = json!([
            {"matcher": "startup", "hooks": [{"type": "command", "command": "echo hello"}]},
            {"matcher": "resume", "hooks": []},
            wanted()
        ]);
        assert_eq!(config, expected);
        assert!(text.starts_with("{\n    \"permissions\": {\n        \"allow\""));
        assert!(text.ends_with("}\n"));
        assert!(verify(&path, SESSION_START, &wanted()).unwrap());

        let before = fs::read(&path).unwrap();
        assert_eq!(
            apply(&path, SESSION_START, Some(&wanted())).unwrap(),
            Change::Unchanged
        );
        assert_eq!(fs::read(&path).unwrap(), before, "re-run must not rewrite");

        assert_eq!(apply(&path, SESSION_START, None).unwrap(), Change::Removed);
        let after = fs::read(&path).unwrap();
        assert_eq!(
            apply(&path, SESSION_START, None).unwrap(),
            Change::Unchanged
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            after,
            "removing nothing must not rewrite"
        );
        let removed: Value = serde_json::from_slice(&after).unwrap();
        assert_eq!(
            removed["hooks"]["SessionStart"],
            json!([
                {"matcher": "startup", "hooks": [{"type": "command", "command": "echo hello"}]},
                {"matcher": "resume", "hooks": []}
            ])
        );
        assert!(!verify(&path, SESSION_START, &wanted()).unwrap());
    }

    #[test]
    fn new_files_get_only_the_hook_and_removal_leaves_no_empty_tables() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("missing/dir/hooks.json");
        assert_eq!(
            apply(&path, SESSION_START, None).unwrap(),
            Change::Unchanged
        );
        assert!(!path.exists());
        assert_eq!(
            apply(&path, SESSION_START, Some(&wanted())).unwrap(),
            Change::Added
        );
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("{\n  \"hooks\": {\n    \"SessionStart\""));
        assert!(text.ends_with("}\n"));
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            json!({"hooks":{"SessionStart":[wanted()]}})
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(apply(&path, SESSION_START, None).unwrap(), Change::Removed);
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}\n");
        let empty = tmp.path().join("empty.json");
        fs::write(&empty, "  \n").unwrap();
        assert_eq!(
            apply(&empty, SESSION_START, Some(&wanted())).unwrap(),
            Change::Added
        );
    }

    #[test]
    fn existing_files_keep_permissions_tabs_and_missing_trailing_newline() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        fs::write(&path, "{\n\t\"theme\": \"dark\"\n}").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        apply(&path, SESSION_START, Some(&wanted())).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("{\n\t\"theme\": \"dark\",\n\t\"hooks\": {\n\t\t"));
        assert!(text.ends_with('}'));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o644
            );
        }
        assert!(
            fs::read_dir(tmp.path()).unwrap().count() == 1,
            "no temporary files left"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_settings_are_updated_in_place_of_the_link_target() {
        let tmp = tempdir().unwrap();
        let real = tmp.path().join("dotfiles/settings.json");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, "{}\n").unwrap();
        let link = tmp.path().join("settings.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        apply(&link, SESSION_START, Some(&wanted())).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(verify(&real, SESSION_START, &wanted()).unwrap());
    }

    #[test]
    fn unreadable_paths_are_errors_not_missing_files() {
        let tmp = tempdir().unwrap();
        let err = format!(
            "{:#}",
            apply(tmp.path(), SESSION_START, Some(&wanted())).unwrap_err()
        );
        assert!(err.contains("Cannot read"), "{err}");
        assert!(tmp.path().is_dir());
    }

    #[test]
    fn unindented_lines_fall_back_to_two_space_indentation() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        fs::write(&path, "{\n\"theme\": \"dark\"\n}\n").unwrap();
        apply(&path, SESSION_START, Some(&wanted())).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("{\n  \"theme\": \"dark\",\n  \"hooks\""),
            "{text}"
        );
    }

    #[test]
    fn invalid_or_unexpected_files_are_refused_and_left_unchanged() {
        let tmp = tempdir().unwrap();
        for (text, message) in [
            ("{\"hooks\": {", "is not valid JSON"),
            ("[]", "must contain a JSON object"),
            ("{\"hooks\": []}", "unexpected hooks layout"),
            (
                "{\"hooks\": {\"SessionStart\": {}}}",
                "unexpected hooks layout",
            ),
        ] {
            let path = tmp.path().join("settings.json");
            fs::write(&path, text).unwrap();
            let err = format!(
                "{:#}",
                apply(&path, SESSION_START, Some(&wanted())).unwrap_err()
            );
            assert!(err.contains(message), "{text}: {err}");
            assert!(err.contains("left unchanged"), "{text}: {err}");
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
        }
    }

    #[test]
    fn the_earlier_form_without_client_is_migrated_on_rerun() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("hooks.json");
        let mut old = wanted();
        old["hooks"][0]["command"] = json!(format!("'{BINARY}' session-start"));
        fs::write(&path, json!({"hooks":{"SessionStart":[old]}}).to_string()).unwrap();
        assert!(!verify(&path, SESSION_START, &wanted()).unwrap());
        assert_eq!(
            apply(&path, SESSION_START, Some(&wanted())).unwrap(),
            Change::Updated
        );
        let config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(config, json!({"hooks":{"SessionStart":[wanted()]}}));
        let codex = entry(SESSION_START, Path::new(BINARY), "codex");
        assert_eq!(
            apply(&path, SESSION_START, Some(&codex)).unwrap(),
            Change::Updated
        );
        let config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(config, json!({"hooks":{"SessionStart":[codex]}}));
        assert!(!ours(
            SESSION_START,
            &json!({"command": "user-memory session-start-other"})
        ));
        assert!(!ours(
            SESSION_START,
            &json!({"command": "other session-start --client claude"})
        ));
    }

    /// Windows forms, built on every platform: Claude Code's in exec form
    /// with forward slashes, Codex's as a cmd.exe line.
    #[test]
    fn windows_hooks_run_without_a_posix_shell() {
        let binary =
            Path::new(r"C:\Users\dev\AppData\Local\Programs\user-memory\bin\user-memory.exe");
        assert_eq!(
            entry_for(SESSION_START, binary, "claude", true),
            json!({"matcher":"startup|resume|clear|compact","hooks":[{
                "type":"command",
                "command":"C:/Users/dev/AppData/Local/Programs/user-memory/bin/user-memory.exe",
                "args":["session-start","--client","claude"],
                "timeout":10,
                "statusMessage":"Loading user memory"}]})
        );
        assert_eq!(
            entry_for(PROMPT_SUBMIT, binary, "codex", true),
            json!({"hooks":[{
                "type":"command",
                "command":r#""C:\Users\dev\AppData\Local\Programs\user-memory\bin\user-memory.exe" prompt-submit --client codex"#,
                "timeout":5}]})
        );
        // Elsewhere both clients keep the shell form.
        assert_eq!(
            entry_for(SESSION_START, Path::new(BINARY), "claude", false),
            wanted_on_unix()
        );
    }

    fn wanted_on_unix() -> Value {
        json!({"matcher":"startup|resume|clear|compact","hooks":[{"type":"command","command":"'/opt/user memory/bin/user-memory' session-start --client claude","timeout":10,"statusMessage":"Loading user memory"}]})
    }

    /// A host that moves between the forms, or an entry from 0.6, gets its
    /// handler replaced, never a second one.
    #[test]
    fn a_handler_in_the_other_form_is_replaced() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let binary = Path::new(r"C:\Program Files\user-memory.exe");
        let exec = entry_for(SESSION_START, binary, "claude", true);
        let shell = entry_for(SESSION_START, binary, "claude", false);
        fs::write(&path, json!({"hooks":{"SessionStart":[shell]}}).to_string()).unwrap();
        assert_eq!(
            apply(&path, SESSION_START, Some(&exec)).unwrap(),
            Change::Updated
        );
        let config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(config, json!({"hooks":{"SessionStart":[exec.clone()]}}));
        assert!(verify(&path, SESSION_START, &exec).unwrap());
        let shell = entry_for(SESSION_START, binary, "claude", false);
        assert_eq!(
            apply(&path, SESSION_START, Some(&shell)).unwrap(),
            Change::Updated
        );
        let config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(config, json!({"hooks":{"SessionStart":[shell]}}));
        // Another tool's exec-form handler is not ours.
        assert!(!ours(
            SESSION_START,
            &json!({"command": "C:/tools/other.exe", "args": ["session-start"]})
        ));
        assert!(!ours(
            SESSION_START,
            &json!({"command": "C:/tools/user-memory.exe", "args": ["prompt-submit"]})
        ));
    }

    #[test]
    fn verification_needs_exactly_one_current_handler() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("hooks.json");
        assert!(!verify(&path, SESSION_START, &wanted()).unwrap());
        let stale = entry(SESSION_START, Path::new("/elsewhere/user-memory"), "claude");
        fs::write(&path, json!({"hooks":{"SessionStart":[stale]}}).to_string()).unwrap();
        assert!(!verify(&path, SESSION_START, &wanted()).unwrap());
        fs::write(
            &path,
            json!({"hooks":{"SessionStart":[wanted(), wanted()]}}).to_string(),
        )
        .unwrap();
        assert!(!verify(&path, SESSION_START, &wanted()).unwrap());
        assert_eq!(
            apply(&path, SESSION_START, Some(&wanted())).unwrap(),
            Change::Updated
        );
        assert!(verify(&path, SESSION_START, &wanted()).unwrap());
        let mut other = wanted();
        other["hooks"][0]["command"] = json!("/usr/bin/other session-start");
        fs::write(
            &path,
            json!({"hooks":{"SessionStart":[wanted(), other]}}).to_string(),
        )
        .unwrap();
        assert!(
            verify(&path, SESSION_START, &wanted()).unwrap(),
            "foreign handlers are not ours"
        );
    }

    #[test]
    fn codex_inline_hooks_and_duplicates_are_reported_without_writing() {
        let tmp = tempdir().unwrap();
        let config = tmp.path().join("config.toml");
        assert_eq!(codex_inline_warning(&config).unwrap(), None);
        for (text, expected) in [
            (
                "# comment\nmodel = \"gpt\"\n[hooks.state.\"a\"]\ntrusted_hash = \"x\"\n",
                None,
            ),
            (
                "[mcp_servers.user-memory]\ncommand = \"/bin/user-memory\"\n# session-start\n",
                None,
            ),
            (
                "[[hooks.PreToolUse]]\nmatcher = \"Bash\"\n",
                Some("defines inline hooks"),
            ),
            ("[hooks]\n", Some("defines inline hooks")),
            ("[hooks.SessionStart]\n", Some("defines inline hooks")),
            (
                "[[hooks.SessionStart.hooks]]\ncommand = \"'/bin/user-memory' session-start\"\n",
                Some("runs it twice"),
            ),
        ] {
            fs::write(&config, text).unwrap();
            let warning = codex_inline_warning(&config).unwrap();
            match expected {
                None => assert_eq!(warning, None, "{text}"),
                Some(part) => assert!(warning.unwrap().contains(part), "{text}"),
            }
            assert_eq!(fs::read_to_string(&config).unwrap(), text);
        }
    }
}
