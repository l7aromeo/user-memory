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

/// Runs a `host token` command on this machine's registry.
pub fn run(command: TokenCommand) -> Result<()> {
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
    }
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
    let described = if read_only { "read-only" } else { "write" };
    let conn = registry::create(registry_path)?;
    let token = registry::new_token();
    let hash = registry::hash(&token);
    match out {
        Some(out) => {
            write_new(out, &token, || {
                registry::add(&conn, &hash, name, scope, expires_at)
            })?;
            eprintln!(
                "Wrote the {described} token for {name} to {}, readable only by you. Copy it to that machine over a private channel and install there with --token-file.",
                out.display()
            );
        }
        None => {
            registry::add(&conn, &hash, name, scope, expires_at)?;
            println!("{token}");
            eprintln!("The {described} token for {name}, shown only this once.");
        }
    }
    Ok(())
}

/// Writes `token` to the new file `out`, private, after `register`
/// succeeds: first to a temporary file beside it, which is then linked in
/// place, so an interrupted add leaves no partial token file and never
/// replaces a file that appeared meanwhile.
fn write_new(out: &Path, token: &str, register: impl FnOnce() -> Result<()>) -> Result<()> {
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
        register()?;
        fs::hard_link(&temporary, out).with_context(|| format!("Cannot write {}", out.display()))
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
