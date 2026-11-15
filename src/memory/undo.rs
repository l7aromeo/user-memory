//! Undoing an import: `forget` with `import`. Each memory the import added
//! or replaced is found by the import's own `import` event, which names it.
//! A memory changed since that event is kept; the rest are deleted, if the
//! import added them, or given back what the import replaced, from that
//! event's `before`. Changes made by another import that was undone since
//! count as no change. Every event the undo writes is marked with
//! [`history::undo_mark`], so a later undo can tell them apart.

use super::history::{self, IMPORT_MARK, IMPORT_ORIGINS, Mark, UNDO_KEPT, kind_of, undo_mark};
use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

/// What an undo does with one memory the import added or replaced.
enum Plan {
    Delete,
    /// Put back the values in `before`; `stale` when its supports changed
    /// since, so its status stays as the triggers left it.
    Restore {
        before: Map<String, Value>,
        stale: bool,
    },
    /// Changed since, by the operation named in `why`; `later` when by a
    /// later import whose undo comes first.
    Keep {
        why: &'static str,
        later: bool,
    },
}

/// One memory the import added or replaced, as its own event recorded it.
struct Touched {
    id: String,
    seq: i64,
    before: Map<String, Value>,
}

/// The reply when no memory carries the import.
const UNKNOWN: &str = "No memory carries this import: its memories were forgotten, its history is past the year it is kept, or no import here gave that name";
/// Added to [`UNKNOWN`] for a name without an import's `@`, such as the
/// time an import from before names ran.
const UNNAMED: &str = "; imports made before imports had names cannot be undone by import, so forget their memories by id";
/// The reply when every memory the import touched was dealt with before.
const ALREADY: &str =
    "This import was undone already; the memories in kept changed since it and stay";
/// The next step when a later import changed memories this one touched.
const LATER_FIRST: &str = "Memories kept since an import changed with a later import: undo that one first, then this one again.";

/// Undoes import `named` in the write turn `tx`: plans every memory it
/// touched, has `copy` make the store's copy when any will be deleted, then
/// writes. Returns the reply, without the parts every forget adds.
pub(super) fn forget_import(
    tx: &Connection,
    named: &str,
    copy: impl FnOnce() -> Result<String>,
) -> Result<Value> {
    let named = named.trim();
    let touched = touched(tx, named)?;
    if touched.is_empty() {
        let undone = undone(tx, named)?;
        let reason = if undone {
            ALREADY.to_owned()
        } else if named.contains(IMPORT_MARK) {
            UNKNOWN.to_owned()
        } else {
            format!("{UNKNOWN}{UNNAMED}")
        };
        return Ok(json!({"deleted":false,"import":named,"reason":reason}));
    }
    let mut plans = Vec::new();
    let mut finished = 0;
    for memory in touched {
        match plan(tx, named, &memory)? {
            Some(plan) => plans.push((memory, plan)),
            None => finished += 1,
        }
    }
    let deleting = plans.iter().any(|(_, plan)| matches!(plan, Plan::Delete));
    let backup = if deleting { Some(copy()?) } else { None };
    let mark = undo_mark(named);
    let (mut ids, mut restored, mut kept) = (Vec::new(), Vec::new(), Vec::new());
    let mut kept_since = Map::new();
    let mut next = BTreeSet::new();
    for (memory, plan) in plans {
        let from = history::mark(tx)?;
        match plan {
            Plan::Delete => {
                tx.execute("DELETE FROM memories WHERE id=?1", [&memory.id])?;
                ids.push(memory.id.clone());
            }
            Plan::Restore { before, stale } => {
                restore(tx, &memory.id, &before, stale)?;
                if let Some(earlier) = undone_version(tx, &memory)? {
                    next.insert(format!(
                        "{} is back as import {earlier} left it, which was undone too; to finish, forget with import \"{earlier}\" (user-memory forget --import \"{earlier}\").",
                        memory.id
                    ));
                }
                restored.push(memory.id.clone());
            }
            Plan::Keep { why, later } => {
                if later {
                    next.insert(LATER_FIRST.to_owned());
                }
                // So the undo of a later import that gives the memory back
                // this one's version can say this one was undone.
                tx.execute(
                    "UPDATE memory_events SET before=json_set(coalesce(before,'{}'),'$.undo_kept',json('true')) WHERE seq=?1",
                    [memory.seq],
                )?;
                kept_since.insert(memory.id.clone(), json!(why));
                kept.push(memory.id);
                continue;
            }
        }
        tx.execute(
            "UPDATE memory_events SET imported=?1 WHERE seq>?2 AND memory_id=?3",
            params![mark, from, memory.id],
        )?;
    }
    let mut reply = json!({
        "deleted": !ids.is_empty(),
        "import": named,
        "count": ids.len(),
        "ids": ids,
        "restored": restored,
        "kept": kept,
        "kept_since": kept_since,
    });
    if let Some(backup) = backup {
        reply["backup"] = json!(backup);
    }
    if ids_empty(&reply) && restored_empty(&reply) && finished > 0 {
        reply["reason"] = json!(ALREADY);
    }
    if !next.is_empty() {
        reply["next"] = json!(next.into_iter().collect::<Vec<_>>().join("\n"));
    }
    Ok(reply)
}

fn ids_empty(reply: &Value) -> bool {
    reply["ids"].as_array().is_some_and(Vec::is_empty)
}

fn restored_empty(reply: &Value) -> bool {
    reply["restored"].as_array().is_some_and(Vec::is_empty)
}

/// The memories import `named` added or replaced that are still stored,
/// by its own events, oldest first.
fn touched(tx: &Connection, named: &str) -> Result<Vec<Touched>> {
    let rows = tx
        .prepare(
            "SELECT e.seq,e.memory_id,e.before FROM memory_events e JOIN memories m ON m.id=e.memory_id WHERE e.imported=?1 AND e.op='import' ORDER BY e.seq",
        )?
        .query_map([named], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|(seq, id, before)| Touched {
            id,
            seq,
            before: before
                .and_then(|b| serde_json::from_str::<Map<String, Value>>(&b).ok())
                .unwrap_or_default(),
        })
        .collect())
}

/// Whether an undo of import `named` wrote any event here.
fn undone(tx: &Connection, named: &str) -> rusqlite::Result<bool> {
    tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM memory_events WHERE imported=?1)",
        [undo_mark(named)],
        |r| r.get(0),
    )
}

/// What to do with `memory`, or none when an undo of this import dealt with
/// it before.
fn plan(tx: &Connection, named: &str, memory: &Touched) -> Result<Option<Plan>> {
    let since = tx
        .prepare(
            "SELECT op,imported FROM memory_events WHERE memory_id=?1 AND seq>?2 ORDER BY seq",
        )?
        .query_map(params![memory.id, memory.seq], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // Imports undone on this memory since: their changes and their undo's
    // cancel out.
    let undone: BTreeSet<&str> = since
        .iter()
        .filter_map(|(_, imported)| match imported.as_deref().map(kind_of) {
            Some(Mark::Undo(other)) => Some(other),
            _ => None,
        })
        .collect();
    if undone.contains(named) {
        return Ok(None);
    }
    let mut stale = false;
    for (op, imported) in &since {
        let mark = imported.as_deref().map(kind_of);
        let counts = match mark {
            // History a later import brought from its file: no change here.
            Some(Mark::Brought(_)) => false,
            Some(Mark::Import(other) | Mark::Undo(other)) => !undone.contains(other),
            None => true,
        };
        if !counts {
            continue;
        }
        let kept = match op.as_str() {
            "stale" => {
                stale = true;
                continue;
            }
            // The memory left and came back: what brought it back decides.
            "origin_removed" | "forget" => continue,
            // A later import that added or replaced it can be undone; one
            // that found it identical and only confirmed it cannot, and
            // keeps it like any confirmation.
            "import" => {
                let later = matches!(mark, Some(Mark::Import(_)));
                return Ok(Some(Plan::Keep {
                    why: "import",
                    later,
                }));
            }
            "confirm" => "confirm",
            "update" => "update",
            "move" => "move",
            "supersede" => "supersede",
            "revive" => "revive",
            "merge" => "merge",
            "distinct" => "distinct",
            // No write but an insert records, and the memory existed.
            _ => "record",
        };
        return Ok(Some(Plan::Keep {
            why: kept,
            later: false,
        }));
    }
    // An origin the import did not bring came since, as from another
    // import that found the memory identical: another conversation stated
    // it, as forgetting by origin keeps such a memory.
    let brought: BTreeSet<String> = memory
        .before
        .get(IMPORT_ORIGINS)
        .and_then(|b| serde_json::from_value(b.clone()).ok())
        .unwrap_or_default();
    let origins: Vec<String> = tx
        .prepare("SELECT origin FROM memory_origins WHERE memory_id=?1")?
        .query_map([&memory.id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    if origins.iter().any(|origin| !brought.contains(origin)) {
        return Ok(Some(Plan::Keep {
            why: "origin",
            later: false,
        }));
    }
    if !history::replaced(&memory.before) {
        return Ok(Some(Plan::Delete));
    }
    let mut before = memory.before.clone();
    before.remove(IMPORT_ORIGINS);
    // A pattern given back its earlier supports needs them all still here.
    let supports: Vec<String> = match before.get("evidence_ids") {
        Some(ids) => serde_json::from_value(ids.clone()).unwrap_or_default(),
        None => tx.query_row(
            "SELECT evidence_ids FROM memories WHERE id=?1",
            [&memory.id],
            |r| {
                let ids: String = r.get(0)?;
                Ok(serde_json::from_str(&ids).unwrap_or_default())
            },
        )?,
    };
    for support in &supports {
        let present: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM memories WHERE id=?1)",
            [support],
            |r| r.get(0),
        )?;
        if !present {
            return Ok(Some(Plan::Keep {
                why: "support gone",
                later: false,
            }));
        }
    }
    Ok(Some(Plan::Restore { before, stale }))
}

/// The memory's fields under their export names, with their columns.
const FIELDS: [(&str, &str); 13] = [
    ("type", "memory_type"),
    ("category", "category"),
    ("context", "context"),
    ("title", "title"),
    ("content", "content"),
    ("reason", "reason"),
    ("scope", "scope"),
    ("project", "project"),
    ("source", "source"),
    ("status", "status"),
    ("evidence", "evidence"),
    ("evidence_ids", "evidence_ids"),
    ("superseded_by", "superseded_by"),
];

/// Gives memory `id` back the values in `before`: its fields, then its
/// origins (the earlier ones in before the import's go, as an import's
/// replacement does) and its times. With `stale`, its status stays.
fn restore(tx: &Connection, id: &str, before: &Map<String, Value>, stale: bool) -> Result<()> {
    let mut sets = Vec::new();
    let mut values: Vec<Value> = Vec::new();
    for (name, column) in FIELDS {
        let Some(value) = before.get(name) else {
            continue;
        };
        if stale && name == "status" {
            continue;
        }
        let value = if name == "evidence_ids" && !value.is_string() {
            json!(value.to_string())
        } else {
            value.clone()
        };
        values.push(value);
        sets.push(format!("{column}=?{}", values.len() + 1));
    }
    for time in ["created_at", "last_confirmed_at"] {
        if let Some(value) = before.get(time) {
            values.push(value.clone());
            sets.push(format!("{time}=?{}", values.len() + 1));
        }
    }
    sets.push("updated_at=CURRENT_TIMESTAMP".to_owned());
    let sql = format!("UPDATE memories SET {} WHERE id=?1", sets.join(","));
    let mut bound: Vec<rusqlite::types::Value> = vec![rusqlite::types::Value::Text(id.to_owned())];
    bound.extend(values.into_iter().map(sql_value));
    tx.execute(&sql, rusqlite::params_from_iter(bound))?;
    if let Some(origins) = before.get("origins").and_then(Value::as_array) {
        let mut keep = Vec::new();
        for origin in origins {
            let (Some(name), Some(first_seen)) =
                (origin["origin"].as_str(), origin["first_seen"].as_str())
            else {
                continue;
            };
            tx.execute(
                "INSERT INTO memory_origins (memory_id,origin,first_seen) VALUES (?1,?2,?3) ON CONFLICT(memory_id,origin) DO UPDATE SET first_seen=excluded.first_seen",
                params![id, name, first_seen],
            )?;
            keep.push(name.to_owned());
        }
        tx.execute(
            "DELETE FROM memory_origins WHERE memory_id=?1 AND origin NOT IN (SELECT value FROM json_each(?2))",
            params![id, serde_json::to_string(&keep)?],
        )?;
    }
    Ok(())
}

/// A JSON value from `before` as SQLite takes it.
fn sql_value(value: Value) -> rusqlite::types::Value {
    match value {
        Value::Null => rusqlite::types::Value::Null,
        Value::String(text) => rusqlite::types::Value::Text(text),
        Value::Number(n) => n.as_i64().map_or(
            rusqlite::types::Value::Null,
            rusqlite::types::Value::Integer,
        ),
        other => rusqlite::types::Value::Text(other.to_string()),
    }
}

/// The import whose version of `memory` this undo just gave back, when an
/// undo of that import ran before and kept the memory: the memory's last
/// change before this import's event was that import's own, so the user
/// has asked for neither.
fn undone_version(tx: &Connection, memory: &Touched) -> Result<Option<String>> {
    let earlier: Vec<(Option<String>, Option<String>)> = tx
        .prepare(
            "SELECT imported,before FROM memory_events WHERE memory_id=?1 AND seq<?2 ORDER BY seq DESC",
        )?
        .query_map(params![memory.id, memory.seq], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (imported, before) in earlier {
        match imported.as_deref().map(kind_of) {
            Some(Mark::Brought(_)) => continue,
            Some(Mark::Import(other)) => {
                let kept = before
                    .and_then(|b| serde_json::from_str::<Value>(&b).ok())
                    .is_some_and(|b| b.get(UNDO_KEPT).is_some());
                return Ok(kept.then(|| other.to_owned()));
            }
            _ => return Ok(None),
        }
    }
    Ok(None)
}
