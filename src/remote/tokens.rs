//! `user-memory host token …`: the client tokens of this machine's memory
//! server, kept in its registry (src/registry.rs).

use crate::{private, registry};
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use rusqlite::Connection;
use std::{
    fs,
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
};

#[derive(Subcommand)]
pub enum TokenCommand {
    /// Issue a token for one client machine (or one agent): written to --out, readable only by you, or shown on a terminal.
    Add {
        /// The client's name, such as its machine: letters, digits, -, _ and ., unique among tokens not revoked.
        name: String,
        /// The token can read and search memories but not change them.
        #[arg(long)]
        read_only: bool,
        /// The token stops working after this many days, or after 90 days unused if that comes first.
        #[arg(long, value_name = "DAYS")]
        expires: Option<u32>,
        /// Write the token to this new file, readable only by you, instead of showing it.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
        #[command(flatten)]
        store: Store,
    },
    /// List the clients' tokens: name, scope, created, last use and expiry.
    List {
        #[command(flatten)]
        store: Store,
    },
    /// Revoke a client's token; its next request is refused.
    Revoke {
        /// The name the token was issued with.
        name: String,
        #[command(flatten)]
        store: Store,
    },
    /// Stop the shared token file letting anyone in: the server refuses its token from the next request, and the file gets a value no client holds. Needs a client token with write access first; on the installer's Docker server, this machine's client gets its own.
    RetireShared {
        /// The server's shared token file, as given to `host --token-file`; the installer's Docker server knows its own.
        #[arg(long, value_name = "FILE")]
        token_file: Option<PathBuf>,
        #[command(flatten)]
        store: Store,
    },
    /// Prints, as JSON, whether the shared file is retired, whether the token with this SHA-256 can write, and whether a token not revoked has this name: what the machine of the installer's Docker server asks its container.
    #[command(hide = true)]
    State {
        #[arg(long)]
        hash: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[command(flatten)]
        store: Store,
    },
}

#[derive(Args)]
pub struct Store {
    /// SQLite path on this server; defaults to its configured memory location.
    #[arg(long)]
    db: Option<PathBuf>,
}

impl Store {
    /// The registry beside the server's store.
    fn registry(&self) -> Result<PathBuf> {
        let db = self.db.clone().unwrap_or_else(crate::db_path);
        if !db.is_absolute() {
            bail!("Server database must use an absolute path");
        }
        Ok(registry::path(&db))
    }
}

/// Runs a `host token` command on this machine's registry, or in the
/// container of the installer's Docker server when this machine runs one
/// and no `--db` names a store here.
pub fn run(command: TokenCommand) -> Result<()> {
    let store = match &command {
        TokenCommand::Add { store, .. }
        | TokenCommand::List { store }
        | TokenCommand::Revoke { store, .. }
        | TokenCommand::RetireShared { store, .. }
        | TokenCommand::State { store, .. } => store,
    };
    if store.db.is_none() {
        match crate::setup::embedding_home()? {
            crate::setup::EmbeddingHome::Local => {}
            crate::setup::EmbeddingHome::Docker(dir) => return in_docker(&dir, command),
            crate::setup::EmbeddingHome::Remote(url) => {
                bail!(
                    "{url} is a memory server on another machine; run user-memory host token there."
                )
            }
        }
    }
    let registry_path = store.registry()?;
    if let Some(folder) = registry_path.parent() {
        crate::server::tokens::sweep(folder);
    }
    match command {
        TokenCommand::Add {
            name,
            read_only,
            expires,
            out,
            store,
        } => add(
            &store.registry()?,
            &name,
            read_only,
            expires,
            out.as_deref(),
        ),
        TokenCommand::List { store } => list(&store.registry()?),
        TokenCommand::Revoke { name, store } => revoke(&store.registry()?, &name),
        TokenCommand::RetireShared { token_file, store } => {
            let token_file = token_file.context(
                "Give --token-file FILE: the shared token file that `host --token-file` names.",
            )?;
            retire_shared(&store.registry()?, &token_file)
        }
        TokenCommand::State { hash, name, store } => {
            state(&store.registry()?, hash.as_deref(), name.as_deref())
        }
    }
}

/// Runs `command` in the container of the installer's Docker server in
/// `dir`. A token reaches this machine only through a file copied out of
/// the volume, or straight to this terminal.
fn in_docker(dir: &Path, command: TokenCommand) -> Result<()> {
    use crate::server::tokens as docker;
    let compose = crate::server::Compose::new(dir.join("compose.yaml"));
    match command {
        TokenCommand::Add {
            name,
            read_only,
            expires,
            out,
            store: _,
        } => match out {
            Some(out) => {
                docker::add_to_file(&compose, &name, read_only, expires, &out, false)?;
                wrote(read_only, &name, &out);
                Ok(())
            }
            None if !std::io::stdout().is_terminal() => bail!(NOT_A_TERMINAL),
            None => {
                let expires = expires.map(|days| days.to_string());
                let mut args = vec!["add", name.as_str()];
                if read_only {
                    args.push("--read-only");
                }
                if let Some(days) = &expires {
                    args.extend(["--expires", days]);
                }
                docker::on_this_terminal(&compose, &args)
            }
        },
        TokenCommand::List { store: _ } => docker::on_this_terminal(&compose, &["list"]),
        TokenCommand::Revoke { name, store: _ } => {
            docker::on_this_terminal(&compose, &["revoke", &name])
        }
        TokenCommand::RetireShared {
            token_file,
            store: _,
        } => {
            if token_file.is_some() {
                bail!(
                    "The installer's Docker server retires its own shared token file; run this without --token-file."
                );
            }
            let client = crate::setup::configured_remote()?
                .context("This machine's client has no saved server to keep its token for")?;
            docker::retire_shared(&compose, dir, &client.token_file)
        }
        TokenCommand::State { .. } => bail!("`host token state` runs in the container"),
    }
}

/// Says where `add` wrote a token.
fn wrote(read_only: bool, name: &str, out: &Path) {
    let described = if read_only { "read-only" } else { "write" };
    eprintln!(
        "Wrote the {described} token for {name} to {}, readable only by you. Copy it to that machine over a private channel and install there with --token-file.",
        out.display()
    );
}

/// Prints what the machine of the installer's Docker server asks: see
/// [`TokenCommand::State`].
fn state(registry_path: &Path, hash: Option<&str>, name: Option<&str>) -> Result<()> {
    let (retired, writer, named) = match registry::open(registry_path)? {
        None => (false, false, false),
        Some(conn) => {
            let now = registry::now();
            let tokens = registry::tokens(&conn)?;
            (
                registry::shared_file_retired(&conn)?,
                hash.is_some_and(|hash| {
                    tokens
                        .iter()
                        .any(|token| token.hash == hash && token.can_write(now))
                }),
                name.is_some_and(|name| {
                    tokens
                        .iter()
                        .any(|token| token.name == name && token.revoked_at.is_none())
                }),
            )
        }
    };
    println!(
        "{}",
        serde_json::json!({"shared_file_retired": retired, "writer": writer, "named": named})
    );
    Ok(())
}

/// Why `add` shows no token: a token printed into a tool's or a script's
/// output would be kept wherever that output goes.
const NOT_A_TERMINAL: &str = "Not showing the token: standard output is not a terminal. Give --out FILE to write it to a file only you can read.";

const DAY_SECONDS: i64 = 24 * 60 * 60;

fn add(
    registry_path: &Path,
    name: &str,
    read_only: bool,
    expires: Option<u32>,
    out: Option<&Path>,
) -> Result<()> {
    if !registry::valid_name(name) {
        bail!(
            "{name:?} cannot name a client: use 1 to {} letters, digits, -, _ and ., starting with a letter or digit.",
            registry::NAME_MAX
        );
    }
    let expires_at = match expires {
        Some(0) => bail!("--expires takes a number of days from 1"),
        Some(days) => Some(registry::now() + i64::from(days) * DAY_SECONDS),
        None => None,
    };
    // Where the token goes is settled before one is made.
    if out.is_none() && !std::io::stdout().is_terminal() {
        bail!(NOT_A_TERMINAL);
    }
    if let Some(out) = out
        && fs::symlink_metadata(out).is_ok()
    {
        bail!("{} exists; remove it or give another --out.", out.display());
    }
    let scope = if read_only {
        registry::Scope::Read
    } else {
        registry::Scope::Write
    };
    let conn = registry::create(registry_path)?;
    let token = registry::new_token();
    let hash = registry::hash(&token);
    match out {
        Some(out) => {
            write_new(out, &token, &conn, |tx| {
                registry::add(tx, &hash, name, scope, expires_at)
            })?;
            wrote(read_only, name, out);
        }
        None => {
            registry::add(&conn, &hash, name, scope, expires_at)?;
            println!("{token}");
            let described = if read_only { "read-only" } else { "write" };
            eprintln!("The {described} token for {name}, shown only this once.");
        }
    }
    Ok(())
}

/// Writes `token` to the new file `out`, private, and `register`s it on
/// `conn`, as one step: the token is written to a temporary file beside
/// `out` and placed with [`private::place_new`], which never replaces a file
/// that appeared meanwhile, inside the transaction that registers it, which
/// commits only once the file is in place. A failure anywhere leaves neither
/// a token file nor a registered token.
fn write_new(
    out: &Path,
    token: &str,
    conn: &rusqlite::Connection,
    register: impl FnOnce(&rusqlite::Connection) -> Result<()>,
) -> Result<()> {
    let folder = match out.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = out
        .file_name()
        .with_context(|| format!("{} is not a file name", out.display()))?;
    let suffix: u64 = rand::random();
    let temporary = folder.join(format!(".{}.{suffix:016x}", name.to_string_lossy()));
    let written = (|| -> Result<()> {
        let mut file = private::create_file(
            fs::OpenOptions::new().write(true).create_new(true),
            &temporary,
        )
        .with_context(|| format!("Cannot write {}", temporary.display()))?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        let tx = conn.unchecked_transaction()?;
        register(&tx)?;
        private::place_new(&temporary, out)
            .with_context(|| format!("Cannot write {}", out.display()))?;
        tx.commit().inspect_err(|_| {
            let _ = fs::remove_file(out);
        })?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    written
}

fn list(registry_path: &Path) -> Result<()> {
    let conn = registry::open(registry_path)?;
    let mut rows = vec![[
        "NAME".to_owned(),
        "SCOPE".to_owned(),
        "CREATED".to_owned(),
        "LAST USED".to_owned(),
        "EXPIRES".to_owned(),
    ]];
    let retired = match &conn {
        Some(conn) => registry::shared_file_retired(conn)?,
        None => false,
    };
    if !retired {
        rows.push([
            "owner (shared file)".to_owned(),
            "write".to_owned(),
            "-".to_owned(),
            "-".to_owned(),
            "never".to_owned(),
        ]);
    }
    if let Some(conn) = &conn {
        let now = registry::now();
        for token in registry::tokens(conn)? {
            if token.revoked_at.is_some() {
                continue;
            }
            let expiry = token.expiry();
            rows.push([
                token.name.clone(),
                match token.scope {
                    registry::Scope::Write => "write".to_owned(),
                    registry::Scope::Read => "read".to_owned(),
                },
                date(conn, token.created_at)?,
                match token.last_used_at {
                    Some(at) => date(conn, at)?,
                    None => "never".to_owned(),
                },
                if expiry <= now {
                    format!("expired {}", date(conn, expiry)?)
                } else {
                    date(conn, expiry)?
                },
            ]);
        }
    }
    let widths: Vec<usize> = (0..5)
        .map(|column| rows.iter().map(|row| row[column].len()).max().unwrap_or(0))
        .collect();
    let mut stdout = std::io::stdout().lock();
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        writeln!(stdout, "{}", line.join("  ").trim_end())?;
    }
    Ok(())
}

/// `at` (Unix seconds) as a UTC date, by SQLite's own calendar.
fn date(conn: &Connection, at: i64) -> Result<String> {
    Ok(conn.query_row(
        "SELECT strftime('%Y-%m-%d', ?1, 'unixepoch')",
        [at],
        |row| row.get(0),
    )?)
}

fn revoke(registry_path: &Path, name: &str) -> Result<()> {
    let revoked = match registry::open(registry_path)? {
        Some(conn) => registry::revoke(&conn, name)?,
        None => false,
    };
    if !revoked {
        bail!("No token named {name}; `user-memory host token list` shows the tokens.");
    }
    eprintln!("Revoked the token for {name}; its next request is refused.");
    Ok(())
}

/// Why `retire-shared` does nothing: without a client token that can write,
/// the shared file would be the only way to change memories.
const NO_WRITER: &str = "No client token has write access; add one with `user-memory host token add NAME` first, so the shared file is not the only way in.";

/// What a leftover new value of `retire-shared` is named, after the file's
/// own name and before a random suffix.
const RETIRE_INFIX: &str = ".retire-";

/// Retires the shared token file `file`: a new value is written beside it,
/// the registry's setting is committed, and the new value is renamed over
/// the file. A stop after the first step leaves an unused file that a rerun
/// deletes; after the second, the server already refuses the shared token
/// and a rerun replaces the file. A server rolled back to a release without
/// the registry then accepts only the new value, which no client holds.
fn retire_shared(registry_path: &Path, file: &Path) -> Result<()> {
    let conn = match registry::open(registry_path)? {
        Some(conn) if registry::has_writer(&conn, registry::now())? => conn,
        _ => bail!(NO_WRITER),
    };
    // Only a file holding one token is replaced: any other file given by
    // mistake is refused before anything is written.
    let current = super::read_token(file).map_err(|err| {
        anyhow::anyhow!(
            "{} is not a token file ({err:#}), so it was left unchanged; give the shared token file that `host --token-file` names.",
            file.display()
        )
    })?;
    let current = registry::hash(&current);
    if registry::tokens(&conn)?
        .iter()
        .any(|token| token.hash == current)
    {
        bail!(
            "{} holds a client token; give the shared token file that `host --token-file` names.",
            file.display()
        );
    }
    let folder = match file.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = file
        .file_name()
        .with_context(|| format!("{} is not a file name", file.display()))?
        .to_string_lossy()
        .into_owned();
    let leftover = format!(".{name}{RETIRE_INFIX}");
    for entry in fs::read_dir(&folder)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(&leftover) {
            fs::remove_file(entry.path())?;
        }
    }
    let suffix: u64 = rand::random();
    let temporary = folder.join(format!("{leftover}{suffix:016x}"));
    let replaced = (|| -> Result<()> {
        let mut new = private::create_file(
            fs::OpenOptions::new().write(true).create_new(true),
            &temporary,
        )
        .with_context(|| format!("Cannot write {}", temporary.display()))?;
        new.write_all(super::new_shared_token().as_bytes())?;
        new.sync_all()?;
        drop(new);
        registry::retire_shared_file(&conn)?;
        fs::rename(&temporary, file).with_context(|| format!("Cannot write {}", file.display()))
    })();
    if replaced.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    replaced?;
    eprintln!(
        "Retired the shared token file {}: the server refuses its token from the next request, and the file now holds a value no client has. A machine still using it needs its own token: `user-memory host token add NAME --out FILE` here, then the installer there with --token-file.",
        file.display()
    );
    Ok(())
}
