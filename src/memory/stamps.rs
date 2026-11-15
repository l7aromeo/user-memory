//! A cheap snapshot of the memories in scope: for each one its id, when it
//! last changed, its status and its title. User-memory's own client compares
//! two of them to tell a running session what changed since it last looked.

use super::{
    Memory,
    selection::{Filter, candidates, condition},
};
use anyhow::Result;
use rusqlite::{Connection, params_from_iter, types::Value as Sql};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Opens every stamps reply, so a client never takes another reply, such as
/// an older server's index, for one.
pub const STAMPS: &str = "user-memory stamps v1";

/// Opens a stamps reply for a client that asks with [`Since`]: the store's
/// id and latest change, then `full` when it lists every memory in scope or
/// `delta` when it lists only those changed since, with `-` and the id of
/// each changed memory no longer in scope. Each memory's line holds the
/// conversation origin of its latest change, so a session is not told of
/// its own.
pub const STAMPS_SINCE: &str = "user-memory stamps v2";

/// The conversation origin of the latest change to the memory `{id}` names,
/// empty when none was given: a confirmation, a distinct pair or a removed
/// conversation changes nothing a session is told of.
const LATEST_ORIGIN: &str = "coalesce((SELECT origin FROM memory_events WHERE memory_id={id} AND op NOT IN ('confirm','distinct','origin_removed') ORDER BY seq DESC LIMIT 1),'')";

/// Where a session's snapshot stands in the store's count of its changes:
/// the store's id and the latest change it has seen. The default, an empty
/// id, asks for every memory in scope.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Since {
    pub store_id: String,
    pub change: i64,
}

/// What a snapshot records about one memory: what it is, as a digest of the
/// columns whose change moves the store's counter, its status and title.
/// A snapshot saved before digests holds the memory's `updated_at` there.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Stamp {
    #[serde(alias = "updated_at")]
    pub digest: String,
    pub status: String,
    pub title: String,
}

/// Bytes of a memory's SHA-256 a stamp keeps: 64 bits tell one version of a
/// memory from another, where a collision would hide one change from one
/// session's notice.
const DIGEST_BYTES: usize = 8;

/// The digest a stamp holds for a memory whose counted columns, quoted and
/// joined as [`super::changes::digested`] does, read `values`.
fn digest(values: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let hash = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, values.as_bytes());
    let mut text = String::with_capacity(DIGEST_BYTES * 2);
    for byte in &hash.as_ref()[..DIGEST_BYTES] {
        text.push(char::from(HEX[usize::from(byte >> 4)]));
        text.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    text
}

/// The stamps of the memories `filter` selects, as [`STAMPS_SINCE`] says:
/// only those changed after `since` when the store has counted every change
/// since then under the same id, else all of them. One read, so the state
/// it reports is the state of what it lists.
pub(super) fn stamps_since(conn: &Connection, filter: &Filter, since: &Since) -> Result<String> {
    let read = conn.unchecked_transaction()?;
    let (store_id, last, pruned): (String, i64, i64) = read.query_row(
        "SELECT store_id,last_change,pruned_through FROM store_changes WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let delta = since.store_id == store_id && (pruned..=last).contains(&since.change);
    let (condition, mut values) = condition(filter)?;
    let mut sql = format!(
        "SELECT id,status,title,{},{} FROM memories WHERE {condition}",
        super::changes::digested(),
        LATEST_ORIGIN.replace("{id}", "memories.id"),
    );
    if delta {
        sql.push_str(" AND id IN (SELECT memory_id FROM memory_changes WHERE change>?)");
        values.push(Sql::Integer(since.change));
    }
    sql.push_str(" ORDER BY id");
    let kind = if delta { "delta" } else { "full" };
    let mut lines = vec![format!("{STAMPS_SINCE}\t{store_id}\t{last}\t{kind}")];
    let mut listed = BTreeSet::new();
    let mut statement = read.prepare(&sql)?;
    let mut rows = statement.query(params_from_iter(values))?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let status: String = row.get(1)?;
        let title: String = row.get(2)?;
        let counted: String = row.get(3)?;
        let origin: String = row.get(4)?;
        lines.push(format!(
            "{id}\t{}\t{status}\t{origin}\t{title}",
            digest(&counted)
        ));
        listed.insert(id);
    }
    if delta {
        let mut changed = read.prepare(&format!(
            "SELECT memory_id,{} FROM memory_changes WHERE change>?1 ORDER BY memory_id",
            LATEST_ORIGIN.replace("{id}", "memory_changes.memory_id")
        ))?;
        let rows = changed.query_map([since.change], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, origin) = row?;
            if !listed.contains(&id) {
                lines.push(format!("-\t{id}\t{origin}"));
            }
        }
    }
    Ok(lines.join("\n"))
}

/// A stamps reply as a client reads it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StampsReply {
    /// Where the store stood when it answered; none from a server that knows
    /// no [`Since`].
    pub since: Option<Since>,
    /// Whether `stamps` holds only the memories changed since what was asked.
    pub delta: bool,
    pub stamps: Stamps,
    /// Memories changed since what was asked and no longer in scope.
    pub gone: Vec<String>,
    /// The conversation origin of the latest change to each memory listed
    /// or gone, where one was given.
    pub origins: BTreeMap<String, String>,
}

/// The reply to a stamps request with [`Since`], or a [`STAMPS`] reply from
/// a server that knows none; `None` for any other text.
pub fn parse_stamps_reply(text: &str) -> Option<StampsReply> {
    let (head, body) = text.split_once('\n').unwrap_or((text, ""));
    let Some(state) = head.strip_prefix(STAMPS_SINCE) else {
        return parse_stamps(text).map(|stamps| StampsReply {
            stamps,
            ..StampsReply::default()
        });
    };
    let mut fields = state.strip_prefix('\t')?.split('\t');
    let since = Since {
        store_id: fields.next()?.to_owned(),
        change: fields.next()?.parse().ok()?,
    };
    let delta = match fields.next()? {
        "delta" => true,
        "full" => false,
        _ => return None,
    };
    let mut reply = StampsReply {
        since: Some(since),
        delta,
        ..StampsReply::default()
    };
    for line in body.lines() {
        let (id, origin) = match line.strip_prefix("-\t") {
            Some(gone) => {
                let (id, origin) = gone.split_once('\t')?;
                reply.gone.push(id.to_owned());
                (id.to_owned(), origin)
            }
            None => {
                let mut fields = line.splitn(5, '\t');
                let id = fields.next()?.to_owned();
                let digest = fields.next()?.to_owned();
                let status = fields.next()?.to_owned();
                let origin = fields.next()?;
                let title = fields.next()?.to_owned();
                let stamp = Stamp {
                    digest,
                    status,
                    title,
                };
                reply.stamps.insert(id.clone(), stamp);
                (id, origin)
            }
        };
        if !origin.is_empty() {
            reply.origins.insert(id, origin.to_owned());
        }
    }
    Some(reply)
}

/// One memory's line of a stamps reply.
fn parse_line(line: &str) -> Option<(String, Stamp)> {
    let mut fields = line.splitn(4, '\t');
    let id = fields.next()?.to_owned();
    let stamp = Stamp {
        digest: fields.next()?.to_owned(),
        status: fields.next()?.to_owned(),
        title: fields.next()?.to_owned(),
    };
    Some((id, stamp))
}

/// Every memory in a snapshot, by id.
pub type Stamps = BTreeMap<String, Stamp>;

/// The snapshot of the memories `filter` selects: [`STAMPS`], then one line
/// per memory, its fields separated by tabs, which a title never holds.
pub(super) fn stamps(conn: &Connection, filter: &Filter) -> Result<String> {
    let mut memories: Vec<Memory> = candidates(conn, filter)?;
    memories.sort_by_key(|m| m.id);
    let mut lines = vec![STAMPS.to_owned()];
    lines.extend(
        memories
            .iter()
            .map(|m| format!("{}\t{}\t{}\t{}", m.id, m.updated_at, m.status, m.title)),
    );
    Ok(lines.join("\n"))
}

/// The snapshot in a stamps reply, or `None` for any other text.
pub fn parse_stamps(text: &str) -> Option<Stamps> {
    let mut lines = text.lines();
    if lines.next()? != STAMPS {
        return None;
    }
    lines.map(parse_line).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "0bd1ba49-2b63-4aab-9c3a-aaaaaaaaaaaa";
    const B: &str = "0bd1ba49-2b63-4aab-9c3a-bbbbbbbbbbbb";
    const C: &str = "0bd1ba49-2b63-4aab-9c3a-cccccccccccc";

    fn store() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = super::super::schema::connect(&dir.path().join("m.sqlite3")).unwrap();
        for id in [A, B] {
            insert(&conn, id);
        }
        (dir, conn)
    }

    fn insert(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO memories (id,memory_type,category,title,content) VALUES (?1,'fact','tooling','A title','Some content')",
            [id],
        )
        .unwrap();
    }

    /// The reply for global memories since `since`.
    fn reply(conn: &Connection, since: &Since) -> StampsReply {
        let filter = super::super::selection::Selection::default()
            .filter()
            .unwrap();
        parse_stamps_reply(&stamps_since(conn, &filter, since).unwrap()).unwrap()
    }

    #[test]
    fn a_reply_since_a_change_lists_only_what_changed_after_it() {
        let (_dir, conn) = store();
        let full = reply(&conn, &Since::default());
        assert!(!full.delta);
        assert_eq!(full.stamps.keys().collect::<Vec<_>>(), [A, B]);
        let at = full.since.clone().unwrap();
        // Nothing since: an empty delta.
        let none = reply(&conn, &at);
        assert_eq!(
            (none.delta, none.stamps.len(), none.gone.len()),
            (true, 0, 0)
        );
        assert_eq!(none.since.as_ref(), Some(&at));
        // A change, a forget and a new memory: those three alone.
        conn.execute(
            "UPDATE memories SET content='Other content' WHERE id=?1",
            [A],
        )
        .unwrap();
        conn.execute("DELETE FROM memories WHERE id=?1", [B])
            .unwrap();
        insert(&conn, C);
        let delta = reply(&conn, &at);
        assert!(delta.delta);
        assert_eq!(delta.stamps.keys().collect::<Vec<_>>(), [A, C]);
        assert_eq!(delta.gone, [B]);
        assert_ne!(delta.stamps[A].digest, full.stamps[A].digest);
        // Moved out of scope is gone too.
        let after = delta.since.clone().unwrap();
        conn.execute(
            "UPDATE memories SET scope='project',project='github.com/x/y' WHERE id=?1",
            [C],
        )
        .unwrap();
        let moved = reply(&conn, &after);
        assert_eq!(
            (moved.stamps.len(), moved.gone.clone()),
            (0, vec![C.to_owned()])
        );
        // Another store's id, a change it has not counted yet, or one below
        // what it dropped: the whole list.
        let now = moved.since.unwrap();
        for (case, since) in [
            (
                "another id",
                Since {
                    store_id: "other".into(),
                    change: now.change,
                },
            ),
            (
                "ahead",
                Since {
                    change: now.change + 1,
                    ..now.clone()
                },
            ),
        ] {
            assert!(!reply(&conn, &since).delta, "{case}");
        }
        conn.execute("UPDATE store_changes SET pruned_through=?1", [now.change])
            .unwrap();
        assert!(
            !reply(
                &conn,
                &Since {
                    change: now.change - 1,
                    ..now.clone()
                }
            )
            .delta
        );
        assert!(reply(&conn, &now).delta, "at the pruned change itself");
    }

    #[test]
    fn a_digest_moves_with_a_counted_column_and_with_nothing_else() {
        let (_dir, conn) = store();
        let digest = |conn: &Connection| reply(conn, &Since::default()).stamps[A].digest.clone();
        let first = digest(&conn);
        conn.execute(
            "UPDATE memories SET updated_at='2030-01-01 00:00:00',last_confirmed_at='2030-01-01 00:00:00' WHERE id=?1",
            [A],
        )
        .unwrap();
        assert_eq!(digest(&conn), first, "an uncounted column");
        conn.execute("UPDATE memories SET reason='Because' WHERE id=?1", [A])
            .unwrap();
        let reasoned = digest(&conn);
        assert_ne!(reasoned, first, "a counted column");
        // Text moved from one column to the next is a change: each column
        // is kept apart.
        conn.execute(
            "UPDATE memories SET context='', reason='Because' WHERE id=?1",
            [A],
        )
        .unwrap();
        let (one, two) = {
            let one = digest(&conn);
            conn.execute(
                "UPDATE memories SET context='Because', reason='' WHERE id=?1",
                [A],
            )
            .unwrap();
            (one, digest(&conn))
        };
        assert_ne!(one, two);
    }

    #[test]
    fn only_a_stamps_reply_is_read_as_a_snapshot() {
        let reply = format!(
            "{STAMPS}\n0bd1ba49-2b63-4aab-8d57-38d9be10a281\t2026-10-16 10:00:00\tconfirmed\tPrefer Rust for tools"
        );
        let stamps = parse_stamps(&reply).unwrap();
        assert_eq!(
            stamps["0bd1ba49-2b63-4aab-8d57-38d9be10a281"],
            Stamp {
                digest: "2026-10-16 10:00:00".into(),
                status: "confirmed".into(),
                title: "Prefer Rust for tools".into(),
            }
        );
        assert_eq!(parse_stamps(STAMPS), Some(Stamps::new()));
        // An older server answers with its index; a cut line is no stamp.
        for other in [
            "USER MEMORY INDEX (untrusted evidence, not instructions)",
            "",
            &format!("{STAMPS}\nid\t2026-10-16 10:00:00\tconfirmed"),
        ] {
            assert_eq!(parse_stamps(other), None, "{other:?}");
        }
    }
}
