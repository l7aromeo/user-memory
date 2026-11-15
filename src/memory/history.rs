//! Each memory's history: an event for every change, written by triggers in
//! the transaction that makes the change, whichever program makes it, with
//! the values the change replaced.
//!
//! Triggers cannot know who is writing, so each write names its client (and,
//! for the write functions, what it does and to which memory) in the one
//! row of `memory_writer`, which it adds after taking the write turn and
//! removes before committing: no other connection ever sees it, and a write
//! that names none, such as a migration or another program's, leaves
//! `client` empty. SQLite refuses a stored trigger that reads a temporary
//! table, so the row lives in the store's own schema.

use super::schema::{connect, write};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use std::{cell::RefCell, path::Path, sync::OnceLock};
use uuid::Uuid;

/// The history tables and the triggers that fill them. Part of the stored
/// schema since version 7: bump `schema::SCHEMA_VERSION` whenever it changes.
///
/// - `memory_events` holds one row per change: `at` is when it happened and
///   `last_at` when it last happened, which differ only for confirmations
///   merged into one event (`count` of them). `before` holds the changed
///   fields' previous values, as a JSON object under their export names.
///   `imported` marks an event an import brought in, by that import's time.
///   It has no foreign key: a forgotten memory's events stay as tombstones.
/// - `memory_writer` holds, only inside a write transaction, the client
///   writing, and for the write functions the operation and the memory it
///   is about (`subject`) and the conversation (`origin`).
///
/// The update trigger tells the operation from what changed: nothing but
/// the confirmation time, or nothing at all in an `update` or a `confirm`
/// of that very memory, is a confirmation, merged into the memory's latest event when
/// that is a confirmation made here; a status alone turning stale is
/// `stale`; an import's changes are `import`; then a status turning
/// superseded or back, a scope or project alone (outside an `update`) and
/// anything else. A change of a timestamp alone, as when a pattern already
/// stale is staled again, is no event.
pub(super) const EVENTS: &str = "CREATE TABLE IF NOT EXISTS memory_events (
    seq INTEGER PRIMARY KEY,
    memory_id TEXT NOT NULL,
    at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    count INTEGER NOT NULL DEFAULT 1 CHECK(count >= 1),
    op TEXT NOT NULL CHECK(op IN ('record','confirm','update','supersede','merge','forget','import','revive','distinct','stale','move','origin_removed')),
    origin TEXT NOT NULL DEFAULT '',
    client TEXT NOT NULL DEFAULT '',
    before TEXT CHECK(before IS NULL OR json_valid(before)),
    imported TEXT
);
CREATE INDEX IF NOT EXISTS memory_events_by_memory ON memory_events(memory_id, seq);
CREATE TABLE IF NOT EXISTS memory_writer (
    id INTEGER PRIMARY KEY CHECK(id=1),
    client TEXT NOT NULL DEFAULT '',
    op TEXT,
    subject TEXT,
    origin TEXT NOT NULL DEFAULT ''
);
CREATE TRIGGER IF NOT EXISTS memory_event_recorded AFTER INSERT ON memories BEGIN
    INSERT INTO memory_events(memory_id,op,origin,client)
    VALUES (NEW.id,
        CASE (SELECT op FROM memory_writer WHERE id=1) WHEN 'merge' THEN 'merge' WHEN 'import' THEN 'import' ELSE 'record' END,
        coalesce((SELECT origin FROM memory_writer WHERE id=1),''),
        coalesce((SELECT client FROM memory_writer WHERE id=1),''));
END;
CREATE TRIGGER IF NOT EXISTS memory_event_changed AFTER UPDATE ON memories BEGIN
    UPDATE memory_events SET last_at=CURRENT_TIMESTAMP,count=count+1,
        origin=coalesce(nullif((SELECT origin FROM memory_writer WHERE id=1),''),origin),
        client=coalesce((SELECT client FROM memory_writer WHERE id=1),'')
    WHERE seq=(SELECT max(seq) FROM memory_events WHERE memory_id=NEW.id) AND op='confirm' AND imported IS NULL
    AND NOT EXISTS (SELECT 1 FROM memory_writer WHERE id=1 AND op='import')
    AND (OLD.memory_type,OLD.category,OLD.context,OLD.title,OLD.content,OLD.reason,OLD.scope,OLD.project,OLD.source,OLD.status,OLD.evidence,OLD.evidence_ids,OLD.superseded_by)
        IS (NEW.memory_type,NEW.category,NEW.context,NEW.title,NEW.content,NEW.reason,NEW.scope,NEW.project,NEW.source,NEW.status,NEW.evidence,NEW.evidence_ids,NEW.superseded_by)
    AND (OLD.last_confirmed_at IS NOT NEW.last_confirmed_at OR EXISTS (SELECT 1 FROM memory_writer WHERE id=1 AND op IN ('update','confirm') AND subject=NEW.id));
    INSERT INTO memory_events(memory_id,op,origin,client,before)
    SELECT NEW.id,
        CASE
            WHEN (SELECT op FROM memory_writer WHERE id=1)='import' THEN 'import'
            WHEN (OLD.memory_type,OLD.category,OLD.context,OLD.title,OLD.content,OLD.reason,OLD.scope,OLD.project,OLD.source,OLD.status,OLD.evidence,OLD.evidence_ids,OLD.superseded_by)
                IS (NEW.memory_type,NEW.category,NEW.context,NEW.title,NEW.content,NEW.reason,NEW.scope,NEW.project,NEW.source,NEW.status,NEW.evidence,NEW.evidence_ids,NEW.superseded_by) THEN 'confirm'
            WHEN NEW.status='stale' AND (OLD.memory_type,OLD.category,OLD.context,OLD.title,OLD.content,OLD.reason,OLD.scope,OLD.project,OLD.source,OLD.evidence,OLD.evidence_ids,OLD.superseded_by)
                IS (NEW.memory_type,NEW.category,NEW.context,NEW.title,NEW.content,NEW.reason,NEW.scope,NEW.project,NEW.source,NEW.evidence,NEW.evidence_ids,NEW.superseded_by) THEN 'stale'
            WHEN NEW.status='superseded' AND OLD.status IS NOT 'superseded' THEN 'supersede'
            WHEN OLD.status='superseded' AND NEW.status IS NOT 'superseded' THEN 'revive'
            WHEN (OLD.memory_type,OLD.category,OLD.context,OLD.title,OLD.content,OLD.reason,OLD.source,OLD.status,OLD.evidence,OLD.evidence_ids,OLD.superseded_by)
                IS (NEW.memory_type,NEW.category,NEW.context,NEW.title,NEW.content,NEW.reason,NEW.source,NEW.status,NEW.evidence,NEW.evidence_ids,NEW.superseded_by)
                AND NOT EXISTS (SELECT 1 FROM memory_writer WHERE id=1 AND op='update' AND subject=NEW.id) THEN 'move'
            ELSE 'update'
        END,
        coalesce((SELECT origin FROM memory_writer WHERE id=1),''),
        coalesce((SELECT client FROM memory_writer WHERE id=1),''),
        CASE WHEN (OLD.memory_type,OLD.category,OLD.context,OLD.title,OLD.content,OLD.reason,OLD.scope,OLD.project,OLD.source,OLD.status,OLD.evidence,OLD.evidence_ids,OLD.superseded_by)
            IS (NEW.memory_type,NEW.category,NEW.context,NEW.title,NEW.content,NEW.reason,NEW.scope,NEW.project,NEW.source,NEW.status,NEW.evidence,NEW.evidence_ids,NEW.superseded_by) THEN NULL
        ELSE json_remove(json_object(
            'type',OLD.memory_type,'category',OLD.category,'context',OLD.context,'title',OLD.title,
            'content',OLD.content,'reason',OLD.reason,'scope',OLD.scope,'project',OLD.project,
            'source',OLD.source,'status',OLD.status,'evidence',OLD.evidence,
            'evidence_ids',CASE WHEN json_valid(OLD.evidence_ids) THEN json(OLD.evidence_ids) ELSE OLD.evidence_ids END,
            'superseded_by',OLD.superseded_by),
            CASE WHEN OLD.memory_type IS NEW.memory_type THEN '$.type' ELSE '$._' END,
            CASE WHEN OLD.category IS NEW.category THEN '$.category' ELSE '$._' END,
            CASE WHEN OLD.context IS NEW.context THEN '$.context' ELSE '$._' END,
            CASE WHEN OLD.title IS NEW.title THEN '$.title' ELSE '$._' END,
            CASE WHEN OLD.content IS NEW.content THEN '$.content' ELSE '$._' END,
            CASE WHEN OLD.reason IS NEW.reason THEN '$.reason' ELSE '$._' END,
            CASE WHEN OLD.scope IS NEW.scope THEN '$.scope' ELSE '$._' END,
            CASE WHEN OLD.project IS NEW.project THEN '$.project' ELSE '$._' END,
            CASE WHEN OLD.source IS NEW.source THEN '$.source' ELSE '$._' END,
            CASE WHEN OLD.status IS NEW.status THEN '$.status' ELSE '$._' END,
            CASE WHEN OLD.evidence IS NEW.evidence THEN '$.evidence' ELSE '$._' END,
            CASE WHEN OLD.evidence_ids IS NEW.evidence_ids THEN '$.evidence_ids' ELSE '$._' END,
            CASE WHEN OLD.superseded_by IS NEW.superseded_by THEN '$.superseded_by' ELSE '$._' END)
        END
    WHERE (OLD.memory_type,OLD.category,OLD.context,OLD.title,OLD.content,OLD.reason,OLD.scope,OLD.project,OLD.source,OLD.status,OLD.evidence,OLD.evidence_ids,OLD.superseded_by)
        IS NOT (NEW.memory_type,NEW.category,NEW.context,NEW.title,NEW.content,NEW.reason,NEW.scope,NEW.project,NEW.source,NEW.status,NEW.evidence,NEW.evidence_ids,NEW.superseded_by)
    OR (OLD.last_confirmed_at IS NOT NEW.last_confirmed_at AND EXISTS (SELECT 1 FROM memory_writer WHERE id=1 AND op='import'))
    OR ((OLD.last_confirmed_at IS NOT NEW.last_confirmed_at OR EXISTS (SELECT 1 FROM memory_writer WHERE id=1 AND op IN ('update','confirm') AND subject=NEW.id))
        AND NOT EXISTS (SELECT 1 FROM memory_events WHERE seq=(SELECT max(seq) FROM memory_events WHERE memory_id=NEW.id) AND op='confirm' AND imported IS NULL));
END;
CREATE TRIGGER IF NOT EXISTS memory_event_forgotten AFTER DELETE ON memories BEGIN
    UPDATE memory_events SET origin='',client='',before=NULL,imported=NULL WHERE memory_id=OLD.id;
    INSERT INTO memory_events(memory_id,op,client) VALUES (OLD.id,'forget',coalesce((SELECT client FROM memory_writer WHERE id=1),''));
END;
CREATE TRIGGER IF NOT EXISTS memory_event_origin_removed AFTER DELETE ON memory_origins
WHEN EXISTS (SELECT 1 FROM memories WHERE id=OLD.memory_id) BEGIN
    INSERT INTO memory_events(memory_id,op,client) VALUES (OLD.memory_id,'origin_removed',coalesce((SELECT client FROM memory_writer WHERE id=1),''));
END;
CREATE TRIGGER IF NOT EXISTS memory_event_distinct AFTER INSERT ON memory_distinct BEGIN
    INSERT INTO memory_events(memory_id,op,origin,client)
    SELECT value,'distinct',coalesce((SELECT origin FROM memory_writer WHERE id=1),''),coalesce((SELECT client FROM memory_writer WHERE id=1),'')
    FROM (SELECT NEW.a AS value UNION ALL SELECT NEW.b);
END;";

/// Every operation an event can name, as the table's check lists them.
pub(super) const OPS: [&str; 12] = [
    "record",
    "confirm",
    "update",
    "supersede",
    "merge",
    "forget",
    "import",
    "revive",
    "distinct",
    "stale",
    "move",
    "origin_removed",
];

/// How long events are kept, as SQLite's date modifier: history is for
/// undoing recent mistakes, and a year covers a correction noticed late
/// while bounding what a store keeps of text the user replaced.
const KEPT_FOR: &str = "-1 year";

/// The name events give the command line as a client.
pub const CLI: &str = "cli";

/// The client of writes on this process when no call names one.
static DEFAULT_CLIENT: OnceLock<String> = OnceLock::new();

thread_local! {
    /// The client a server's call on this thread writes for.
    static CLIENT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Names the client of every write this process makes outside a call that
/// names its own, such as the command line's. Set once; later calls keep
/// the first name.
pub fn set_default_client(name: &str) {
    let _ = DEFAULT_CLIENT.set(name.to_owned());
}

/// Runs `work` with its writes on this thread made for `client`.
pub fn acting_as<T>(client: &str, work: impl FnOnce() -> T) -> T {
    let earlier = CLIENT.with(|c| c.replace(Some(client.to_owned())));
    let done = work();
    CLIENT.with(|c| *c.borrow_mut() = earlier);
    done
}

/// The client this thread writes for, if any is named.
fn client() -> Option<String> {
    CLIENT
        .with(|c| c.borrow().clone())
        .or_else(|| DEFAULT_CLIENT.get().cloned())
}

/// Names this thread's client in `memory_writer`, inside a write
/// transaction, when the store has history and a client is named; returns
/// whether it wrote the row, which the write removes before it commits.
pub(super) fn begin(conn: &Connection) -> rusqlite::Result<bool> {
    let Some(client) = client() else {
        return Ok(false);
    };
    // A store being created has no table yet, and SQLite resolves a
    // statement's tables before it runs, so this asks first.
    let created: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type='table' AND name='memory_writer')",
        [],
        |r| r.get(0),
    )?;
    if !created {
        return Ok(false);
    }
    conn.execute(
        "INSERT OR REPLACE INTO memory_writer(id,client) VALUES (1,?1)",
        [client],
    )?;
    Ok(true)
}

/// Says what the write under way does, for the events its changes add:
/// `op` (`update`, `confirm`, `merge` or `import`; none for the rest), the
/// memory it is about, and the conversation. Keeps the client [`begin`] named.
pub(crate) fn note(
    conn: &Connection,
    op: Option<&str>,
    subject: Option<Uuid>,
    origin: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO memory_writer(id,client,op,subject,origin) VALUES (1,?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET op=excluded.op,subject=excluded.subject,origin=excluded.origin",
        params![
            client().unwrap_or_default(),
            op,
            subject.map(|id| id.to_string()),
            origin
        ],
    )?;
    Ok(())
}

/// Removes the write's row from `memory_writer` before it commits.
pub(crate) fn end(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM memory_writer", [])?;
    Ok(())
}

/// Reduces the history of memory `id` for `update` with `drop_history`:
/// its events up to number `earlier` become tombstones, their operation and
/// times only, and the events this write added after it keep everything
/// but the values they replaced. With `earlier` none, every event is this
/// write's.
pub(super) fn drop_history(
    conn: &Connection,
    id: Uuid,
    earlier: Option<i64>,
) -> rusqlite::Result<()> {
    let id = id.to_string();
    let earlier = earlier.unwrap_or(i64::MIN);
    conn.execute(
        "UPDATE memory_events SET origin='',client='',before=NULL,imported=NULL WHERE memory_id=?1 AND seq<=?2",
        params![id, earlier],
    )?;
    conn.execute(
        "UPDATE memory_events SET before=NULL WHERE memory_id=?1 AND seq>?2",
        params![id, earlier],
    )?;
    Ok(())
}

/// The newest event of memory `id`, by its number.
pub(super) fn latest(conn: &Connection, id: Uuid) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT max(seq) FROM memory_events WHERE memory_id=?1",
        [id.to_string()],
        |r| r.get(0),
    )
}

/// Removes the conversation `origin` from every event that names it, as
/// forgetting that conversation asks.
pub(super) fn drop_origin(conn: &Connection, origin: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE memory_events SET origin='' WHERE origin=?1",
        [origin],
    )
}

/// One stored event as `read` and `history` show it: empty fields left out,
/// and `last_at` and `count` only for merged confirmations.
fn shown(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let mut event = Map::new();
    event.insert("op".into(), json!(r.get::<_, String>("op")?));
    event.insert("at".into(), json!(r.get::<_, String>("at")?));
    let count: i64 = r.get("count")?;
    if count > 1 {
        event.insert("last_at".into(), json!(r.get::<_, String>("last_at")?));
        event.insert("count".into(), json!(count));
    }
    for field in ["origin", "client"] {
        let text: String = r.get(field)?;
        if !text.is_empty() {
            event.insert(field.into(), json!(text));
        }
    }
    if let Some(before) = r.get::<_, Option<String>>("before")? {
        event.insert(
            "before".into(),
            serde_json::from_str(&before).unwrap_or(Value::String(before)),
        );
    }
    if let Some(imported) = r.get::<_, Option<String>>("imported")? {
        event.insert("imported".into(), json!(imported));
    }
    Ok(Value::Object(event))
}

/// The events of memory `id`, oldest first.
pub(super) fn events(conn: &Connection, id: Uuid) -> Result<Vec<Value>> {
    Ok(conn
        .prepare_cached(
            "SELECT op,at,last_at,count,origin,client,before,imported FROM memory_events WHERE memory_id=?1 ORDER BY at,seq",
        )?
        .query_map([id.to_string()], shown)?
        .collect::<rusqlite::Result<_>>()?)
}

/// The memory a reference names among the events, for a memory that is
/// gone: its full id, or the one forgotten memory whose id starts with it.
pub(super) fn forgotten(conn: &Connection, wanted: &str) -> Result<Option<Uuid>> {
    let wanted = wanted.trim().to_ascii_lowercase();
    if let Ok(id) = Uuid::parse_str(&wanted) {
        let known: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM memory_events WHERE memory_id=?1)",
            [id.to_string()],
            |r| r.get(0),
        )?;
        return Ok(known.then_some(id));
    }
    if wanted.len() != super::ids::SHORT_ID || !wanted.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let found: Vec<String> = conn
        .prepare("SELECT DISTINCT memory_id FROM memory_events WHERE substr(memory_id,1,?2)=?1 AND memory_id NOT IN (SELECT id FROM memories) LIMIT 2")?
        .query_map(params![wanted, super::ids::SHORT_ID], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(match found.as_slice() {
        [one] => Uuid::parse_str(one).ok(),
        _ => None,
    })
}

/// Drops the events older than [`KEPT_FOR`] from the store at `db`, as a
/// write that removes text, so their values leave the file and its log.
/// Runs as an automatic backup is made, before the copy; returns how many
/// went. The backup the write would make is the one being made.
pub(super) fn prune(db: &Path) -> Result<usize> {
    // A store this binary cannot open as it is, such as one waiting for
    // its upgrade, is backed up as it is.
    let found = super::schema::version(&Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)?;
    if found != super::schema::SCHEMA_VERSION {
        return Ok(0);
    }
    let conn = connect(db)?;
    let tx = write(&conn)?;
    let dropped = tx.execute(
        "DELETE FROM memory_events WHERE last_at < datetime('now', ?1)",
        [KEPT_FOR],
    )?;
    // Forgotten memories' rows of the change counter go on the same day.
    let removed = super::changes::prune(&tx)?;
    if dropped + removed > 0 {
        tx.commit_removing()?;
    } else {
        tx.rollback()?;
    }
    Ok(dropped)
}

/// The events an export carries for memories `ids`, oldest first, every
/// field included.
pub(super) fn exported(conn: &Connection, ids: &[Uuid]) -> Result<Vec<Value>> {
    let mut statement = conn.prepare(
        "SELECT memory_id,op,at,last_at,count,origin,client,before,imported FROM memory_events WHERE memory_id IN (SELECT value FROM json_each(?1)) ORDER BY at,seq",
    )?;
    let ids = serde_json::to_string(&ids.iter().map(Uuid::to_string).collect::<Vec<_>>())?;
    let rows = statement.query_map([ids], |r| {
        let before: Option<String> = r.get(7)?;
        Ok(json!({
            "memory_id": r.get::<_, String>(0)?,
            "op": r.get::<_, String>(1)?,
            "at": r.get::<_, String>(2)?,
            "last_at": r.get::<_, String>(3)?,
            "count": r.get::<_, i64>(4)?,
            "origin": r.get::<_, String>(5)?,
            "client": r.get::<_, String>(6)?,
            "before": before.map(|b| serde_json::from_str::<Value>(&b).unwrap_or(Value::String(b))),
            "imported": r.get::<_, Option<String>>(8)?,
        }))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// An event from an export file, checked.
pub(super) struct Imported {
    pub(super) memory_id: Uuid,
    op: String,
    at: String,
    last_at: String,
    count: i64,
    origin: String,
    client: String,
    before: Option<String>,
}

/// Longest origin or client name an event carries, as a stored origin may:
/// an imported event's longer one is refused, and an MCP client's name is
/// cut to it.
const NAME_LIMIT: usize = 240;

/// The client name for an MCP client that sent none in `initialize`.
const UNNAMED_CLIENT: &str = "unknown";

/// The client name an MCP client's writes record, from the `name` it sent
/// in `initialize`: cleaned as stored text is (one line, single spaces),
/// prefixed `host:` on a server shared by several machines, and cut to
/// [`NAME_LIMIT`]. A client sends what it likes, and `history` shows it to
/// agents.
pub fn client_name(name: &str, shared: bool) -> String {
    let name = super::clean(name, usize::MAX);
    let name = if name.is_empty() {
        UNNAMED_CLIENT.to_owned()
    } else {
        name
    };
    let name = if shared { format!("host:{name}") } else { name };
    name.chars().take(NAME_LIMIT).collect()
}

/// The event `value` of an export's history, or `None` when it is not one
/// this store could have written: imported events are untrusted, since a
/// file can be edited by hand or come from someone else. `timestamp` turns
/// a file's time into the store's form.
pub(super) fn parse(value: &Value, timestamp: impl Fn(&str) -> Option<String>) -> Option<Imported> {
    let text = |field: &str| value.get(field).and_then(Value::as_str);
    let memory_id = Uuid::parse_str(text("memory_id")?).ok()?;
    let op = text("op").filter(|op| OPS.contains(op))?.to_owned();
    let at = timestamp(text("at")?)?;
    let last_at = match text("last_at") {
        Some(last) => timestamp(last)?,
        None => at.clone(),
    };
    let count = match value.get("count") {
        None | Some(Value::Null) => 1,
        Some(count) => count.as_i64().filter(|&c| c >= 1)?,
    };
    let name = |field: &str| match value.get(field) {
        None | Some(Value::Null) => Some(String::new()),
        Some(Value::String(s)) if s.chars().count() <= NAME_LIMIT => {
            Some(super::clean(s, usize::MAX))
        }
        Some(_) => None,
    };
    let before = match value.get("before") {
        None | Some(Value::Null) => None,
        Some(object @ Value::Object(_)) => Some(object.to_string()),
        Some(_) => return None,
    };
    Some(Imported {
        memory_id,
        op,
        at,
        last_at,
        count,
        origin: name("origin")?,
        client: name("client")?,
        before,
    })
}

/// Whether the store has the writer row's table (schema version 7 on).
fn has_writer(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='memory_writer')",
        [],
        |r| r.get(0),
    )
}

/// What is wrong with the history's tables: a writer row left after a
/// write, which every write clears before committing. Only another program
/// writing it, or SQL run by hand, leaves one, and the triggers would then
/// record its client for other programs' writes.
pub(super) fn problems(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    if !has_writer(conn)? {
        return Ok(Vec::new());
    }
    let left: i64 = conn.query_row("SELECT count(*) FROM memory_writer", [], |r| r.get(0))?;
    Ok(if left > 0 {
        vec!["history: a write's client is left in memory_writer, so other programs' writes would be recorded as its".to_owned()]
    } else {
        Vec::new()
    })
}

/// Clears a writer row left after a write (see [`problems`]).
pub(super) fn clear_writer(conn: &Connection) -> rusqlite::Result<()> {
    if has_writer(conn)? {
        conn.execute("DELETE FROM memory_writer", [])?;
    }
    Ok(())
}

/// The number of the store's newest event, or 0: an import compares the
/// file's events with those up to here.
pub(super) fn mark(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT coalesce(max(seq),0) FROM memory_events", [], |r| {
        r.get(0)
    })
}

/// Adds `event`, marked as brought in by the import made at `import`,
/// unless the store held the same event up to event number `mark`, before
/// this import wrote anything, as when a store imports its own export
/// again; returns whether it was added.
pub(super) fn add_imported(
    conn: &Connection,
    event: &Imported,
    import: &str,
    mark: i64,
) -> Result<bool> {
    let known: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM memory_events WHERE memory_id=?1 AND op=?2 AND at=?3 AND last_at=?4 AND count=?5 AND before IS ?6 AND seq<=?7)",
        params![
            event.memory_id.to_string(),
            event.op,
            event.at,
            event.last_at,
            event.count,
            event.before,
            mark
        ],
        |r| r.get(0),
    )?;
    if known {
        return Ok(false);
    }
    conn.execute(
        "INSERT INTO memory_events(memory_id,op,at,last_at,count,origin,client,before,imported) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            event.memory_id.to_string(),
            event.op,
            event.at,
            event.last_at,
            event.count,
            event.origin,
            event.client,
            event.before,
            import
        ],
    )?;
    Ok(true)
}

/// Whether the store behind `conn` keeps history (schema version 7 on).
pub(super) fn kept(conn: &Connection) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='memory_events'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}
