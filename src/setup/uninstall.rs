//! `user-memory uninstall`: undoes what setup did on this machine, with
//! setup's own removal code: client registrations and hooks first, so no
//! agent starts a session against a half-removed install, then the files,
//! then the memories, which stay unless the user confirms deleting them.
//! The binary itself is left to the uninstall script, and the installer's
//! Docker server and its volume to `server remove`, which the script runs
//! next. Nothing setup did not create is touched.

use super::{
    Client, RECEIPT, SERVER, answered, claude_user_entry, current_directory, find_client,
    git_ignore, hooks, installation, instructions,
    removal::{self, Memories, Report},
    skill,
};
use crate::paths;
use anyhow::{Context, Result, bail};
use clap::Args;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

#[derive(Args)]
pub struct Options {
    /// List what would be removed or kept, and change nothing.
    #[arg(long)]
    dry_run: bool,
    /// Delete the memories too: the local store and its backups. On a
    /// terminal uninstall asks anyway; without one this needs --yes.
    #[arg(long)]
    purge: bool,
    /// With --purge, delete without asking.
    #[arg(long, requires = "purge")]
    yes: bool,
    /// With --purge, skip the export taken before deleting.
    #[arg(long, requires = "purge")]
    no_export: bool,
    /// Remove the registration and hooks of every client, or only of one.
    #[arg(long, value_enum, default_value = "auto")]
    client: Client,
    /// Directory of the installed binary and its saved setup (default: this
    /// binary's).
    #[arg(long)]
    install_dir: Option<PathBuf>,
    /// Set by the uninstall scripts, which remove the binary themselves afterwards.
    #[arg(long, hide = true)]
    binary_removed_after: bool,
}

pub fn uninstall(options: &Options) -> Result<()> {
    let choice = Memories::choose(
        options.purge,
        options.yes,
        options.no_export,
        options.dry_run,
    )?;
    let install_dir = options
        .install_dir
        .clone()
        .or_else(current_directory)
        .context("Cannot locate the installed binary; pass --install-dir")?;
    let saved = installation(&install_dir).ok().flatten();
    let mut report = Report {
        dry_run: options.dry_run,
        ..Report::default()
    };
    println!(
        "{}",
        if options.dry_run {
            "Uninstalling user memory (dry run: nothing is changed)…"
        } else {
            "Uninstalling user memory…"
        }
    );
    let kinds: Vec<Client> = match options.client {
        Client::Claude => vec![Client::Claude],
        Client::Codex => vec![Client::Codex],
        _ => vec![Client::Claude, Client::Codex],
    };
    for kind in &kinds {
        registration(*kind, &mut report)?;
    }
    for kind in &kinds {
        session_hooks(*kind, &mut report)?;
    }
    skill_folder(&mut report)?;
    instruction_files(&mut report)?;
    git_ignore_lines(&mut report)?;
    state_folder(&mut report)?;
    client_token(saved.as_ref(), &mut report)?;
    path_entry(saved.as_ref(), &install_dir, &mut report)?;
    let receipt = install_dir.join(RECEIPT);
    if receipt.is_file() {
        if !options.dry_run {
            fs::remove_file(&receipt)
                .with_context(|| format!("Cannot remove {}", receipt.display()))?;
        }
        report.removed(format!("saved setup {}", receipt.display()));
    }
    memories(saved.as_ref(), choice, &mut report)?;
    folder_files(&mut report);
    if report.changed == 0 && report.left.is_empty() {
        println!("  Nothing to remove.");
    }
    let binary = install_dir.join(paths::BINARY);
    if binary.exists() && !options.dry_run && !options.binary_removed_after {
        report.note(if cfg!(windows) {
            format!(
                "The user-memory binary is still at {}; uninstall.ps1 removes it, or run: Remove-Item -LiteralPath '{}'",
                binary.display(),
                binary.display().to_string().replace('\'', "''")
            )
        } else {
            format!(
                "The user-memory binary is still at {}; uninstall.sh removes it, or run: rm {}",
                binary.display(),
                crate::shell::quote(&binary)
            )
        });
    }
    for note in &report.notes {
        println!("\n{note}");
    }
    if !report.left.is_empty() {
        bail!(
            "{} item(s) could not be removed (\"Left\" above, each with what to do)",
            report.left.len()
        );
    }
    Ok(())
}

/// Removes the user-scope registration; a project- or local-scope Claude
/// Code entry is the user's own and only named.
fn registration(kind: Client, report: &mut Report) -> Result<()> {
    let (name, label) = if kind == Client::Codex {
        ("codex", "Codex")
    } else {
        ("claude", "Claude Code")
    };
    let command = find_client(name);
    let remove_args: &[&str] = if kind == Client::Codex {
        &["mcp", "remove", SERVER]
    } else {
        &["mcp", "remove", "--scope", "user", SERVER]
    };
    let manual = format!("{name} {}", remove_args.join(" "));
    // The client's own answer; a query that fails is no answer.
    let asked = |client: &Path, args: &[&str]| -> std::result::Result<Output, String> {
        let output = Command::new(client)
            .args(args)
            .output()
            .map_err(|err| format!("{label} could not start: {err}"))?;
        answered(output, &format!("Checking the {label} registration"))
            .map_err(|err| format!("{err:#}"))
    };
    let registered = match (kind, &command) {
        // In Claude Code's user configuration, or shown at user scope.
        (Client::Claude, command) => {
            if claude_user_entry(SERVER)?.is_some() {
                true
            } else {
                match command
                    .as_ref()
                    .map(|claude| asked(claude, &["mcp", "get", SERVER]))
                {
                    Some(Ok(out)) => {
                        out.status.success()
                            && String::from_utf8_lossy(&out.stdout)
                                .lines()
                                .any(|line| line.trim().starts_with("Scope: User config"))
                    }
                    Some(Err(failed)) => {
                        report.left(format!(
                            "{label} registration: {failed}; run uninstall again once {name} works, or run: {manual}"
                        ));
                        false
                    }
                    None => false,
                }
            }
        }
        (_, Some(codex)) => match asked(codex, &["mcp", "get", SERVER, "--json"]) {
            Ok(out) => out.status.success(),
            Err(failed) => {
                report.left(format!(
                    "{label} registration: {failed}; run uninstall again once {name} works, or run: {manual}"
                ));
                false
            }
        },
        (_, None) => fs::read_to_string(hooks::codex_config()?)
            .is_ok_and(|text| text.contains(&format!("[mcp_servers.{SERVER}]"))),
    };
    if registered {
        match &command {
            None => report.left(format!(
                "{label} registration ({name} not found to remove it); run: {manual}"
            )),
            Some(_) if report.dry_run => report.removed(format!("{label} registration")),
            Some(command) => {
                let removed = Command::new(command)
                    .args(remove_args)
                    .output()
                    .is_ok_and(|out| out.status.success());
                if removed {
                    report.removed(format!("{label} registration"));
                } else {
                    report.left(format!("{label} registration; run: {manual}"));
                }
            }
        }
    }
    if kind == Client::Claude
        && let Some(claude) = &command
        && let Ok(out) = Command::new(claude).args(["mcp", "get", SERVER]).output()
        && out.status.success()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(scope) = text
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with("Scope:") && !line.starts_with("Scope: User config"))
        {
            let here = env::current_dir()
                .map(|dir| dir.display().to_string())
                .unwrap_or_default();
            let flag = if scope.contains("Local") {
                "local"
            } else {
                "project"
            };
            report.note(format!(
                "Claude Code still has a {flag}-scope user-memory entry in {here} ({scope}); it is yours and was left. To remove it: claude mcp remove {SERVER} --scope {flag}"
            ));
        }
    }
    Ok(())
}

fn session_hooks(kind: Client, report: &mut Report) -> Result<()> {
    let (file, label) = if kind == Client::Codex {
        (hooks::codex_hooks()?, "Codex")
    } else {
        (hooks::claude_settings()?, "Claude Code")
    };
    let mut removed_any = false;
    for hook in hooks::HOOKS {
        if !hooks::present(&file, hook)? {
            continue;
        }
        if !report.dry_run {
            hooks::apply(&file, hook, None)?;
        }
        removed_any = true;
        report.removed(format!("{label} {} hook ({})", hook.event, file.display()));
    }
    // A file this run emptied holds nothing of the user's: setup created it
    // for the hooks.
    if removed_any
        && !report.dry_run
        && !instructions::is_link(&file)
        && fs::read_to_string(&file).is_ok_and(|text| {
            serde_json::from_str::<serde_json::Value>(&text)
                .is_ok_and(|value| value == serde_json::json!({}))
        })
    {
        fs::remove_file(&file).with_context(|| format!("Cannot remove {}", file.display()))?;
        report.removed(format!("{} (left empty)", file.display()));
    }
    Ok(())
}

fn skill_folder(report: &mut Report) -> Result<()> {
    let places = skill::Places::for_clients(true)?;
    if let skill::State::Foreign(path) = skill::state(&places, skill::BUNDLED)? {
        report.note(format!(
            "{} is a user-memory skill setup did not install; it was left.",
            path.display()
        ));
    }
    if report.dry_run {
        // The shared folder alone: a client's link may be missing.
        let folder = skill::Places::for_clients(false)?;
        if matches!(
            skill::state(&folder, skill::BUNDLED)?,
            skill::State::Ok | skill::State::Outdated
        ) {
            report.removed(format!("skill ({})", places.describe()));
        }
        return Ok(());
    }
    // Removes only the folder and link setup installed.
    if skill::remove(&places, skill::BUNDLED)? == skill::Change::Removed {
        report.removed(format!("skill ({})", places.describe()));
    }
    Ok(())
}

fn instruction_files(report: &mut Report) -> Result<()> {
    for path in instructions::holding_rules()? {
        if instructions::is_link(&path) {
            report.left(format!(
                "the user-memory rules in {}, a symbolic link user-memory does not write through; remove them there yourself (the whole generated file, or the block between <!-- user-memory:start --> and <!-- user-memory:end -->)",
                path.display()
            ));
            continue;
        }
        if !report.dry_run {
            instructions::remove_rules(&path)?;
        }
        report.removed(format!("the user-memory rules in {}", path.display()));
    }
    Ok(())
}

fn git_ignore_lines(report: &mut Report) -> Result<()> {
    let Some(added) = git_ignore::added()? else {
        return Ok(());
    };
    let lines = added.lines.join(", ");
    if let Some(reason) = &added.blocked {
        report.left(format!(
            "the lines {lines} in {} ({reason}); remove them there yourself",
            added.target.display()
        ));
        return Ok(());
    }
    if !report.dry_run {
        git_ignore::remove_added(&added)?;
    }
    report.removed(format!(
        "the global git ignore lines {lines} ({})",
        added.target.display()
    ));
    Ok(())
}

/// A folder of ours, when it is a real folder; never through a link.
fn own_folder(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// Session snapshots and anything else in user-memory's state folder.
fn state_folder(report: &mut Report) -> Result<()> {
    let Some(folder) = paths::state_dir() else {
        return Ok(());
    };
    if own_folder(&folder) {
        if !report.dry_run {
            fs::remove_dir_all(&folder)
                .with_context(|| format!("Cannot remove {}", folder.display()))?;
        }
        report.removed(format!("session snapshots ({})", folder.display()));
    }
    Ok(())
}

/// The user PATH entry setup added for the bin folder (Windows); an entry
/// the user made is theirs and stays.
fn path_entry(
    saved: Option<&super::Installation>,
    install_dir: &Path,
    report: &mut Report,
) -> Result<()> {
    if !saved.is_some_and(|saved| saved.path_added) {
        return Ok(());
    }
    let entry = format!("the PATH entry {}", install_dir.display());
    if report.dry_run {
        report.removed(entry);
        return Ok(());
    }
    #[cfg(windows)]
    if super::path_env::remove(install_dir)
        .context("Cannot remove the bin folder from your PATH")?
    {
        report.removed(entry);
    }
    #[cfg(not(windows))]
    let _ = entry;
    Ok(())
}

/// The token file the installer saved, never one the user named.
fn client_token(saved: Option<&super::Installation>, report: &mut Report) -> Result<()> {
    let Some(folder) = paths::config_dir() else {
        return Ok(());
    };
    let token = folder.join("client-token");
    if fs::symlink_metadata(&token).is_ok_and(|meta| meta.is_file()) {
        if !report.dry_run {
            fs::remove_file(&token)
                .with_context(|| format!("Cannot remove {}", token.display()))?;
            let _ = fs::remove_dir(&folder);
        }
        report.removed(format!("the access token {}", token.display()));
    }
    if let Some(remote) = saved.and_then(|saved| saved.remote.as_ref())
        && remote.token_file != token
        && remote.token_file.exists()
    {
        report.note(format!(
            "The token file you gave setup, {}, is yours and was left.",
            remote.token_file.display()
        ));
    }
    Ok(())
}

/// Everything a local store consists of: the database, its journal files,
/// its writers' lock file, the backups beside it that migrate, repair and
/// project move write, and its automatic backups. A backups folder that
/// cannot be read is an error, so a purge never reports the memories
/// deleted while their automatic backups stay.
fn store_files(db: &Path) -> Result<Vec<PathBuf>> {
    let Some(name) = db
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return Ok(Vec::new());
    };
    let mut files: Vec<PathBuf> = ["", "-wal", "-shm", ".lock"]
        .iter()
        .map(|suffix| db.with_file_name(format!("{name}{suffix}")))
        .filter(|path| path.is_file())
        .collect();
    if let Some(dir) = db.parent()
        && let Ok(entries) = fs::read_dir(dir)
    {
        let mut backups: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name().is_some_and(|file| {
                    let file = file.to_string_lossy();
                    file.starts_with(&format!("{name}.")) && file.ends_with(".bak")
                })
            })
            .collect();
        backups.sort();
        files.extend(backups);
    }
    files.extend(crate::memory::backups::files(db)?);
    Ok(files)
}

fn memories(
    saved: Option<&super::Installation>,
    choice: Memories,
    report: &mut Report,
) -> Result<()> {
    if let Some(remote) = saved.and_then(|saved| saved.remote.as_ref()) {
        report.note(if docker_server(&remote.url) {
            "Your memories are in the Docker memory server the installer set up on this machine; the uninstall script removes that server and asks about them first.".to_owned()
        } else {
            format!(
                "Your memories stay on the memory server at {}; uninstall never touches a server.",
                remote.url
            )
        });
        return Ok(());
    }
    let db = saved
        .and_then(|saved| saved.db.clone())
        .unwrap_or_else(crate::db_path);
    let files = match store_files(&db) {
        Ok(files) => files,
        Err(err) if matches!(choice, Memories::Keep) => {
            report.note(format!(
                "Your memories were kept: {}. Their automatic backups could not be listed: {err:#}",
                db.display()
            ));
            return Ok(());
        }
        Err(err) => {
            report.left(format!(
                "your memories at {}, unchanged: {err:#}; fix that, then run uninstall again",
                db.display()
            ));
            return Ok(());
        }
    };
    if files.is_empty() {
        return Ok(());
    }
    let listed = files
        .iter()
        .map(|path| crate::shell::quote(path))
        .collect::<Vec<_>>()
        .join(" ");
    let keep = |report: &mut Report| {
        report.note(format!(
            "Your memories were kept: {} ({} file(s) with backups). To delete them later: rm {listed}",
            db.display(),
            files.len()
        ));
    };
    if report.dry_run {
        println!(
            "  Would keep: your memories at {} (unless you confirm deleting them)",
            db.display()
        );
        return Ok(());
    }
    let Some(export) = choice.decide(&db.display().to_string())? else {
        keep(report);
        return Ok(());
    };
    if export {
        let (document, _) = crate::memory::export_document(&db, &crate::memory::Subset::default())?;
        let path = removal::export_path()?;
        crate::memory::write_export(&path, &document, false)?;
        println!(
            "  Exported your memories to {} ({})",
            path.display(),
            removal::PRIVATE
        );
    }
    for path in &files {
        fs::remove_file(path).with_context(|| format!("Cannot remove {}", path.display()))?;
    }
    report.removed(format!(
        "your memories ({}, {} file(s))",
        db.display(),
        files.len()
    ));
    let _ = fs::remove_dir(crate::memory::backups::folder(&db));
    if let Some(dir) = db.parent() {
        let _ = fs::remove_dir(dir);
    }
    Ok(())
}

/// Whether `url` is the Docker server the installer set up on this machine.
fn docker_server(url: &str) -> bool {
    super::embedding::docker_dir(url).is_some()
}

/// Names a `.user-memory` or `.user-memory-key` in the current folder: the
/// user's own files, which uninstall leaves.
fn folder_files(report: &mut Report) {
    let Ok(here) = env::current_dir() else {
        return;
    };
    for name in [crate::project::LIST_FILE, crate::project::KEY_FILE] {
        let path = here.join(name);
        if path.is_file() {
            report.note(format!("{} is yours and was left.", path.display()));
        }
    }
}
