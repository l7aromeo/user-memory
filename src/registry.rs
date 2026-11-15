//! The server's token registry, `accounts.sqlite3` beside the server's
//! memory store: the client tokens and the server's own settings, outside
//! every memory store, so restoring a memory backup never brings back a
//! revoked token. Only `host token add` creates it; a server without client
//! tokens has none.

use crate::private;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// The registry's file name, in the folder that holds the server's store.
pub const FILE: &str = "accounts.sqlite3";

/// Stamped into `PRAGMA user_version`. A new version adds its step to
/// [`MIGRATIONS`] and a snapshot in tests/fixtures/registry/.
pub const SCHEMA_VERSION: i64 = 1;

/// Each step upgrades the registry from its index to the next version: the
/// first creates version 1 in a new file.
const MIGRATIONS: [&str; SCHEMA_VERSION as usize] = [V1];

/// Version 1: the client tokens (#48) and the server's settings. A token is
/// kept as its SHA-256 only. Times are Unix seconds. A name is unique among
/// the tokens not revoked, so a revoked one can be issued again.
const V1: &str = "CREATE TABLE client_tokens (
    hash TEXT PRIMARY KEY NOT NULL CHECK(length(hash)=64 AND hash NOT GLOB '*[^0-9a-f]*'),
    name TEXT NOT NULL CHECK(length(name) BETWEEN 1 AND 64),
    scope TEXT NOT NULL CHECK(scope IN ('write','read')),
    created_at INTEGER NOT NULL,
    last_used_at INTEGER,
    expires_at INTEGER,
    revoked_at INTEGER
);
CREATE UNIQUE INDEX client_tokens_live_name ON client_tokens(name) WHERE revoked_at IS NULL;
CREATE TABLE settings (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);";

/// The setting `host token retire-shared` sets: the shared token file no
/// longer lets anyone in.
pub const SHARED_FILE_RETIRED: &str = "shared_file_retired";

/// How long a connection waits for another's write: a `host token` command
/// and the server's last-use write each take milliseconds.
const BUSY: Duration = Duration::from_secs(5);

/// The registry beside the store `db`.
pub fn path(db: &Path) -> PathBuf {
    db.parent().unwrap_or_else(|| Path::new("/")).join(FILE)
}

/// Seconds since the Unix epoch, now.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

/// The refusal for a registry that cannot be read, with what to do: every
/// authenticated request gets it, so the operator can recover.
pub fn unreadable(path: &Path, err: &impl std::fmt::Display) -> String {
    format!(
        "The server's token registry {} cannot be read ({err}). Move it aside and issue each client a new token with `user-memory host token add`.",
        path.display()
    )
}

/// Opens the registry at `path`, upgrading an older one; `None` when there
/// is none. Never creates it.
pub fn open(path: &Path) -> Result<Option<Connection>> {
    if !path.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|err| anyhow::anyhow!(unreadable(path, &err)))?;
    ready(conn, path).map(Some)
}

/// Opens the registry at `path`, creating it owner-only when it is absent:
/// what the first `host token add` does.
pub fn create(path: &Path) -> Result<Connection> {
    match private::create_file(fs::OpenOptions::new().write(true).create_new(true), path) {
        Ok(created) => drop(created),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err).with_context(|| format!("Cannot create {}", path.display())),
    }
    open(path)?.with_context(|| format!("{} disappeared while it was created", path.display()))
}

/// Checks the registry's version, upgrades an older one in one transaction,
/// and keeps it owner-only.
fn ready(mut conn: Connection, path: &Path) -> Result<Connection> {
    let damaged = |err: rusqlite::Error| anyhow::anyhow!(unreadable(path, &err));
    conn.busy_timeout(BUSY).map_err(damaged)?;
    // A damaged file fails here, at its first read.
    let found: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(damaged)?;
    if found > SCHEMA_VERSION {
        bail!(
            "The server's token registry {} has schema version {found}, but this user-memory is older than it (it uses version {SCHEMA_VERSION}); update user-memory. The registry was left unchanged.",
            path.display()
        );
    }
    if found < SCHEMA_VERSION {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(damaged)?;
        // Read again under the write lock: another first `add` may have
        // created it meanwhile.
        let found: i64 = tx
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(damaged)?;
        for step in found..SCHEMA_VERSION {
            let index = usize::try_from(step).context("negative registry version")?;
            tx.execute_batch(MIGRATIONS[index]).map_err(damaged)?;
        }
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
    }
    private::restrict(path)?;
    Ok(conn)
}

/// A token's scope: what its holder may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Write,
    Read,
}

impl Scope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Read => "read",
        }
    }

    fn parse(text: &str) -> Self {
        if text == "read" {
            Self::Read
        } else {
            Self::Write
        }
    }
}

/// One client token as the registry keeps it.
#[derive(Clone, Debug)]
pub struct ClientToken {
    pub name: String,
    pub scope: Scope,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

impl ClientToken {
    /// When the token stops working unless it is used again: whichever of
    /// its `--expires` date and [`INACTIVITY_EXPIRY`] after its last use (or
    /// its creation) comes first.
    pub fn expiry(&self) -> i64 {
        let idle = self.last_used_at.unwrap_or(self.created_at) + INACTIVITY_EXPIRY.as_secs_i64();
        self.expires_at.map_or(idle, |date| date.min(idle))
    }
}

/// A token unused this long is refused as expired: long enough for a
/// machine that is used every few weeks, short enough that a forgotten one
/// stops working on its own.
pub const INACTIVITY_EXPIRY: Duration = Duration::from_secs(90 * 24 * 60 * 60);

trait SecondsI64 {
    fn as_secs_i64(&self) -> i64;
}

impl SecondsI64 for Duration {
    fn as_secs_i64(&self) -> i64 {
        i64::try_from(self.as_secs()).unwrap_or(i64::MAX)
    }
}

/// Every token, revoked ones included, oldest first.
pub fn tokens(conn: &Connection) -> Result<Vec<ClientToken>> {
    let mut statement = conn.prepare(
        "SELECT name, scope, created_at, last_used_at, expires_at, revoked_at
         FROM client_tokens ORDER BY created_at, name",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(ClientToken {
            name: row.get(0)?,
            scope: Scope::parse(&row.get::<_, String>(1)?),
            created_at: row.get(2)?,
            last_used_at: row.get(3)?,
            expires_at: row.get(4)?,
            revoked_at: row.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Whether `retire-shared` has stopped the shared token file.
pub fn shared_file_retired(conn: &Connection) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            [SHARED_FILE_RETIRED],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .is_some_and(|value| value == "1"))
}

/// The most characters a client name has.
pub const NAME_MAX: usize = 64;

/// Whether `name` can name a client: 1 to [`NAME_MAX`] ASCII letters,
/// digits, `-`, `_` and `.`, starting with a letter or digit, so it reads
/// the same in a table, a file name and a shell.
pub fn valid_name(name: &str) -> bool {
    (1..=NAME_MAX).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Adds a token named `name` with `scope`, expiring at `expires_at` if
/// given; fails when a token not revoked has that name.
pub fn add(
    conn: &Connection,
    hash: &str,
    name: &str,
    scope: Scope,
    expires_at: Option<i64>,
) -> Result<()> {
    let added = conn.execute(
        "INSERT INTO client_tokens (hash, name, scope, created_at, expires_at)
         SELECT ?1, ?2, ?3, ?4, ?5
         WHERE NOT EXISTS (SELECT 1 FROM client_tokens WHERE name = ?2 AND revoked_at IS NULL)",
        rusqlite::params![hash, name, scope.as_str(), now(), expires_at],
    )?;
    if added == 0 {
        bail!(
            "A token named {name} exists; revoke it first with `user-memory host token revoke {name}`."
        );
    }
    Ok(())
}

/// Revokes the token named `name`; false when no token not revoked has it.
pub fn revoke(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE client_tokens SET revoked_at = ?2 WHERE name = ?1 AND revoked_at IS NULL",
        rusqlite::params![name, now()],
    )? > 0)
}

/// How many bytes of randomness a client token holds: 256 bits, beyond
/// guessing.
const TOKEN_BYTES: usize = 32;

/// The prefix every client token starts with, so secret scanners can tell
/// one.
pub const TOKEN_PREFIX: &str = "umt_";

/// A new client token: [`TOKEN_PREFIX`] and [`TOKEN_BYTES`] random bytes in
/// RFC 4648 base32, lowercase and without padding.
pub fn new_token() -> String {
    let bytes: [u8; TOKEN_BYTES] = rand::random();
    format!("{TOKEN_PREFIX}{}", base32(&bytes))
}

/// RFC 4648 base32 of `bytes`, lowercase, without padding.
fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut buffer: u16 = 0;
    let mut bits = 0;
    for &byte in bytes {
        buffer = (buffer << 8) | u16::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(ALPHABET[usize::from((buffer >> bits) & 0x1f)]));
        }
    }
    if bits > 0 {
        out.push(char::from(
            ALPHABET[usize::from((buffer << (5 - bits)) & 0x1f)],
        ));
    }
    out
}

/// The SHA-256 the registry keeps for `token`, in hex.
pub fn hash(token: &str) -> String {
    crate::setup::sha256(token.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_follows_rfc_4648() {
        // RFC 4648's test vectors, lowercase and without padding.
        for (input, output) in [
            ("", ""),
            ("f", "my"),
            ("fo", "mzxq"),
            ("foo", "mzxw6"),
            ("foob", "mzxw6yq"),
            ("fooba", "mzxw6ytb"),
            ("foobar", "mzxw6ytboi"),
        ] {
            assert_eq!(base32(input.as_bytes()), output, "{input:?}");
        }
    }

    #[test]
    fn a_token_is_the_prefix_and_52_base32_characters() {
        let token = new_token();
        let body = token.strip_prefix(TOKEN_PREFIX).unwrap();
        assert_eq!(body.len(), 52);
        assert!(
            body.bytes()
                .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
        );
        assert_ne!(new_token(), token);
    }

    #[test]
    fn names_are_short_and_plain() {
        for good in ["laptop", "a", "ci-runner_2.local", &"x".repeat(NAME_MAX)] {
            assert!(valid_name(good), "{good}");
        }
        for bad in [
            "",
            "-x",
            ".hidden",
            "with space",
            "tab\t",
            "é",
            &"x".repeat(NAME_MAX + 1),
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn expiry_is_the_earlier_of_the_date_and_idleness() {
        let idle = INACTIVITY_EXPIRY.as_secs_i64();
        let token = |last: Option<i64>, expires: Option<i64>| ClientToken {
            name: String::new(),
            scope: Scope::Write,
            created_at: 1_000,
            last_used_at: last,
            expires_at: expires,
            revoked_at: None,
        };
        assert_eq!(token(None, None).expiry(), 1_000 + idle);
        assert_eq!(token(Some(5_000), None).expiry(), 5_000 + idle);
        assert_eq!(token(Some(5_000), Some(2_000)).expiry(), 2_000);
        assert_eq!(token(None, Some(i64::MAX)).expiry(), 1_000 + idle);
    }
}
