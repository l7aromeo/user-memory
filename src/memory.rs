//! Typed user memory with explicit provenance and UUID identity.
//!
//! `schema` owns storage, `write` owns recording and corrections, `read` owns
//! search and reading whole memories, `overview` owns the index of every
//! memory, `index` owns derived search indexes, and `backups` owns the
//! store's automatic daily backups.

use clap::ValueEnum;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod backups;
mod calibration;
pub mod embedding;
mod ids;
mod index;
mod links;
pub(crate) mod migrate;
mod overview;
mod page;
mod read;
mod repair;
mod review;
mod rules;
mod schema;
mod selection;
mod similar;
mod stamps;
mod titles;
mod transfer;
mod write;

pub use ids::MemoryRef;
pub use index::reindex;
pub use links::{Link, link};
pub use migrate::{Migration, check_migration, migrate};
pub use overview::{
    INDEX_UNAVAILABLE, Index, ProjectIndex, index, index_failure, merge, next_cursor, shorten,
    tiered,
};
pub use page::stale_cursor;
pub use read::{Read, Search, read, search};
pub use repair::{problems, repair};
pub use review::{Review, review};
pub use rules::{GENERATED, MAX_BYTES as RULES_MAX_BYTES, instructions};
pub(crate) use schema::BUSY;
pub use schema::connect;
pub use selection::{Subset, local_path};
pub use stamps::{Stamp, Stamps, parse_stamps};
pub use titles::{Titles, parse_titles};
pub use transfer::{
    ExportFile, ExportMemories, ImportMemories, ImportRequest, MAX_BYTES, OnConflict,
    export_document, export_named, import_document, import_named, write_export,
};
pub use write::{Forget, Record, Update, forget, record, update};

#[derive(
    Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, ValueEnum, PartialEq, Eq,
)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    #[default]
    Explicit,
    Observed,
    Inferred,
}
impl Source {
    fn name(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Observed => "observed",
            Self::Inferred => "inferred",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Global,
    Project,
}
impl Scope {
    fn name(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Memory {
    pub id: Uuid,
    pub created_at: String,
    pub updated_at: String,
    #[serde(rename = "type")]
    pub memory_type: String,
    pub category: String,
    pub context: String,
    /// One line an agent can act on without reading the rest; listed in the index.
    pub title: String,
    pub content: String,
    pub reason: String,
    pub source: String,
    pub status: String,
    pub scope: String,
    pub project: String,
    pub evidence: String,
    pub evidence_ids: Vec<Uuid>,
    /// Conversations that stated or confirmed this memory, sorted; empty when unknown.
    pub origins: Vec<String>,
    /// Number of distinct conversations in `origins`.
    pub confirmations: usize,
    /// When the memory was last confirmed in another conversation; null until then.
    pub last_confirmed_at: Option<String>,
    /// The memory that replaced this one, when its status is `superseded`.
    pub superseded_by: Option<Uuid>,
    #[serde(skip)]
    sequence: i64,
}

/// Result of one embedding request made to check the configuration.
pub struct EmbeddingProbe {
    pub model: String,
    pub dimensions: usize,
    pub elapsed: std::time::Duration,
}

/// Validates embedding settings without contacting the endpoint; returns the
/// model, or `None` when semantic recall is off.
pub fn embedding_model(setting: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Option<String>> {
    Ok(index::embed::Config::from_settings(setting)?.map(|config| config.identity().0.to_owned()))
}

pub use calibration::Calibration;

/// What record-time duplicate detection uses for vectors of `length` from
/// `model` in the store at `db`: built-in or calibrated cutoffs, or keywords
/// only when the model could not be or is not yet calibrated.
pub fn calibration(
    db: &std::path::Path,
    model: &str,
    length: usize,
) -> anyhow::Result<Calibration> {
    calibration::state(&connect(db)?, model, length)
}

/// Validates embedding settings and embeds one probe text.
pub fn probe_embedding(
    setting: &dyn Fn(&str) -> Option<String>,
) -> anyhow::Result<Option<EmbeddingProbe>> {
    let Some(config) = index::embed::Config::from_settings(setting)? else {
        return Ok(None);
    };
    let started = std::time::Instant::now();
    let vectors = config.embed(&["user-memory doctor probe".to_owned()])?;
    Ok(Some(EmbeddingProbe {
        model: config.identity().0.to_owned(),
        dimensions: vectors.first().map_or(0, Vec::len),
        elapsed: started.elapsed(),
    }))
}

/// Longest title: one index line, about a terminal row, so the session
/// index stays readable and grows slowly.
pub(crate) const TITLE_LIMIT: usize = 120;

pub fn clean(text: &str, limit: usize) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(limit)
        .collect()
}

/// Canonical project identity shared by writes and read filters.
///
/// Git remotes collapse to lowercase `host/path` without scheme, user, port,
/// trailing slash or `.git` suffix, so SSH and HTTPS clones of one repository
/// share memories. Any other identifier is whitespace-cleaned and lowercased.
pub(crate) fn project_key(raw: &str) -> String {
    let key = clean(raw, usize::MAX).to_lowercase();
    remote_key(&key).unwrap_or(key)
}

/// The canonical key of a network git remote, or `None` for anything else,
/// such as a local path that would make the key device-specific.
pub(crate) fn remote_project_key(raw: &str) -> Option<String> {
    remote_key(&clean(raw, usize::MAX).to_lowercase())
}

/// `value` is already cleaned and lowercased.
fn remote_key(value: &str) -> Option<String> {
    if value.contains(char::is_whitespace) {
        return None;
    }
    let (host, path) = if let Some((scheme, rest)) = value.split_once("://") {
        if !matches!(
            scheme,
            "ssh" | "git" | "http" | "https" | "git+ssh" | "ssh+git"
        ) {
            return None;
        }
        let rest = rest.split(['?', '#']).next().unwrap_or_default();
        let (authority, path) = rest.split_once('/')?;
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        (without_port(host), path)
    } else if let Some((login, path)) = value.split_once(':')
        && !login.contains('/')
    {
        // scp-like `[user@]host:path`; a bare `word:rest` without a user or a
        // dotted host stays an ordinary identifier.
        match login.rsplit_once('@') {
            Some((_, host)) => (host, path),
            None if login.contains('.') => (login, path),
            None => return None,
        }
    } else {
        let (host, path) = value.split_once('/')?;
        if !host.contains('.') {
            return None;
        }
        (host, path)
    };
    let valid_host = if let Some(ip) = host.strip_prefix('[') {
        ip.strip_suffix(']').is_some_and(|ip| {
            !ip.is_empty() && ip.chars().all(|c| c.is_ascii_hexdigit() || c == ':')
        })
    } else {
        host.split('.').all(|label| {
            !label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
    };
    let path = path.trim_matches('/');
    let path = path
        .strip_suffix(".git")
        .unwrap_or(path)
        .trim_end_matches('/');
    (valid_host && !path.is_empty()).then(|| format!("{host}/{path}"))
}

fn without_port(host: &str) -> &str {
    if host.starts_with('[') {
        return host.split_inclusive(']').next().unwrap_or(host);
    }
    match host.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    }
}

#[cfg(test)]
mod tests {
    use super::{project_key, remote_project_key};

    // Built-in cutoffs win; other models and lengths are calibrated, could
    // not be, or wait to be, as the store records.
    #[test]
    fn a_store_reports_each_model_s_calibration() {
        use super::Calibration;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("m.sqlite3");
        let conn = super::connect(&db).unwrap();
        conn.execute_batch(
            "INSERT INTO embedding_cutoffs (model,length,duplicate,related) VALUES
             ('qwen3-embedding:4b',1024,0.5,0.4),('other-model',768,0.8,0.6),('other-model',384,NULL,NULL)",
        )
        .unwrap();
        for (model, length, expected) in [
            ("qwen3-embedding:4b", 1024, Calibration::BuiltIn),
            ("qwen3-embedding:4b", 2560, Calibration::BuiltIn),
            (
                "other-model",
                768,
                Calibration::Calibrated {
                    duplicate: 0.8,
                    related: 0.6,
                },
            ),
            ("other-model", 384, Calibration::Unseparable),
            ("other-model", 1024, Calibration::Missing),
            ("qwen3-embedding:4b", 768, Calibration::Missing),
        ] {
            assert_eq!(
                super::calibration(&db, model, length).unwrap(),
                expected,
                "{model} at {length}"
            );
        }
    }

    #[test]
    fn git_remotes_of_one_repository_share_a_key() {
        for remote in [
            "git@github.com:Owner/Repo.git",
            "https://github.com/owner/repo",
            "ssh://git@github.com/owner/repo.git",
            "https://github.com/Owner/Repo.git/",
            "HTTPS://user:secret@GitHub.com:443/owner/repo.git",
            "ssh://git@github.com:22/owner/repo",
            "git+ssh://git@github.com/owner/repo.git",
            "git://github.com/owner/repo.git",
            "https://github.com/owner/repo.git?ref=main#readme",
            "github.com/owner/repo",
            "github.com/Owner/Repo.git",
            "  git@github.com:owner/repo.git  ",
            "github.com:owner/repo.git",
            "https://github.com/owner/repo.GIT",
        ] {
            assert_eq!(project_key(remote), "github.com/owner/repo", "{remote}");
        }
    }

    #[test]
    fn nested_groups_and_ip_hosts_keep_their_full_path() {
        assert_eq!(
            project_key("git@gitlab.example.com:Group/Sub/Repo.git"),
            "gitlab.example.com/group/sub/repo"
        );
        assert_eq!(
            project_key("ssh://git@[::1]:2222/team/repo.git"),
            "[::1]/team/repo"
        );
        assert_eq!(
            project_key("ssh://git@10.0.0.5:2222/team/repo.git"),
            "10.0.0.5/team/repo"
        );
        assert_eq!(
            project_key("git@internal:team/repo.git"),
            "internal/team/repo"
        );
    }

    #[test]
    fn other_identifiers_are_cleaned_and_lowercased_only() {
        assert_eq!(project_key("  Side   Quest "), "side quest");
        assert_eq!(project_key("Notebooks"), "notebooks");
        assert_eq!(project_key("acme-platform.git"), "acme-platform.git");
        assert_eq!(project_key("team/Repo.git"), "team/repo.git");
        assert_eq!(project_key("client:acme"), "client:acme");
        assert_eq!(project_key("C:\\Work\\Repo"), "c:\\work\\repo");
        assert_eq!(
            project_key("file:///Users/me/Repo.git"),
            "file:///users/me/repo.git"
        );
        assert_eq!(project_key("https://github.com"), "https://github.com");
        assert_eq!(project_key("git@github.com:"), "git@github.com:");
        assert_eq!(project_key("git@:owner/repo"), "git@:owner/repo");
        assert_eq!(
            project_key("git@bad_host:owner/repo"),
            "git@bad_host:owner/repo"
        );
        assert_eq!(
            project_key("https://github.com/.git"),
            "https://github.com/.git"
        );
        assert_eq!(project_key("git:"), "git:");
        assert_eq!(project_key(""), "");
        // Keys are never cut: writes refuse one over their limit.
        assert_eq!(project_key("a".repeat(300).as_str()).len(), 300);
    }

    #[test]
    fn only_network_remotes_have_a_remote_key() {
        assert_eq!(
            remote_project_key(" git@github.com:Owner/Repo.git ").as_deref(),
            Some("github.com/owner/repo")
        );
        assert_eq!(remote_project_key("/srv/git/repo.git"), None);
        assert_eq!(remote_project_key("file:///srv/git/repo.git"), None);
        assert_eq!(remote_project_key("../repo"), None);
        assert_eq!(remote_project_key("./repo.git"), None);
        assert_eq!(remote_project_key("https://[]/owner/repo"), None);
        assert_eq!(remote_project_key("https://[::1/owner/repo"), None);
        assert_eq!(remote_project_key("https://a..b/owner/repo"), None);
        assert_eq!(
            remote_project_key("https://github.com:http/owner/repo"),
            None
        );
    }

    #[test]
    fn git_root_keys_are_kept_verbatim_apart_from_case() {
        assert_eq!(project_key("git-root:ABC123def"), "git-root:abc123def");
    }
}
