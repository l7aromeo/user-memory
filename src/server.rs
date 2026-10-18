//! The Docker server the installer sets up on this machine: its project
//! folder (Dockerfile, compose.yaml, embedding.env), image, container, store
//! upgrade, access token and removal. The installers and uninstallers call
//! these hidden subcommands, so one implementation serves every platform;
//! they keep the questions about installing, the downloads and the checksum
//! checks.

use crate::{
    private,
    setup::removal::{self, Memories, Report},
};
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use std::{
    fs,
    io::ErrorKind,
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

#[derive(Subcommand)]
pub enum ServerCommand {
    /// Fails, saying why, when the server cannot be set up or upgraded
    /// here: no Docker, Docker not running, or for a new server a port or
    /// volume another server uses. Run before any question is asked.
    Check(Check),
    /// Prints the Linux build the Docker daemon runs, such as
    /// aarch64-unknown-linux-gnu.
    Target,
    /// Writes the project from the given server build, builds and starts
    /// it, upgrading the store first when it replaces an earlier server.
    Install(Install),
    /// Stops and removes the server, its image and its project folder; its
    /// volume, and the memories in it, only when the user confirms deleting
    /// them. Exits 1 when something was left, each with what to do.
    Remove(Remove),
}

/// Where the installer's Docker project lives.
#[derive(Args)]
pub struct Place {
    /// The project folder, such as ~/.local/share/user-memory/docker.
    #[arg(long)]
    dir: PathBuf,
    /// The Docker project name; its volume is NAME-data.
    #[arg(long)]
    name: String,
    /// The port on 127.0.0.1 this machine's clients use.
    #[arg(long)]
    port: u16,
    /// The project replaces an earlier server of this installer.
    #[arg(long)]
    upgrade: bool,
}

#[derive(Args)]
pub struct Check {
    #[command(flatten)]
    place: Place,
}

#[derive(Args)]
pub struct Install {
    #[command(flatten)]
    place: Place,
    /// The verified Linux server build to run.
    #[arg(long)]
    build: PathBuf,
    /// A host name an HTTPS proxy forwards for (repeatable); an upgrade
    /// without any keeps the earlier ones.
    #[arg(long = "allowed-host")]
    allowed_hosts: Vec<String>,
    /// Also publish the server on this IPv4 address; an upgrade without it
    /// keeps the earlier one.
    #[arg(long)]
    bind: Option<String>,
    /// The titles map for an upgrade from a store before schema version 3.
    #[arg(long)]
    titles: Option<PathBuf>,
    /// Copy the server's access token here (mode 0600, in a 0700 folder).
    #[arg(long)]
    token_file: Option<PathBuf>,
}

#[derive(Args)]
pub struct Remove {
    /// The project folder, such as ~/.local/share/user-memory/docker.
    #[arg(long)]
    dir: PathBuf,
    /// The Docker project name; its volume is NAME-data.
    #[arg(long)]
    name: String,
    /// List what would be removed or kept, and change nothing.
    #[arg(long)]
    dry_run: bool,
    /// Delete the memories in the server's volume too. On a terminal this
    /// asks anyway; without one it needs --yes.
    #[arg(long)]
    purge: bool,
    /// With --purge, delete without asking.
    #[arg(long, requires = "purge")]
    yes: bool,
    /// With --purge, skip the export taken before deleting.
    #[arg(long, requires = "purge")]
    no_export: bool,
}

/// `server remove`'s exit status when something was left, as `uninstall`'s.
const LEFT: i32 = 1;
/// `server remove`'s exit status when it failed or refused to start.
const FAILED: i32 = 2;

/// The address every installed server listens on inside its container.
const CONTAINER_PORT: u16 = 8765;
/// How long the port probe waits; a free port refuses at once.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// The publish address this machine's own clients always use.
const LOOPBACK: &str = "127.0.0.1";

/// Runs `command`, prefixing an error as the installer's own messages are.
pub fn run(command: ServerCommand) -> i32 {
    let result = match command {
        ServerCommand::Check(check) => check_place(&check.place),
        ServerCommand::Target => target().map(|target| println!("{target}")),
        ServerCommand::Install(install) => self::install(&install),
        ServerCommand::Remove(remove) => {
            return match self::remove(&remove) {
                Ok(true) => 0,
                Ok(false) => LEFT,
                Err(err) => {
                    eprintln!("user-memory uninstall: {err:#}");
                    FAILED
                }
            };
        }
    };
    match result {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("user-memory install: {err:#}");
            1
        }
    }
}

/// Docker is installed and running; for a new server, its port is free and
/// no container uses its volume, so it never takes over another server's
/// port or memories.
fn check_place(place: &Place) -> Result<()> {
    match docker_state() {
        Docker::Missing => bail!("Docker is not installed; use --local native, or install Docker"),
        Docker::Stopped => {
            bail!("Docker is not running (docker info failed); start it, or use --local native")
        }
        Docker::Running => {}
    }
    if place.upgrade {
        return Ok(());
    }
    if port_in_use(place.port) {
        bail!(
            "127.0.0.1:{} is already in use; stop what listens there, or pass --port",
            place.port
        );
    }
    let volume = volume(&place.name);
    let users = docker_output(&[
        "ps",
        "-a",
        "--filter",
        &format!("volume={volume}"),
        "--format",
        "{{.Names}}",
    ])?;
    if !users.is_empty() {
        bail!(
            "a container already uses the {volume} volume ({users}); remove it first, so two servers never share one store"
        );
    }
    Ok(())
}

enum Docker {
    Missing,
    Stopped,
    Running,
}

/// Whether Docker is installed, and running.
fn docker_state() -> Docker {
    let info = Command::new("docker")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match info {
        Err(err) if err.kind() == ErrorKind::NotFound => Docker::Missing,
        Ok(status) if status.success() => Docker::Running,
        _ => Docker::Stopped,
    }
}

/// Whether anything accepts a connection on 127.0.0.1:`port`; only a
/// refused connection counts as free, as a timeout may be a busy server.
fn port_in_use(port: u16) -> bool {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    !matches!(
        TcpStream::connect_timeout(&address, PROBE_TIMEOUT),
        Err(err) if err.kind() == ErrorKind::ConnectionRefused
    )
}

/// The Linux build for the Docker daemon's architecture.
fn target() -> Result<&'static str> {
    let architecture =
        docker_output(&["info", "--format", "{{.Architecture}}"]).unwrap_or_default();
    linux_target(&architecture)
        .with_context(|| format!("there is no server build for Docker on {architecture}"))
}

fn linux_target(architecture: &str) -> Option<&'static str> {
    match architecture {
        "aarch64" | "arm64" => Some("aarch64-unknown-linux-gnu"),
        "x86_64" | "amd64" => Some("x86_64-unknown-linux-gnu"),
        _ => None,
    }
}

fn volume(name: &str) -> String {
    format!("{name}-data")
}

fn install(install: &Install) -> Result<()> {
    let place = &install.place;
    check_place(place)?;
    let compose_file = place.dir.join("compose.yaml");
    // An upgrade keeps the shared-server choices it was not given again.
    let earlier = if place.upgrade {
        fs::read_to_string(&compose_file).unwrap_or_default()
    } else {
        String::new()
    };
    let hosts = if install.allowed_hosts.is_empty() {
        earlier_hosts(&earlier)
    } else {
        install.allowed_hosts.clone()
    };
    let bind = install
        .bind
        .clone()
        .or_else(|| earlier_bind(&earlier))
        .unwrap_or_else(|| LOOPBACK.to_owned());
    fs::create_dir_all(&place.dir)
        .with_context(|| format!("cannot create {}", place.dir.display()))?;
    // Before compose.yaml is rewritten, which may hold an older installer's
    // embedding settings.
    let env_file = place.dir.join("embedding.env");
    if !place.upgrade {
        fs::write(&env_file, "").with_context(|| format!("cannot write {}", env_file.display()))?;
    } else if !env_file.exists() {
        fs::write(&env_file, inline_embedding_env(&earlier))
            .with_context(|| format!("cannot write {}", env_file.display()))?;
    }
    fs::copy(&install.build, place.dir.join("user-memory"))
        .with_context(|| format!("cannot copy {}", install.build.display()))?;
    fs::write(place.dir.join("Dockerfile"), DOCKERFILE)
        .with_context(|| format!("cannot write {}", place.dir.display()))?;
    fs::write(
        &compose_file,
        compose_yaml(&place.dir, &place.name, place.port, &bind, &hosts),
    )
    .with_context(|| format!("cannot write {}", compose_file.display()))?;
    let compose = Compose(compose_file);
    compose.run(&["build"])?;
    if place.upgrade {
        // The new image migrates the store (backing it up first) before the
        // new server starts, as the repository's upgrade does. It checks the
        // store first, so a store that needs titles leaves the running
        // server untouched.
        if !compose.store_check(install.titles.as_deref())? {
            bail!(
                "the store cannot be upgraded yet, and the server keeps running the previous version; when the message above lists memories without titles, write the titles map the user approves and run the installer again with --titles FILE"
            );
        }
        compose.run(&["stop", "memory"])?;
        let migrated = match &install.titles {
            Some(titles) => compose.status(
                &["run", "--rm", "-T", "memory", "migrate", "--titles", "-"],
                Some(titles),
            )?,
            None => compose.status(&["run", "--rm", "memory", "migrate"], None)?,
        };
        if !migrated {
            return Err(compose.migrate_failed(install.titles.as_deref()));
        }
    } else if install.titles.is_some() {
        println!("No titles map was needed: a new server starts with an empty store.");
    }
    compose.run(&["up", "-d", "--wait"])?;
    if let Some(token_file) = &install.token_file {
        compose.copy_token(token_file)?;
    }
    Ok(())
}

/// Removes the installer's Docker server, as `uninstall` removes a local
/// store: the memories only when the user confirms, after an export unless
/// declined. Whether nothing was left.
fn remove(remove: &Remove) -> Result<bool> {
    let compose_file = remove.dir.join("compose.yaml");
    if !compose_file.is_file() {
        return Ok(true);
    }
    // Before anything changes, so a refused purge changes nothing.
    let choice = Memories::choose(remove.purge, remove.yes, remove.no_export, remove.dry_run)?;
    let mut report = Report {
        dry_run: remove.dry_run,
        ..Report::default()
    };
    let dir = remove.dir.display();
    let volume = volume(&remove.name);
    if !matches!(docker_state(), Docker::Running) {
        report.left(format!(
            "the Docker memory server in {dir} (Docker is not running); start Docker and run this again"
        ));
        return Ok(false);
    }
    if remove.dry_run {
        println!(
            "  Would keep: your memories (the Docker volume {volume}), unless you confirm deleting them"
        );
        report.removed(format!(
            "the Docker memory server (container, image and network) and {dir}"
        ));
        return Ok(true);
    }
    let decided = choice.decide(&format!("the Docker volume {volume}"))?;
    let compose = Compose(compose_file.clone());
    if decided == Some(true) {
        let path = removal::export_path()?;
        let file = private::create_file(fs::OpenOptions::new().write(true).create_new(true), &path)
            .with_context(|| format!("cannot create {}", path.display()))?;
        let exported = compose
            .command(&["run", "--rm", "-T", "memory", "export", "-"])
            .stdout(file)
            .status()
            .is_ok_and(|status| status.success());
        if !exported {
            let _ = fs::remove_file(&path);
            report.left(
                "the Docker memory server and its memories: the export failed, so nothing was deleted; fix Docker and run this again, or pass --no-export"
                    .to_owned(),
            );
            return Ok(false);
        }
        println!(
            "  Exported your memories to {} ({})",
            path.display(),
            removal::PRIVATE
        );
    }
    let delete = decided.is_some();
    let mut down = compose.command(&["down", "--rmi", "local"]);
    if delete {
        down.arg("-v");
    }
    let removed = down
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !removed {
        report.left(format!(
            "the Docker memory server; run: docker compose -f {} down --rmi local",
            compose_file.display()
        ));
        return Ok(false);
    }
    report.removed("the Docker memory server (container, image and network)");
    if delete {
        report.removed(format!("your memories (the Docker volume {volume})"));
    }
    fs::remove_dir_all(&remove.dir).with_context(|| format!("cannot remove {dir}"))?;
    report.removed(dir.to_string());
    if !delete {
        println!(
            "\nYour memories were kept in the Docker volume {volume}. To delete them later: docker volume rm {volume}"
        );
    }
    Ok(true)
}

/// The shared-server host names an earlier compose.yaml accepts.
fn earlier_hosts(compose: &str) -> Vec<String> {
    compose
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("- --allowed-host="))
        .map(str::to_owned)
        .collect()
}

/// The address besides 127.0.0.1 an earlier compose.yaml publishes on.
fn earlier_bind(compose: &str) -> Option<String> {
    compose.lines().find_map(|line| {
        let published = line.trim_start().strip_prefix("- \"")?.strip_suffix("\"")?;
        let (address, rest) = published.split_once(':')?;
        let (port, inner) = rest.split_once(':')?;
        let numeric = |text: &str| {
            !text.is_empty()
                && text
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte == b'.')
        };
        (inner == CONTAINER_PORT.to_string()
            && numeric(address)
            && port.bytes().all(|byte| byte.is_ascii_digit())
            && address != LOOPBACK)
            .then(|| address.to_owned())
    })
}

/// The embedding settings inline in a compose.yaml from installers before
/// 0.6.0, as embedding.env lines.
fn inline_embedding_env(compose: &str) -> String {
    let mut text = String::new();
    for line in compose.lines() {
        let Some((name, value)) = line.trim_start().split_once(": ") else {
            continue;
        };
        if !name.starts_with("USER_MEMORY_EMBED_")
            || !name[18..]
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte == b'_')
        {
            continue;
        }
        let value = if let Some(quoted) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"'))
        {
            quoted.to_owned()
        } else if let Some(quoted) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
            quoted.replace("''", "'")
        } else {
            continue;
        };
        text.push_str(&format!("{name}={value}\n"));
    }
    text
}

/// The runtime stage of the repository's Dockerfile, with the released
/// Linux client in place of a source build.
const DOCKERFILE: &str = r#"# Written by the user-memory installer; running it again rewrites this file.
# The runtime stage of the repository's Dockerfile, with the released Linux
# client in place of a source build.
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 memory \
    && useradd --uid 10001 --gid memory --no-create-home memory \
    && mkdir -p /data && chown memory:memory /data
COPY user-memory /usr/local/bin/user-memory
USER memory:memory
WORKDIR /data
ENV USER_MEMORY_DB=/data/memories.sqlite3
EXPOSE 8765
STOPSIGNAL SIGINT
ENTRYPOINT ["/usr/local/bin/user-memory"]
CMD ["host", "--listen", "0.0.0.0:8765", "--db", "/data/memories.sqlite3", "--token-file", "/data/access-token"]
"#;

/// The project's compose.yaml: the server on 127.0.0.1:`port`, also on
/// `bind` for a proxy elsewhere, accepting `hosts` besides this machine.
fn compose_yaml(dir: &Path, name: &str, port: u16, bind: &str, hosts: &[String]) -> String {
    let dir = dir.display();
    let mut text = format!(
        r#"# Written by the user-memory installer; running it again rewrites this file.
# Manage it with: docker compose -f {dir}/compose.yaml stop|start|logs
# The volume is the one the repository's compose.yaml uses by default: both
# serve the same memories, so never run them at the same time.
name: {name}
services:
  memory:
    build: .
    image: {name}:installed
    restart: unless-stopped
    init: true
    stop_signal: SIGINT
    stop_grace_period: 15s
    read_only: true
    cap_drop: [ALL]
    security_opt: [no-new-privileges:true]
    healthcheck:
      # A string test runs through the container's shell (same as CMD-SHELL).
      test: "USER_MEMORY_URL=http://127.0.0.1:8765/mcp USER_MEMORY_TOKEN_FILE=/data/access-token user-memory index >/dev/null"
      interval: 30s
      timeout: 25s
      start_period: 20s
      retries: 3
    # The semantic search settings, empty for keyword search only; kept
    # when the installer runs again.
    env_file: embedding.env
    # This machine, for an embeddings endpoint such as Ollama running here.
    extra_hosts:
      - "host.docker.internal:host-gateway"
    ports:
      - "127.0.0.1:{port}:{CONTAINER_PORT}"
"#
    );
    if bind != LOOPBACK {
        text.push_str(&format!("      - \"{bind}:{port}:{CONTAINER_PORT}\"\n"));
    }
    text.push_str(
        "    volumes:
      - memory-data:/data
    command:
      - host
      - --listen
      - 0.0.0.0:8765
      - --db
      - /data/memories.sqlite3
      - --token-file
      - /data/access-token
      - --allowed-host
      - localhost
      - --allowed-host
      - 127.0.0.1
",
    );
    for host in hosts {
        text.push_str(&format!("      - --allowed-host={host}\n"));
    }
    text.push_str(&format!(
        "
volumes:
  memory-data:
    name: {}
",
        volume(name)
    ));
    text
}

fn docker_output(args: &[&str]) -> Result<String> {
    let out = Command::new("docker")
        .args(args)
        .stderr(Stdio::null())
        .output()
        .context("Cannot run docker")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// `docker compose` for the project, with Docker's own output shown as it
/// comes.
struct Compose(PathBuf);

impl Compose {
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("docker");
        command.arg("compose").arg("-f").arg(&self.0).args(args);
        command
    }
    /// Whether `args` succeeded, with `input` (a titles map) as stdin.
    fn status(&self, args: &[&str], input: Option<&Path>) -> Result<bool> {
        let mut command = self.command(args);
        if let Some(input) = input {
            command.stdin(
                fs::File::open(input)
                    .with_context(|| format!("cannot read the titles map {}", input.display()))?,
            );
        }
        Ok(command.status().context("Cannot run docker")?.success())
    }
    fn run(&self, args: &[&str]) -> Result<()> {
        if !self.status(args, None)? {
            bail!("docker compose {} failed (above)", args.join(" "));
        }
        Ok(())
    }
    /// The new image's `migrate --check`, with the titles map when given.
    fn store_check(&self, titles: Option<&Path>) -> Result<bool> {
        match titles {
            Some(titles) => self.status(
                &[
                    "run", "--rm", "-T", "memory", "migrate", "--check", "--titles", "-",
                ],
                Some(titles),
            ),
            None => self.status(&["run", "--rm", "memory", "migrate", "--check"], None),
        }
    }
    /// Why the migration failed. The new image refuses the old store, so
    /// the server cannot start again until it is migrated. The check passed
    /// before the stop; a second check says whether memories without titles
    /// arrived in between or Docker or the store itself is failing.
    fn migrate_failed(&self, titles: Option<&Path>) -> anyhow::Error {
        let mut recheck = match titles {
            Some(_) => self.command(&[
                "run", "--rm", "-T", "memory", "migrate", "--check", "--titles", "-",
            ]),
            None => self.command(&["run", "--rm", "memory", "migrate", "--check"]),
        };
        if let Some(titles) = titles
            && let Ok(file) = fs::File::open(titles)
        {
            recheck.stdin(file);
        }
        let Ok(out) = recheck.output() else {
            return anyhow::anyhow!(
                "the store was not upgraded and the server stays stopped; checking the store again failed too (above), so Docker or the store may be failing; fix that, then run the installer again"
            );
        };
        if out.status.success() {
            return anyhow::anyhow!(
                "the store was not upgraded and the server stays stopped; see the message above, then run the installer again"
            );
        }
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if said.contains("needs a title for every memory") {
            return anyhow::anyhow!(
                "memories recorded after the check and before the server stopped need titles too (listed above); the store was not upgraded and the server stays stopped; add them to the titles map the user approves and run the installer again with --titles FILE"
            );
        }
        eprintln!("{}", said.trim_end());
        anyhow::anyhow!(
            "the store was not upgraded and the server stays stopped; checking the store again failed too (above), so Docker or the store may be failing; fix that, then run the installer again"
        )
    }
    /// Copies the server's access token to `token_file`, readable only by
    /// this user, replacing it in one rename.
    fn copy_token(&self, token_file: &Path) -> Result<()> {
        let folder = token_file.parent().context("Invalid token file path")?;
        fs::create_dir_all(folder)
            .with_context(|| format!("cannot create {}", folder.display()))?;
        private::restrict(folder).with_context(|| format!("cannot create {}", folder.display()))?;
        let temporary = token_file.with_extension(format!("{}", std::process::id()));
        let copied = self
            .command(&["cp", "memory:/data/access-token"])
            .arg(&temporary)
            .status();
        if !copied.is_ok_and(|status| status.success()) {
            let _ = fs::remove_file(&temporary);
            bail!("cannot copy the access token out of the container");
        }
        private::restrict(&temporary)?;
        fs::rename(&temporary, token_file)
            .with_context(|| format!("cannot save the access token as {}", token_file.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_daemons_architecture_names_its_server_build() {
        for (architecture, target) in [
            ("aarch64", Some("aarch64-unknown-linux-gnu")),
            ("arm64", Some("aarch64-unknown-linux-gnu")),
            ("x86_64", Some("x86_64-unknown-linux-gnu")),
            ("amd64", Some("x86_64-unknown-linux-gnu")),
            ("riscv64", None),
            ("", None),
        ] {
            assert_eq!(linux_target(architecture), target, "{architecture}");
        }
    }

    /// The files are exactly the ones install.sh wrote before this module
    /// took its place (rendered by that script into the fixtures).
    #[test]
    fn the_project_files_are_the_ones_the_shell_installer_wrote() {
        assert_eq!(
            DOCKERFILE,
            include_str!("../tests/fixtures/server/Dockerfile")
        );
        assert_eq!(
            compose_yaml(
                Path::new("/d"),
                "user-memory",
                8765,
                "10.0.0.5",
                &["memory.example.com".into(), "memory.internal".into()],
            ),
            include_str!("../tests/fixtures/server/shared-compose.yaml")
        );
        assert_eq!(
            compose_yaml(Path::new("/d"), "user-memory", 9000, LOOPBACK, &[]),
            include_str!("../tests/fixtures/server/local-compose.yaml")
        );
    }

    /// The repository's compose.yaml and the installer's default server are
    /// one Docker project on one volume, so a server started from either
    /// replaces the other's container: one machine never runs two on one
    /// store, and a checkout's container name never depends on its folder.
    #[test]
    fn the_checkout_and_the_installer_are_one_project() {
        let checkout = include_str!("../compose.yaml");
        let installed = compose_yaml(Path::new("/d"), "user-memory", 8765, LOOPBACK, &[]);
        for compose in [checkout, installed.as_str()] {
            assert!(
                compose.lines().any(|line| line == "name: user-memory"),
                "{compose}"
            );
        }
        assert_eq!(volume("user-memory"), "user-memory-data");
        assert!(
            checkout.contains("name: \"${MEMORY_VOLUME_NAME:-user-memory-data}\""),
            "{checkout}"
        );
    }

    #[test]
    fn an_upgrade_reads_the_shared_server_choices_back() {
        let compose = compose_yaml(
            Path::new("/d"),
            "user-memory",
            8765,
            "10.0.0.5",
            &["memory.example.com".into(), "memory.internal".into()],
        );
        assert_eq!(
            earlier_hosts(&compose),
            ["memory.example.com", "memory.internal"]
        );
        assert_eq!(earlier_bind(&compose).as_deref(), Some("10.0.0.5"));
        let local = compose_yaml(Path::new("/d"), "user-memory", 9000, LOOPBACK, &[]);
        assert!(earlier_hosts(&local).is_empty());
        assert_eq!(earlier_bind(&local), None);
        assert!(
            local.contains("      - \"127.0.0.1:9000:8765\"\n"),
            "{local}"
        );
    }

    #[test]
    fn an_older_installers_inline_settings_become_embedding_env_lines() {
        let compose = "    environment:\n      USER_MEMORY_EMBED_URL: \"http://host.docker.internal:11434/v1/embeddings\"\n      USER_MEMORY_EMBED_ALLOW_HTTP: \"1\"\n      USER_MEMORY_EMBED_QUERY_INSTRUCTION: 'Instruct: the user''s memory\\nQuery:'\n      OTHER: \"x\"\n";
        assert_eq!(
            inline_embedding_env(compose),
            "USER_MEMORY_EMBED_URL=http://host.docker.internal:11434/v1/embeddings\nUSER_MEMORY_EMBED_ALLOW_HTTP=1\nUSER_MEMORY_EMBED_QUERY_INSTRUCTION=Instruct: the user's memory\\nQuery:\n"
        );
        assert_eq!(inline_embedding_env(""), "");
    }

    /// Removing a server the installer never set up changes nothing and
    /// asks nothing; the removal itself is covered by uninstall.sh's tests.
    #[test]
    fn removing_where_there_is_no_server_is_done_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let remove = Remove {
            dir: dir.path().join("docker"),
            name: "user-memory".into(),
            dry_run: false,
            purge: true,
            yes: true,
            no_export: false,
        };
        assert!(super::remove(&remove).unwrap());
        assert!(!remove.dir.exists());
    }

    /// A free port is covered by the installer tests, whose ports no other
    /// test can take; a port released here could be.
    #[test]
    fn a_listening_port_is_in_use() {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        assert!(port_in_use(listener.local_addr().unwrap().port()));
    }
}
