//! Embedding settings for local installs: read from a dotenv file and the
//! environment at setup, saved beside the binary, and read back at runtime
//! after the environment. The installer's Docker server reads them from an
//! `embedding.env` beside its compose.yaml instead.

use crate::paths;
use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Every embedding setting; other keys in a dotenv file are ignored.
pub(crate) const SETTINGS: [&str; 6] = [
    "USER_MEMORY_EMBED_URL",
    "USER_MEMORY_EMBED_MODEL",
    "USER_MEMORY_EMBED_DIMENSIONS",
    "USER_MEMORY_EMBED_QUERY_INSTRUCTION",
    "USER_MEMORY_EMBED_API_KEY_FILE",
    "USER_MEMORY_EMBED_ALLOW_HTTP",
];
const PREFIX: &str = "USER_MEMORY_EMBED_";

pub(super) type Settings = BTreeMap<String, String>;

/// Embedding settings from simple `KEY=VALUE` dotenv text: blank lines and
/// `#` comments are skipped, `export ` is allowed, values may be single- or
/// double-quoted and are taken literally (no expansion or escapes). Unknown
/// `USER_MEMORY_EMBED_*` names are refused so a typo cannot go unnoticed.
fn parse(text: &str, origin: &Path) -> Result<Settings> {
    let mut settings = Settings::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        let line = line.strip_prefix("export ").map_or(line, str::trim_start);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if !key.starts_with(PREFIX) {
            continue;
        }
        let at = || format!("{}:{}", origin.display(), index + 1);
        if !SETTINGS.contains(&key) {
            bail!("Unknown embedding setting {key} at {}", at());
        }
        let value = value.trim();
        let value = match value.chars().next() {
            Some(quote @ ('"' | '\'')) => {
                let rest = &value[1..];
                let end = rest
                    .find(quote)
                    .with_context(|| format!("Unclosed quote at {}", at()))?;
                let after = rest[end + 1..].trim_start();
                if !(after.is_empty() || after.starts_with('#')) {
                    bail!("Unexpected text after the quoted value at {}", at());
                }
                &rest[..end]
            }
            _ => value
                .find(" #")
                .map_or(value, |comment| &value[..comment])
                .trim_end(),
        };
        settings.insert(key.to_owned(), value.to_owned());
    }
    Ok(settings)
}

/// Settings to save at setup: the environment overrides the file. `None`
/// when neither names any setting, so the saved settings stay as they are.
pub(super) fn resolve(file: Option<&Path>) -> Result<Option<Settings>> {
    let mut settings = match file {
        Some(path) => {
            let text = fs::read_to_string(path)
                .with_context(|| format!("Cannot read {}", path.display()))?;
            parse(&text, path)?
        }
        None => Settings::new(),
    };
    let mut given = file.is_some();
    for name in SETTINGS {
        if let Ok(value) = env::var(name) {
            settings.insert(name.to_owned(), value);
            given = true;
        }
    }
    settings.retain(|_, value| !value.trim().is_empty());
    Ok(given.then_some(settings))
}

/// Where an install's store keeps its embedding setting.
#[derive(Debug, PartialEq)]
pub enum Home {
    /// A store on this machine: the setting is saved beside the binary.
    Local,
    /// The Docker server the installer set up, from this folder.
    Docker(PathBuf),
    /// Another memory server, which owns its setting.
    Remote(String),
}

/// The file the installer's Docker server reads its settings from.
pub(super) const DOCKER_ENV: &str = "embedding.env";
/// Where that server reads an API key copied into its volume.
const DOCKER_KEY: &str = "/data/embedding-key";
/// This machine as the container reaches it (compose's host-gateway entry).
const DOCKER_HOST: &str = "host.docker.internal";

/// The folder of the Docker server the installer set up, when `url` is
/// that server: its compose.yaml publishes the URL's port on 127.0.0.1.
/// Found where install.sh puts it, for the same Docker project name.
pub(crate) fn docker_dir(url: &str) -> Option<PathBuf> {
    let name = env::var("USER_MEMORY_DOCKER_NAME").unwrap_or_else(|_| "user-memory".into());
    docker_dir_in(&paths::docker_dir(&name)?, url)
}

/// [`docker_dir`] for the project folder `dir`, when it serves `url`.
fn docker_dir_in(dir: &Path, url: &str) -> Option<PathBuf> {
    let dir = dir.to_path_buf();
    let compose = fs::read_to_string(dir.join("compose.yaml")).ok()?;
    let port = compose.lines().find_map(|line| {
        line.trim()
            .strip_prefix("- \"127.0.0.1:")?
            .strip_suffix(":8765\"")
    })?;
    (url == format!("http://127.0.0.1:{port}/mcp")).then_some(dir)
}

/// The Docker server's `embedding.env` for `settings`, as the container
/// sees them: plain http to this machine goes to the host gateway, with
/// plain http allowed for it, and an API key file becomes the copy in the
/// volume. Values are written unquoted, which compose takes literally.
pub(super) fn docker_env(settings: &Settings) -> Result<String> {
    let mut text = String::new();
    let mut allow_http = settings.get("USER_MEMORY_EMBED_ALLOW_HTTP").cloned();
    for name in SETTINGS {
        let Some(value) = settings.get(name) else {
            continue;
        };
        if value.contains(['\n', '\r']) {
            bail!("{name} cannot hold a line break");
        }
        let value = match name {
            "USER_MEMORY_EMBED_URL" => {
                let mut url = reqwest::Url::parse(value.trim())
                    .context("USER_MEMORY_EMBED_URL is not a URL")?;
                let here = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
                if url.scheme() == "http" && here {
                    if settings.contains_key("USER_MEMORY_EMBED_API_KEY_FILE") {
                        bail!(
                            "the Docker server reaches {value} over plain http at {DOCKER_HOST}, and an API key is never sent over plain http to another host; use an https endpoint"
                        );
                    }
                    url.set_host(Some(DOCKER_HOST))
                        .context("USER_MEMORY_EMBED_URL cannot point at the host gateway")?;
                    allow_http = Some("1".into());
                }
                url.to_string()
            }
            "USER_MEMORY_EMBED_API_KEY_FILE" => DOCKER_KEY.into(),
            "USER_MEMORY_EMBED_ALLOW_HTTP" => continue,
            _ => value.clone(),
        };
        text.push_str(&format!("{name}={value}\n"));
    }
    if let Some(allow) = allow_http {
        text.push_str(&format!("USER_MEMORY_EMBED_ALLOW_HTTP={allow}\n"));
    }
    Ok(text)
}

/// Saves `settings` for the Docker server in `dir` and applies them: its
/// embedding.env, the API key in its volume (or none), a recreated
/// container, then `embedding sync` inside it, which probes the endpoint as
/// the container reaches it, calibrates and re-embeds. Returns what sync
/// printed. When any step fails, the previous file and key are put back and
/// the container recreated with them, so a failed change leaves the server
/// as it was.
pub(super) fn apply_docker(dir: &Path, settings: &Settings) -> Result<String> {
    apply_docker_with(Path::new("docker"), dir, settings)
}

/// [`apply_docker`] with `docker` as the Docker command.
fn apply_docker_with(docker: &Path, dir: &Path, settings: &Settings) -> Result<String> {
    let text = docker_env(settings)?;
    let file = dir.join(DOCKER_ENV);
    let previous = match fs::read_to_string(&file) {
        Ok(previous) => previous,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("Cannot read {}", file.display())),
    };
    let compose = Compose {
        docker,
        file: dir.join("compose.yaml"),
    };
    // The old key waits beside the new one until the change has worked.
    compose.shell(
        &format!("[ ! -f {DOCKER_KEY} ] || mv -f {DOCKER_KEY} {DOCKER_KEY}.previous"),
        Stdio::null(),
    )?;
    let changed = (|| {
        write_atomically(&file, &text)?;
        if let Some(key) = settings.get("USER_MEMORY_EMBED_API_KEY_FILE") {
            let key = fs::File::open(key).with_context(|| format!("Cannot read {key}"))?;
            compose.shell(&format!("umask 077 && cat >{DOCKER_KEY}"), key.into())?;
        }
        compose.run(&["up", "-d", "--wait"], Stdio::null())?;
        compose.run(
            &["exec", "-T", "memory", "user-memory", "embedding", "sync"],
            Stdio::null(),
        )
    })();
    match changed {
        Ok(said) => {
            compose.shell(&format!("rm -f {DOCKER_KEY}.previous"), Stdio::null())?;
            Ok(said)
        }
        Err(err) => {
            let restored = write_atomically(&file, &previous)
                .and_then(|()| {
                    compose.shell(
                        &format!("rm -f {DOCKER_KEY} && if [ -f {DOCKER_KEY}.previous ]; then mv -f {DOCKER_KEY}.previous {DOCKER_KEY}; fi"),
                        Stdio::null(),
                    )
                })
                .and_then(|_| compose.run(&["up", "-d", "--wait"], Stdio::null()));
            match restored {
                Ok(_) => Err(err.context("The embedding setting was not changed")),
                Err(again) => Err(err.context(format!(
                    "Putting the previous embedding setting back failed too ({again:#}); check {}",
                    file.display()
                ))),
            }
        }
    }
}

/// Replaces `file` with `text` in one rename.
fn write_atomically(file: &Path, text: &str) -> Result<()> {
    let temporary = file.with_extension(format!("{}", std::process::id()));
    fs::write(&temporary, text)
        .and_then(|()| fs::rename(&temporary, file))
        .with_context(|| format!("Cannot write {}", file.display()))
}

/// `docker compose` for the installer's server, by its compose.yaml.
struct Compose<'a> {
    docker: &'a Path,
    file: PathBuf,
}

impl Compose<'_> {
    /// Runs `docker compose -f FILE ARGS` with `input`, its errors shown to
    /// the user; returns its trimmed output.
    fn run(&self, args: &[&str], input: Stdio) -> Result<String> {
        let out = Command::new(self.docker)
            .arg("compose")
            .arg("-f")
            .arg(&self.file)
            .args(args)
            .stdin(input)
            .stderr(Stdio::inherit())
            .output()
            .context("Cannot run docker")?;
        if !out.status.success() {
            bail!("docker compose {} failed (above)", args.join(" "));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }
    /// Runs a shell command in a one-off server container, with the volume.
    fn shell(&self, script: &str, input: Stdio) -> Result<String> {
        self.run(
            &[
                "run",
                "--rm",
                "-T",
                "--entrypoint",
                "sh",
                "memory",
                "-c",
                script,
            ],
            input,
        )
    }
}

/// One setting: the environment first, then `saved`, with where it came from.
pub(super) fn lookup<'a>(saved: &'a Settings, name: &str) -> Option<(String, &'a str)> {
    match env::var(name) {
        Ok(value) => Some((value, "environment")),
        Err(_) => saved.get(name).map(|value| (value.clone(), "saved setup")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Result<Settings> {
        parse(text, Path::new("/x/.env"))
    }

    #[test]
    fn only_embedding_settings_are_read_literally() {
        let text = "\
# Docker settings are ignored
MEMORY_PORT=8765
export USER_MEMORY_EMBED_URL = http://127.0.0.1:11434/v1/embeddings  # local Ollama
USER_MEMORY_EMBED_MODEL='qwen3-embedding:4b'
USER_MEMORY_EMBED_QUERY_INSTRUCTION=\"Instruct: find $HOME memory\\nQuery:\" # kept literally
USER_MEMORY_EMBED_DIMENSIONS=
USER_MEMORY_EMBED_ALLOW_HTTP=no#not-a-comment
not a setting
USER_MEMORY_EMBED_MODEL=qwen3-embedding:8b
";
        let settings = parsed(text).unwrap();
        assert_eq!(
            settings,
            Settings::from([
                (
                    "USER_MEMORY_EMBED_URL".into(),
                    "http://127.0.0.1:11434/v1/embeddings".into()
                ),
                (
                    "USER_MEMORY_EMBED_MODEL".into(),
                    "qwen3-embedding:8b".into()
                ),
                (
                    "USER_MEMORY_EMBED_QUERY_INSTRUCTION".into(),
                    "Instruct: find $HOME memory\\nQuery:".into()
                ),
                ("USER_MEMORY_EMBED_DIMENSIONS".into(), String::new()),
                (
                    "USER_MEMORY_EMBED_ALLOW_HTTP".into(),
                    "no#not-a-comment".into()
                ),
            ])
        );
    }

    fn settings(pairs: &[(&str, &str)]) -> Settings {
        pairs
            .iter()
            .map(|(name, value)| (format!("USER_MEMORY_EMBED_{name}"), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn the_docker_server_reaches_this_machine_through_the_host_gateway() {
        let ollama = settings(&[
            ("URL", "http://127.0.0.1:11434/v1/embeddings"),
            ("MODEL", "qwen3-embedding:4b"),
            ("DIMENSIONS", "1024"),
            (
                "QUERY_INSTRUCTION",
                "Instruct: Given a question, retrieve the user's matching memory\\nQuery:",
            ),
        ]);
        assert_eq!(
            docker_env(&ollama).unwrap(),
            "USER_MEMORY_EMBED_URL=http://host.docker.internal:11434/v1/embeddings\n\
             USER_MEMORY_EMBED_MODEL=qwen3-embedding:4b\n\
             USER_MEMORY_EMBED_DIMENSIONS=1024\n\
             USER_MEMORY_EMBED_QUERY_INSTRUCTION=Instruct: Given a question, retrieve the user's matching memory\\nQuery:\n\
             USER_MEMORY_EMBED_ALLOW_HTTP=1\n"
        );
        for here in [
            "http://localhost:8080/embeddings",
            "http://[::1]:8080/embeddings",
        ] {
            let text = docker_env(&settings(&[("URL", here), ("MODEL", "m")])).unwrap();
            assert!(
                text.starts_with(
                    "USER_MEMORY_EMBED_URL=http://host.docker.internal:8080/embeddings\n"
                ),
                "{text}"
            );
        }
        // An https endpoint is left as it is; its key is the volume's copy.
        assert_eq!(
            docker_env(&settings(&[
                ("URL", "https://api.example.com/v1/embeddings"),
                ("MODEL", "text-embedding-3-small"),
                ("API_KEY_FILE", "/home/me/.config/user-memory/embedding-key"),
            ]))
            .unwrap(),
            "USER_MEMORY_EMBED_URL=https://api.example.com/v1/embeddings\n\
             USER_MEMORY_EMBED_MODEL=text-embedding-3-small\n\
             USER_MEMORY_EMBED_API_KEY_FILE=/data/embedding-key\n"
        );
        assert_eq!(docker_env(&Settings::new()).unwrap(), "");
    }

    #[test]
    fn the_docker_server_refuses_what_its_settings_file_cannot_carry() {
        for (pairs, message) in [
            (
                &[
                    ("URL", "http://127.0.0.1:8080/embeddings"),
                    ("API_KEY_FILE", "/k"),
                ][..],
                "an API key is never sent over plain http to another host",
            ),
            (
                &[("MODEL", "a\nb")][..],
                "USER_MEMORY_EMBED_MODEL cannot hold a line break",
            ),
            (
                &[("URL", "not a url")][..],
                "USER_MEMORY_EMBED_URL is not a URL",
            ),
        ] {
            let err = format!("{:#}", docker_env(&settings(pairs)).unwrap_err());
            assert!(err.contains(message), "{err}");
        }
    }

    /// Docker for [`apply_docker_with`]: logs each command; runs a one-off
    /// container's shell with `$VOLUME` as /data; `embedding sync` fails
    /// while `$VOLUME/sync-fails` exists.
    // A shell script stands in for Docker, so this test runs on macOS and
    // Linux; the code it covers is the same on Windows.
    #[cfg(unix)]
    const FAKE_DOCKER: &str = r#"#!/bin/sh
printf '%s\n' "$*" >>"$VOLUME/docker.log"
case " $* " in
    *" --entrypoint sh memory -c "*)
        eval "script=\${$#}"
        sh -c "$(printf '%s' "$script" | sed "s|/data/|$VOLUME/|g")"
        ;;
    *" embedding sync "*)
        [ ! -f "$VOLUME/sync-fails" ] || { echo "probe failed" >&2; exit 1; }
        echo "Embedded 3 memories"
        ;;
esac
"#;

    #[cfg(unix)]
    #[test]
    fn a_docker_change_applies_whole_or_puts_the_old_setting_back() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let (dir, volume) = (tmp.path().join("docker"), tmp.path().join("volume"));
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&volume).unwrap();
        let docker = tmp.path().join("fake-docker");
        fs::write(
            &docker,
            FAKE_DOCKER.replace("$VOLUME", volume.to_str().unwrap()),
        )
        .unwrap();
        fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();
        let old_env = "USER_MEMORY_EMBED_URL=https://old.example.com/v1/embeddings\nUSER_MEMORY_EMBED_MODEL=old\nUSER_MEMORY_EMBED_API_KEY_FILE=/data/embedding-key\n";
        fs::write(dir.join(DOCKER_ENV), old_env).unwrap();
        fs::write(volume.join("embedding-key"), "old-key\n").unwrap();
        let key = tmp.path().join("new key");
        fs::write(&key, "new-key\n").unwrap();
        let new = settings(&[
            ("URL", "https://new.example.com/v1/embeddings"),
            ("MODEL", "new"),
            ("API_KEY_FILE", key.to_str().unwrap()),
        ]);

        // The sync inside the container fails: everything is put back.
        fs::write(volume.join("sync-fails"), "").unwrap();
        let err = format!("{:#}", apply_docker_with(&docker, &dir, &new).unwrap_err());
        assert!(
            err.starts_with("The embedding setting was not changed: docker compose exec -T memory user-memory embedding sync failed"),
            "{err}"
        );
        assert_eq!(fs::read_to_string(dir.join(DOCKER_ENV)).unwrap(), old_env);
        assert_eq!(
            fs::read_to_string(volume.join("embedding-key")).unwrap(),
            "old-key\n"
        );
        assert!(!volume.join("embedding-key.previous").exists());
        let log = fs::read_to_string(volume.join("docker.log")).unwrap();
        assert_eq!(
            log.matches(" up -d --wait").count(),
            2,
            "recreated with the old setting: {log}"
        );

        // It works: the new setting and key stay, the old key goes.
        fs::remove_file(volume.join("sync-fails")).unwrap();
        assert_eq!(
            apply_docker_with(&docker, &dir, &new).unwrap(),
            "Embedded 3 memories"
        );
        assert_eq!(
            fs::read_to_string(dir.join(DOCKER_ENV)).unwrap(),
            docker_env(&new).unwrap()
        );
        assert_eq!(
            fs::read_to_string(volume.join("embedding-key")).unwrap(),
            "new-key\n"
        );
        assert!(!volume.join("embedding-key.previous").exists());

        // Off: no settings, and no key left in the volume.
        assert_eq!(
            apply_docker_with(&docker, &dir, &Settings::new()).unwrap(),
            "Embedded 3 memories"
        );
        assert_eq!(fs::read_to_string(dir.join(DOCKER_ENV)).unwrap(), "");
        assert!(!volume.join("embedding-key").exists());
        assert!(!volume.join("embedding-key.previous").exists());
    }

    #[test]
    fn only_the_installers_docker_server_is_found() {
        let data = tempfile::tempdir().unwrap();
        let dir = data.path().join("docker");
        assert_eq!(docker_dir_in(&dir, "http://127.0.0.1:8765/mcp"), None);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("compose.yaml"),
            "    ports:\n      - \"127.0.0.1:8765:8765\"\n      - \"10.0.0.5:8765:8765\"\n",
        )
        .unwrap();
        assert_eq!(
            docker_dir_in(&dir, "http://127.0.0.1:8765/mcp"),
            Some(dir.clone())
        );
        assert_eq!(docker_dir_in(&dir, "http://127.0.0.1:9000/mcp"), None);
        assert_eq!(docker_dir_in(&dir, "https://memory.example.com/mcp"), None);
    }

    #[test]
    fn malformed_embedding_lines_name_the_file_and_line() {
        for (text, message) in [
            (
                "A=1\nUSER_MEMORY_EMBED_MODLE=x\n",
                "Unknown embedding setting USER_MEMORY_EMBED_MODLE at /x/.env:2",
            ),
            (
                "USER_MEMORY_EMBED_MODEL=\"x\n",
                "Unclosed quote at /x/.env:1",
            ),
            (
                "USER_MEMORY_EMBED_MODEL='x' y\n",
                "Unexpected text after the quoted value at /x/.env:1",
            ),
        ] {
            let err = parsed(text).unwrap_err().to_string();
            assert_eq!(err, message, "{text}");
        }
        assert!(
            parsed("OTHER=\"unclosed\nMEMORY_HOST='a' b\n")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            parsed("USER_MEMORY_EMBED_MODEL='x' # note\n").unwrap()["USER_MEMORY_EMBED_MODEL"],
            "x"
        );
    }
}
