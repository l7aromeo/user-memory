//! `host token …` on the machine that runs the installer's Docker server:
//! each command runs in the server's container. A new token reaches this
//! machine only through a file copied out of the volume, owner-only, or
//! straight to this terminal; never through Docker's captured output or a
//! command line.

use super::{Compose, compose::SERVICE};
use crate::private;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    fs,
    io::IsTerminal,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

/// What a token file written into the volume for copying out is named
/// before its random suffix.
pub const ADD_PREFIX: &str = ".token-add-";
/// The volume's folder in the container.
const DATA: &str = "/data";
/// The container's shared token file.
const SHARED_FILE: &str = "/data/access-token";
/// The client token this machine's own client gets when the shared file is
/// retired.
pub const OWN_NAME: &str = "docker-host";
/// What the healthcheck an older installer wrote holds: it reads the shared
/// token file, so the container would turn unhealthy once it is retired.
const TOKEN_HEALTHCHECK: &str = "USER_MEMORY_TOKEN_FILE=/data/access-token";

/// How old a file left in the store's folder by an interrupted copy must be
/// before it is deleted: a copy takes seconds, so ten minutes never deletes
/// one in use.
pub const LEFTOVER_AGE: Duration = Duration::from_secs(10 * 60);

/// Deletes the token files that interrupted copies left in `folder`, the
/// store's folder; one younger than [`LEFTOVER_AGE`] may still be copied.
pub fn sweep(folder: &Path) {
    let Ok(entries) = fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > LEFTOVER_AGE);
        if old && entry.file_name().to_string_lossy().starts_with(ADD_PREFIX) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Runs `user-memory host token ARGS` in the container with this terminal
/// for its output; exits with its status when it fails, as it said why.
pub fn on_this_terminal(compose: &Compose, args: &[&str]) -> Result<()> {
    let mut command = compose.command(&["exec"]);
    if !std::io::stdin().is_terminal() {
        command.arg("-T");
    }
    let status = command
        .args([SERVICE, "user-memory", "host", "token"])
        .args(args)
        .status()
        .context("Cannot run docker")?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Runs `user-memory host token ARGS` in the container, failing with what
/// it said when it fails.
fn run(compose: &Compose, args: &[&str]) -> Result<String> {
    let args = [&["host", "token"], args].concat();
    let out = compose.exec(&[], &args)?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What the container says of its registry: see `host token state`.
#[derive(Deserialize)]
pub struct State {
    pub shared_file_retired: bool,
    pub writer: bool,
    pub named: bool,
}

/// Asks the container whether its shared file is retired, whether the token
/// whose SHA-256 is `hash` can write, and whether a live token has `name`.
pub fn state(compose: &Compose, hash: Option<&str>, name: Option<&str>) -> Result<State> {
    let mut args = vec!["state"];
    if let Some(hash) = hash {
        args.extend(["--hash", hash]);
    }
    if let Some(name) = name {
        args.extend(["--name", name]);
    }
    let said = run(compose, &args)?;
    serde_json::from_str(said.trim())
        .with_context(|| format!("The Docker server answered `host token state` with {said:?}"))
}

/// Issues a token named `name` in the container and copies it to `out`,
/// owner-only: a new file, or replacing `out` when `replace`. The copy in
/// the volume is deleted, or by [`sweep`] if this stops first.
pub fn add_to_file(
    compose: &Compose,
    name: &str,
    read_only: bool,
    expires: Option<u32>,
    out: &Path,
    replace: bool,
) -> Result<()> {
    if !replace && fs::symlink_metadata(out).is_ok() {
        bail!("{} exists; remove it or give another --out.", out.display());
    }
    let suffix: u64 = rand::random();
    let inside = format!("{DATA}/{ADD_PREFIX}{suffix:016x}");
    let expires = expires.map(|days| days.to_string());
    let mut args = vec!["add", name, "--out", &inside];
    if read_only {
        args.push("--read-only");
    }
    if let Some(days) = &expires {
        args.extend(["--expires", days]);
    }
    let copied = run(compose, &args).and_then(|_| copy_out(compose, &inside, out, replace));
    let _ = compose
        .command(&["exec", "-T", SERVICE, "rm", "-f", &inside])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    copied
}

/// Copies the container's file `inside` to `out` through a folder only this
/// user can read, so the token is never readable by others on the way.
fn copy_out(compose: &Compose, inside: &str, out: &Path, replace: bool) -> Result<()> {
    let folder = match out.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    fs::create_dir_all(&folder).with_context(|| format!("Cannot create {}", folder.display()))?;
    let name = out
        .file_name()
        .with_context(|| format!("{} is not a file name", out.display()))?
        .to_string_lossy()
        .into_owned();
    let suffix: u64 = rand::random();
    let staging = folder.join(format!(".{name}.{suffix:016x}"));
    private::create_dir(&staging)
        .with_context(|| format!("Cannot create {}", staging.display()))?;
    let copy = staging.join("token");
    let placed = (|| -> Result<()> {
        let status = compose
            .command(&["cp", &format!("{SERVICE}:{inside}")])
            .arg(&copy)
            .status()
            .context("Cannot run docker")?;
        if !status.success() {
            bail!("Cannot copy the token out of the container");
        }
        private::restrict(&copy)?;
        if replace {
            fs::rename(&copy, out)
        } else {
            fs::hard_link(&copy, out)
        }
        .with_context(|| format!("Cannot write {}", out.display()))
    })();
    let _ = fs::remove_dir_all(&staging);
    placed
}

/// What the container says of the token in `token_file`, when there is one.
fn of_file(compose: &Compose, token_file: &Path) -> Result<State> {
    let hash = fs::read_to_string(token_file)
        .ok()
        .map(|token| crate::registry::hash(token.trim()));
    state(compose, hash.as_deref(), Some(OWN_NAME))
}

/// This machine's client token after an install: kept while it can write,
/// such as its own token; else, while the shared file works, the shared
/// token, which `copy_shared` copies; else its own token.
pub fn client_token(
    compose: &Compose,
    token_file: &Path,
    copy_shared: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let found = of_file(compose, token_file)?;
    if found.writer {
        return Ok(());
    }
    if !found.shared_file_retired {
        return copy_shared();
    }
    issue_own(compose, token_file, &found)
}

/// Gives this machine's client its own write token, [`OWN_NAME`], in
/// `token_file`, unless the token there already can write.
pub fn own_token(compose: &Compose, token_file: &Path) -> Result<()> {
    let found = of_file(compose, token_file)?;
    if found.writer {
        return Ok(());
    }
    issue_own(compose, token_file, &found)
}

/// Issues [`OWN_NAME`] into `token_file`; a stale one (from a stop before
/// the file was replaced, or revoked by hand) is revoked first.
fn issue_own(compose: &Compose, token_file: &Path, found: &State) -> Result<()> {
    if found.named {
        run(compose, &["revoke", OWN_NAME])?;
    }
    add_to_file(compose, OWN_NAME, false, None, token_file, true)?;
    eprintln!(
        "This machine's client now has its own token, {OWN_NAME}, in {}.",
        token_file.display()
    );
    Ok(())
}

/// Retires the shared token file of the installer's Docker server in `dir`:
/// once its healthcheck needs no token, this machine's client gets its own
/// token in `token_file`, then the container retires its shared file.
pub fn retire_shared(compose: &Compose, dir: &Path, token_file: &Path) -> Result<()> {
    let file = dir.join("compose.yaml");
    let written =
        fs::read_to_string(&file).with_context(|| format!("Cannot read {}", file.display()))?;
    if written.contains(TOKEN_HEALTHCHECK) {
        bail!(
            "The Docker server's healthcheck still uses the shared token file; run the installer again, which updates it, then retire the shared file."
        );
    }
    own_token(compose, token_file)?;
    let out = compose.exec(
        &[],
        &[
            "host",
            "token",
            "retire-shared",
            "--token-file",
            SHARED_FILE,
        ],
    )?;
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        std::process::exit(out.status.code().unwrap_or(1));
    }
    Ok(())
}
