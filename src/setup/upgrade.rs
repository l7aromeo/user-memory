//! `user-memory upgrade`: moves this machine to a newer release the way it
//! was installed, from the saved setup beside the binary. A client of a
//! memory server takes its server's version, so it never runs ahead of it;
//! a native install or the machine running the installer's Docker server
//! takes the newest release, or the one `--version` names. Upgrades only
//! move forward; going back is the playbook's rollback.

use super::{
    Client, Installation, current_directory, installation, removal, remove_set_aside,
    replace_aside, update,
    versions::{self, Version},
};
use anyhow::{Context, Result, bail};
use clap::Args;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

#[derive(Args)]
pub struct Options {
    /// Install this release, such as v0.10.0, instead of the newest; only
    /// where this machine picks its own version (a native install or the
    /// memory server's own machine).
    #[arg(long, value_name = "vX.Y.Z")]
    version: Option<String>,
    /// Only say what the upgrade would do; change nothing.
    #[arg(long)]
    check: bool,
    /// Upgrade without asking: needed where there is no terminal to ask
    /// on, such as an agent's shell or a script, once the user said yes.
    #[arg(long)]
    yes: bool,
}

/// What a machine set up without the installer is told.
const NOT_INSTALLED: &str = "This user-memory wasn't installed by the installer, so upgrade can't tell how to upgrade it; upgrade it the way it was installed.";

/// How long a download may take: a release binary is about 30 MB, which a
/// slow 1 Mbit/s link takes about four minutes to bring.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// How long reaching the releases page may take before the upgrade gives
/// up, as the installer's own download waits.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The client build of this platform, as the release names its assets.
const TARGET: Option<&str> = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some("aarch64-apple-darwin")
} else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    Some("x86_64-apple-darwin")
} else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some("x86_64-unknown-linux-gnu")
} else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
    Some("aarch64-unknown-linux-gnu")
} else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
    Some("x86_64-pc-windows-msvc")
} else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
    Some("aarch64-pc-windows-msvc")
} else {
    None
};

/// How this machine was installed.
enum Machine {
    /// A store on this machine.
    Native,
    /// The installer's Docker server runs here.
    Docker { dir: PathBuf, url: String },
    /// A client of a memory server elsewhere.
    Client(crate::remote::Connection),
}

/// What the upgrade will do.
struct Plan {
    from: Version,
    to: Version,
    /// Said before anything else, such as why this version.
    note: Option<String>,
    /// Where a client's server is, for its refusals.
    server: Option<String>,
}

pub async fn upgrade(options: &Options) -> Result<()> {
    let binary = env::current_exe().context("Cannot locate this binary")?;
    let dir = current_directory().context("Cannot locate this binary")?;
    if cfg!(windows) {
        // What earlier upgrades set aside goes once nothing runs it.
        remove_set_aside(&dir);
    }
    let saved = installation(&dir).ok().flatten().context(NOT_INSTALLED)?;
    let from = Version::parse(versions::running()).with_context(|| {
        format!(
            "This user-memory ({}) isn't a release; upgrade it the way it was installed.",
            versions::running()
        )
    })?;
    let base = update::releases_base();
    let machine = match &saved.remote {
        None => Machine::Native,
        Some(remote) => match super::docker_dir(&remote.url) {
            Some(dir) => Machine::Docker {
                dir,
                url: remote.url.clone(),
            },
            None => Machine::Client(remote.clone()),
        },
    };
    let Some(plan) = plan(options, &machine, from, &base).await? else {
        return Ok(());
    };
    if let Some(note) = &plan.note {
        println!("{note}");
    }
    if options.check {
        println!(
            "Would upgrade user-memory from {} to {} on this machine{}; nothing was changed.",
            plan.from,
            plan.to,
            match &machine {
                Machine::Docker { dir, .. } =>
                    format!(", and the Docker memory server in {}", dir.display()),
                _ => String::new(),
            }
        );
        return Ok(());
    }
    if !options.yes {
        confirm(&plan)?;
    }
    let target = TARGET.context("No release of user-memory is built for this system")?;
    let assets = format!("{base}/download/v{}", plan.to);
    let staged = dir.join(format!(
        ".user-memory-upgrade-{}{}",
        std::process::id(),
        env::consts::EXE_SUFFIX
    ));
    println!("Downloading user-memory {} for {target}", plan.to);
    let downloaded = download(
        &assets,
        &format!("user-memory-{target}{}", env::consts::EXE_SUFFIX),
        &staged,
    )
    .await;
    if let Err(err) = downloaded {
        let _ = fs::remove_file(&staged);
        return Err(match (err, &plan.server) {
            (Download::Missing, Some(server)) => anyhow::anyhow!(
                "The memory server at {server} runs {}, which isn't a release; this machine stays at {}.",
                plan.to,
                plan.from
            ),
            (Download::Missing, None) => anyhow::anyhow!(
                "user-memory {} has no build for this system ({target}) at {assets}; nothing was changed.",
                plan.to
            ),
            (Download::Mismatch, _) => anyhow::anyhow!(
                "The download of user-memory {} doesn't match its published checksum; nothing was installed.",
                plan.to
            ),
            (Download::Failed(err), _) => err.context("Nothing was installed"),
        });
    }
    println!("Checksum: OK");
    let installed = async {
        if let Machine::Docker { dir, url } = &machine {
            docker_server(&staged, dir, url, &assets).await?;
        }
        install(&staged, &binary)
    }
    .await;
    if let Err(err) = installed {
        let _ = fs::remove_file(&staged);
        return Err(err);
    }
    // Setup with the saved choices: a native store is backed up and
    // migrated, the agent apps' registrations and hooks refreshed.
    let status = installed_command(&binary)
        .args(setup_arguments(&dir, &saved))
        .stdin(Stdio::null())
        .status()
        .context("Cannot run setup with the upgraded binary")?;
    if !status.success() {
        bail!(
            "user-memory {} is installed, but its setup failed (above); fix what it says, then run: {} setup",
            plan.to,
            crate::shell::quote(&binary)
        );
    }
    // The installer's Docker server runs in its container, rebuilt above.
    let host = !matches!(machine, Machine::Docker { .. }) && own_host_runs(&binary);
    if host {
        println!(
            "Upgraded to {}. The memory server running on this machine (user-memory host) keeps {} until it restarts; restart it the way you run it.",
            plan.to, plan.from
        );
    } else {
        println!("Upgraded to {}.", plan.to);
    }
    Ok(())
}

/// The release this machine moves to, or `None` once it said there is
/// nothing to do.
async fn plan(
    options: &Options,
    machine: &Machine,
    from: Version,
    base: &str,
) -> Result<Option<Plan>> {
    if let Machine::Client(connection) = machine {
        let server = &connection.url;
        let said = connection.handshake().await.map_err(|_| {
            anyhow::anyhow!(
                "The memory server at {server} can't be reached, so its version is unknown; nothing was changed."
            )
        })?;
        let version = said
            .pointer("/serverInfo/version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("no version")
            .to_owned();
        if options.version.is_some() {
            bail!(
                "This machine follows the memory server at {server} ({version}); upgrade the server on its machine to move it."
            );
        }
        let Some(to) = Version::parse(&version) else {
            bail!(
                "The memory server at {server} runs {version}, which isn't a release; this machine stays at {from}."
            );
        };
        if to < from {
            println!(
                "Nothing to upgrade: this machine ({from}) is ahead of the memory server at {server} ({to}), which is upgraded on its own machine."
            );
            return Ok(None);
        }
        if to == from {
            println!(
                "Nothing to upgrade: this machine already runs {from}, as the memory server at {server} does."
            );
            return Ok(None);
        }
        let newest = update::latest(base)
            .await
            .as_deref()
            .and_then(Version::parse)
            .filter(|newest| *newest > to);
        let note = match newest {
            Some(newest) => format!(
                "Upgrading to {to} to match the memory server at {server} ({newest} is the newest release; the server is upgraded on its own machine)"
            ),
            None => format!("Upgrading to {to} to match the memory server at {server}"),
        };
        return Ok(Some(Plan {
            from,
            to,
            note: Some(note),
            server: Some(server.clone()),
        }));
    }
    let to = match &options.version {
        Some(tag) => Version::parse(tag.strip_prefix('v').unwrap_or(tag))
            .context("--version takes a release tag such as v0.10.0")?,
        None => {
            let newest = update::latest(base).await.with_context(|| {
                format!("Cannot read the newest release from {base}; nothing was changed.")
            })?;
            Version::parse(&newest).with_context(|| {
                format!("The newest release, {newest}, isn't a version upgrade can install; nothing was changed.")
            })?
        }
    };
    if to < from {
        bail!(
            "user-memory upgrade only moves forward (this is {from}). To go back, restore the backup the upgrade made: see the user-memory skill's upgrade playbook."
        );
    }
    if to == from {
        println!("Nothing to upgrade: this machine already runs {from}.");
        return Ok(None);
    }
    Ok(Some(Plan {
        from,
        to,
        note: None,
        server: None,
    }))
}

/// Asks the user at the terminal, naming the versions; refuses without one,
/// where only `--yes` confirms.
fn confirm(plan: &Plan) -> Result<()> {
    let question = format!(
        "Upgrade user-memory from {} to {} on this machine? [y/N] ",
        plan.from, plan.to
    );
    let Some(mut terminal) = removal::Terminal::open() else {
        bail!(
            "user-memory upgrade changes this machine; without a terminal to confirm, it also needs --yes. Nothing was changed."
        );
    };
    if !terminal.ask(&question, false)? {
        bail!("Nothing was changed.");
    }
    Ok(())
}

/// Why a download installed nothing.
enum Download {
    /// The release has no such asset.
    Missing,
    /// It doesn't match its published SHA-256.
    Mismatch,
    Failed(anyhow::Error),
}

/// Downloads `name` and its `.sha256` from `assets` into `path`, keeping it
/// only when it matches, as the installer does.
async fn download(assets: &str, name: &str, path: &Path) -> Result<(), Download> {
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .map_err(|err| Download::Failed(err.into()))?;
    let fetch = |url: String| {
        let client = client.clone();
        async move {
            update::allowed(&url).then_some(()).ok_or_else(|| {
                Download::Failed(anyhow::anyhow!(
                    "{url} is neither HTTPS nor on this machine"
                ))
            })?;
            let response = client.get(&url).send().await.map_err(|err| {
                Download::Failed(anyhow::Error::from(err).context(format!("Cannot download {url}")))
            })?;
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Err(Download::Missing);
            }
            let response = response.error_for_status().map_err(|err| {
                Download::Failed(anyhow::Error::from(err).context(format!("Cannot download {url}")))
            })?;
            response.bytes().await.map_err(|err| {
                Download::Failed(anyhow::Error::from(err).context(format!("Cannot download {url}")))
            })
        }
    };
    let checksum = fetch(format!("{assets}/{name}.sha256")).await?;
    let expected = String::from_utf8_lossy(&checksum)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase();
    let body = fetch(format!("{assets}/{name}")).await?;
    if sha256(&body) != expected {
        return Err(Download::Mismatch);
    }
    write_executable(path, &body)
        .map_err(|err| Download::Failed(err.context(format!("Cannot write {}", path.display()))))
}

/// The SHA-256 of `bytes` in lowercase hex, as the release's `.sha256`
/// files give it.
fn sha256(bytes: &[u8]) -> String {
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn write_executable(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// Puts the verified `staged` binary in place of `binary` in one rename;
/// on Windows, which can't replace a running program, the one running is
/// renamed aside first, and those earlier upgrades set aside go once
/// nothing runs them.
fn install(staged: &Path, binary: &Path) -> Result<()> {
    let replaced = if cfg!(windows) {
        replace_aside(staged, binary)
    } else {
        fs::rename(staged, binary)
    };
    replaced.with_context(|| format!("Cannot install {}", binary.display()))
}

/// Rebuilds the installer's Docker server in `dir` from the release at
/// `assets`, with the new binary `staged`, which upgrades its store after a
/// backup, as the installer does; before the client binary is replaced, so
/// a failure leaves this machine as it was.
async fn docker_server(staged: &Path, dir: &Path, url: &str, assets: &str) -> Result<()> {
    let name = env::var("USER_MEMORY_DOCKER_NAME").unwrap_or_else(|_| "user-memory".into());
    let port = reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.port())
        .context("The Docker memory server's address has no port")?
        .to_string();
    let place = [
        "--dir".into(),
        dir.as_os_str().to_owned(),
        "--name".into(),
        name.into(),
        "--port".into(),
        port.into(),
        "--upgrade".into(),
    ];
    let run = |args: &[std::ffi::OsString]| -> Result<String> {
        let output = installed_command(staged)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .output()
            .with_context(|| format!("Cannot run {}", staged.display()))?;
        if !output.status.success() {
            bail!(
                "The Docker memory server was not upgraded (above); nothing was changed on this machine."
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let check: Vec<std::ffi::OsString> = ["server".into(), "check".into()]
        .into_iter()
        .chain(place.iter().cloned())
        .collect();
    print!("{}", run(&check)?);
    let target = run(&["server".into(), "target".into()])?.trim().to_owned();
    println!("Downloading the {target} server build");
    let build = staged.with_extension("server");
    let downloaded = download(assets, &format!("user-memory-{target}"), &build).await;
    let result = match downloaded {
        Ok(()) => {
            let install: Vec<std::ffi::OsString> = ["server".into(), "install".into()]
                .into_iter()
                .chain(place.iter().cloned())
                .chain(["--build".into(), build.as_os_str().to_owned()])
                .collect();
            let status = installed_command(staged)
                .args(&install)
                .stdin(Stdio::null())
                .status()
                .with_context(|| format!("Cannot run {}", staged.display()))?;
            if status.success() {
                Ok(())
            } else {
                Err(anyhow::anyhow!(
                    "The Docker memory server was not upgraded (above); nothing was changed on this machine."
                ))
            }
        }
        Err(Download::Mismatch) => Err(anyhow::anyhow!(
            "The download of the {target} server build doesn't match its published checksum; nothing was installed."
        )),
        Err(Download::Missing) => Err(anyhow::anyhow!(
            "The release has no {target} server build at {assets}; nothing was changed."
        )),
        Err(Download::Failed(err)) => Err(err.context("Nothing was installed")),
    };
    let _ = fs::remove_file(&build);
    result
}

/// A command running the release being installed. A debug build's test
/// version names this process, not the binary it installs.
fn installed_command(binary: &Path) -> Command {
    let mut command = Command::new(binary);
    #[cfg(debug_assertions)]
    command.env_remove(versions::TEST_VERSION);
    command
}

/// The setup command that refreshes this install with its saved choices.
fn setup_arguments(dir: &Path, saved: &Installation) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> =
        vec!["setup".into(), "--install-dir".into(), dir.into()];
    if saved.server_only {
        args.push("--server-only".into());
    } else {
        let client = match (
            saved.clients.contains(&Client::Codex),
            saved.clients.contains(&Client::Claude),
        ) {
            (true, true) => "both",
            (true, false) => "codex",
            (false, true) => "claude",
            (false, false) => "auto",
        };
        args.extend(["--client".into(), client.into()]);
    }
    args
}

/// Whether a `user-memory host` runs from `binary` on this machine: an
/// operator's own server, which the upgrade never restarts.
fn own_host_runs(binary: &Path) -> bool {
    let binary = crate::paths::resolved(binary);
    let listed = if cfg!(windows) {
        Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-CimInstance Win32_Process -Filter \"Name='user-memory.exe'\" | ForEach-Object { $_.ExecutablePath + [char]9 + $_.CommandLine }",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
    } else {
        Command::new("ps")
            .args(["-axww", "-o", "args="])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
    };
    let Ok(listed) = listed else {
        return false;
    };
    String::from_utf8_lossy(&listed.stdout).lines().any(|line| {
        let (program, args) = if cfg!(windows) {
            let (path, command) = line.split_once('\t').unwrap_or((line, ""));
            let args = command.trim_start_matches('"');
            let args = args.split_once(".exe").map_or(args, |(_, rest)| rest);
            (path.to_owned(), args.trim_start_matches('"').to_owned())
        } else {
            let (program, args) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
            (program.to_owned(), args.to_owned())
        };
        args.split_whitespace().next() == Some("host")
            && crate::paths::same(&crate::paths::resolved(Path::new(&program)), &binary)
    })
}
