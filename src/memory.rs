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
mod changes;
mod delivery;
pub(crate) mod delivery_files;
#[cfg(test)]
pub use changes::position as store_position;
pub use changes::{marked, reconcile};
pub mod embedding;
mod history;
mod ids;
mod index;
mod links;
#[cfg(test)]
mod matcher;
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
mod undo;
mod words;
mod write;

pub use history::{CLI, acting_as, client_name, set_default_client};
pub(crate) use history::{end as end_writer, note as note_writer};
pub use ids::MemoryRef;
pub use index::{outdated as search_index_outdated, reindex};
pub use links::{Link, link};
pub use migrate::{Migration, check_migration, migrate};
pub use overview::{
    FitUnit, INDEX_FLOOR, INDEX_UNAVAILABLE, Index, ProjectIndex, RulesLeftOut, index,
    index_failure, merge, next_cursor, rules_headings, rules_left_out, rules_part, shorten, tiered,
};
pub use page::stale_cursor;
pub use read::{Read, Search, read, search};
pub use repair::{problems, repair};
pub use review::{Review, review};
pub use rules::{GENERATED, MAX_BYTES as RULES_MAX_BYTES, instructions};
pub(crate) use schema::BUSY;
pub use schema::{
    compact, compact_skipped, compacted, connect, expect_migration, hold, servable, store_turn,
};
pub use selection::{Subset, local_path};
#[cfg(test)]
pub use stamps::Rule;
pub use stamps::{PRINT_FORMAT, Position, Stamp, Stamps, StampsReply, parse_stamps_reply};
pub use titles::{Titles, parse_titles};
pub use transfer::{
    ExportFile, ExportMemories, ImportMemories, ImportRequest, MAX_BYTES, OnConflict,
    export_document, export_named, export_next, import_document, import_named, import_next,
    write_export,
};
pub use write::{COPY_ON_SERVER, Forget, Record, Update, forget, record, update};

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

/// What a tentative memory is and what to do with it, given beside its full
/// text: an agent reading only the content can take an observation for the
/// user's rule. It is context until the user says otherwise.
const TENTATIVE_NOTE: &str = "An agent's observation or inference, not the user's rule: don't apply it unasked; you may mention it or ask the user, and only their word makes it a rule.";

/// Adds [`TENTATIVE_NOTE`] as `note` to a memory in a reply when it is tentative.
fn note_tentative(memory: &mut serde_json::Value) {
    if memory.get("status").and_then(serde_json::Value::as_str) == Some("tentative")
        && let Some(fields) = memory.as_object_mut()
    {
        fields.insert("note".into(), TENTATIVE_NOTE.into());
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

pub(crate) use index::embed::{DEFAULT_MODEL as DEFAULT_EMBED_MODEL, DOCKER_HOST, loopback};

/// The host of the embeddings endpoint `setting` names, when it is not on
/// this machine: memory text and searches are sent there. `None` when
/// semantic search is off, its endpoint is on this machine, or its address
/// is not a URL, which semantic search refuses.
pub fn embedding_elsewhere(setting: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let url = reqwest::Url::parse(setting("USER_MEMORY_EMBED_URL")?.trim()).ok()?;
    if index::embed::on_this_machine(&url) {
        return None;
    }
    url.host_str().map(str::to_owned)
}

/// What `embedding set`, `embedding show` and doctor say when semantic
/// search sends memory text off this machine: the endpoint's host, since
/// every memory's text and every search goes there.
pub fn embedding_disclosure(setting: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    embedding_elsewhere(setting)
        .map(|host| format!("Semantic search sends memory text and searches to {host}"))
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

/// In debug builds, waits here while the file the environment variable `var`
/// names exists, after creating that name with `.reached` added, so a test
/// can order one process's step against another's.
#[cfg(debug_assertions)]
pub(crate) fn hold_for_tests(var: &str) {
    let Some(hold) = std::env::var_os(var) else {
        return;
    };
    let mut reached = hold.clone();
    reached.push(".reached");
    let _ = std::fs::write(&reached, "");
    while std::path::Path::new(&hold).exists() {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        delivery::{self, DeliveryIndex},
        index,
        matcher::{self, Action, Kind, Window},
        project_key, remote_project_key,
    };

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

    /// The delivery index of a real store's scope, written to a file and loaded,
    /// matches a passage whose text is then cut from the store; another
    /// project's memory and an observation about the user stay out of it.
    #[test]
    fn a_store_s_scope_is_indexed_and_its_passage_cut_from_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memories.sqlite3");
        let record = |title: &str, content: &str, project: &str, kind: &str, source: &str| {
            let input = serde_json::json!({
                "type": kind, "category": "build", "title": title, "content": content,
                "scope": "project", "project": project, "source": source,
                "evidence": "seen in the build log", "user_confirmed": true,
            });
            let out = super::record(&db, serde_json::from_value(input).unwrap()).unwrap();
            assert_eq!(out["stored"], true, "{out}");
        };
        let ours = "github.com/example/ledger";
        let door = "The zeppelin hangar's glacier door sticks in winter.";
        record("Hangar door", door, ours, "fact", "explicit");
        record(
            "Crossing log",
            "Pilots write every zeppelin glacier crossing into the flight ledger by hand.",
            "github.com/example/other",
            "fact",
            "explicit",
        );
        record(
            "Map habit",
            "Before a zeppelin trip the user studies glacier maps over breakfast.",
            ours,
            "behavior",
            "observed",
        );
        let file = dir.path().join("delivery.index");
        let built = delivery::build(&db, &[ours.to_owned()], |_, change| change.to_string());
        std::fs::write(&file, built.unwrap()).unwrap();
        let index = DeliveryIndex::load(&file).unwrap();
        assert_eq!(index.len(), 1, "only this project's stated memory");
        let mut window = Window::default();
        window.push(&Action {
            kind: Kind::Prompt,
            text: "zeppelin glacier".into(),
            failed: false,
        });
        let found = index.rank(&window).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        let cut = matcher::text(&conn, index.id(found[0].memory).unwrap(), &found[0]).unwrap();
        assert_eq!(cut, door);
    }

    /// The rows of the store at `db` for the global scope and `projects`,
    /// as the delivery index was built before its one pass: each stored
    /// token decoded to its term, which the writer encodes again.
    fn delivery_sources(db: &std::path::Path, projects: &[String]) -> Vec<delivery::Source> {
        let conn = rusqlite::Connection::open(db).unwrap();
        let scope = serde_json::to_string(projects).unwrap();
        let mut keys = std::collections::HashMap::new();
        let mut sources: Vec<delivery::Source> = Vec::new();
        let mut found = conn.prepare(delivery::CANDIDATES).unwrap();
        let mut found = found.query([scope]).unwrap();
        while let Some(r) = found.next().unwrap() {
            keys.insert(r.get::<_, i64>(0).unwrap(), sources.len());
            sources.push(delivery::Source {
                id: r.get(1).unwrap(),
                rows: Vec::new(),
            });
        }
        let mut rows = conn
            .prepare("SELECT rowid,category,title,type,body,anchors,start,length FROM search_words ORDER BY rowid")
            .unwrap();
        let mut rows = rows.query([]).unwrap();
        while let Some(r) = rows.next().unwrap() {
            let number: i64 = r.get(0).unwrap();
            let Some(&m) = keys.get(&number.div_euclid(index::SLOTS)) else {
                continue;
            };
            let mut stems = Vec::new();
            for (c, field) in index::stems::Field::ALL.into_iter().enumerate() {
                let column: String = r.get(c + 1).unwrap();
                stems.extend(
                    column
                        .split_whitespace()
                        .map(|t| (delivery::decode(t), field)),
                );
            }
            let anchors: String = r.get(5).unwrap();
            let start: Option<i64> = r.get(6).unwrap();
            let length: Option<i64> = r.get(7).unwrap();
            sources[m].rows.push(delivery::SourceRow {
                slot: number.rem_euclid(index::SLOTS),
                start: usize::try_from(start.unwrap_or(0)).unwrap(),
                length: usize::try_from(length.unwrap_or(0)).unwrap(),
                stems,
                anchors: anchors.split_whitespace().map(delivery::decode).collect(),
            });
        }
        sources
    }

    /// The one-pass build of a real store writes, byte for byte, the file
    /// the writer makes from the same rows decoded: across scripts, code
    /// spans, paths, every field and memories of several passages.
    #[test]
    fn the_delivery_index_built_in_one_pass_is_the_file_its_rows_make() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memories.sqlite3");
        let ours = "github.com/example/ledger";
        let long = [
            "Run `cargo build --release` before tagging; the archive goes to /srv/releases/ledger, and the checksum file beside it is written by the same step, never by hand, so a release that was rebuilt on another machine is caught before anyone downloads it and before the notes go out to the list.",
            "Die Größe der Datei prüft `stat -c %s`, nie `ls`; ältere Läufe schrieben falsche Werte, weil `ls` gerundete Größen zeigt und die Prüfsumme danach nicht mehr stimmte, was erst beim Herunterladen auffiel und die ganze Veröffentlichung um einen Tag verzögerte.",
            "漢字のテストは `just check` で動く。失敗したら /tmp/ledger-ci を消してからもう一度実行する。古いキャッシュが残っていると、文字コードの比較が前回の結果を使ってしまい、正しい失敗が隠れてしまうからだ。これは三回起きた。",
        ]
        .join("\n");
        for (title, content, scope, kind, context) in [
            (
                "Release steps",
                long.as_str(),
                "project",
                "procedure",
                "Asked in a release review.",
            ),
            (
                "Quoting style",
                "Prefer straight quotes in docs; `sed -i ''` on macOS.",
                "global",
                "preference",
                "",
            ),
            (
                "Ledger owner",
                "The ledger service belongs to the payments group.",
                "project",
                "fact",
                "From the team page.",
            ),
            (
                "Größe",
                "Füße und Straßen: Ünïcödé stems stay whole.",
                "global",
                "fact",
                "",
            ),
        ] {
            let input = serde_json::json!({
                "type": kind, "category": "build", "title": title, "content": content,
                "context": context, "scope": scope, "project": if scope == "project" { ours } else { "" },
                "reason": "the user said so", "evidence": "said in a review", "user_confirmed": true,
            });
            let out = super::record(&db, serde_json::from_value(input).unwrap()).unwrap();
            assert_eq!(out["stored"], true, "{out}");
        }
        let projects = [ours.to_owned()];
        let built = delivery::build(&db, &projects, |_, change| change.to_string()).unwrap();
        let header = DeliveryIndex::read(built.clone()).unwrap().header;
        let sources = delivery_sources(&db, &projects);
        assert_eq!(sources.len(), 4);
        assert!(
            sources.iter().any(|s| s.rows.len() > 3),
            "a memory of several passages"
        );
        assert!(built == DeliveryIndex::write_with(&header, &sources).unwrap());
    }
}
