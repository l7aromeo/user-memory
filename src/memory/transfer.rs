//! Export and import of the whole store as one JSON document, for backups,
//! moving to a server, upgrades and hand edits.
//!
//! Import validates the whole file first and writes nothing when anything is
//! invalid. It merges by memory id against the store as it was before the
//! import: new memories are added, identical ones gain the file's origins,
//! and differing ones are conflicts resolved by `OnConflict`. Changed
//! supports stale their dependents through the normal triggers.
//!
//! Both take read's filters ([`Subset`]); without any they cover every
//! memory. A filter selects memories and never edits one: an exported
//! memory keeps its links even when their other end is not exported, and
//! import settles each link by what is there, among the imported memories
//! or in the store:
//! - a distinct pair with an end missing is dropped;
//! - a `superseded_by` naming a missing memory is kept, as the store keeps
//!   one whose replacement was forgotten;
//! - an active inference with a missing support is skipped when the file or
//!   the import is filtered, and refused otherwise, since only a filter can
//!   leave a support behind. Stale and superseded inferences keep theirs.
//!
//! Agents reach files only inside `exports_dir`, and results never contain
//! memory contents, file contents or the store's path.

use super::{
    Memory, Scope, clean,
    schema::{SCHEMA_VERSION, SELECT, connect, row, scratch, write},
    selection::{Filter, Subset, candidates},
    similar::{Draft, Pool, similar},
    titles::{self, Titles},
    write::{Missing, Record, check_support, normalize},
};
use crate::private;
use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
use rusqlite::{Connection, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

/// Largest export file accepted.
pub const MAX_BYTES: u64 = 64 * 1024 * 1024;
const FORMAT: &str = "user-memory-export";
/// Version of the logical memory model in the file, independent of the
/// database schema. Version 2 added each memory's `title`.
const FORMAT_VERSION: u64 = 2;
/// Exports written before titles existed; still imported, because they
/// are the user's backups, with each title from a map the user approved.
const TITLELESS_VERSION: u64 = 1;
/// Longest file name an agent may use in the exports directory.
const NAME_LIMIT: usize = 255;
const ORIGIN_LIMIT: usize = 240;

/// What import does with a memory whose id exists with different contents.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum, Deserialize, Serialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum OnConflict {
    /// Write nothing and list the conflicts.
    #[default]
    Abort,
    /// Keep the store's version; still add the file's origins.
    KeepStore,
    /// Replace the store's version with the file's.
    UseFile,
}

#[derive(Deserialize, JsonSchema)]
pub struct ExportMemories {
    /// File name in the exports directory beside the memory store, such as backup-2026-10-15.json. A name, never a path.
    pub name: String,
    /// Replace an existing export with that name.
    #[serde(default)]
    pub overwrite: bool,
    /// Which memories to export; none of these means every memory.
    #[serde(flatten)]
    pub subset: Subset,
}

#[derive(Deserialize, JsonSchema)]
pub struct ImportMemories {
    /// File name of an export in the exports directory beside the memory store.
    pub name: String,
    /// Write the import; without it (or without user_confirmed) the result is a preview, which stores nothing but holds the store's write lock while it tries replacements, so a large use-file preview takes about as long as applying.
    #[serde(default)]
    pub apply: bool,
    /// For memories whose id exists with different contents: abort (default), keep-store or use-file.
    #[serde(default)]
    pub on_conflict: OnConflict,
    /// True only when the user explicitly asked to import this file.
    #[serde(default)]
    pub user_confirmed: bool,
    /// For an export from before titles (format version 1): each memory id to the title the user approved.
    #[serde(default, deserialize_with = "titles::unique_option")]
    pub titles: Option<Titles>,
    /// Which of the file's memories to import; none of these means all of them.
    #[serde(flatten)]
    pub subset: Subset,
}

/// An import's options apart from the document, as a client sends them to
/// the memory server's import endpoint.
#[derive(Default, Deserialize, Serialize)]
pub struct ImportRequest {
    #[serde(default)]
    pub apply: bool,
    #[serde(default)]
    pub on_conflict: OnConflict,
    #[serde(default, deserialize_with = "titles::unique_option")]
    pub titles: Option<Titles>,
    #[serde(flatten)]
    pub subset: Subset,
}

// ---------------------------------------------------------------- file model

#[derive(Clone, Debug, Serialize)]
struct Origin {
    origin: String,
    first_seen: String,
}

/// One memory as written to and read from an export file.
#[derive(Clone, Debug, Serialize)]
struct Entry {
    id: Uuid,
    #[serde(rename = "type")]
    memory_type: String,
    category: String,
    title: String,
    content: String,
    context: String,
    reason: String,
    scope: String,
    project: String,
    source: String,
    status: String,
    evidence: String,
    evidence_ids: Vec<Uuid>,
    origins: Vec<Origin>,
    superseded_by: Option<Uuid>,
    created_at: String,
    updated_at: String,
    last_confirmed_at: Option<String>,
}

impl Entry {
    /// Names of the content-bearing fields that differ from `other`.
    fn differences(&self, other: &Entry) -> Vec<&'static str> {
        let mut fields = Vec::new();
        let pairs: [(&'static str, bool); 13] = [
            ("type", self.memory_type == other.memory_type),
            ("category", self.category == other.category),
            ("title", self.title == other.title),
            ("content", self.content == other.content),
            ("context", self.context == other.context),
            ("reason", self.reason == other.reason),
            ("scope", self.scope == other.scope),
            ("project", self.project == other.project),
            ("source", self.source == other.source),
            ("status", self.status == other.status),
            ("evidence", self.evidence == other.evidence),
            ("evidence_ids", self.evidence_ids == other.evidence_ids),
            ("superseded_by", self.superseded_by == other.superseded_by),
        ];
        for (name, same) in pairs {
            if !same {
                fields.push(name);
            }
        }
        fields
    }

    fn draft(&self) -> Draft<'_> {
        Draft {
            memory_type: &self.memory_type,
            category: &self.category,
            content: &self.content,
            context: &self.context,
            reason: &self.reason,
            evidence: &self.evidence,
            scope: if self.scope == "project" {
                Scope::Project
            } else {
                Scope::Global
            },
            project: &self.project,
        }
    }

    /// The entry as a record, for the shared evidence rules.
    fn record(&self) -> Record {
        serde_json::from_value(json!({
            "type": self.memory_type, "category": self.category, "title": self.title,
            "content": self.content,
            "scope": self.scope, "project": self.project, "source": self.source,
            "evidence_ids": self.evidence_ids, "user_confirmed": true,
        }))
        .expect("a validated entry is a record")
    }

    fn active(&self) -> bool {
        matches!(self.status.as_str(), "confirmed" | "tentative")
    }
}

/// The store's memories as entries, in insertion order.
fn entries(conn: &Connection) -> Result<Vec<Entry>> {
    let memories = conn
        .prepare(&format!("{SELECT} ORDER BY rowid"))?
        .query_map([], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut origins: HashMap<Uuid, Vec<Origin>> = HashMap::new();
    let mut statement = conn.prepare(
        "SELECT memory_id,origin,first_seen FROM memory_origins ORDER BY memory_id,origin",
    )?;
    let rows = statement.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            Origin {
                origin: r.get(1)?,
                first_seen: r.get(2)?,
            },
        ))
    })?;
    for found in rows {
        let (id, origin) = found?;
        if let Ok(id) = Uuid::parse_str(&id) {
            origins.entry(id).or_default().push(origin);
        }
    }
    Ok(memories
        .into_iter()
        .map(|m| entry(m, &mut origins))
        .collect())
}

fn entry(m: Memory, origins: &mut HashMap<Uuid, Vec<Origin>>) -> Entry {
    Entry {
        id: m.id,
        memory_type: m.memory_type,
        category: m.category,
        title: m.title,
        content: m.content,
        context: m.context,
        reason: m.reason,
        scope: m.scope,
        project: m.project,
        source: m.source,
        status: m.status,
        evidence: m.evidence,
        evidence_ids: m.evidence_ids,
        origins: origins.remove(&m.id).unwrap_or_default(),
        superseded_by: m.superseded_by,
        created_at: m.created_at,
        updated_at: m.updated_at,
        last_confirmed_at: m.last_confirmed_at,
    }
}

fn distinct_pairs(conn: &Connection) -> Result<Vec<(Uuid, Uuid)>> {
    let pairs = conn
        .prepare("SELECT a,b FROM memory_distinct ORDER BY a,b")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(pairs
        .into_iter()
        .filter_map(|(a, b)| Some((Uuid::parse_str(&a).ok()?, Uuid::parse_str(&b).ok()?)))
        .collect())
}

fn status_counts<'a>(statuses: impl Iterator<Item = &'a str>) -> Value {
    let mut counts = BTreeMap::from([
        ("confirmed", 0),
        ("tentative", 0),
        ("stale", 0),
        ("superseded", 0),
    ]);
    for status in statuses {
        if let Some(count) = counts.get_mut(status) {
            *count += 1;
        }
    }
    json!(counts)
}

// ---------------------------------------------------------------- export

/// The store's memories `subset` covers as an export document (pretty JSON,
/// trailing newline) and a summary of what it holds. A filtered document
/// records its filters, and the summary counts the links whose other end
/// it leaves out.
pub fn export_document(db: &Path, subset: &Subset) -> Result<(String, Value)> {
    let filter = subset.filter()?;
    let conn = connect(db)?;
    let mut memories = entries(&conn)?;
    let mut pairs = distinct_pairs(&conn)?;
    let exported_at: String =
        conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%SZ','now')", [], |r| {
            r.get(0)
        })?;
    let mut outside = None;
    if let Some(filter) = &filter {
        let chosen: HashSet<Uuid> = candidates(&conn, filter, None)?
            .into_iter()
            .map(|m| m.id)
            .collect();
        memories.retain(|m| chosen.contains(&m.id));
        pairs.retain(|(a, b)| chosen.contains(a) || chosen.contains(b));
        let out = |id: &Uuid| !chosen.contains(id);
        outside = Some(json!({
            "supports": memories.iter().flat_map(|m| &m.evidence_ids).filter(|id| out(id)).count(),
            "replacements": memories.iter().filter_map(|m| m.superseded_by.as_ref()).filter(|id| out(id)).count(),
            "distinct_pairs": pairs.iter().filter(|(a, b)| out(a) || out(b)).count(),
        }));
    }
    let mut summary = json!({
        "memories": status_counts(memories.iter().map(|m| m.status.as_str())),
        "distinct_pairs": pairs.len(),
    });
    let mut document = json!({
        "format": FORMAT,
        "format_version": FORMAT_VERSION,
        "schema_version": SCHEMA_VERSION,
        "exported_at": exported_at,
    });
    if let Some(outside) = outside {
        document["selection"] = json!(subset);
        summary["selection"] = json!(subset);
        summary["links_outside"] = outside;
    }
    document["memories"] = json!(memories);
    document["distinct_pairs"] = json!(pairs.iter().map(|(a, b)| [a, b]).collect::<Vec<_>>());
    Ok((serde_json::to_string_pretty(&document)? + "\n", summary))
}

/// Writes `document` to `path` atomically with mode 0600: a dot-prefixed
/// temporary file in the same directory, fsync, rename. An existing path
/// (even a dangling symlink) is refused unless `force`.
pub fn write_export(path: &Path, document: &str, force: bool) -> Result<()> {
    let mut file = ExportFile::create(path, force)?;
    file.write(document.as_bytes())?;
    file.finish()
}

/// An export file being written: a private temporary file beside `path`
/// until [`ExportFile::finish`] syncs it and renames it into place, which
/// replaces a symlink entry instead of following it. Dropped unfinished, it
/// removes the temporary file. The file is closed before either, as
/// Windows renames and deletes a file freely only once no handle is open.
pub struct ExportFile {
    temporary: PathBuf,
    path: PathBuf,
    /// Open until [`ExportFile::finish`] or the drop closes it.
    file: Option<fs::File>,
    done: bool,
}

impl ExportFile {
    /// Starts the export file `path`; an existing path (even a dangling
    /// symlink) is refused unless `force`.
    pub fn create(path: &Path, force: bool) -> Result<Self> {
        if !force && fs::symlink_metadata(path).is_ok() {
            bail!(
                "{} already exists; use --force to replace it",
                path.display()
            );
        }
        let name = path
            .file_name()
            .context("Export path must name a file")?
            .to_string_lossy()
            .into_owned();
        Self::create_in(directory_of(path), &name)
            .with_context(|| format!("Cannot write {}", path.display()))
    }

    /// Starts the file `name` in `directory`.
    fn create_in(directory: &Path, name: &str) -> Result<Self> {
        let temporary = directory.join(format!(".{name}.{}.tmp", std::process::id()));
        let file = private::create_file(
            fs::OpenOptions::new().write(true).create_new(true),
            &temporary,
        )?;
        Ok(Self {
            temporary,
            path: directory.join(name),
            file: Some(file),
            done: false,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let file = self.file.as_mut().context("The export file is closed")?;
        file.write_all(bytes)
            .with_context(|| format!("Cannot write {}", self.path.display()))
    }

    /// Syncs and closes the file, then puts it in place.
    pub fn finish(mut self) -> Result<()> {
        let file = self.file.take().context("The export file is closed")?;
        file.sync_all()
            .and_then(|()| {
                drop(file);
                fs::rename(&self.temporary, &self.path)
            })
            .with_context(|| format!("Cannot write {}", self.path.display()))?;
        self.done = true;
        Ok(())
    }
}

impl Drop for ExportFile {
    fn drop(&mut self) {
        drop(self.file.take());
        if !self.done {
            let _ = fs::remove_file(&self.temporary);
        }
    }
}

/// The directory holding `path`; `.` for a bare file name.
fn directory_of(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Writes `document` as the file `name` in `directory`, as [`ExportFile`].
fn write_atomically(directory: &Path, name: &str, document: &str) -> Result<()> {
    let mut file = ExportFile::create_in(directory, name)?;
    file.write(document.as_bytes())?;
    file.finish()
}

// ---------------------------------------------------------------- validation

/// One problem in the file, by memory index and id. Never quotes values.
#[derive(Serialize)]
struct Invalid {
    index: Option<usize>,
    id: Option<String>,
    rule: String,
}

fn invalid(index: usize, id: Option<Uuid>, rule: impl Into<String>) -> Invalid {
    Invalid {
        index: Some(index),
        id: id.map(|id| id.to_string()),
        rule: rule.into(),
    }
}

/// `YYYY-MM-DD HH:MM:SS`, or ISO 8601 UTC `YYYY-MM-DDTHH:MM:SSZ` converted to
/// it. Store timestamps are compared as strings, so any other form would
/// break recency.
fn timestamp(value: &str) -> Option<String> {
    let stored = match value.len() {
        19 => value.to_owned(),
        20 if value.ends_with('Z') && value.as_bytes()[10] == b'T' => {
            format!("{} {}", &value[..10], &value[11..19])
        }
        _ => return None,
    };
    let b = stored.as_bytes();
    let digits = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18];
    let ok = digits.iter().all(|&i| b[i].is_ascii_digit())
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b' '
        && b[13] == b':'
        && b[16] == b':';
    ok.then_some(stored)
}

fn text<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    match value.get(field) {
        None | Some(Value::Null) => Some(""),
        Some(Value::String(s)) => Some(s),
        Some(_) => None,
    }
}

/// Parses and normalizes one file memory. Returns the entry and whether
/// normalization changed it, or the rules it breaks.
fn parse_entry(index: usize, value: &Value) -> std::result::Result<(Entry, bool), Vec<Invalid>> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .and_then(|id| Uuid::parse_str(id).ok())
        .filter(|id| id.get_version_num() == 4 && id.get_variant() == uuid::Variant::RFC4122);
    let fail = |rule: &str| vec![invalid(index, id, rule)];
    let Some(id) = id else {
        return Err(fail("id must be a version 4 UUID"));
    };
    let mut problems = Vec::new();
    let mut field = |name: &str| -> String {
        text(value, name).map(str::to_owned).unwrap_or_else(|| {
            problems.push(invalid(index, Some(id), format!("{name} must be a string")));
            String::new()
        })
    };
    let (memory_type, category, title, content, context, reason) = (
        field("type"),
        field("category"),
        field("title"),
        field("content"),
        field("context"),
        field("reason"),
    );
    let (scope, project, source, status, evidence) = (
        field("scope"),
        field("project"),
        field("source"),
        field("status"),
        field("evidence"),
    );
    let (created_at, updated_at) = (field("created_at"), field("updated_at"));
    if !problems.is_empty() {
        return Err(problems);
    }
    if !matches!(scope.as_str(), "global" | "project") {
        return Err(fail("scope must be global or project"));
    }
    let allowed: &[&str] = match source.as_str() {
        "explicit" => &["confirmed", "superseded"],
        "observed" => &["tentative", "superseded"],
        "inferred" => &["tentative", "stale", "superseded"],
        _ => return Err(fail("source must be explicit, observed or inferred")),
    };
    if !allowed.contains(&status.as_str()) {
        return Err(fail("status is not allowed for this source"));
    }
    let evidence_ids: Vec<Uuid> = match value.get("evidence_ids") {
        None | Some(Value::Null) => Vec::new(),
        Some(ids) => match serde_json::from_value(ids.clone()) {
            Ok(ids) => ids,
            Err(_) => return Err(fail("evidence_ids must be UUIDs")),
        },
    };
    let superseded_by: Option<Uuid> = match value.get("superseded_by") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => match Uuid::parse_str(s) {
            Ok(target) => Some(target),
            Err(_) => return Err(fail("superseded_by must be a UUID")),
        },
        Some(_) => return Err(fail("superseded_by must be a UUID")),
    };
    if (status == "superseded") != superseded_by.is_some() {
        return Err(fail(
            "superseded_by is set exactly when status is superseded",
        ));
    }
    let mut record: Record = match serde_json::from_value(json!({
        "type": memory_type, "category": category, "title": title, "content": content,
        "context": context, "reason": reason, "scope": scope, "project": project,
        "source": source, "evidence": evidence, "evidence_ids": evidence_ids,
        "user_confirmed": true,
    })) {
        Ok(record) => record,
        Err(_) => return Err(fail("not a valid memory")),
    };
    if let Some(reason) = normalize(&mut record) {
        return Err(fail(&reason));
    }
    let mut timestamps_changed = false;
    let mut stamp = |value: &str| -> Option<String> {
        let stored = timestamp(value)?;
        timestamps_changed |= stored != value;
        Some(stored)
    };
    let (Some(created_at), Some(updated_at)) = (stamp(&created_at), stamp(&updated_at)) else {
        return Err(fail(
            "timestamps must be YYYY-MM-DD HH:MM:SS or ISO 8601 UTC",
        ));
    };
    let last_confirmed_at = match value.get("last_confirmed_at") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => match stamp(s) {
            Some(stored) => Some(stored),
            None => {
                return Err(fail(
                    "timestamps must be YYYY-MM-DD HH:MM:SS or ISO 8601 UTC",
                ));
            }
        },
        Some(_) => return Err(fail("last_confirmed_at must be a timestamp")),
    };
    let mut origins: BTreeMap<String, String> = BTreeMap::new();
    for origin in value
        .get("origins")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let (Some(name), Some(first_seen)) = (
            origin.get("origin").and_then(Value::as_str),
            origin.get("first_seen").and_then(Value::as_str),
        ) else {
            return Err(fail("origins must list origin and first_seen"));
        };
        let name = super::clean(name, usize::MAX);
        if name.is_empty() || name.chars().count() > ORIGIN_LIMIT {
            return Err(fail("origins must be 1 to 240 characters"));
        }
        let Some(first_seen) = stamp(first_seen) else {
            return Err(fail(
                "timestamps must be YYYY-MM-DD HH:MM:SS or ISO 8601 UTC",
            ));
        };
        let earliest = origins.entry(name).or_insert_with(|| first_seen.clone());
        if first_seen < *earliest {
            *earliest = first_seen;
        }
    }
    let entry = Entry {
        id,
        memory_type: record.memory_type,
        category: record.category,
        title: record.title,
        content: record.content,
        context: record.context,
        reason: record.reason,
        scope: record.scope.name().to_owned(),
        project: record.project,
        source: source.clone(),
        status: status.clone(),
        evidence: record.evidence,
        evidence_ids: record.evidence_ids,
        origins: origins
            .into_iter()
            .map(|(origin, first_seen)| Origin { origin, first_seen })
            .collect(),
        superseded_by,
        created_at,
        updated_at,
        last_confirmed_at,
    };
    let changed = timestamps_changed
        || entry.title != text(value, "title").unwrap_or_default()
        || entry.memory_type != text(value, "type").unwrap_or_default()
        || entry.category != text(value, "category").unwrap_or_default()
        || entry.content != text(value, "content").unwrap_or_default()
        || entry.context != text(value, "context").unwrap_or_default()
        || entry.reason != text(value, "reason").unwrap_or_default()
        || entry.project != text(value, "project").unwrap_or_default()
        || entry.evidence != text(value, "evidence").unwrap_or_default()
        || entry.evidence_ids != evidence_ids;
    Ok((entry, changed))
}

// ---------------------------------------------------------------- import

/// How a refusal for missing titles lists the untitled memories.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Untitled {
    /// With each memory's content, so the user can title them.
    WithContent,
    /// By id only: an agent's import shows it summaries, never the file's
    /// memories.
    IdsOnly,
}

/// The memories of a file from before titles existed, each given a title,
/// and the ids whose map entry went unused. A memory the store already holds
/// keeps the store's title, so only the others need one from `titles`;
/// without them, a refusal lists what is missing. Memories are matched by
/// the id written in the file.
fn with_titles(
    values: &[Value],
    titles: Option<&Titles>,
    listing: Untitled,
    store: &HashMap<Uuid, Entry>,
) -> Result<(Vec<Value>, Vec<String>)> {
    let field = |value: &Value, name| {
        value
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let stored = |id: &str| Uuid::parse_str(id).ok().and_then(|id| store.get(&id));
    let untitled: Vec<(String, String)> = values
        .iter()
        .map(|value| (field(value, "id"), value))
        .filter(|(id, _)| stored(id).is_none())
        .map(|(id, value)| {
            let content = match listing {
                Untitled::WithContent => clean(&field(value, "content"), usize::MAX),
                Untitled::IdsOnly => String::new(),
            };
            (id, content)
        })
        .collect();
    let (ignored, needed): (Titles, Titles) = titles
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .partition(|(id, _)| stored(id).is_some());
    let retry = match listing {
        Untitled::WithContent => "`import FILE --titles FILE` (or the import tool's `titles`)",
        // The agent sees no content to title; the user does, on the command line.
        Untitled::IdsOnly => {
            "the import tool's `titles`, once the user has run `user-memory import FILE` to see each memory's content and approved a title for each,"
        }
    };
    let assigned = titles::assign(
        &untitled,
        titles.map(|_| &needed),
        "Importing an export from before titles (format version 1)",
        retry,
    )?;
    let titled = values
        .iter()
        .map(|value| {
            let id = field(value, "id");
            let title = stored(&id)
                .map(|entry| entry.title.clone())
                .or_else(|| assigned.get(&id).cloned());
            let mut value = value.clone();
            if let (Some(memory), Some(title)) = (value.as_object_mut(), title) {
                memory.insert("title".into(), title.into());
            }
            value
        })
        .collect();
    // In the file's order, as the user reads it.
    let ignored = values
        .iter()
        .map(|value| field(value, "id"))
        .filter(|id| ignored.contains_key(id))
        .collect();
    Ok((titled, ignored))
}

/// What happens to one file memory.
enum Plan {
    Add,
    Unchanged,
    Conflict,
}

/// Validates `document`, merges it against the store and, when `apply` is
/// set and nothing blocks it, writes it in one transaction. Without `apply`
/// the result is a preview and nothing is written. `apply` counts as the
/// user's confirmation of every explicit memory in the file. A file from
/// before titles existed takes each memory's title from `titles`; without a
/// title for each, the refusal lists the untitled memories with their content.
/// Only the memories `subset` covers are imported; the rest of the file is
/// skipped.
pub fn import_document(
    db: &Path,
    document: &str,
    apply: bool,
    on_conflict: OnConflict,
    titles: Option<&Titles>,
    subset: &Subset,
) -> Result<Value> {
    import(
        db,
        document,
        apply,
        on_conflict,
        titles,
        subset,
        Untitled::WithContent,
    )
}

/// The ids of `rows` that `filter` covers, by the store's own query run
/// over them.
fn covered(rows: &[Entry], filter: &Filter) -> Result<HashSet<Uuid>> {
    let conn = scratch()?;
    for entry in rows {
        insert(&conn, entry)?;
        merge_origins(&conn, entry)?;
    }
    Ok(candidates(&conn, filter, None)?
        .into_iter()
        .map(|m| m.id)
        .collect())
}

fn import(
    db: &Path,
    document: &str,
    apply: bool,
    on_conflict: OnConflict,
    titles: Option<&Titles>,
    subset: &Subset,
    listing: Untitled,
) -> Result<Value> {
    let filter = subset.filter()?;
    if document.len() as u64 > MAX_BYTES {
        bail!("Export files are limited to 64 MiB");
    }
    let file: Value = serde_json::from_str(document).map_err(|err| {
        anyhow!(
            "Not a user-memory export: invalid JSON at line {}, column {}",
            err.line(),
            err.column()
        )
    })?;
    if file.get("format").and_then(Value::as_str) != Some(FORMAT) {
        bail!("Not a user-memory export");
    }
    let version = file
        .get("format_version")
        .and_then(Value::as_u64)
        .context("Not a user-memory export: unknown format version")?;
    match version.cmp(&FORMAT_VERSION) {
        Ordering::Equal => {}
        Ordering::Greater => {
            bail!("This export needs a newer user-memory (format version {version})")
        }
        Ordering::Less if version == TITLELESS_VERSION => {}
        Ordering::Less => bail!("Not a user-memory export: unknown format version"),
    }
    let Some(values) = file.get("memories").and_then(Value::as_array) else {
        bail!("Not a user-memory export: memories must be a list");
    };
    let conn = connect(db)?;
    let store: HashMap<Uuid, Entry> = entries(&conn)?.into_iter().map(|e| (e.id, e)).collect();
    let (titled, titles_ignored);
    let values = if version == TITLELESS_VERSION {
        (titled, titles_ignored) = with_titles(values, titles, listing, &store)?;
        &titled
    } else {
        titles_ignored = Vec::new();
        values
    };

    // Phase 1: validate everything.
    let mut problems = Vec::new();
    let mut rows: Vec<Entry> = Vec::new();
    let mut normalized = HashSet::new();
    let mut seen = HashSet::new();
    for (index, value) in values.iter().enumerate() {
        match parse_entry(index, value) {
            Ok((entry, changed)) => {
                if !seen.insert(entry.id) {
                    problems.push(invalid(index, Some(entry.id), "id appears more than once"));
                    continue;
                }
                if changed {
                    normalized.insert(entry.id);
                }
                rows.push(entry);
            }
            Err(found) => problems.extend(found),
        }
    }
    // The filter selects among the rows; the others are skipped. Only a
    // filter, this one or the export's, can leave an active inference
    // without its supports, which then is skipped too.
    let file_selection = file.get("selection").cloned();
    let selected = match &filter {
        Some(filter) => {
            let covered = covered(&rows, filter)?;
            let total = rows.len();
            rows.retain(|e| covered.contains(&e.id));
            Some((rows.len(), total - rows.len()))
        }
        None => None,
    };
    let mut unsupported = 0;
    if filter.is_some() || file_selection.is_some() {
        let ids: HashSet<Uuid> = rows.iter().map(|e| e.id).collect();
        rows.retain(|e| {
            let missing = e.status == "tentative"
                && e.evidence_ids
                    .iter()
                    .any(|id| !ids.contains(id) && !store.contains_key(id));
            unsupported += usize::from(missing);
            !missing
        });
    }
    let in_file: HashMap<Uuid, &Entry> = rows.iter().map(|e| (e.id, e)).collect();
    let resolve =
        |id: &Uuid| -> Option<&Entry> { in_file.get(id).copied().or_else(|| store.get(id)) };
    let file_ids: HashSet<Uuid> = in_file.keys().copied().collect();
    let mut replacements_missing = 0;
    for (index, entry) in rows.iter().enumerate() {
        // Supports in the store: the shared structural rules. Stale and
        // superseded inferences may cite supports forgotten since.
        let missing = if entry.status == "tentative" {
            Missing::Refuse
        } else {
            Missing::Allow
        };
        if let Some(reason) = check_support(&conn, &entry.record(), &file_ids, missing)? {
            problems.push(invalid(index, Some(entry.id), reason));
        }
        if let Some(target) = entry.superseded_by {
            match resolve(&target) {
                Some(t) if t.scope == entry.scope && t.project == entry.project => {}
                Some(_) => problems.push(invalid(
                    index,
                    Some(entry.id),
                    "superseded_by must name a memory in the same scope and project",
                )),
                None => replacements_missing += 1,
            }
        }
        for support in &entry.evidence_ids {
            let Some(found) = in_file.get(support) else {
                continue;
            };
            if found.source == "inferred" {
                problems.push(invalid(
                    index,
                    Some(entry.id),
                    "Evidence must be active explicit or observed memory, not another inference",
                ));
            } else if found.scope == "project"
                && (entry.scope != "project" || entry.project != found.project)
            {
                problems.push(invalid(
                    index,
                    Some(entry.id),
                    "Inferences cannot broaden or cross the scope of their evidence",
                ));
            }
        }
    }
    let mut pairs = BTreeSet::new();
    let mut dropped_pairs = 0;
    for pair in file
        .get("distinct_pairs")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let ids: Option<Vec<Uuid>> = pair.as_array().and_then(|p| {
            (p.len() == 2)
                .then(|| {
                    p.iter()
                        .map(|id| id.as_str().and_then(|s| Uuid::parse_str(s).ok()))
                        .collect()
                })
                .flatten()
        });
        match ids {
            Some(ids) if ids[0] != ids[1] && ids.iter().all(|id| resolve(id).is_some()) => {
                pairs.insert((ids[0].min(ids[1]), ids[0].max(ids[1])));
            }
            _ => dropped_pairs += 1,
        }
    }

    // Phase 2: merge by id against the pre-import store.
    let mut plans = Vec::new();
    let mut conflicts = Vec::new();
    let mut unchanged = 0;
    let mut origins_merged = 0;
    for (index, entry) in rows.iter().enumerate() {
        let plan = match store.get(&entry.id) {
            None => Plan::Add,
            Some(old) => {
                let fields = entry.differences(old);
                if fields.is_empty() {
                    unchanged += 1;
                    Plan::Unchanged
                } else if old.status == "superseded" && entry.status != "superseded" {
                    problems.push(invalid(
                        index,
                        Some(entry.id),
                        "superseded in the store; supersede or forget instead",
                    ));
                    Plan::Unchanged
                } else {
                    conflicts.push(json!({"id": entry.id, "fields": fields}));
                    Plan::Conflict
                }
            }
        };
        if let Some(old) = store.get(&entry.id) {
            let known: HashSet<&str> = old.origins.iter().map(|o| o.origin.as_str()).collect();
            if !matches!(plan, Plan::Conflict if on_conflict == OnConflict::UseFile)
                && entry
                    .origins
                    .iter()
                    .any(|o| !known.contains(o.origin.as_str()))
            {
                origins_merged += 1;
            }
        }
        plans.push(plan);
    }
    let replaced: Vec<Uuid> = if on_conflict == OnConflict::UseFile {
        rows.iter()
            .zip(&plans)
            .filter(|(_, plan)| matches!(plan, Plan::Conflict))
            .map(|(entry, _)| entry.id)
            .collect()
    } else {
        Vec::new()
    };
    let will_stale = if replaced.is_empty() {
        Vec::new()
    } else {
        // The triggers decide what goes stale: try the replacements, read
        // what they staled, and roll them back. The write lock is taken at
        // the start, waiting for other writers, and the statuses compared
        // are read under it, so a concurrent write is neither an error nor
        // counted as this import's doing.
        let trial = write(&conn)?;
        let before = statuses(&trial)?;
        for entry in rows.iter().filter(|e| replaced.contains(&e.id)) {
            replace(&trial, entry)?;
        }
        let staled = newly_stale(&trial, &before)?;
        trial.rollback()?;
        staled
    };
    let similar_to_existing = rows
        .iter()
        .zip(&plans)
        .filter(|(entry, plan)| matches!(plan, Plan::Add) && entry.active())
        .filter(|(entry, _)| {
            let pool = if entry.scope == "project" {
                Pool::Project(&entry.project)
            } else {
                Pool::Global
            };
            similar(&conn, &entry.draft(), None, pool)
                .map(|found| found.iter().any(|c| !in_file.contains_key(&c.id)))
                .unwrap_or(false)
        })
        .count();
    let added = status_counts(
        rows.iter()
            .zip(&plans)
            .filter(|(_, plan)| matches!(plan, Plan::Add))
            .map(|(entry, _)| entry.status.as_str()),
    );
    let blocked =
        !problems.is_empty() || (!conflicts.is_empty() && on_conflict == OnConflict::Abort);
    let mut summary = Map::new();
    summary.insert("applied".into(), json!(false));
    summary.insert("added".into(), added);
    summary.insert("unchanged".into(), json!(unchanged));
    summary.insert("origins_merged".into(), json!(origins_merged));
    // Only the rows being imported, in the file's order.
    let normalized: Vec<String> = rows
        .iter()
        .filter(|e| normalized.contains(&e.id))
        .map(|e| e.id.to_string())
        .collect();
    summary.insert("normalized".into(), json!(normalized));
    summary.insert("conflicts".into(), json!(conflicts));
    summary.insert("invalid".into(), json!(problems));
    summary.insert("will_stale".into(), json!(will_stale));
    summary.insert("dropped_distinct_pairs".into(), json!(dropped_pairs));
    summary.insert("similar_to_existing".into(), json!(similar_to_existing));
    if replacements_missing > 0 {
        summary.insert("replacements_missing".into(), json!(replacements_missing));
    }
    if let Some(selection) = &file_selection {
        summary.insert("file_selection".into(), selection.clone());
    }
    if let Some((count, not_selected)) = selected {
        summary.insert("selection".into(), json!(subset));
        summary.insert("selected".into(), json!(count));
        summary.insert(
            "skipped".into(),
            json!({"not_selected": not_selected, "unsupported_inferences": unsupported}),
        );
    } else if file_selection.is_some() {
        summary.insert(
            "skipped".into(),
            json!({"unsupported_inferences": unsupported}),
        );
    }
    if selected.is_some_and(|(count, _)| count == 0) {
        summary.insert(
            "note".into(),
            json!("The filter selected no memories; nothing was imported"),
        );
        return Ok(Value::Object(summary));
    }
    if !titles_ignored.is_empty() {
        summary.insert(
            "titles_ignored".into(),
            json!({
                "ids": titles_ignored,
                "note": "These memories are already in the store, which keeps its titles; their map entries were not used.",
            }),
        );
    }
    summary.insert(
        "next".into(),
        json!("run review to consolidate similar memories; run reindex to rebuild embeddings"),
    );
    if !apply || blocked {
        return Ok(Value::Object(summary));
    }

    // Phase 3: write.
    let tx = write(&conn)?;
    let before = statuses(&tx)?;
    for (entry, plan) in rows.iter().zip(&plans) {
        if matches!(plan, Plan::Conflict) && on_conflict == OnConflict::UseFile {
            replace(&tx, entry)?;
        }
    }
    for (entry, plan) in rows.iter().zip(&plans) {
        if matches!(plan, Plan::Add) {
            insert(&tx, entry)?;
        }
    }
    for (entry, plan) in rows.iter().zip(&plans) {
        let replaced = matches!(plan, Plan::Conflict) && on_conflict == OnConflict::UseFile;
        if !replaced {
            merge_origins(&tx, entry)?;
        }
    }
    for (a, b) in &pairs {
        tx.execute(
            "INSERT OR IGNORE INTO memory_distinct (a,b) VALUES (?1,?2)",
            params![a.to_string(), b.to_string()],
        )?;
    }
    let staled = newly_stale(&tx, &before)?;
    tx.commit()?;
    summary.insert("applied".into(), json!(true));
    summary.remove("will_stale");
    summary.insert("staled".into(), json!(staled));
    Ok(Value::Object(summary))
}

/// Each stored memory's status.
fn statuses(conn: &Connection) -> Result<HashMap<Uuid, String>> {
    Ok(entries(conn)?
        .into_iter()
        .map(|e| (e.id, e.status))
        .collect())
}

/// Memories stale now that were stored and not stale `before`.
fn newly_stale(conn: &Connection, before: &HashMap<Uuid, String>) -> Result<Vec<String>> {
    Ok(entries(conn)?
        .into_iter()
        .filter(|e| e.status == "stale" && before.get(&e.id).is_some_and(|s| s != "stale"))
        .map(|e| e.id.to_string())
        .collect())
}

fn insert(conn: &Connection, e: &Entry) -> Result<()> {
    conn.execute(
        "INSERT INTO memories (id,created_at,updated_at,memory_type,category,context,content,reason,scope,project,source,status,evidence,evidence_ids,superseded_by,last_confirmed_at,title) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
        params![
            e.id.to_string(),
            e.created_at,
            e.updated_at,
            e.memory_type,
            e.category,
            e.context,
            e.content,
            e.reason,
            e.scope,
            e.project,
            e.source,
            e.status,
            e.evidence,
            serde_json::to_string(&e.evidence_ids)?,
            e.superseded_by.map(|id| id.to_string()),
            e.last_confirmed_at,
            e.title,
        ],
    )?;
    Ok(())
}

/// The store row takes the file's fields, origins and timestamps.
fn replace(conn: &Connection, e: &Entry) -> Result<()> {
    conn.execute(
        "UPDATE memories SET memory_type=?2,category=?3,context=?4,content=?5,reason=?6,scope=?7,project=?8,source=?9,status=?10,evidence=?11,evidence_ids=?12,superseded_by=?13,created_at=?14,updated_at=?15,last_confirmed_at=?16,title=?17 WHERE id=?1",
        params![
            e.id.to_string(),
            e.memory_type,
            e.category,
            e.context,
            e.content,
            e.reason,
            e.scope,
            e.project,
            e.source,
            e.status,
            e.evidence,
            serde_json::to_string(&e.evidence_ids)?,
            e.superseded_by.map(|id| id.to_string()),
            e.created_at,
            e.updated_at,
            e.last_confirmed_at,
            e.title,
        ],
    )?;
    // The file's conversations go in before the others come out: removing
    // first could leave a pattern built on this memory, for a moment, with
    // fewer than two conversations, and the trigger would stale it.
    for origin in &e.origins {
        conn.execute(
            "INSERT INTO memory_origins (memory_id,origin,first_seen) VALUES (?1,?2,?3) ON CONFLICT(memory_id,origin) DO UPDATE SET first_seen=excluded.first_seen",
            params![e.id.to_string(), origin.origin, origin.first_seen],
        )?;
    }
    let keep = serde_json::to_string(&e.origins.iter().map(|o| &o.origin).collect::<Vec<_>>())?;
    conn.execute(
        "DELETE FROM memory_origins WHERE memory_id=?1 AND origin NOT IN (SELECT value FROM json_each(?2))",
        params![e.id.to_string(), keep],
    )?;
    Ok(())
}

/// Adds the file's origins (the earliest first_seen wins) and keeps the later
/// last_confirmed_at. Adding origins never stales anything.
fn merge_origins(conn: &Connection, e: &Entry) -> Result<()> {
    for origin in &e.origins {
        conn.execute(
            "INSERT INTO memory_origins (memory_id,origin,first_seen) VALUES (?1,?2,?3) ON CONFLICT(memory_id,origin) DO UPDATE SET first_seen=min(first_seen,excluded.first_seen)",
            params![e.id.to_string(), origin.origin, origin.first_seen],
        )?;
    }
    if let Some(confirmed) = &e.last_confirmed_at {
        conn.execute(
            "UPDATE memories SET last_confirmed_at=?2 WHERE id=?1 AND (last_confirmed_at IS NULL OR last_confirmed_at<?2)",
            params![e.id.to_string(), confirmed],
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------- exports directory

/// The only directory agents can export to or import from: `exports` beside
/// the store.
pub fn exports_dir(db: &Path) -> PathBuf {
    directory_of(db).join("exports")
}

/// A file name in the exports directory: `[A-Za-z0-9._-]+.json`, no leading
/// dot, at most 255 bytes. Never a path.
fn valid_name(name: &str) -> bool {
    name.len() <= NAME_LIMIT
        && name.ends_with(".json")
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn checked_name(name: &str) -> Result<&str> {
    if valid_name(name) {
        Ok(name)
    } else {
        bail!(
            "Export names are file names like backup.json: letters, digits, '.', '_' and '-', ending in .json"
        )
    }
}

/// The exports directory, created private when missing. An existing one
/// must be a real directory (not a symlink) and private: on Unix mode 0700
/// and owned by the store's owner. Anything else is refused, never
/// repaired.
fn exports(db: &Path, create: bool) -> Result<PathBuf> {
    let dir = exports_dir(db);
    match fs::symlink_metadata(&dir) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && create => {
            private::create_dir(&dir).context("Cannot create the exports directory")?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            bail!("No exports yet: the exports directory does not exist")
        }
        Err(_) => bail!("Cannot read the exports directory"),
        Ok(metadata) => {
            if !metadata.file_type().is_dir() {
                bail!("The exports directory is not a plain directory; refusing to use it");
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                let owner = fs::metadata(directory_of(db))
                    .map(|m| m.uid())
                    .context("Cannot read the store directory")?;
                if metadata.permissions().mode() & 0o777 != 0o700 || metadata.uid() != owner {
                    bail!(
                        "The exports directory must be private (mode 0700, owned by the store's owner)"
                    );
                }
            }
            #[cfg(windows)]
            if !private::is_private(&dir).context("Cannot read the exports directory")? {
                bail!("The exports directory must be private (accessible only to this user)");
            }
        }
    }
    Ok(dir)
}

/// Exports the store to `name` in the exports directory.
pub fn export_named(db: &Path, input: &ExportMemories) -> Result<Value> {
    let name = checked_name(&input.name)?;
    let dir = exports(db, true)?;
    if !input.overwrite && fs::symlink_metadata(dir.join(name)).is_ok() {
        bail!("An export named {name} already exists; set overwrite to replace it");
    }
    let (document, mut summary) = export_document(db, &input.subset)?;
    write_atomically(&dir, name, &document)
        .map_err(|_| anyhow!("Cannot write the export {name}"))?;
    summary["exported"] = json!(true);
    summary["file"] = json!(name);
    Ok(summary)
}

/// Previews, or with `apply` and `user_confirmed` applies, the import of
/// `name` from the exports directory.
pub fn import_named(db: &Path, input: &ImportMemories) -> Result<Value> {
    let name = checked_name(&input.name)?;
    let dir = exports(db, false)?;
    let document = read_regular(&dir.join(name)).map_err(|err| anyhow!("{name}: {err}"))?;
    import(
        db,
        &document,
        input.apply && input.user_confirmed,
        input.on_conflict,
        input.titles.as_ref(),
        &input.subset,
        Untitled::IdsOnly,
    )
}

/// Reads a regular file of at most `MAX_BYTES`, refusing symlinks, FIFOs and
/// devices, and a file swapped between the check and the open.
fn read_regular(path: &Path) -> Result<String> {
    let checked = fs::symlink_metadata(path).map_err(|_| anyhow!("no such export"))?;
    if !checked.file_type().is_file() {
        bail!("not a regular file");
    }
    let mut file = fs::File::open(path).map_err(|_| anyhow!("cannot open the export"))?;
    let opened = file
        .metadata()
        .map_err(|_| anyhow!("cannot open the export"))?;
    if !same_file(&checked, &opened) {
        bail!("the export changed while it was being opened");
    }
    // Reads at most one byte past the cap, so a file that grew since the
    // check is refused without reading it whole.
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow!("cannot read the export"))?;
    if bytes.len() as u64 > MAX_BYTES {
        bail!("export files are limited to 64 MiB");
    }
    String::from_utf8(bytes).map_err(|_| anyhow!("not UTF-8 text"))
}

/// Whether `opened` is the regular file `checked` described (same device
/// and inode on unix).
fn same_file(checked: &fs::Metadata, opened: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if checked.dev() != opened.dev() || checked.ino() != opened.ino() {
            return false;
        }
    }
    #[cfg(not(unix))]
    let _ = checked;
    opened.file_type().is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Store {
        _dir: tempfile::TempDir,
        db: PathBuf,
        conn: Connection,
    }

    fn store() -> Store {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memory.sqlite3");
        let conn = connect(&db).unwrap();
        Store {
            _dir: dir,
            db,
            conn,
        }
    }

    const NOW: &str = "2026-10-15 05:20:11";

    /// Inserts a memory row; `extra` overrides columns as SQL assignments.
    fn insert_row(conn: &Connection, content: &str, extra: &[(&str, &str)]) -> Uuid {
        let id = Uuid::new_v4();
        conn.execute(
            "INSERT INTO memories (id,memory_type,category,content,title,created_at,updated_at) VALUES (?1,'preference','tools',?2,?2,?3,?3)",
            params![id.to_string(), content, NOW],
        )
        .unwrap();
        for (column, value) in extra {
            conn.execute(
                &format!("UPDATE memories SET {column}=?2 WHERE id=?1"),
                params![id.to_string(), value],
            )
            .unwrap();
        }
        id
    }

    fn origin(conn: &Connection, id: Uuid, origin: &str, first_seen: &str) {
        conn.execute(
            "INSERT INTO memory_origins (memory_id,origin,first_seen) VALUES (?1,?2,?3)",
            params![id.to_string(), origin, first_seen],
        )
        .unwrap();
    }

    /// A store with every kind of row an export must carry.
    fn populated() -> (Store, [Uuid; 7]) {
        let s = store();
        let c = &s.conn;
        let a = insert_row(c, "Use Helix as the text editor", &[]);
        origin(c, a, "codex:thread/two", "2026-10-15 06:00:00");
        origin(c, a, "claude:session/one", "2026-10-15 05:20:11");
        c.execute(
            "UPDATE memories SET last_confirmed_at='2026-10-15 06:00:00',updated_at='2026-10-15 06:00:00' WHERE id=?1",
            [a.to_string()],
        )
        .unwrap();
        let b = insert_row(
            c,
            "Asked for a demo",
            &[
                ("memory_type", "behavior"),
                ("source", "observed"),
                ("status", "tentative"),
                ("evidence", "Asked twice"),
            ],
        );
        // Same second as `a`: file order, not the timestamp, keeps them apart.
        let gone = insert_row(c, "Prefer examples", &[]);
        let inferred = insert_row(
            c,
            "Learns by example",
            &[
                ("memory_type", "pattern"),
                ("source", "inferred"),
                ("status", "tentative"),
            ],
        );
        c.execute(
            "UPDATE memories SET evidence_ids=?2 WHERE id=?1",
            params![
                inferred.to_string(),
                serde_json::to_string(&[a.min(gone), a.max(gone)]).unwrap()
            ],
        )
        .unwrap();
        c.execute("DELETE FROM memories WHERE id=?1", [gone.to_string()])
            .unwrap();
        let old = insert_row(c, "Indent with tabs", &[]);
        let new = insert_row(c, "Indent with 4 spaces", &[]);
        c.execute(
            "UPDATE memories SET status='superseded',superseded_by=?2 WHERE id=?1",
            [old.to_string(), new.to_string()],
        )
        .unwrap();
        let local = insert_row(
            c,
            "Use Go here",
            &[("scope", "project"), ("project", "github.com/acme/svc")],
        );
        c.execute(
            "INSERT INTO memory_distinct (a,b) VALUES (?1,?2)",
            [new.min(local).to_string(), new.max(local).to_string()],
        )
        .unwrap();
        (s, [a, b, gone, inferred, old, new, local])
    }

    fn without_time(document: &str) -> Value {
        let mut value: Value = serde_json::from_str(document).unwrap();
        value.as_object_mut().unwrap().remove("exported_at");
        value
    }

    fn import(s: &Store, document: &str, apply: bool, mode: OnConflict) -> Value {
        import_document(&s.db, document, apply, mode, None, &Subset::default()).unwrap()
    }

    #[test]
    fn a_round_trip_through_an_empty_store_reproduces_the_export() {
        let (s, [a, b, _, inferred, old, new, local]) = populated();
        let (first, summary) = export_document(&s.db, &Subset::default()).unwrap();
        let value: Value = serde_json::from_str(&first).unwrap();
        let order: Vec<&str> = value["memories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        let expected = [a, b, inferred, old, new, local].map(|id| id.to_string());
        assert_eq!(order, expected, "memories are exported in insertion order");
        assert_eq!(
            value["memories"][0]["origins"],
            json!([
                {"origin":"claude:session/one","first_seen":"2026-10-15 05:20:11"},
                {"origin":"codex:thread/two","first_seen":"2026-10-15 06:00:00"}
            ])
        );
        let at = value["exported_at"].as_str().unwrap();
        assert!(
            at.len() == 20 && at.as_bytes()[10] == b'T' && at.ends_with('Z'),
            "{at}"
        );
        assert_eq!(
            summary,
            json!({"memories":{"confirmed":3,"stale":1,"superseded":1,"tentative":1},"distinct_pairs":1})
        );
        // The deleted support staled its inference, which keeps the dangling id.
        assert!(first.contains(&inferred.to_string()));
        let fresh = store();
        let result = import(&fresh, &first, true, OnConflict::Abort);
        assert_eq!(result["applied"], true, "{result}");
        assert_eq!(result["invalid"], json!([]));
        let (second, _) = export_document(&fresh.db, &Subset::default()).unwrap();
        assert_eq!(without_time(&first), without_time(&second));
        assert!(first.ends_with("}\n") && first.starts_with("{\n  \"format\""));
    }

    #[test]
    fn a_preview_writes_nothing() {
        let (s, _) = populated();
        let (document, _) = export_document(&s.db, &Subset::default()).unwrap();
        let fresh = store();
        drop(fresh.conn);
        let before = fs::read(&fresh.db).unwrap();
        let result = import_document(
            &fresh.db,
            &document,
            false,
            OnConflict::Abort,
            None,
            &Subset::default(),
        )
        .unwrap();
        assert_eq!(result["applied"], false);
        assert_eq!(result["added"]["confirmed"], 3);
        assert_eq!(fs::read(&fresh.db).unwrap(), before);
    }

    /// The export with memory `id`'s fields changed by `edit`.
    fn edited(document: &str, id: Uuid, edit: impl Fn(&mut Value)) -> String {
        let mut value: Value = serde_json::from_str(document).unwrap();
        for m in value["memories"].as_array_mut().unwrap() {
            if m["id"] == id.to_string() {
                edit(m);
            }
        }
        serde_json::to_string(&value).unwrap()
    }

    fn content(s: &Store, id: Uuid) -> (String, String, Vec<String>) {
        s.conn
            .query_row(&format!("{SELECT} WHERE id=?1"), [id.to_string()], row)
            .map(|m| (m.content, m.status, m.origins))
            .unwrap()
    }

    fn first_seen(s: &Store, id: Uuid) -> Vec<(String, String)> {
        s.conn
            .prepare(
                "SELECT origin,first_seen FROM memory_origins WHERE memory_id=?1 ORDER BY origin",
            )
            .unwrap()
            .query_map([id.to_string()], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn column(s: &Store, id: Uuid, name: &str) -> Option<String> {
        s.conn
            .query_row(
                &format!("SELECT {name} FROM memories WHERE id=?1"),
                [id.to_string()],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn identical_memories_gain_the_files_origins_and_later_confirmation() {
        let (s, [a, ..]) = populated();
        let (document, _) = export_document(&s.db, &Subset::default()).unwrap();
        let document = edited(&document, a, |m| {
            m["origins"]
                .as_array_mut()
                .unwrap()
                .push(json!({"origin":"claude:session/three","first_seen":"2026-10-16 01:00:00"}));
            m["last_confirmed_at"] = json!("2026-10-16 01:00:00");
        });
        let result = import(&s, &document, true, OnConflict::Abort);
        assert_eq!(result["unchanged"], 6, "{result}");
        assert_eq!(result["origins_merged"], 1);
        assert_eq!(content(&s, a).2.len(), 3);
        let confirmed: String = s
            .conn
            .query_row(
                "SELECT last_confirmed_at FROM memories WHERE id=?1",
                [a.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(confirmed, "2026-10-16 01:00:00");
        let (document, _) = export_document(&s.db, &Subset::default()).unwrap();
        let earlier = edited(&document, a, |m| {
            m["last_confirmed_at"] = json!("2026-10-15 00:00:00")
        });
        let b = populated_id(&document, 1);
        let earlier = edited(&earlier, b, |m| {
            m["last_confirmed_at"] = json!("2026-10-15 00:00:00")
        });
        import(&s, &earlier, true, OnConflict::Abort);
        assert_eq!(
            column(&s, a, "last_confirmed_at").unwrap(),
            "2026-10-16 01:00:00"
        );
        assert_eq!(
            column(&s, b, "last_confirmed_at").unwrap(),
            "2026-10-15 00:00:00"
        );
    }

    /// The id of the `index`th memory in `document`.
    fn populated_id(document: &str, index: usize) -> Uuid {
        let value: Value = serde_json::from_str(document).unwrap();
        value["memories"][index]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }

    #[test]
    fn conflicts_abort_keep_the_store_or_take_the_file() {
        let (s, [a, ..]) = populated();
        let (document, _) = export_document(&s.db, &Subset::default()).unwrap();
        let document = edited(&document, a, |m| {
            m["content"] = json!("Use Kakoune as the text editor");
            m["updated_at"] = json!("2026-10-16 01:00:00");
            m["origins"] = json!([
                {"origin":"claude:session/new","first_seen":"2026-10-16 01:00:00"},
                {"origin":"claude:session/one","first_seen":"2026-10-16 02:00:00"}
            ]);
        });
        let aborted = import(&s, &document, true, OnConflict::Abort);
        assert_eq!(aborted["applied"], false);
        assert_eq!(
            aborted["conflicts"],
            json!([{"id": a, "fields": ["content"]}])
        );
        assert_eq!(content(&s, a).0, "Use Helix as the text editor");
        let kept = import(&s, &document, true, OnConflict::KeepStore);
        assert_eq!(kept["applied"], true);
        let (text, _, origins) = content(&s, a);
        assert_eq!(text, "Use Helix as the text editor");
        assert_eq!(
            origins,
            [
                "claude:session/new",
                "claude:session/one",
                "codex:thread/two"
            ]
        );
        assert_eq!(
            first_seen(&s, a)[1].1,
            "2026-10-15 05:20:11",
            "earliest wins"
        );
        assert_eq!(column(&s, a, "updated_at").unwrap(), "2026-10-15 06:00:00");
        let replaced = import(&s, &document, true, OnConflict::UseFile);
        assert_eq!(replaced["applied"], true, "{replaced}");
        let (text, _, _) = content(&s, a);
        assert_eq!(text, "Use Kakoune as the text editor");
        assert_eq!(
            first_seen(&s, a),
            [
                ("claude:session/new".into(), "2026-10-16 01:00:00".into()),
                ("claude:session/one".into(), "2026-10-16 02:00:00".into())
            ]
        );
        assert_eq!(column(&s, a, "updated_at").unwrap(), "2026-10-16 01:00:00");
        // Replaced memories take the file's origins; only the others merge.
        let (document, _) = export_document(&s.db, &Subset::default()).unwrap();
        let b = populated_id(&document, 1);
        let document = edited(&document, a, |m| m["content"] = json!("Use Vim"));
        let document = edited(&document, b, |m| {
            m["origins"] = json!([{"origin":"codex:thread/b","first_seen":NOW}])
        });
        let preview = import(&s, &document, false, OnConflict::UseFile);
        assert_eq!(preview["origins_merged"], 1, "{preview}");
    }

    #[test]
    fn a_changed_support_stales_the_inference_the_file_still_holds() {
        let s = store();
        let c = &s.conn;
        let x = insert_row(c, "Prefer examples", &[]);
        let y = insert_row(c, "Prefer demos", &[]);
        let i = insert_row(
            c,
            "Learns by example",
            &[
                ("memory_type", "pattern"),
                ("source", "inferred"),
                ("status", "tentative"),
            ],
        );
        c.execute(
            "UPDATE memories SET evidence_ids=?2 WHERE id=?1",
            params![
                i.to_string(),
                serde_json::to_string(&[x.min(y), x.max(y)]).unwrap()
            ],
        )
        .unwrap();
        let w = insert_row(c, "Prefer screenshots", &[]);
        let unrelated = insert_row(
            c,
            "Learns visually",
            &[
                ("memory_type", "pattern"),
                ("source", "inferred"),
                ("status", "tentative"),
            ],
        );
        c.execute(
            "UPDATE memories SET evidence_ids=?2 WHERE id=?1",
            params![
                unrelated.to_string(),
                serde_json::to_string(&[y.min(w), y.max(w)]).unwrap()
            ],
        )
        .unwrap();
        let old = insert_row(
            c,
            "Learned from demos",
            &[
                ("memory_type", "pattern"),
                ("source", "inferred"),
                ("status", "stale"),
            ],
        );
        c.execute(
            "UPDATE memories SET evidence_ids=?2 WHERE id=?1",
            params![
                old.to_string(),
                serde_json::to_string(&[x.min(y), x.max(y)]).unwrap()
            ],
        )
        .unwrap();
        let (document, _) = export_document(&s.db, &Subset::default()).unwrap();
        let document = edited(&document, x, |m| {
            m["content"] = json!("Prefer reference material")
        });
        let preview = import(&s, &document, false, OnConflict::UseFile);
        assert_eq!(preview["will_stale"], json!([i.to_string()]), "{preview}");
        let applied = import(&s, &document, true, OnConflict::UseFile);
        assert_eq!(applied["staled"], json!([i.to_string()]), "{applied}");
        assert_eq!(content(&s, i).1, "stale");
        assert_eq!(content(&s, y).1, "confirmed");
        assert_eq!(content(&s, unrelated).1, "tentative");
    }

    #[test]
    fn a_preview_waits_for_another_writer_instead_of_failing() {
        let s = store();
        let c = &s.conn;
        let x = insert_row(c, "Prefer examples", &[]);
        let y = insert_row(c, "Prefer demos", &[]);
        let i = insert_row(
            c,
            "Learns by example",
            &[
                ("memory_type", "pattern"),
                ("source", "inferred"),
                ("status", "tentative"),
            ],
        );
        c.execute(
            "UPDATE memories SET evidence_ids=?2 WHERE id=?1",
            params![
                i.to_string(),
                serde_json::to_string(&[x.min(y), x.max(y)]).unwrap()
            ],
        )
        .unwrap();
        let (document, _) = export_document(&s.db, &Subset::default()).unwrap();
        let document = edited(&document, x, |m| {
            m["content"] = json!("Prefer reference material")
        });
        // Another writer holds the write lock for a moment, as a server
        // recording a memory would.
        let writer = Connection::open(&s.db).unwrap();
        writer
            .execute_batch("BEGIN IMMEDIATE; UPDATE memories SET reason='held' WHERE rowid=1;")
            .unwrap();
        let held = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            writer.execute_batch("COMMIT").unwrap();
        });
        let preview = import(&s, &document, false, OnConflict::UseFile);
        held.join().unwrap();
        assert_eq!(preview["will_stale"], json!([i.to_string()]), "{preview}");
        assert_eq!(content(&s, i).1, "tentative");
    }

    #[test]
    fn a_status_difference_alone_is_a_conflict_and_superseded_rows_are_never_revived() {
        let (s, [a, _, _, _, old, ..]) = populated();
        let (document, _) = export_document(&s.db, &Subset::default()).unwrap();
        let superseding = edited(&document, a, |m| {
            m["status"] = json!("superseded");
            m["superseded_by"] = json!(old.to_string());
        });
        let result = import(&s, &superseding, false, OnConflict::Abort);
        assert_eq!(
            result["conflicts"],
            json!([{"id": a, "fields": ["status", "superseded_by"]}])
        );
        let revived = edited(&document, old, |m| {
            m["status"] = json!("confirmed");
            m["superseded_by"] = Value::Null;
        });
        let result = import(&s, &revived, true, OnConflict::UseFile);
        assert_eq!(result["applied"], false);
        assert_eq!(
            result["invalid"][0]["rule"],
            "superseded in the store; supersede or forget instead"
        );
        assert_eq!(content(&s, old).1, "superseded");
        // The reverse, superseding an active memory, is an ordinary change.
        let result = import(&s, &superseding, true, OnConflict::UseFile);
        assert_eq!(result["applied"], true, "{result}");
        assert_eq!(content(&s, a).1, "superseded");
    }

    #[test]
    fn normalization_is_applied_and_reported() {
        let (s, [a, ..]) = populated();
        let (document, _) = export_document(&s.db, &Subset::default()).unwrap();
        let fresh = store();
        let document = edited(&document, a, |m| {
            m["category"] = json!("Tools");
            m["content"] = json!("  Use   Helix as the text editor ");
            m["created_at"] = json!("2026-10-15T05:20:11Z");
        });
        let result = import(&fresh, &document, true, OnConflict::Abort);
        assert_eq!(result["normalized"], json!([a.to_string()]), "{result}");
        let (exported, _) = export_document(&fresh.db, &Subset::default()).unwrap();
        assert_eq!(
            without_time(&exported),
            without_time(&export_document(&s.db, &Subset::default()).unwrap().0)
        );
    }

    #[test]
    fn invalid_files_change_nothing_and_name_index_id_and_rule() {
        let (s, [a, b, _, inferred, old, _, local]) = populated();
        let (document, _) = export_document(&s.db, &Subset::default()).unwrap();
        let fresh = store();
        drop(fresh.conn);
        let before = fs::read(&fresh.db).unwrap();
        for (bad, rule) in [
            (
                edited(&document, a, |m| m["source"] = json!("observed")),
                "status is not allowed",
            ),
            (
                edited(&document, b, |m| m["evidence"] = json!("")),
                "concrete evidence",
            ),
            (
                edited(&document, a, |m| m["created_at"] = json!("yesterday")),
                "timestamps",
            ),
            (
                edited(&document, old, |m| m["superseded_by"] = Value::Null),
                "superseded_by is set exactly",
            ),
            (
                edited(&document, old, |m| {
                    m["superseded_by"] = json!(local.to_string())
                }),
                "same scope and project",
            ),
            (
                edited(&document, inferred, |m| m["status"] = json!("tentative")),
                "does not exist",
            ),
            (
                edited(&document, local, |m| m["scope"] = json!("team")),
                "scope must be",
            ),
            (
                edited(&document, a, |m| m["id"] = json!(b.to_string())),
                "more than once",
            ),
            (
                edited(&document, a, |m| m["id"] = json!("12")),
                "version 4 UUID",
            ),
            (
                // Version 1, RFC 4122 variant.
                edited(&document, a, |m| {
                    m["id"] = json!("6ba7b810-9dad-11d1-80b4-00c04fd430c8")
                }),
                "version 4 UUID",
            ),
            (
                // Version 4, Microsoft variant.
                edited(&document, a, |m| {
                    m["id"] = json!("6ba7b810-9dad-41d1-c0b4-00c04fd430c8")
                }),
                "version 4 UUID",
            ),
            (
                edited(&document, b, |m| {
                    m["evidence_ids"] = json!([a.to_string(), local.to_string()])
                }),
                "Only inferred",
            ),
            (
                edited(&document, a, |m| m["title"] = json!(" ")),
                "title is required",
            ),
            (
                edited(&document, a, |m| m["title"] = json!("s".repeat(121))),
                "title has 121 characters; the limit is 120",
            ),
        ] {
            let result = import_document(
                &fresh.db,
                &bad,
                true,
                OnConflict::Abort,
                None,
                &Subset::default(),
            )
            .unwrap();
            assert_eq!(result["applied"], false, "{rule}");
            let rules: Vec<&str> = result["invalid"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["rule"].as_str().unwrap())
                .collect();
            assert!(rules.iter().any(|r| r.contains(rule)), "{rule}: {rules:?}");
            assert!(result["invalid"][0]["index"].is_u64(), "{rule}");
            assert_eq!(fs::read(&fresh.db).unwrap(), before, "{rule}");
        }
        for (bad, message) in [
            ("{not json", "invalid JSON"),
            (
                r#"{"format":"other","format_version":1,"memories":[]}"#,
                "Not a user-memory export",
            ),
            (
                r#"{"format":"user-memory-export","format_version":3,"memories":[]}"#,
                "needs a newer user-memory",
            ),
        ] {
            let err = import_document(
                &fresh.db,
                bad,
                true,
                OnConflict::Abort,
                None,
                &Subset::default(),
            )
            .unwrap_err();
            assert!(err.to_string().contains(message), "{err}");
        }
    }

    #[test]
    fn inferences_in_the_file_may_not_cite_inferences_or_broaden_scope() {
        let s = store();
        let c = &s.conn;
        let x = insert_row(c, "Prefer examples", &[]);
        let p = insert_row(
            c,
            "Project-only evidence",
            &[("scope", "project"), ("project", "lab")],
        );
        let i = insert_row(
            c,
            "Learns by example",
            &[
                ("memory_type", "pattern"),
                ("source", "inferred"),
                ("status", "tentative"),
            ],
        );
        c.execute(
            "UPDATE memories SET evidence_ids=?2 WHERE id=?1",
            params![
                i.to_string(),
                serde_json::to_string(&[x.min(p), x.max(p)]).unwrap()
            ],
        )
        .unwrap();
        let j = insert_row(
            c,
            "Learns by doing",
            &[
                ("memory_type", "pattern"),
                ("source", "inferred"),
                ("status", "tentative"),
            ],
        );
        c.execute(
            "UPDATE memories SET evidence_ids=?2 WHERE id=?1",
            params![
                j.to_string(),
                serde_json::to_string(&[x.min(i), x.max(i)]).unwrap()
            ],
        )
        .unwrap();
        let (document, _) = export_document(&s.db, &Subset::default()).unwrap();
        let fresh = store();
        let result = import(&fresh, &document, false, OnConflict::Abort);
        let rules: Vec<&str> = result["invalid"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["rule"].as_str().unwrap())
            .collect();
        assert_eq!(
            rules,
            [
                "Inferences cannot broaden or cross the scope of their evidence",
                "Evidence must be active explicit or observed memory, not another inference"
            ]
        );
    }

    #[test]
    fn distinct_pairs_are_sorted_deduplicated_and_dropped_when_unresolved() {
        let (s, [a, b, ..]) = populated();
        let (document, _) = export_document(&s.db, &Subset::default()).unwrap();
        let mut value: Value = serde_json::from_str(&document).unwrap();
        value["distinct_pairs"] = json!([
            [a.max(b).to_string(), a.min(b).to_string()],
            [a.min(b).to_string(), a.max(b).to_string()],
            [a.to_string(), Uuid::new_v4().to_string()],
            [a.to_string(), a.to_string()]
        ]);
        let fresh = store();
        let result = import(&fresh, &value.to_string(), true, OnConflict::Abort);
        assert_eq!(result["dropped_distinct_pairs"], 2, "{result}");
        assert_eq!(distinct_pairs(&fresh.conn).unwrap(), [(a.min(b), a.max(b))]);
    }

    #[test]
    fn distinct_pairs_are_exported_in_order() {
        let s = store();
        let mut ids: Vec<Uuid> = (0..4)
            .map(|k| insert_row(&s.conn, &format!("Note {k}"), &[]))
            .collect();
        ids.sort();
        for (a, b) in [(ids[2], ids[3]), (ids[0], ids[1])] {
            s.conn
                .execute(
                    "INSERT INTO memory_distinct (a,b) VALUES (?1,?2)",
                    [a.to_string(), b.to_string()],
                )
                .unwrap();
        }
        let (document, _) = export_document(&s.db, &Subset::default()).unwrap();
        let value: Value = serde_json::from_str(&document).unwrap();
        assert_eq!(
            value["distinct_pairs"],
            json!([[ids[0], ids[1]], [ids[2], ids[3]]])
        );
    }

    #[test]
    fn similar_to_existing_counts_active_imports_matching_the_store_before_apply() {
        let s = store();
        insert_row(&s.conn, "Use Helix as the text editor", &[]);
        let other = store();
        insert_row(&other.conn, "Use Helix as the text editor please", &[]);
        insert_row(
            &other.conn,
            "Plant tomatoes in spring",
            &[("category", "garden")],
        );
        let replaced = insert_row(&other.conn, "Use Helix as the editor", &[]);
        let successor = insert_row(&other.conn, "Use Zed as the editor", &[]);
        other
            .conn
            .execute(
                "UPDATE memories SET status='superseded',superseded_by=?2 WHERE id=?1",
                [replaced.to_string(), successor.to_string()],
            )
            .unwrap();
        let (document, _) = export_document(&other.db, &Subset::default()).unwrap();
        let result = import(&s, &document, false, OnConflict::Abort);
        assert_eq!(result["similar_to_existing"], 1, "{result}");
    }

    #[test]
    fn export_files_are_private_and_never_replaced_without_force() {
        let (s, _) = populated();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backup.json");
        let (document, _) = export_document(&s.db, &Subset::default()).unwrap();
        write_export(&path, &document, false).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), document);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(write_export(&path, "x", false).is_err());
        write_export(&path, "replaced\n", true).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "replaced\n");
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temporary file left"
        );
    }

    #[test]
    fn export_names_are_plain_json_file_names() {
        for good in ["backup.json", "backup-2026-10-15.json", "a_b.c.json"] {
            assert!(valid_name(good), "{good}");
        }
        let long = format!("{}.json", "a".repeat(251));
        for bad in [
            "../backup.json",
            "a/b.json",
            "/tmp/x.json",
            ".hidden.json",
            ".json",
            "backup.txt",
            "backup",
            "backup.json/",
            "bäckup.json",
            "back up.json",
            "backup\u{2215}x.json",
            &long,
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert!(valid_name(&format!("{}.json", "a".repeat(250))));
    }

    #[test]
    fn exports_are_capped_at_64_mib() {
        assert_eq!(MAX_BYTES, 64 << 20);
        let fresh = store();
        let mut padding = " ".repeat(MAX_BYTES as usize + 1);
        let err = import_document(
            &fresh.db,
            &padding,
            false,
            OnConflict::Abort,
            None,
            &Subset::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("64 MiB"), "{err}");
        padding.pop();
        let err = import_document(
            &fresh.db,
            &padding,
            false,
            OnConflict::Abort,
            None,
            &Subset::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid JSON"), "{err}");
    }

    #[test]
    fn timestamps_are_store_form_or_iso_utc() {
        for (value, stored) in [
            ("2026-10-15 05:20:11", Some("2026-10-15 05:20:11")),
            ("2026-10-15T05:20:11Z", Some("2026-10-15 05:20:11")),
            ("2026-10-15T05:20:11", None),
            ("2026-10-15 05:20:11Z", None),
            ("2026-10-15T05:20:11+", None),
            ("2026-10-15T05:20:11.5Z", None),
            ("2026/10-15 05:20:11", None),
            ("2026-10/15 05:20:11", None),
            ("2026-10-15_05:20:11", None),
            ("2026-10-15 05-20:11", None),
            ("2026-10-15 05:20-11", None),
            ("2026-10-15 05:20:1x", None),
            ("", None),
        ] {
            assert_eq!(timestamp(value).as_deref(), stored, "{value}");
        }
    }

    #[test]
    fn every_normalized_field_is_reported() {
        let s = store();
        let c = &s.conn;
        let x = insert_row(c, "Prefer examples", &[]);
        let y = insert_row(c, "Prefer demos", &[]);
        let i = insert_row(
            c,
            "Learns by example",
            &[
                ("memory_type", "pattern"),
                ("source", "inferred"),
                ("status", "tentative"),
                ("context", "when learning"),
                ("reason", "said so"),
                ("evidence", "two answers"),
            ],
        );
        c.execute(
            "UPDATE memories SET evidence_ids=?2 WHERE id=?1",
            params![
                i.to_string(),
                serde_json::to_string(&[x.min(y), x.max(y)]).unwrap()
            ],
        )
        .unwrap();
        let local = insert_row(
            c,
            "Use Go here",
            &[("scope", "project"), ("project", "github.com/acme/svc")],
        );
        let (document, _) = export_document(&s.db, &Subset::default()).unwrap();
        let unsorted = json!([x.max(y), x.min(y)]);
        for (id, field, value) in [
            (i, "type", json!("Pattern")),
            (i, "category", json!("Tools")),
            (i, "content", json!("Learns  by example")),
            (i, "context", json!(" when learning")),
            (i, "reason", json!("said so ")),
            (i, "evidence", json!("two  answers")),
            (i, "evidence_ids", unsorted.clone()),
            (i, "updated_at", json!("2026-10-15T05:20:11Z")),
            (local, "project", json!("https://github.com/acme/svc.git")),
        ] {
            let changed = edited(&document, id, |m| m[field] = value.clone());
            let result = import(&s, &changed, false, OnConflict::Abort);
            assert_eq!(
                result["normalized"],
                json!([id.to_string()]),
                "{field}: {result}"
            );
            assert_eq!(result["conflicts"], json!([]), "{field}: {result}");
        }
        let result = import(&s, &document, false, OnConflict::Abort);
        assert_eq!(result["normalized"], json!([]));
        assert_eq!(result["origins_merged"], 0, "nothing new to merge");
    }

    #[test]
    fn origins_are_bounded_and_keep_their_earliest_sighting() {
        let (s, [a, ..]) = populated();
        let (document, _) = export_document(&s.db, &Subset::default()).unwrap();
        let with = |origins: Value| edited(&document, a, |m| m["origins"] = origins.clone());
        let longest = "o".repeat(ORIGIN_LIMIT);
        let fresh = store();
        for (origin, ok) in [
            (longest.clone(), true),
            (format!("{longest}o"), false),
            (" ".into(), false),
        ] {
            let result = import(
                &fresh,
                &with(json!([{"origin": origin, "first_seen": NOW}])),
                false,
                OnConflict::Abort,
            );
            assert_eq!(
                result["invalid"].as_array().unwrap().is_empty(),
                ok,
                "{origin:?}"
            );
        }
        let repeated = with(json!([
            {"origin":"claude:session/one","first_seen":"2026-10-15 07:00:00"},
            {"origin":"claude:session/one","first_seen":"2026-10-15 05:00:00"},
            {"origin":"claude:session/one","first_seen":"2026-10-15 06:00:00"}
        ]));
        assert_eq!(
            import(&fresh, &repeated, true, OnConflict::Abort)["applied"],
            true
        );
        assert_eq!(
            first_seen(&fresh, a),
            [("claude:session/one".into(), "2026-10-15 05:00:00".into())]
        );
    }

    #[test]
    fn unknown_format_versions_are_refused() {
        let fresh = store();
        let err = import_document(
            &fresh.db,
            r#"{"format":"user-memory-export","format_version":0,"memories":[]}"#,
            false,
            OnConflict::Abort,
            None,
            &Subset::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown format version"), "{err}");
    }

    #[test]
    fn project_rows_must_stay_in_their_project() {
        let s = store();
        let c = &s.conn;
        let lab = |content| insert_row(c, content, &[("scope", "project"), ("project", "lab")]);
        let old = lab("Indent with tabs");
        let new = lab("Indent with 4 spaces");
        let shop = insert_row(
            c,
            "Use Go here",
            &[("scope", "project"), ("project", "shop")],
        );
        c.execute(
            "UPDATE memories SET status='superseded',superseded_by=?2 WHERE id=?1",
            [old.to_string(), new.to_string()],
        )
        .unwrap();
        let x = insert_row(c, "Prefer examples", &[]);
        let i = insert_row(
            c,
            "Learns by example",
            &[
                ("memory_type", "pattern"),
                ("source", "inferred"),
                ("status", "tentative"),
                ("scope", "project"),
                ("project", "lab"),
            ],
        );
        c.execute(
            "UPDATE memories SET evidence_ids=?2 WHERE id=?1",
            params![
                i.to_string(),
                serde_json::to_string(&[x.min(new), x.max(new)]).unwrap()
            ],
        )
        .unwrap();
        let (document, _) = export_document(&s.db, &Subset::default()).unwrap();
        let fresh = store();
        let result = import(&fresh, &document, false, OnConflict::Abort);
        assert_eq!(
            result["invalid"],
            json!([]),
            "same project is allowed: {result}"
        );
        let rules = |document: &str| -> Vec<String> {
            import(&fresh, document, false, OnConflict::Abort)["invalid"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["rule"].as_str().unwrap().to_owned())
                .collect()
        };
        let elsewhere = edited(&document, old, |m| {
            m["superseded_by"] = json!(shop.to_string())
        });
        assert_eq!(
            rules(&elsewhere),
            ["superseded_by must name a memory in the same scope and project"]
        );
        let moved = edited(&document, i, |m| m["project"] = json!("shop"));
        assert_eq!(
            rules(&moved),
            ["Inferences cannot broaden or cross the scope of their evidence"]
        );
    }

    #[test]
    fn similar_to_existing_looks_in_the_imported_memorys_project() {
        let s = store();
        insert_row(
            &s.conn,
            "Use Go for this service",
            &[("scope", "project"), ("project", "lab")],
        );
        let other = store();
        insert_row(
            &other.conn,
            "Use Go for this service please",
            &[("scope", "project"), ("project", "lab")],
        );
        let (document, _) = export_document(&other.db, &Subset::default()).unwrap();
        let result = import(&s, &document, false, OnConflict::Abort);
        assert_eq!(result["similar_to_existing"], 1, "{result}");
    }

    #[test]
    fn bare_file_names_live_in_the_current_directory() {
        assert_eq!(directory_of(Path::new("backup.json")), Path::new("."));
        assert_eq!(directory_of(Path::new("/a/backup.json")), Path::new("/a"));
        assert_eq!(
            exports_dir(Path::new("memory.sqlite3")),
            Path::new("./exports")
        );
    }

    #[cfg(unix)]
    mod exports_directory {
        use super::*;
        use std::os::unix::fs::{PermissionsExt, symlink};

        fn request(name: &str, overwrite: bool) -> ExportMemories {
            ExportMemories {
                name: name.into(),
                overwrite,
                subset: Subset::default(),
            }
        }

        fn import_request(name: &str) -> ImportMemories {
            ImportMemories {
                name: name.into(),
                apply: true,
                on_conflict: OnConflict::Abort,
                user_confirmed: true,
                titles: None,
                subset: Subset::default(),
            }
        }

        #[test]
        fn exports_land_privately_beside_the_store_and_name_only_the_file() {
            let (s, _) = populated();
            let summary = export_named(&s.db, &request("backup.json", false)).unwrap();
            assert_eq!(summary["exported"], true);
            assert_eq!(summary["file"], "backup.json");
            assert!(
                !summary
                    .to_string()
                    .contains(s.db.parent().unwrap().to_str().unwrap())
            );
            let dir = exports_dir(&s.db);
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            let file = dir.join("backup.json");
            assert_eq!(
                fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert!(export_named(&s.db, &request("backup.json", false)).is_err());
            export_named(&s.db, &request("backup.json", true)).unwrap();
            assert!(export_named(&s.db, &request("../escape.json", false)).is_err());
        }

        #[test]
        fn imports_are_previews_without_both_apply_and_user_confirmation() {
            let (s, _) = populated();
            export_named(&s.db, &request("backup.json", false)).unwrap();
            let fresh = store();
            fs::create_dir(exports_dir(&fresh.db)).unwrap();
            fs::set_permissions(exports_dir(&fresh.db), fs::Permissions::from_mode(0o700)).unwrap();
            fs::copy(
                exports_dir(&s.db).join("backup.json"),
                exports_dir(&fresh.db).join("backup.json"),
            )
            .unwrap();
            let unconfirmed = ImportMemories {
                user_confirmed: false,
                ..import_request("backup.json")
            };
            assert_eq!(
                import_named(&fresh.db, &unconfirmed).unwrap()["applied"],
                false
            );
            assert_eq!(
                import_named(&fresh.db, &import_request("backup.json")).unwrap()["applied"],
                true
            );
        }

        #[test]
        fn symlinks_fifos_oversized_files_and_unsafe_directories_are_refused() {
            let (s, _) = populated();
            export_named(&s.db, &request("backup.json", false)).unwrap();
            let dir = exports_dir(&s.db);
            let outside = tempfile::tempdir().unwrap();
            let target = outside.path().join("secret.json");
            fs::write(&target, "planted-secret-value").unwrap();
            symlink(&target, dir.join("evil.json")).unwrap();
            let err = import_named(&s.db, &import_request("evil.json"))
                .unwrap_err()
                .to_string();
            assert!(err.contains("not a regular file"), "{err}");
            assert!(!err.contains("planted") && !err.contains(outside.path().to_str().unwrap()));
            assert!(export_named(&s.db, &request("evil.json", false)).is_err());
            export_named(&s.db, &request("evil.json", true)).unwrap();
            assert!(
                !fs::symlink_metadata(dir.join("evil.json"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(fs::read_to_string(&target).unwrap(), "planted-secret-value");

            let fifo = dir.join("pipe.json");
            assert!(
                std::process::Command::new("mkfifo")
                    .arg(&fifo)
                    .status()
                    .unwrap()
                    .success()
            );
            // Opening a FIFO waits for a writer that never comes, so an
            // import that opened it would never return.
            const STUCK: std::time::Duration = std::time::Duration::from_secs(60);
            let (done, finished) = std::sync::mpsc::channel();
            let db = s.db.clone();
            std::thread::spawn(move || {
                let _ = done.send(
                    import_named(&db, &import_request("pipe.json")).map_err(|e| e.to_string()),
                );
            });
            let err = finished
                .recv_timeout(STUCK)
                .expect("import opened the FIFO and waited for a writer")
                .unwrap_err();
            assert!(err.contains("not a regular file"), "{err}");

            let big = fs::File::create(dir.join("big.json")).unwrap();
            big.set_len(MAX_BYTES + 1).unwrap();
            let err = import_named(&s.db, &import_request("big.json"))
                .unwrap_err()
                .to_string();
            assert!(err.contains("64 MiB"), "{err}");
            big.set_len(MAX_BYTES).unwrap();
            let err = import_named(&s.db, &import_request("big.json"))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("invalid JSON"),
                "a file at the cap is read: {err}"
            );

            fs::write(
                dir.join("plant.json"),
                "{\"format\":\"planted-secret-value\"}",
            )
            .unwrap();
            fs::set_permissions(dir.join("plant.json"), fs::Permissions::from_mode(0o600)).unwrap();
            let err = import_named(&s.db, &import_request("plant.json"))
                .unwrap_err()
                .to_string();
            assert!(!err.contains("planted"), "{err}");

            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(
                export_named(&s.db, &request("other.json", false))
                    .unwrap_err()
                    .to_string()
                    .contains("private")
            );
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();

            let linked = store();
            symlink(&dir, exports_dir(&linked.db)).unwrap();
            let err = export_named(&linked.db, &request("x.json", false))
                .unwrap_err()
                .to_string();
            assert!(err.contains("not a plain directory"), "{err}");
            assert!(import_named(&linked.db, &import_request("backup.json")).is_err());
        }

        #[test]
        fn a_file_swapped_after_the_check_is_recognised() {
            let dir = tempfile::tempdir().unwrap();
            let first = dir.path().join("a.json");
            let second = dir.path().join("b.json");
            fs::write(&first, "{}").unwrap();
            fs::write(&second, "{}").unwrap();
            let checked = fs::symlink_metadata(&first).unwrap();
            assert!(same_file(&checked, &fs::metadata(&first).unwrap()));
            assert!(!same_file(&checked, &fs::metadata(&second).unwrap()));
            let directory = fs::metadata(dir.path()).unwrap();
            assert!(!same_file(&directory, &directory), "not a regular file");
        }

        #[test]
        fn imports_never_create_the_exports_directory() {
            let fresh = store();
            let err = import_named(&fresh.db, &import_request("backup.json"))
                .unwrap_err()
                .to_string();
            assert!(err.contains("No exports yet"), "{err}");
            assert!(fs::symlink_metadata(exports_dir(&fresh.db)).is_err());
        }

        #[test]
        fn an_unreadable_exports_location_is_reported_not_created() {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("not-a-directory");
            fs::write(&file, "").unwrap();
            let db = file.join("memory.sqlite3");
            let err = export_named(&db, &request("backup.json", false))
                .unwrap_err()
                .to_string();
            assert!(err.contains("Cannot read the exports directory"), "{err}");
        }
    }
}
