//! Local installation and client registration. Client CLIs own their MCP
//! configuration; session hooks are merged into the clients' user files.

use crate::{paths, private};
use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::Duration,
};

mod codex_trust;
mod embedding;
pub mod git_ignore;
mod hooks;
mod instructions;
mod path_env;
pub(crate) mod removal;
mod skill;
pub mod uninstall;
mod update;
mod versions;

pub(crate) use embedding::docker_dir;

const SERVER: &str = "user-memory";

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Client {
    Auto,
    Codex,
    Claude,
    Both,
}

#[derive(Args)]
pub struct Options {
    /// Shared remote MCP endpoint; omit for a local server.
    #[arg(long, requires = "token_file", conflicts_with = "db")]
    url: Option<String>,
    /// Local credential file for the shared remote server.
    #[arg(long, requires = "url")]
    token_file: Option<PathBuf>,
    /// Connect all detected clients (auto, the default), or select a
    /// specific client. Given on a server-only install, it makes this
    /// machine a client too.
    #[arg(long, value_enum)]
    client: Option<Client>,
    /// For a host that runs the memory server and no agent app: setup
    /// installs the binary for the server it is given and connects no
    /// client, writing no hooks, skill, instructions or git-ignore lines,
    /// and remembers that; doctor checks only the server side.
    #[arg(long, conflicts_with = "client")]
    server_only: bool,
    /// Shared SQLite database (absolute path).
    #[arg(long)]
    db: Option<PathBuf>,
    /// Stable binary directory (defaults to ~/.local/bin).
    #[arg(long)]
    install_dir: Option<PathBuf>,
    /// Stop loading memory through client session hooks: setup removes its
    /// hooks and remembers the choice, so later runs and doctor respect it.
    #[arg(long, conflicts_with = "hooks")]
    no_hooks: bool,
    /// Load memory through client session hooks again after --no-hooks.
    #[arg(long)]
    hooks: bool,
    /// Do not install the user-memory skill: setup removes the skill it
    /// installed and remembers the choice, so later runs and doctor respect it.
    #[arg(long, conflicts_with = "skill")]
    no_skill: bool,
    /// Install the user-memory skill again after --no-skill.
    #[arg(long)]
    skill: bool,
    /// Keep the user's global rules out of the clients' instruction files:
    /// setup removes them and remembers the choice, so later runs, writes,
    /// session starts and doctor respect it.
    #[arg(long, conflicts_with = "instructions")]
    no_instructions: bool,
    /// Write the user's global rules into the clients' instruction files
    /// again after --no-instructions.
    #[arg(long)]
    instructions: bool,
    /// Stop doctor from checking for a newer release: setup remembers the
    /// choice. USER_MEMORY_NO_UPDATE_CHECK=1 does the same for one run.
    #[arg(long, conflicts_with = "update_check")]
    no_update_check: bool,
    /// Let doctor check for a newer release again after --no-update-check.
    #[arg(long)]
    update_check: bool,
    /// Dotenv file with USER_MEMORY_EMBED_* settings to save for a local
    /// install; other keys are ignored and the environment overrides it.
    #[arg(long)]
    env_file: Option<PathBuf>,
    /// JSON object mapping each memory id to its title, or `-` for stdin,
    /// for upgrading a local store to schema version 3; a server's store is
    /// upgraded on the server.
    #[arg(long, value_name = "FILE", conflicts_with = "url")]
    titles: Option<PathBuf>,
}

struct Paths {
    binary: PathBuf,
    db: PathBuf,
    remote: Option<crate::remote::Connection>,
}

impl Paths {
    fn environment(&self) -> Vec<(String, String)> {
        if let Some(remote) = &self.remote {
            vec![
                ("USER_MEMORY_URL".into(), remote.url.clone()),
                (
                    "USER_MEMORY_TOKEN_FILE".into(),
                    remote.token_file.to_string_lossy().into_owned(),
                ),
            ]
        } else {
            vec![(
                "USER_MEMORY_DB".into(),
                self.db.to_string_lossy().into_owned(),
            )]
        }
    }
}

const RECEIPT: &str = ".user-memory.json";

#[derive(Serialize, Deserialize)]
struct Installation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    db: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remote: Option<crate::remote::Connection>,
    clients: Vec<Client>,
    #[serde(default = "enabled")]
    hooks: bool,
    #[serde(default = "enabled")]
    skill: bool,
    #[serde(default = "enabled")]
    instructions: bool,
    /// Whether doctor checks for a newer release.
    #[serde(default = "enabled")]
    update_check: bool,
    /// A host that runs the memory server and no agent app.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    server_only: bool,
    /// Setup added the bin folder to the user PATH (Windows), so uninstall
    /// removes that entry and no other.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    path_added: bool,
    /// Embedding settings for a local install; the binary reads the
    /// environment first. API keys stay in files named here, never inline.
    #[serde(default, skip_serializing_if = "embedding::Settings::is_empty")]
    embedding: embedding::Settings,
}

fn enabled() -> bool {
    true
}

fn installation(directory: &Path) -> Result<Option<Installation>> {
    let path = directory.join(RECEIPT);
    match fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes)
                .context("Saved setup is invalid; run setup with --db to repair it")?,
        )),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).context("Cannot read saved setup"),
    }
}

fn current_directory() -> Option<PathBuf> {
    env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// An embedding setting for this process: the environment first, then the
/// settings saved by setup beside this binary.
pub fn embedding_setting(name: &str) -> Option<String> {
    static SAVED: std::sync::OnceLock<embedding::Settings> = std::sync::OnceLock::new();
    let saved = SAVED.get_or_init(|| {
        current_directory()
            .and_then(|dir| installation(&dir).ok().flatten())
            .map(|saved| saved.embedding)
            .unwrap_or_default()
    });
    embedding::lookup(saved, name).map(|(value, _)| value)
}

/// Every embedding setting in effect, environment first, then the saved
/// setup; blank ones are left out.
pub fn embedding_settings() -> std::collections::BTreeMap<String, String> {
    embedding::SETTINGS
        .iter()
        .filter_map(|name| {
            embedding_setting(name)
                .filter(|value| !value.trim().is_empty())
                .map(|value| ((*name).to_owned(), value))
        })
        .collect()
}

pub use embedding::Home as EmbeddingHome;

/// Where this install's store keeps its embedding setting: beside this
/// binary for a store on this machine, the installer's Docker server, or
/// another server that owns it.
pub fn embedding_home() -> Result<EmbeddingHome> {
    Ok(match configured_remote()? {
        None => EmbeddingHome::Local,
        Some(remote) => match embedding::docker_dir(&remote.url) {
            Some(dir) => EmbeddingHome::Docker(dir),
            None => EmbeddingHome::Remote(remote.url),
        },
    })
}

/// Saves `settings` (empty for keyword search only) where the store lives
/// and applies them, returning what to tell the user. A store on this
/// machine keeps them in the saved setup; the installer's Docker server is
/// recreated with them and calibrates and re-embeds itself; another server
/// is refused, as it owns its setting.
pub fn apply_embedding(
    home: &EmbeddingHome,
    settings: &std::collections::BTreeMap<String, String>,
) -> Result<String> {
    crate::memory::embedding_model(&|name| settings.get(name).cloned())
        .context("Embedding settings are invalid; nothing was changed")?;
    match home {
        EmbeddingHome::Local => {
            let directory = current_directory().context("Cannot locate this binary")?;
            let mut saved = installation(&directory)?
                .context("No saved setup beside this binary; run user-memory setup first")?;
            saved.embedding = settings.clone();
            write_installation(&directory, &saved)?;
            let shadowed: Vec<&str> = embedding::SETTINGS
                .into_iter()
                .filter(|name| env::var_os(name).is_some())
                .collect();
            Ok(if shadowed.is_empty() {
                format!("Saved in {}", directory.join(RECEIPT).display())
            } else {
                format!(
                    "Saved in {}; {} set in this environment take precedence over it",
                    directory.join(RECEIPT).display(),
                    shadowed.join(", ")
                )
            })
        }
        EmbeddingHome::Docker(dir) => embedding::apply_docker(dir, settings),
        EmbeddingHome::Remote(url) => bail!(
            "{url} is a memory server that owns its embedding setting; run `user-memory embedding set` on that server's machine"
        ),
    }
}

pub fn configured_db() -> Option<PathBuf> {
    installation(&current_directory()?).ok()??.db
}

pub fn configured_remote() -> Result<Option<crate::remote::Connection>> {
    if let Ok(url) = env::var("USER_MEMORY_URL") {
        return Ok(Some(crate::remote::Connection {
            url,
            token_file: env::var_os("USER_MEMORY_TOKEN_FILE")
                .map(PathBuf::from)
                .context("USER_MEMORY_URL requires USER_MEMORY_TOKEN_FILE")?,
        }));
    }
    if env::var_os("USER_MEMORY_DB").is_some() {
        return Ok(None);
    }
    let Some(directory) = current_directory() else {
        return Ok(None);
    };
    Ok(installation(&directory)?.and_then(|saved| saved.remote))
}

/// Replaces the receipt in `directory` in one rename, readable only by the
/// user.
fn write_installation(directory: &Path, saved: &Installation) -> Result<()> {
    let temporary = directory.join(format!("{RECEIPT}.{}", std::process::id()));
    let file = private::create_file(
        fs::OpenOptions::new().write(true).create_new(true),
        &temporary,
    )?;
    let result = (|| -> Result<()> {
        serde_json::to_writer_pretty(&file, saved)?;
        file.sync_all()?;
        fs::rename(&temporary, directory.join(RECEIPT))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result.context("Cannot save setup preferences")
}

impl Options {
    /// The explicit flag, otherwise the saved choice, otherwise hooks on.
    fn hooks_enabled(&self, saved: Option<&Installation>) -> bool {
        if self.hooks || self.no_hooks {
            return self.hooks;
        }
        saved.is_none_or(|saved| saved.hooks)
    }
    /// The explicit flag, otherwise the saved choice, otherwise the skill on.
    fn skill_enabled(&self, saved: Option<&Installation>) -> bool {
        if self.skill || self.no_skill {
            return self.skill;
        }
        saved.is_none_or(|saved| saved.skill)
    }
    /// The explicit flag, otherwise the saved choice, otherwise instruction
    /// files on.
    fn instructions_enabled(&self, saved: Option<&Installation>) -> bool {
        if self.instructions || self.no_instructions {
            return self.instructions;
        }
        saved.is_none_or(|saved| saved.instructions)
    }
    /// The clients to connect or check: the flag, else every one found.
    fn client(&self) -> Client {
        self.client.unwrap_or(Client::Auto)
    }
    /// Whether this is a server-only host: the flag, otherwise the saved
    /// choice, which --client or another server's --url ends.
    fn server_only_enabled(&self, saved: Option<&Installation>) -> bool {
        self.server_only
            || (self.client.is_none()
                && saved.is_some_and(|saved| {
                    saved.server_only
                        && self.url.as_ref().is_none_or(|url| {
                            saved
                                .remote
                                .as_ref()
                                .is_some_and(|remote| remote.url == *url)
                        })
                }))
    }
    /// The explicit flag, otherwise the saved choice, otherwise the check on.
    fn update_check_enabled(&self, saved: Option<&Installation>) -> bool {
        if self.update_check || self.no_update_check {
            return self.update_check;
        }
        saved.is_none_or(|saved| saved.update_check)
    }
    fn paths(&self) -> Result<Paths> {
        let directory = self
            .install_dir
            .clone()
            .or_else(|| current_directory().filter(|dir| dir.join(RECEIPT).is_file()))
            .or_else(paths::bin_dir)
            .context(
                "Cannot locate your home directory; use --install-dir with an absolute path",
            )?;
        let binary = directory.join(paths::BINARY);
        let saved = if self.db.is_some() {
            None
        } else {
            installation(&directory)?
        };
        let db = self
            .db
            .clone()
            .or_else(|| env::var_os("USER_MEMORY_DB").map(PathBuf::from))
            .or_else(|| saved.as_ref().and_then(|s| s.db.clone()))
            .unwrap_or_else(crate::db_path);
        if !binary.is_absolute() || !db.is_absolute() {
            bail!("Database and install directory must use absolute paths");
        }
        let remote = if let Some(url) = &self.url {
            Some(crate::remote::Connection {
                url: url.clone(),
                token_file: self
                    .token_file
                    .clone()
                    .context("Remote setup requires --token-file")?,
            })
        } else if self.db.is_some() || env::var_os("USER_MEMORY_DB").is_some() {
            None
        } else {
            saved.and_then(|s| s.remote)
        };
        if let Some(remote) = &remote {
            remote.validate()?;
        }
        Ok(Paths { binary, db, remote })
    }
}

struct InstalledClient {
    kind: Client,
    command: PathBuf,
}

impl InstalledClient {
    fn label(&self) -> &'static str {
        if self.kind == Client::Codex {
            "Codex"
        } else {
            "Claude Code"
        }
    }
    /// Name the session hook passes as `--client`.
    fn hook_client(&self) -> &'static str {
        if self.kind == Client::Codex {
            "codex"
        } else {
            "claude"
        }
    }
    fn hook_file(&self) -> Result<PathBuf> {
        if self.kind == Client::Codex {
            hooks::codex_hooks()
        } else {
            hooks::claude_settings()
        }
    }
    /// What to say about this client's release: only for Claude Code on
    /// Windows with its hooks on, as [`versions::claude_step`] says. A
    /// `claude --version` that fails reads as unreadable, never as new
    /// enough; elsewhere it is not run at all.
    fn version_step(&self, hooks: bool) -> Option<versions::ClaudeStep> {
        if self.kind != Client::Claude || !hooks || !cfg!(windows) {
            return None;
        }
        let printed = Command::new(&self.command)
            .arg("--version")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        versions::claude_step(&printed, cfg!(windows))
    }
    /// Installs or removes the session hooks and describes what changed.
    fn set_hook(&self, paths: &Paths, enabled: bool) -> Result<String> {
        let file = self.hook_file()?;
        let mut changes = Vec::new();
        for hook in hooks::HOOKS {
            let wanted = enabled.then(|| hooks::entry(hook, &paths.binary, self.hook_client()));
            let change = hooks::apply(&file, hook, wanted.as_ref())?;
            changes.push(format!("{} {}", hook.event, change.describe()));
        }
        let mut note = format!("session hooks {} ({})", changes.join(", "), file.display());
        if enabled && self.kind == Client::Codex {
            match self.codex_trust(paths)? {
                codex_trust::Trust::Approved => {}
                codex_trust::Trust::Unverifiable(_) => {
                    note.push_str(&format!(
                        "; if Codex has not run them yet, {}",
                        codex_trust::APPROVE
                    ));
                }
                codex_trust::Trust::Unapproved | codex_trust::Trust::Modified => {
                    note.push_str(&format!(
                        "; Codex skips them until you {}",
                        codex_trust::APPROVE
                    ));
                }
            }
        }
        Ok(note)
    }
    fn codex_trust(&self, paths: &Paths) -> Result<codex_trust::Trust> {
        let commands: Vec<String> = hooks::HOOKS
            .iter()
            .map(|hook| hooks::command(*hook, &paths.binary, self.hook_client()))
            .collect();
        Ok(codex_trust::trust(
            &self.command,
            &self.hook_file()?,
            &commands,
        ))
    }
    /// Checks the session hooks: fine, with a note for the user when one
    /// applies, or waiting for a step only the user can take.
    fn verify_hook(&self, paths: &Paths) -> Result<HookState> {
        let file = self.hook_file()?;
        for hook in hooks::HOOKS {
            let wanted = hooks::entry(hook, &paths.binary, self.hook_client());
            if !hooks::verify(&file, hook, &wanted)? {
                bail!(
                    "The {} hook in {} is missing or outdated. Run setup again.",
                    hook.event,
                    file.display()
                );
            }
        }
        if self.kind != Client::Codex {
            return Ok(HookState::Fine(None));
        }
        let mut note = match self.codex_trust(paths)? {
            codex_trust::Trust::Approved => "approved in Codex".to_owned(),
            codex_trust::Trust::Unapproved => {
                return Ok(HookState::Action(format!(
                    "Codex session hooks: Codex has not approved them yet, so they never run; {}",
                    codex_trust::APPROVE
                )));
            }
            codex_trust::Trust::Modified => {
                return Ok(HookState::Action(format!(
                    "Codex session hooks: they changed since Codex approved them, so Codex skips them; {}",
                    codex_trust::APPROVE
                )));
            }
            codex_trust::Trust::Unverifiable(reason) => {
                format!(
                    "warning: approval not verifiable ({reason}); if Codex does not load memory, {}",
                    codex_trust::APPROVE
                )
            }
        };
        if let Some(warning) = hooks::codex_inline_warning(&hooks::codex_config()?)? {
            note.push_str(&format!(". Warning: {warning}"));
        }
        Ok(HookState::Fine(Some(note)))
    }
    /// The client's `mcp get` for the server: its answer, found or not, or
    /// an error when the query itself failed.
    fn config(&self) -> Result<Output> {
        let mut cmd = Command::new(&self.command);
        cmd.args(["mcp", "get", SERVER]);
        if self.kind == Client::Codex {
            cmd.arg("--json");
        }
        let output = cmd
            .output()
            .with_context(|| format!("Cannot inspect {} configuration", self.label()))?;
        answered(
            output,
            &format!("Checking the {} registration", self.label()),
        )
    }
    fn matches(&self, output: &Output, paths: &Paths) -> bool {
        if !output.status.success() {
            return false;
        }
        if self.kind == Client::Codex {
            let Ok(config) = serde_json::from_slice::<Value>(&output.stdout) else {
                return false;
            };
            let transport = &config["transport"];
            return config["enabled"] != false
                && transport["type"] == "stdio"
                && transport["command"]
                    .as_str()
                    .is_some_and(|command| paths::same(Path::new(command), &paths.binary))
                && transport["args"] == serde_json::json!(["serve"])
                && paths
                    .environment()
                    .iter()
                    .all(|(key, value)| transport["env"][key].as_str() == Some(value.as_str()));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<_> = text.lines().map(str::trim).collect();
        lines.contains(&format!("Command: {}", paths.binary.display()).as_str())
            && lines.contains(&"Args: serve")
            && paths
                .environment()
                .iter()
                .all(|(key, value)| lines.contains(&format!("{key}={value}").as_str()))
            && lines
                .iter()
                .any(|line| line.starts_with("Scope: User config"))
            && !text.contains("Failed to connect")
    }
    fn add(&self, paths: &Paths) -> Result<()> {
        let mut cmd = Command::new(&self.command);
        if self.kind == Client::Codex {
            cmd.args(["mcp", "add", SERVER]);
            for (key, value) in paths.environment() {
                cmd.arg("--env").arg(format!("{key}={value}"));
            }
            cmd.arg("--");
        } else {
            // --env is variadic; --transport separates it from the server name.
            cmd.args(["mcp", "add", "--scope", "user"]);
            for (key, value) in paths.environment() {
                cmd.arg("--env").arg(format!("{key}={value}"));
            }
            cmd.args(["--transport", "stdio", SERVER, "--"]);
        }
        cmd.arg(&paths.binary).arg("serve");
        checked(&mut cmd, &format!("{} registration", self.label()))
    }
    /// Registers the shared server at user scope. Returns the step the user
    /// must take when a project- or local-scope entry overrides it in the
    /// current folder; that entry is theirs and stays as it is.
    fn register(&self, paths: &Paths) -> Result<Option<String>> {
        let existing = self.config()?;
        if let Some(step) = self.override_step(&existing) {
            // `mcp get` shows the override here, so the user-scope entry is
            // read from Claude Code's user configuration instead.
            if !claude_user_entry(SERVER)?.is_some_and(|entry| user_entry_matches(&entry, paths)) {
                self.replace_user_entry(paths)?;
            }
            return Ok(Some(step));
        }
        if self.matches(&existing, paths) {
            return Ok(None);
        }
        self.replace_user_entry(paths)?;
        Ok(None)
    }

    /// Adds the shared server, at user scope for Claude Code, replacing an
    /// older user entry and restoring it if the replacement fails.
    fn replace_user_entry(&self, paths: &Paths) -> Result<()> {
        // Claude refuses duplicate names. Preserve the full old user entry so
        // a failed replacement can restore all of its options, not just paths.
        let previous = if self.kind == Client::Claude {
            claude_user_entry(SERVER)?
        } else {
            None
        };
        if let Some(old) = previous {
            checked(
                Command::new(&self.command).args(["mcp", "remove", "--scope", "user", SERVER]),
                "Updating Claude Code registration",
            )?;
            if let Err(err) = self.add(paths) {
                let restore = checked(
                    Command::new(&self.command)
                        .args(["mcp", "add-json", "--scope", "user", SERVER])
                        .arg(old.to_string()),
                    "Restoring Claude Code registration",
                );
                return match restore {
                    Ok(()) => Err(err.context("Previous Claude Code registration restored")),
                    Err(restore_err) => Err(err.context(format!(
                        "Could not restore previous registration: {restore_err:#}"
                    ))),
                };
            }
            Ok(())
        } else {
            self.add(paths)
        }
    }

    /// Checks the registration setup just made: through `mcp get`, or, when
    /// an override hides it in this folder, in Claude Code's user
    /// configuration.
    fn verify_registration(&self, paths: &Paths, overridden: bool) -> Result<()> {
        if !overridden {
            return verify_client(self, paths);
        }
        if claude_user_entry(SERVER)?.is_some_and(|entry| user_entry_matches(&entry, paths)) {
            Ok(())
        } else {
            bail!(
                "The user-scope registration does not match the installed binary and shared database. Run setup again."
            )
        }
    }

    /// A project- or local-scope Claude Code entry for user-memory in the
    /// current folder overrides the shared server there: what the user can
    /// do about it, or `None`.
    fn override_step(&self, output: &Output) -> Option<String> {
        if self.kind != Client::Claude || !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let scope = text
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with("Scope:"))
            .filter(|line| !line.starts_with("Scope: User config"))?;
        let here = env::current_dir()
            .and_then(fs::canonicalize)
            .map_or_else(|_| "this folder".into(), |dir| dir.display().to_string());
        let flag = if scope.contains("Local") {
            "local"
        } else if scope.contains("Project") {
            "project"
        } else {
            return Some(format!(
                "Claude Code: {scope} overrides shared memory in {here}; memory works in every other folder. Check Claude Code's /mcp settings there"
            ));
        };
        Some(format!(
            "Claude Code: a {flag}-scope {SERVER} server overrides shared memory in {here}; memory works in every other folder. To use it there too, run `claude mcp remove {SERVER} --scope {flag}` in that folder"
        ))
    }
    fn check_override(&self, output: &Output) -> Result<()> {
        if self.kind != Client::Claude || !output.status.success() {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let scope = text
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with("Scope:"));
        if let Some(scope) = scope.filter(|line| !line.starts_with("Scope: User config")) {
            let flag = if scope.contains("Local") {
                "local"
            } else if scope.contains("Project") {
                "project"
            } else {
                bail!(
                    "{scope} overrides shared memory in this project. Check Claude Code's /mcp settings."
                );
            };
            bail!(
                "{scope} overrides shared memory in this project. To use shared memory here, run `claude mcp remove {SERVER} --scope {flag}` in this project, then run setup again. The existing override was preserved."
            );
        }
        Ok(())
    }
}

/// Whether a Claude Code user-scope entry runs this install's binary with
/// its store or server.
fn user_entry_matches(entry: &Value, paths: &Paths) -> bool {
    let env: serde_json::Map<String, Value> = paths
        .environment()
        .into_iter()
        .map(|(key, value)| (key, Value::String(value)))
        .collect();
    entry["command"]
        .as_str()
        .is_some_and(|command| paths::same(Path::new(command), &paths.binary))
        && entry["args"] == json!(["serve"])
        && entry["env"] == Value::Object(env)
}

fn claude_user_entry(name: &str) -> Result<Option<Value>> {
    let directory = env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(paths::home)
        .context("Cannot locate Claude Code user configuration")?;
    let path = directory.join(".claude.json");
    match fs::read(&path) {
        Ok(bytes) => {
            let config: Value = serde_json::from_slice(&bytes)
                .context("Claude Code user configuration is not valid JSON")?;
            Ok(config.get("mcpServers").and_then(|v| v.get(name)).cloned())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).context("Cannot read Claude Code user configuration"),
    }
}

fn checked(command: &mut Command, action: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("{action} could not start"))?;
    if !output.status.success() {
        bail!("{action} failed: {}", failure(&output));
    }
    Ok(())
}

/// The exit code Codex and Claude Code end `mcp get` with for a server
/// they do not have.
const NOT_REGISTERED: i32 = 1;

/// `output` of an `mcp get` when the client answered: found (exit 0) or not
/// registered ([`NOT_REGISTERED`]). Any other end, such as a crash or a
/// kill, is an error saying how it ended, so a failed query is never taken
/// for an answer.
fn answered(output: Output, action: &str) -> Result<Output> {
    match output.status.code() {
        Some(0 | NOT_REGISTERED) => Ok(output),
        _ => bail!("{action} failed: {}", failure(&output)),
    }
}

/// Why a command failed: its error output, or, when it printed none, how
/// it ended (its exit code, or the signal that stopped it).
fn failure(output: &Output) -> String {
    let said = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if said.is_empty() {
        format!("it printed no error and ended with {}", output.status)
    } else {
        said
    }
}

/// Whether `path` is a program this system runs: on Unix a file with an
/// execute bit; on Windows any file, whose extension says how it runs.
fn executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The extensions Windows tries for a bare program name when PATHEXT is
/// unset: its default, in order.
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// The file names a PATH folder may hold for the program `name`: the name
/// itself on Unix; on Windows the name with each PATHEXT extension in
/// order, as cmd.exe tries them, so npm's `codex.cmd` shim is found.
fn program_files(name: &str, windows: bool, pathext: Option<&std::ffi::OsStr>) -> Vec<String> {
    if !windows {
        return vec![name.to_owned()];
    }
    let pathext = pathext
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_PATHEXT.to_owned());
    pathext
        .split(';')
        .map(str::trim)
        .filter(|extension| extension.starts_with('.') && extension.len() > 1)
        .map(|extension| format!("{name}{}", extension.to_ascii_lowercase()))
        .collect()
}

fn find_client(name: &str) -> Option<PathBuf> {
    let files = program_files(name, cfg!(windows), env::var_os("PATHEXT").as_deref());
    if let Some(path) = env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .flat_map(|dir| files.iter().map(move |file| dir.join(file)))
            .find(|p| executable(p))
    }) {
        return Some(path);
    }
    #[cfg(target_os = "macos")]
    if name == "codex" {
        let relative = "Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex";
        let roots = application_dirs();
        for app in ["ChatGPT.app", "Codex.app"] {
            for root in &roots {
                let candidate = root.join(app).join(relative);
                if executable(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// Directories searched for app bundles that ship a client CLI:
/// `USER_MEMORY_APP_DIRS` as a path list like PATH (empty disables the
/// search), otherwise /Applications and ~/Applications.
#[cfg(target_os = "macos")]
fn application_dirs() -> Vec<PathBuf> {
    if let Some(dirs) = env::var_os("USER_MEMORY_APP_DIRS") {
        return env::split_paths(&dirs)
            .filter(|dir| !dir.as_os_str().is_empty())
            .collect();
    }
    let mut roots = vec![PathBuf::from("/Applications")];
    if let Some(home) = paths::home() {
        roots.push(home.join("Applications"));
    }
    roots
}

fn clients(selection: Client) -> Result<Vec<InstalledClient>> {
    let requested = match selection {
        Client::Codex => vec![Client::Codex],
        Client::Claude => vec![Client::Claude],
        _ => vec![Client::Codex, Client::Claude],
    };
    let mut found = Vec::new();
    for kind in requested {
        let name = if kind == Client::Codex {
            "codex"
        } else {
            "claude"
        };
        if let Some(command) = find_client(name) {
            found.push(InstalledClient { kind, command });
        } else if selection != Client::Auto {
            bail!(
                "{} is not installed or cannot be found. Install the client, then run setup again.",
                if kind == Client::Codex {
                    "Codex"
                } else {
                    "Claude Code"
                }
            );
        }
    }
    if found.is_empty() {
        bail!("No supported clients found. Install Codex or Claude Code, then run setup again.");
    }
    Ok(found)
}

fn install_binary(destination: &Path) -> Result<()> {
    let source = env::current_exe()?.canonicalize()?;
    if fs::symlink_metadata(destination).is_ok_and(|m| !m.file_type().is_symlink())
        && paths::same(destination, &source)
    {
        return Ok(());
    }
    let parent = destination.parent().context("Invalid install directory")?;
    fs::create_dir_all(parent).with_context(|| format!("Cannot create {}", parent.display()))?;
    if cfg!(windows) {
        remove_set_aside(parent);
    }
    let temporary = parent.join(format!(
        ".user-memory-{}{}",
        std::process::id(),
        env::consts::EXE_SUFFIX
    ));
    let mut target = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| -> Result<()> {
        let mut input = fs::File::open(&source)?;
        io::copy(&mut input, &mut target)?;
        target.set_permissions(fs::metadata(&source)?.permissions())?;
        target.sync_all()?;
        drop(target);
        if cfg!(windows) {
            replace_aside(&temporary, destination)?;
        } else {
            fs::rename(&temporary, destination)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("Cannot install {}", destination.display()))
}

/// Puts the folder of `binary` on the user PATH on Windows, saying so when
/// it was missing; whether setup added it, now or in an earlier run, so
/// uninstall removes only an entry setup made. Elsewhere the shell's own
/// PATH is the user's to set.
fn user_path(binary: &Path, saved: Option<&Installation>) -> Result<bool> {
    let earlier = saved.is_some_and(|saved| saved.path_added);
    #[cfg(windows)]
    {
        let folder = binary.parent().context("Invalid binary path")?;
        if path_env::add(folder).context("Cannot add the bin folder to your PATH")? {
            println!(
                "  Added {} to your PATH; open a new terminal to use user-memory",
                folder.display()
            );
            return Ok(true);
        }
    }
    #[cfg(not(windows))]
    let _ = binary;
    Ok(earlier)
}

/// The prefix of an earlier binary an upgrade set aside, in the bin folder.
const SET_ASIDE: &str = "user-memory.old-";

/// Puts `new` in place of `destination` on Windows, which never overwrites
/// or deletes a running program but renames one: every agent session runs
/// the installed binary. The earlier one is renamed aside first and
/// deleted when no session runs it any more; if the new one cannot take its
/// place, the earlier one goes back.
fn replace_aside(new: &Path, destination: &Path) -> io::Result<()> {
    if fs::symlink_metadata(destination).is_err() {
        return fs::rename(new, destination);
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let aside = destination.with_file_name(format!(
        "{SET_ASIDE}{}-{nanos}{}",
        std::process::id(),
        env::consts::EXE_SUFFIX
    ));
    fs::rename(destination, &aside)?;
    if let Err(err) = fs::rename(new, destination) {
        let _ = fs::rename(&aside, destination);
        return Err(err);
    }
    let _ = fs::remove_file(&aside);
    Ok(())
}

/// Deletes the binaries earlier upgrades set aside in `dir` once no session
/// runs them; one still running stays for a later setup.
fn remove_set_aside(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(SET_ASIDE) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Embedding settings to save, checked before anything is written: new
/// settings from the file or environment, otherwise the saved ones. Remote
/// installs save none because the memory server owns embedding.
fn embedding_to_save(
    options: &Options,
    paths: &Paths,
    saved: Option<&Installation>,
) -> Result<(embedding::Settings, String)> {
    let given = embedding::resolve(options.env_file.as_deref())?;
    if paths.remote.is_some() {
        let note = if given.as_ref().is_some_and(|given| !given.is_empty()) {
            "not saved; the memory server owns embedding"
        } else {
            "managed by the memory server"
        };
        return Ok((embedding::Settings::new(), note.into()));
    }
    let (settings, source) = match given {
        Some(given) => (given, "saved"),
        None => (
            saved
                .map(|saved| saved.embedding.clone())
                .unwrap_or_default(),
            "kept",
        ),
    };
    let model = crate::memory::embedding_model(&|name| settings.get(name).cloned())
        .context("Embedding settings are invalid; nothing was installed")?;
    let note = match model {
        Some(model) => format!(
            "{model} at {} ({source})",
            settings["USER_MEMORY_EMBED_URL"].trim()
        ),
        None => "off; search uses keywords only".into(),
    };
    Ok((settings, note))
}

pub async fn install(options: &Options) -> Result<()> {
    let paths = options.paths()?;
    if let Some(remote) = &paths.remote {
        remote.probe().await?;
    }
    // A damaged receipt is repaired by this run, so it only loses the saved choices.
    let saved = installation(paths.binary.parent().context("Invalid binary path")?)
        .ok()
        .flatten();
    let (embedding, embedding_note) = embedding_to_save(options, &paths, saved.as_ref())?;
    let server_only = options.server_only_enabled(saved.as_ref());
    if server_only && paths.remote.is_none() {
        bail!(
            "--server-only installs the binary beside a memory server; give that server's --url and --token-file"
        );
    }
    // Don't write files if a requested client is absent.
    let clients = if server_only {
        Vec::new()
    } else {
        clients(options.client())?
    };
    if paths.remote.is_none() {
        // Upgrades an earlier store (after backing it up), then creates a
        // fresh store or refuses a foreign one, all before anything is
        // installed, and makes the store's first automatic backup when it
        // holds memories and has none from the last day.
        let titles = options
            .titles
            .as_deref()
            .map(crate::read_titles)
            .transpose()?;
        let migration = if paths.db.is_file() {
            crate::memory::migrate(&paths.db, titles.as_ref())?
        } else {
            crate::memory::Migration::Empty
        };
        if matches!(migration, crate::memory::Migration::Migrated { .. }) {
            println!("{}", migration.describe(&paths.db));
        }
        if titles.is_some()
            && let Some(note) = migration.unused_titles()
        {
            println!("{note}");
        }
        crate::connect(&paths.db)?;
        crate::memory::backups::back_up_if_due(&paths.db);
    }
    println!("Installing user memory…");
    install_binary(&paths.binary)?;
    let path_added = user_path(&paths.binary, saved.as_ref())?;
    let hooks = options.hooks_enabled(saved.as_ref());
    let mut failures = Vec::new();
    // Steps only the user can take; nothing is broken. Printed last.
    let mut actions = Vec::new();
    for client in &clients {
        print!("  {}: ", client.label());
        match client.register(&paths).and_then(|step| {
            client.verify_registration(&paths, step.is_some())?;
            Ok(step)
        }) {
            Ok(None) => println!("connected"),
            Ok(Some(step)) => {
                println!("connected; action needed (below)");
                actions.push(step);
            }
            Err(err) => {
                println!("needs attention");
                failures.push(format!("{}: {err:#}", client.label()));
                continue;
            }
        }
        match client.set_hook(&paths, hooks) {
            Ok(note) => println!("    {note}"),
            Err(err) => failures.push(format!("{} session hook: {err:#}", client.label())),
        }
        // The hooks are written whatever the release: `claude update` then
        // makes them run, with no second setup.
        match client.version_step(hooks) {
            Some(versions::ClaudeStep::Action(step)) => actions.push(step),
            Some(versions::ClaudeStep::Note(note)) => println!("    {note}"),
            None => {}
        }
    }
    let skill = options.skill_enabled(saved.as_ref());
    let instructions = options.instructions_enabled(saved.as_ref());
    if server_only {
        println!("  Agent apps: none connected (server only)");
    } else {
        let claude = clients.iter().any(|client| client.kind == Client::Claude);
        match set_skill(skill, claude) {
            Ok(Skill::Set(note)) => println!("  Skill: {note}"),
            Ok(Skill::Foreign(path)) => {
                println!("  Skill: not installed; action needed (below)");
                actions.push(foreign_skill_step(&path));
            }
            Err(err) => failures.push(format!("Skill: {err:#}")),
        }
        match set_instructions(instructions, &clients, &paths).await {
            Ok(notes) => {
                for note in notes {
                    println!("  Instructions: {note}");
                }
            }
            Err(err) => failures.push(format!("Instructions: {err:#}")),
        }
        // Advice for the user's own git, so a problem is said, never fatal;
        // doctor reports an ignore still missing.
        match git_ignore::ensure() {
            Ok(ignore) => println!("  Git ignore: {}", ignore.describe()),
            Err(err) => println!(
                "  Git ignore: not written ({err:#}); add {} to your global git ignore, one per line",
                git_ignore::NAMES.join(" and ")
            ),
        }
    }
    if !failures.is_empty() {
        bail!(
            "{}\nRun setup again after resolving these errors.",
            failures.join("\n")
        );
    }
    if paths.remote.is_none() {
        verify_database(&paths)?;
    }
    write_installation(
        paths.binary.parent().context("Invalid binary path")?,
        &Installation {
            db: paths.remote.is_none().then(|| paths.db.clone()),
            remote: paths.remote.clone(),
            clients: clients.iter().map(|c| c.kind).collect(),
            hooks,
            skill,
            instructions,
            update_check: options.update_check_enabled(saved.as_ref()),
            server_only,
            path_added,
            embedding,
        },
    )?;
    if server_only {
        println!(
            "\nSetup complete. This machine runs the memory server only; no agent app uses it here."
        );
    } else {
        println!(
            "\nSetup complete. Restart your connected clients once; memory starts automatically after that."
        );
    }
    if let Some(remote) = &paths.remote {
        println!("Memory server: {}", remote.url);
    } else {
        println!("Memory: {}", paths.db.display());
    }
    println!("Embedding: {embedding_note}");
    println!(
        "Health check: {} doctor",
        crate::shell::quote(&paths.binary)
    );
    if !actions.is_empty() {
        println!();
        for step in &actions {
            println!("Action needed: {step}");
        }
    }
    Ok(())
}

/// Installs, updates or removes the user-memory skill and says what changed.
/// Opting out removes it from every place setup may have put it.
/// What setup did with the skill.
enum Skill {
    /// Installed, updated or removed, as the note says.
    Set(String),
    /// Someone else's skill with this name is there; left alone, and no
    /// skill installed.
    Foreign(PathBuf),
}

/// What the user can do about someone else's skill at `path`.
fn foreign_skill_step(path: &Path) -> String {
    format!(
        "Skill: {} was not installed by user-memory setup; remove it, or run `user-memory setup --no-skill`",
        path.display()
    )
}

fn set_skill(enabled: bool, claude: bool) -> Result<Skill> {
    if enabled {
        let places = skill::Places::for_clients(claude)?;
        if let skill::State::Foreign(path) = skill::state(&places, skill::BUNDLED)? {
            return Ok(Skill::Foreign(path));
        }
        let change = skill::install(&places, skill::BUNDLED)?;
        Ok(Skill::Set(format!(
            "{} ({})",
            change.describe(),
            places.describe()
        )))
    } else {
        let places = skill::Places::for_clients(true)?;
        Ok(Skill::Set(
            skill::remove(&places, skill::BUNDLED)?.describe().into(),
        ))
    }
}

/// The rules view from the server `remote`, else the store at `db`, within
/// `max_bytes`.
async fn render_rules(
    remote: Option<&crate::remote::Connection>,
    db: &Path,
    max_bytes: usize,
) -> Result<String> {
    match remote {
        Some(remote) => instructions::checked(
            remote
                .call(
                    "index",
                    json!({"instructions": true, "instructions_max_bytes": max_bytes}),
                )
                .await?,
        ),
        None => {
            let db = db.to_owned();
            tokio::task::spawn_blocking(move || crate::memory::instructions(&db, max_bytes)).await?
        }
    }
}

/// The rules view for `places` from the store or server `paths` names.
async fn rules_view(paths: &Paths, places: &[instructions::Place]) -> Result<String> {
    render_rules(paths.remote.as_ref(), &paths.db, instructions::room(places)).await
}

/// Writes the user's global rules into each client's instruction file, or
/// removes them from every place when turned off, and says what it did.
async fn set_instructions(
    enabled: bool,
    clients: &[InstalledClient],
    paths: &Paths,
) -> Result<Vec<String>> {
    if !enabled {
        let removed = instructions::remove_all()?;
        return Ok(vec![if removed.is_empty() {
            "off".into()
        } else {
            let removed: Vec<String> = removed.iter().map(|p| p.display().to_string()).collect();
            format!("off (removed from {})", removed.join(", "))
        }]);
    }
    let has = |kind| clients.iter().any(|client| client.kind == kind);
    let places = instructions::Place::for_clients(has(Client::Claude), has(Client::Codex))?;
    let rules = rules_view(paths, &places).await?;
    places
        .iter()
        .map(|place| {
            // The user's own layout, often a public dotfiles repository: say
            // so and go on with the install.
            if instructions::is_link(place.path()) {
                return Ok(format!(
                    "{} rules: not written ({} is a symbolic link; user-memory leaves it and its target as they are)",
                    place.label(),
                    place.path().display()
                ));
            }
            let change = match place.write(&rules)? {
                instructions::Change::Written => "written",
                instructions::Change::Unchanged => "up to date",
            };
            Ok(format!(
                "{} rules in {} ({change})",
                place.label(),
                place.path().display()
            ))
        })
        .collect()
}

/// Brings the instruction files of the clients this install set up in line
/// with the store, as setup chose; does nothing without a saved setup that
/// has them on. Returns the clients whose file already held the current
/// rules: a client reads its file at launch, so one written just now is
/// not yet in its session.
/// Callers ignore its error: a failed write never fails a tool call or a
/// session start, and doctor reports it.
pub async fn sync_instructions() -> Result<Vec<Client>> {
    // One sync at a time in this process, from fetching the rules to the
    // last write, so the last to finish wrote the newest rules.
    static SYNC: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one_at_a_time = SYNC.lock().await;
    let Some(directory) = current_directory() else {
        return Ok(Vec::new());
    };
    let Some(saved) = installation(&directory)? else {
        return Ok(Vec::new());
    };
    if !saved.instructions {
        return Ok(Vec::new());
    }
    let has = |kind| saved.clients.contains(&kind);
    let places = instructions::Place::for_clients(has(Client::Claude), has(Client::Codex))?;
    #[cfg(debug_assertions)]
    stall_for_tests().await;
    let rules = render_rules(
        configured_remote()?.as_ref(),
        &crate::db_path(),
        instructions::room(&places),
    )
    .await?;
    let kinds: Vec<Client> = [Client::Claude, Client::Codex]
        .into_iter()
        .filter(|kind| has(*kind))
        .collect();
    // The writes wait and retry while the user saves a file: off the async
    // workers, so tool calls and replies go on meanwhile.
    tokio::task::spawn_blocking(move || {
        kinds
            .into_iter()
            .zip(places)
            .filter(|(_, place)| matches!(place.write(&rules), Ok(instructions::Change::Unchanged)))
            .map(|(kind, _)| kind)
            .collect()
    })
    .await
    .context("Instruction files sync stopped")
}

/// In debug builds only: while the file `USER_MEMORY_STALL_SYNC` names
/// exists, a sync waits before fetching the rules, as one stuck on a slow
/// server would, so tests can show that nothing else waits for it. A sync
/// that stops waiting, by the file going or by its time limit, creates that
/// path with `.left` appended, so a test can tell it was still stuck.
#[cfg(debug_assertions)]
async fn stall_for_tests() {
    struct Left(PathBuf);
    impl Drop for Left {
        fn drop(&mut self) {
            let _ = fs::write(&self.0, "");
        }
    }
    let Some(stall) = env::var_os("USER_MEMORY_STALL_SYNC") else {
        return;
    };
    let mut left = stall.clone();
    left.push(".left");
    let _left = Left(left.into());
    while Path::new(&stall).exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Longest a sync may take, from waiting its turn to its last write. One
/// that overruns leaves the files outdated, which the next write or session
/// start repairs and doctor reports meanwhile.
pub const SYNC_LIMIT: Duration = Duration::from_secs(5);

/// [`sync_instructions`] within [`SYNC_LIMIT`] (stretched only in a debug
/// build's tests, by [`crate::session::hook_time`]).
pub async fn sync_bounded() -> Result<Vec<Client>> {
    tokio::time::timeout(crate::session::hook_time(SYNC_LIMIT), sync_instructions())
        .await
        .context("Instruction files sync timed out")?
}

/// Syncs started after a server's writes, still running.
static BACKGROUND: std::sync::Mutex<Option<tokio::task::JoinSet<()>>> = std::sync::Mutex::new(None);

/// Starts a bounded sync beside the caller, which goes on at once: a tool
/// reply never waits for the instruction files.
pub fn sync_in_background() {
    if let Ok(mut running) = BACKGROUND.lock() {
        let running = running.get_or_insert_with(tokio::task::JoinSet::new);
        // A long-lived server would otherwise keep one finished entry per write.
        while running.try_join_next().is_some() {}
        running.spawn(async {
            let _ = sync_bounded().await;
        });
    }
}

/// Waits, at most [`SYNC_LIMIT`], for the background syncs still running,
/// so a server that stops right after a write still updates the files.
pub async fn finish_background_syncs() {
    let running = BACKGROUND
        .lock()
        .ok()
        .and_then(|mut running| running.take());
    if let Some(mut running) = running {
        let _ = tokio::time::timeout(crate::session::hook_time(SYNC_LIMIT), async {
            while running.join_next().await.is_some() {}
        })
        .await;
    }
}

/// Reports each effective embedding setting with its source, then embeds one
/// probe text and reads the model's calibration from the store at `db`.
/// Prints file paths, never key contents. Returns the step for the user
/// when the model is not calibrated yet.
fn check_embedding(saved: Option<&Installation>, db: &Path) -> Result<Option<String>> {
    let empty = embedding::Settings::new();
    let saved = saved.map_or(&empty, |saved| &saved.embedding);
    for name in embedding::SETTINGS {
        if let Some((value, source)) = embedding::lookup(saved, name) {
            println!("  {name}={} ({source})", one_line(&value));
        }
    }
    let setting = |name: &str| embedding::lookup(saved, name).map(|(value, _)| value);
    match crate::memory::probe_embedding(&setting)? {
        Some(probe) => {
            println!(
                "Embedding: OK ({}, {} dimensions, {} ms)",
                probe.model,
                probe.dimensions,
                probe.elapsed.as_millis()
            );
            let model = format!("{} ({} dimensions)", probe.model, probe.dimensions);
            match crate::memory::calibration(db, &probe.model, probe.dimensions)? {
                crate::memory::Calibration::BuiltIn
                | crate::memory::Calibration::Calibrated { .. } => {}
                crate::memory::Calibration::Unseparable => println!(
                    "Duplicate detection: keyword only; {model} cannot separate the labelled pairs"
                ),
                crate::memory::Calibration::Missing => {
                    println!(
                        "Duplicate detection: keyword only for {model}, action needed (below)"
                    );
                    let url = setting("USER_MEMORY_EMBED_URL").unwrap_or_default();
                    return Ok(Some(format!(
                        "Embedding: {model} is not calibrated, so duplicate detection uses keywords; run `user-memory embedding set --url {url} --model {}` to calibrate it",
                        probe.model
                    )));
                }
            }
        }
        None => println!("Embedding: off; search uses keywords only"),
    }
    Ok(None)
}

/// `value` on one line: a newline, tab or other control character shown as
/// its escape, as `.env` writes the newline in a query instruction.
fn one_line(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_control() {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

fn verify_client(client: &InstalledClient, paths: &Paths) -> Result<()> {
    let output = client.config()?;
    client.check_override(&output)?;
    if !client.matches(&output, paths) {
        bail!(
            "Registration does not match the installed binary and shared database. Run setup again."
        );
    }
    Ok(())
}

/// Checks the local store and returns its journal mode.
fn verify_database(paths: &Paths) -> Result<String> {
    if !paths.db.is_file() {
        bail!("Memory database is missing. Run setup first.");
    }
    let conn = Connection::open_with_flags(&paths.db, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let found = crate::memory::problems(&conn)?;
    if !found.is_empty() {
        bail!(
            "Database health check failed:\n  {}\nRun `user-memory repair`, which backs the store up first.",
            found.join("\n  ")
        );
    }
    drop(conn);
    // Refuses a store from another schema version with a clear message.
    let conn = crate::connect(&paths.db)?;
    Ok(conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?)
}

/// Doctor's line for the store's journal mode.
fn journal_line(mode: &str) -> String {
    if mode.eq_ignore_ascii_case("wal") {
        "Journal mode: WAL; keep the store on a local disk, since a network filesystem (NFS, SMB) cannot share its write-ahead log between processes".into()
    } else {
        format!(
            "Journal mode: {mode}; writers wait for readers until a later open switches the store to WAL, which another process holding the store can delay"
        )
    }
}

/// What a client's session hooks need.
enum HookState {
    /// Installed and running, with a note for the user when one applies.
    Fine(Option<String>),
    /// Installed, but the client runs them only after a step the user must
    /// take; nothing is broken.
    Action(String),
}

/// Doctor's exit status when nothing is broken but the user must still act
/// before memory works everywhere; clap already uses 2 for usage errors.
pub const ACTION_NEEDED: i32 = 3;

/// What doctor found when nothing failed.
#[derive(Debug, PartialEq, Eq)]
pub enum Health {
    Good,
    /// Nothing is broken, but the user must take the steps doctor printed.
    ActionNeeded,
}

pub async fn doctor(options: &Options) -> Result<Health> {
    let paths = options.paths()?;
    let saved = installation(paths.binary.parent().context("Invalid binary path")?)?;
    if options.server_only_enabled(saved.as_ref()) {
        return server_doctor(options, &paths, saved.as_ref()).await;
    }
    // Nothing installed here, yet a store named: inside the Docker server's
    // container, whose image sets USER_MEMORY_DB, the store is all there is
    // to check. A store found only by default, or an install with a part
    // missing, still asks for setup.
    if saved.is_none()
        && paths.remote.is_none()
        && !executable(&paths.binary)
        && (options.db.is_some() || env::var_os("USER_MEMORY_DB").is_some())
    {
        println!(
            "Nothing is installed here; checking the store at {}",
            paths.db.display()
        );
        return store_doctor(&paths);
    }
    let journal = if let Some(remote) = &paths.remote {
        remote.probe().await?;
        None
    } else {
        Some(verify_database(&paths)?)
    };
    if !executable(&paths.binary) {
        bail!("Installed binary is missing. Run setup first.");
    }
    if paths.remote.is_none() {
        checked(
            Command::new(&paths.binary)
                .arg("index")
                .env("USER_MEMORY_DB", &paths.db)
                .env_remove("USER_MEMORY_URL"),
            "Memory index check",
        )?;
        println!("Binary: OK\nDatabase: OK\nMemory index: OK");
        if let Some(journal) = journal {
            println!("{}", journal_line(&journal));
        }
    } else {
        println!("Binary: OK\nRemote MCP connection and tools: OK");
    }
    let mut failures = Vec::new();
    let mut actions = Vec::new();
    if options.update_check_enabled(saved.as_ref())
        && !update::off_by_environment()
        && let Some(notice) = update::notice(env!("CARGO_PKG_VERSION")).await
    {
        println!("{notice}");
    }
    if paths.remote.is_some() {
        println!("Embedding: managed by the memory server");
    } else {
        match check_embedding(saved.as_ref(), &paths.db) {
            Ok(Some(step)) => actions.push(step),
            Ok(None) => {}
            Err(err) => failures.push(format!("Embedding: {err:#}")),
        }
    }
    if paths.remote.is_none() {
        check_backups(&paths.db, &mut actions, &mut failures);
        check_privacy(&paths.db, &mut actions, &mut failures);
    }
    let check_hooks = options.hooks_enabled(saved.as_ref());
    let check_skill = options.skill_enabled(saved.as_ref());
    let check_instructions = options.instructions_enabled(saved.as_ref());
    let selected = if options.client() == Client::Auto {
        if let Some(saved) = saved {
            saved
                .clients
                .into_iter()
                .map(clients)
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect()
        } else {
            clients(Client::Auto)?
        }
    } else {
        clients(options.client())?
    };
    if check_skill {
        let claude = selected.iter().any(|client| client.kind == Client::Claude);
        match skill::state(&skill::Places::for_clients(claude)?, skill::BUNDLED)? {
            skill::State::Ok => println!("Skill: OK"),
            skill::State::Outdated => {
                failures.push("Skill: outdated; run setup to update it".into())
            }
            skill::State::Missing => {
                failures.push("Skill: missing; run setup to install it".into())
            }
            skill::State::Foreign(path) => {
                println!("Skill: action needed (below)");
                actions.push(foreign_skill_step(&path));
            }
        }
    } else {
        println!("Skill: off (setup --skill installs it)");
    }
    if check_instructions {
        let has = |kind| selected.iter().any(|client| client.kind == kind);
        let places = instructions::Place::for_clients(has(Client::Claude), has(Client::Codex))?;
        let rules = rules_view(&paths, &places).await?;
        for place in places {
            if instructions::is_link(place.path()) {
                println!(
                    "{} instructions ({}): not written; it is a symbolic link, which user-memory leaves as it is",
                    place.label(),
                    place.path().display()
                );
                continue;
            }
            let label = format!(
                "{} instructions ({})",
                place.label(),
                place.path().display()
            );
            match place.state(&rules) {
                Ok(instructions::State::Current) => println!("{label}: current"),
                // Another machine's write reaches this file at the next
                // session start or write here.
                Ok(instructions::State::Outdated) => println!(
                    "{label}: outdated; the next session start or memory change updates it"
                ),
                Ok(instructions::State::Missing) => {
                    failures.push(format!("{label}: missing; run setup to write it"))
                }
                Err(err) => failures.push(format!("{label}: {err:#}")),
            }
        }
    } else {
        println!("Instructions: off (setup --instructions writes them)");
    }
    match git_ignore::state() {
        Ok(git_ignore::Ignore::Covered(target)) => println!(
            "Git ignore: OK ({} ignores {})",
            target.display(),
            git_ignore::NAMES.join(" and ")
        ),
        Ok(ignore) => match ignore.step() {
            Some(step) => {
                println!("Git ignore: action needed (below)");
                actions.push(format!("Git ignore: {step}"));
            }
            None => println!("Git ignore: {}", ignore.describe()),
        },
        Err(err) => failures.push(format!("Git ignore: {err:#}")),
    }
    for tracked in git_ignore::tracked_here().unwrap_or_default() {
        let name = tracked.file_name().unwrap_or_default().to_string_lossy();
        println!(
            "Git ignore: this repository tracks {}; an ignore rule does not untrack it (`git rm --cached {name}` does)",
            tracked.display()
        );
    }
    // A key file that differs from the remote waits for its memories to
    // move; doctor says how, for this folder and the folders it lists.
    if let Ok(here) = env::current_dir() {
        // Only this folder's project depends on it; nothing else doctor
        // checks reads this checkout.
        if let Some((dot_git, problem)) = crate::project::broken_checkout(&here) {
            println!(
                "Project: git cannot read the checkout at {} ({problem}); repair or remove it so sessions here can find this folder's project",
                dot_git.display()
            );
        }
        let folder = crate::project::folder(&here);
        if let (Ok(key), Some(pending)) = (&folder.key, &folder.pending) {
            println!("Project: {}", crate::project::move_advice(pending, key));
        }
        for include in &folder.includes {
            if let (Ok(key), Some(pending)) = (&include.key, &include.pending) {
                println!(
                    "Project @{}: {}",
                    include.path.display(),
                    crate::project::move_advice(pending, key)
                );
            }
        }
    }
    for client in selected {
        let overridden = client
            .config()
            .map(|output| client.override_step(&output))
            .unwrap_or(None);
        match overridden {
            Some(step) => {
                println!("{}: action needed (below)", client.label());
                actions.push(step);
            }
            None => match verify_client(&client, &paths) {
                Ok(()) => println!("{}: OK", client.label()),
                Err(err) => failures.push(format!("{}: {err:#}", client.label())),
            },
        }
        if !check_hooks {
            continue;
        }
        match client.verify_hook(&paths) {
            Ok(HookState::Fine(None)) => println!("{} session hook: OK", client.label()),
            Ok(HookState::Fine(Some(note))) => {
                println!("{} session hook: OK ({note})", client.label())
            }
            Ok(HookState::Action(step)) => {
                println!(
                    "{} session hook: installed; action needed (below)",
                    client.label()
                );
                actions.push(step);
            }
            Err(err) => failures.push(format!("{} session hook: {err:#}", client.label())),
        }
        match client.version_step(check_hooks) {
            Some(versions::ClaudeStep::Action(step)) => {
                println!("{} version: action needed (below)", client.label());
                actions.push(step);
            }
            Some(versions::ClaudeStep::Note(note)) => println!("{note}"),
            None => {}
        }
    }
    conclude(&actions, &failures)
}

/// Doctor's ending: the steps left for the user, last so they stand out,
/// then any real failure, which fails the run.
fn conclude(actions: &[String], failures: &[String]) -> Result<Health> {
    for step in actions {
        println!("Action needed: {step}");
    }
    if !failures.is_empty() {
        bail!("{}", failures.join("\n"));
    }
    Ok(if actions.is_empty() {
        Health::Good
    } else {
        Health::ActionNeeded
    })
}

/// Doctor for a server-only host, which no agent app uses: the server
/// answers with this machine's token, then the server's store is checked
/// where it lives, inside the installer's Docker server. Run inside that
/// container, it checks the store there.
async fn server_doctor(
    options: &Options,
    paths: &Paths,
    saved: Option<&Installation>,
) -> Result<Health> {
    let Some(remote) = &paths.remote else {
        return store_doctor(paths);
    };
    remote.probe().await?;
    if !executable(&paths.binary) {
        bail!("Installed binary is missing. Run setup first.");
    }
    println!("Binary: OK\nRemote MCP connection and tools: OK");
    if options.update_check_enabled(saved)
        && !update::off_by_environment()
        && let Some(notice) = update::notice(env!("CARGO_PKG_VERSION")).await
    {
        println!("{notice}");
    }
    let EmbeddingHome::Docker(dir) = embedding_home()? else {
        println!(
            "Server store: checked where it lives, with `user-memory doctor --server-only` there"
        );
        return Ok(Health::Good);
    };
    // The store's own checks, by the release inside the container; it
    // reaches GitHub through this doctor only.
    let out = Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(dir.join("compose.yaml"))
        .args(["exec", "-T", "-e", &format!("{}=1", update::OFF)])
        .args(["memory", "user-memory", "doctor", "--server-only"])
        .stdin(std::process::Stdio::null())
        .output()
        .context("Cannot run docker")?;
    print!("{}", String::from_utf8_lossy(&out.stdout));
    match out.status.code() {
        Some(0) => Ok(Health::Good),
        Some(ACTION_NEEDED) => Ok(Health::ActionNeeded),
        _ => bail!(
            "The server's store checks failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// Doctor where the store lives and no agent app runs, such as inside the
/// installer's Docker server: the store, its index and journal, the
/// embedding setting this process reads from its environment, and the
/// automatic backups beside the store.
fn store_doctor(paths: &Paths) -> Result<Health> {
    let journal = verify_database(paths)?;
    checked(
        Command::new(env::current_exe().context("Cannot locate this binary")?)
            .arg("index")
            .env("USER_MEMORY_DB", &paths.db)
            .env_remove("USER_MEMORY_URL"),
        "Memory index check",
    )?;
    println!("Database: OK\nMemory index: OK\n{}", journal_line(&journal));
    let (mut actions, mut failures) = (Vec::new(), Vec::new());
    match check_embedding(None, &paths.db) {
        Ok(Some(step)) => actions.push(step),
        Ok(None) => {}
        Err(err) => failures.push(format!("Embedding: {err:#}")),
    }
    check_backups(&paths.db, &mut actions, &mut failures);
    conclude(&actions, &failures)
}

/// The store's automatic backups: their line, and a step when they need
/// the user.
fn check_backups(db: &Path, actions: &mut Vec<String>, failures: &mut Vec<String>) {
    match crate::memory::backups::status(db) {
        Ok(backups) => {
            println!("{}", backups.line());
            actions.extend(backups.action());
        }
        Err(err) => failures.push(format!("Automatic backups: {err:#}")),
    }
}

/// Whether only this user can read the store: its line, or a step for each
/// file or folder other accounts can read. On Unix that is the store file,
/// as SQLite gives its `-wal` and `-shm` the store's mode and the backups
/// have their own private folder; on Windows those files take the folder's
/// access, so the folder is checked too. The store inside the installer's
/// Docker server is reached only through Docker and is not checked.
fn check_privacy(db: &Path, actions: &mut Vec<String>, failures: &mut Vec<String>) {
    let folder = db
        .parent()
        .filter(|folder| !folder.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let checked: &[&Path] = if cfg!(windows) { &[folder, db] } else { &[db] };
    let mut wider = Vec::new();
    for &path in checked {
        match private::is_private(path) {
            Ok(true) => {}
            Ok(false) => wider.push(path),
            Err(err) => {
                failures.push(format!(
                    "Store privacy: cannot check {}: {err}",
                    path.display()
                ));
                return;
            }
        }
    }
    if wider.is_empty() {
        println!("Store privacy: OK");
    }
    actions.extend(wider.into_iter().map(privacy_step));
}

/// The step that makes `path` readable only by this user again.
fn privacy_step(path: &Path) -> String {
    let shown = path.display();
    if cfg!(windows) {
        format!(
            "Store privacy: other accounts on this machine can read {shown}; see who with: icacls \"{shown}\", and make it yours only with: user-memory secure-file \"{shown}\""
        )
    } else {
        let mode = if path.is_dir() { "700" } else { "600" };
        format!(
            "Store privacy: other accounts on this machine can read {shown}; run: chmod {mode} {}",
            crate::shell::quote(path)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `mcp get` that answers, found or not registered, is an answer;
    /// one that ends any other way is a failed query, never "not there".
    #[cfg(unix)]
    #[test]
    fn only_found_or_not_registered_counts_as_an_answer() {
        let ended = |script: &str| {
            answered(
                Command::new("sh").args(["-c", script]).output().unwrap(),
                "Checking the Codex registration",
            )
            .map(|out| out.status.code())
            .map_err(|err| err.to_string())
        };
        assert_eq!(ended("exit 0"), Ok(Some(0)));
        assert_eq!(ended("exit 1"), Ok(Some(1)));
        assert_eq!(
            ended("exit 2"),
            Err("Checking the Codex registration failed: it printed no error and ended with exit status: 2".into())
        );
        assert!(ended("kill -9 $$").unwrap_err().starts_with(
            "Checking the Codex registration failed: it printed no error and ended with signal: 9"
        ),);
    }

    /// A client command that fails without a word still says how it ended,
    /// so the failure can be told apart from one the client explained.
    #[cfg(unix)]
    #[test]
    fn a_silent_failure_names_how_the_command_ended() {
        let error = checked(
            Command::new("sh").args(["-c", "exit 3"]),
            "Claude Code registration",
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            error,
            "Claude Code registration failed: it printed no error and ended with exit status: 3"
        );
        let error = checked(
            Command::new("sh").args(["-c", "kill -9 $$"]),
            "Codex registration",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.starts_with(
                "Codex registration failed: it printed no error and ended with signal: 9"
            ),
            "{error}"
        );
        let error = checked(
            Command::new("sh").args(["-c", "echo refused >&2; exit 1"]),
            "Codex registration",
        )
        .unwrap_err()
        .to_string();
        assert_eq!(error, "Codex registration failed: refused");
    }

    #[cfg(unix)]
    #[test]
    fn doctor_names_a_store_other_accounts_can_read() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("store");
        fs::create_dir(&folder).unwrap();
        // Others may list the folder; they read nothing in it.
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).unwrap();
        let db = folder.join("memories.sqlite3");
        fs::write(&db, "").unwrap();
        fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
        let check = || {
            let (mut actions, mut failures) = (Vec::new(), Vec::new());
            check_privacy(&db, &mut actions, &mut failures);
            assert!(failures.is_empty(), "{failures:?}");
            actions
        };
        assert!(check().is_empty());
        fs::set_permissions(&db, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            check(),
            [format!(
                "Store privacy: other accounts on this machine can read {}; run: chmod 600 '{}'",
                db.display(),
                db.display()
            )]
        );
    }

    /// The Windows upgrade path, which renames only, run here too: the new
    /// binary takes the place, the earlier one is gone once nothing runs
    /// it, and a failed swap puts the earlier one back.
    #[test]
    fn an_upgrade_sets_the_earlier_binary_aside() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("user-memory.exe");
        let new = dir.path().join(".user-memory-1.exe");
        fs::write(&new, "new").unwrap();
        replace_aside(&new, &destination).unwrap();
        assert_eq!(fs::read_to_string(&destination).unwrap(), "new");
        fs::write(&new, "newer").unwrap();
        replace_aside(&new, &destination).unwrap();
        assert_eq!(fs::read_to_string(&destination).unwrap(), "newer");
        let names = |dir: &Path| {
            let mut names: Vec<String> = fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        assert_eq!(names(dir.path()), ["user-memory.exe"]);
        // A missing new binary fails, and the earlier one stays in place.
        let error = replace_aside(&dir.path().join("missing.exe"), &destination);
        assert!(error.is_err());
        assert_eq!(fs::read_to_string(&destination).unwrap(), "newer");
        assert_eq!(names(dir.path()), ["user-memory.exe"]);
        // Left-over copies are cleared; other files stay.
        fs::write(dir.path().join("user-memory.old-7-1.exe"), "old").unwrap();
        fs::write(dir.path().join("notes.txt"), "mine").unwrap();
        remove_set_aside(dir.path());
        assert_eq!(names(dir.path()), ["notes.txt", "user-memory.exe"]);
    }

    #[test]
    fn windows_finds_a_client_through_its_pathext_extensions() {
        use std::ffi::OsStr;
        assert_eq!(program_files("codex", false, None), ["codex"]);
        assert_eq!(
            program_files("codex", true, None),
            ["codex.com", "codex.exe", "codex.bat", "codex.cmd"]
        );
        assert_eq!(
            program_files("claude", true, Some(OsStr::new(".EXE; .CMD;;bad;.")),),
            ["claude.exe", "claude.cmd"]
        );
        assert_eq!(
            program_files("claude", true, Some(OsStr::new(" "))),
            ["claude.com", "claude.exe", "claude.bat", "claude.cmd"]
        );
    }

    #[tokio::test]
    async fn finished_background_syncs_are_reaped() {
        // Without a saved setup beside the test binary each sync ends at once.
        for _ in 0..50 {
            sync_in_background();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let running = BACKGROUND
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, tokio::task::JoinSet::len);
        assert!(running <= 2, "{running} entries kept");
        finish_background_syncs().await;
    }
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        options: Options,
    }

    fn options(args: &[&str]) -> Options {
        Cli::parse_from(std::iter::once("setup").chain(args.iter().copied())).options
    }

    #[test]
    fn remote_installs_never_save_embedding_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join(".env");
        fs::write(
            &file,
            "USER_MEMORY_EMBED_URL=https://e.example.com/v1/embeddings\n",
        )
        .unwrap();
        let paths = Paths {
            binary: tmp.path().join("user-memory"),
            db: tmp.path().join("memory.sqlite3"),
            remote: Some(crate::remote::Connection {
                url: "https://memory.example.com/mcp".into(),
                token_file: tmp.path().join("token"),
            }),
        };
        let saved = Installation {
            db: None,
            remote: None,
            clients: vec![],
            hooks: true,
            skill: true,
            instructions: true,
            update_check: true,
            server_only: false,
            path_added: false,
            embedding: embedding::Settings::from([(
                "USER_MEMORY_EMBED_URL".into(),
                "https://old.example.com".into(),
            )]),
        };
        let given = options(&["--env-file", file.to_str().unwrap()]);
        let (settings, note) = embedding_to_save(&given, &paths, Some(&saved)).unwrap();
        assert!(settings.is_empty());
        assert_eq!(note, "not saved; the memory server owns embedding");
        let (settings, note) = embedding_to_save(&options(&[]), &paths, Some(&saved)).unwrap();
        assert!(settings.is_empty());
        assert_eq!(note, "managed by the memory server");
    }
}
