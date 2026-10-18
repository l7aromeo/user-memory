//! Memory ids as agents and the user give them: a full id, or the first
//! characters the index shows.

use anyhow::{Result, bail};
use rusqlite::Connection;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, fmt, str::FromStr};
use uuid::Uuid;

/// Shortest id prefix accepted: the 8 characters the index shows. Fewer would
/// often match several memories.
pub(super) const SHORT_ID: usize = 8;

/// A memory named by its full id or by a prefix of at least [`SHORT_ID`]
/// characters, such as the index prints.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemoryRef(String);

/// Advertised as a UUID, as these fields always were: a remote client of
/// 0.7 or earlier refuses a server whose memory ids are not UUIDs. A prefix
/// is still accepted, since a format only describes the value.
impl JsonSchema for MemoryRef {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Uuid::schema_name()
    }
    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        Uuid::json_schema(generator)
    }
}

impl MemoryRef {
    /// The full id, when this names one by it.
    pub(super) fn full(&self) -> Option<Uuid> {
        Uuid::parse_str(&self.0).ok()
    }
}

impl From<Uuid> for MemoryRef {
    fn from(id: Uuid) -> Self {
        Self(id.to_string())
    }
}

/// Typed on the command line: text that cannot name a memory is refused
/// before anything runs.
impl FromStr for MemoryRef {
    type Err = String;
    fn from_str(text: &str) -> Result<Self, String> {
        if Uuid::parse_str(text.trim()).is_err() && matches!(prefix(text), Prefix::NotAnId) {
            return Err(not_an_id(text));
        }
        Ok(Self(text.trim().to_owned()))
    }
}

impl fmt::Display for MemoryRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The memories whose id starts with `prefix`, in id order.
fn matching(conn: &Connection, prefix: &str) -> Result<Vec<String>> {
    Ok(conn
        .prepare("SELECT id FROM memories WHERE substr(id,1,length(?1))=?1 ORDER BY id")?
        .query_map([prefix], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

/// What `wanted` names, when it is no full id: a prefix, or why it is none.
enum Prefix {
    /// Text that cannot be an id, or is too short to name one memory.
    NotAnId,
    /// A valid prefix, lowercased.
    Valid(String),
}

fn prefix(wanted: &str) -> Prefix {
    let wanted = wanted.trim().to_lowercase();
    if wanted.chars().count() < SHORT_ID
        || !wanted.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
    {
        return Prefix::NotAnId;
    }
    Prefix::Valid(wanted)
}

fn not_an_id(wanted: &str) -> String {
    format!(
        "{} is not a memory id: give a full id or its first {SHORT_ID} characters",
        wanted.trim().to_lowercase()
    )
}

/// Ids that `wanted` names: a full id, or a prefix of at least [`SHORT_ID`]
/// characters. A prefix matching several memories is an error that lists them.
pub(super) fn resolve(conn: &Connection, wanted: &str) -> Result<Option<Uuid>> {
    if let Ok(id) = Uuid::parse_str(wanted.trim()) {
        return Ok(super::schema::get(conn, id)?.map(|m| m.id));
    }
    let wanted = match prefix(wanted) {
        Prefix::NotAnId => bail!(not_an_id(wanted)),
        Prefix::Valid(wanted) => wanted,
    };
    match matching(conn, &wanted)?.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(Uuid::parse_str(one)?)),
        many => bail!("{wanted} matches several memories: {}", many.join(", ")),
    }
}

/// The full id `named` stands for: a full id as given, whether or not that
/// memory exists, so each tool says in its own words that it is missing;
/// otherwise the one memory its prefix matches. `Err` holds the refusal for
/// a prefix matching none or several, or text that is no id.
pub(super) fn full_id(conn: &Connection, named: &MemoryRef) -> Result<Result<Uuid, String>> {
    if let Some(id) = named.full() {
        return Ok(Ok(id));
    }
    let wanted = match prefix(&named.0) {
        Prefix::NotAnId => return Ok(Err(not_an_id(&named.0))),
        Prefix::Valid(wanted) => wanted,
    };
    Ok(match matching(conn, &wanted)?.as_slice() {
        [] => Err(format!("{wanted} names no memory")),
        [one] => Ok(Uuid::parse_str(one)?),
        many => Err(format!(
            "{wanted} matches several memories: {}",
            many.join(", ")
        )),
    })
}

/// Rewrites each of `refs` to the full id it names, sorted and without
/// repeats; the refusal for the first that names no memory or several.
pub(super) fn resolve_all(conn: &Connection, refs: &mut Vec<MemoryRef>) -> Result<Option<String>> {
    let mut ids = Vec::with_capacity(refs.len());
    for named in refs.iter() {
        match full_id(conn, named)? {
            Ok(id) => ids.push(id),
            Err(refusal) => return Ok(Some(refusal)),
        }
    }
    ids.sort_unstable();
    ids.dedup();
    *refs = ids.into_iter().map(MemoryRef::from).collect();
    Ok(None)
}

/// The full ids among `refs`, which [`resolve_all`] made all of them.
pub(super) fn uuids(refs: &[MemoryRef]) -> Vec<Uuid> {
    refs.iter().filter_map(MemoryRef::full).collect()
}
