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

/// Opens a stamps reply for a client that asks with a [`Position`]: the
/// store's position now, then `full` when it lists every memory in scope or
/// `delta` when it lists only those changed since, with `-` and the id of
/// each changed memory no longer in scope. Each memory's line holds the
/// conversation origin of its latest change, so a session is not told of
/// its own.
pub const STAMPS_SINCE: &str = "user-memory stamps v2";

/// The conversation origin of the latest change to the memory `{id}` names,
/// empty when none was given: a confirmation, a distinct pair or a removed
/// conversation changes nothing a session is told of.
const LATEST_ORIGIN: &str = "coalesce((SELECT origin FROM memory_events WHERE memory_id={id} AND op NOT IN ('confirm','distinct','origin_removed') ORDER BY seq DESC LIMIT 1),'')";

/// The one conversation origin of every change to the memory `{id}` names
/// after the event `{seq}`, empty when there were several or one gave
/// none: a session is spared only a change that was all its own, never one
/// another session made before it.
const ONLY_ORIGIN: &str = "coalesce((SELECT CASE WHEN count(DISTINCT origin)=1 THEN max(origin) END FROM memory_events WHERE memory_id={id} AND seq>{seq} AND op NOT IN ('confirm','distinct','origin_removed')),'')";

/// Where a session's snapshot stands in the store's count of its changes,
/// as the store wrote it: a string a hook keeps and sends back as it is,
/// like a paging cursor. Only the store reads what is inside, and its
/// format may change between versions; a position the store does not know,
/// whether empty, from another store or a restored one, in another format
/// or no position at all, gets every memory in scope.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Position(String);

/// Opens each position this version writes, so a store tells its own
/// format from another version's or from text that is no position.
const POSITION_FORMAT: &str = "1";

impl Position {
    /// The position at `change` of the store whose id is `store_id`, when
    /// its history's latest event was `seq`: the format, a digest of the
    /// id, so the id itself never leaves the store, the change and the
    /// event. A marker holds no event; its position names 0, which
    /// [`Position::same_change`] does not compare.
    pub(super) fn at(store_id: &str, change: i64, seq: i64) -> Self {
        Self(format!(
            "{POSITION_FORMAT}.{}.{change}.{seq}",
            digest(store_id)
        ))
    }

    /// Whether this position and `other` name the same change of the same
    /// store, in a format this version writes: then nothing a session is
    /// told of changed between them.
    pub fn same_change(&self, other: &Position) -> bool {
        match (self.parts(), other.parts()) {
            (Some((store, change, _)), Some((other_store, other_change, _))) => {
                store == other_store && change == other_change
            }
            _ => false,
        }
    }

    /// The store's digest, the change and the event this position names,
    /// when it is in a format this version writes.
    fn parts(&self) -> Option<(&str, i64, i64)> {
        let rest = self.0.strip_prefix(POSITION_FORMAT)?.strip_prefix('.')?;
        let mut parts = rest.split('.');
        let store = parts.next()?;
        let change = parts.next()?.parse().ok()?;
        let seq = parts.next()?.parse().ok()?;
        parts.next().is_none().then_some((store, change, seq))
    }

    /// A position kept as text, such as a server's in its marker on this
    /// machine, taken as it is.
    pub fn kept(text: String) -> Self {
        Self(text)
    }

    /// The position as the store wrote it, to keep.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The change and the event this position names in the store whose id
    /// is `store_id`; none for another store's position or one this version
    /// does not write.
    fn change_in(&self, store_id: &str) -> Option<(i64, i64)> {
        let (store, change, seq) = self.parts()?;
        (store == digest(store_id)).then_some((change, seq))
    }
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

/// The first [`DIGEST_BYTES`] of the SHA-256 of `values`, in hex: what a
/// stamp holds for a memory whose counted columns, quoted and joined as
/// [`super::changes::digested`] does, read `values`, and what a position
/// holds for the store id.
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
/// only those changed after `since` when it is this store's position and
/// the store still counts every change since then, else all of them. One
/// read, so the position it reports is that of what it lists.
pub(super) fn stamps_since(conn: &Connection, filter: &Filter, since: &Position) -> Result<String> {
    let read = conn.unchecked_transaction()?;
    let (store_id, last, pruned, seq): (String, i64, i64, i64) = read.query_row(
        "SELECT store_id,last_change,pruned_through,(SELECT coalesce(max(seq),0) FROM memory_events) FROM store_changes WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let known = since.change_in(&store_id);
    let after = known
        .map(|(change, _)| change)
        .filter(|change| (pruned..=last).contains(change));
    let delta = after.is_some();
    // Since a position of this store, a change is the session's own only
    // when every change since was; otherwise its latest change says.
    let origin = |id: &str| match known {
        Some((_, since_seq)) => ONLY_ORIGIN
            .replace("{id}", id)
            .replace("{seq}", &since_seq.to_string()),
        None => LATEST_ORIGIN.replace("{id}", id),
    };
    let (condition, mut values) = condition(filter)?;
    let mut sql = format!(
        "SELECT id,status,title,{},{} FROM memories WHERE {condition}",
        super::changes::digested(),
        origin("memories.id"),
    );
    if let Some(after) = after {
        sql.push_str(" AND id IN (SELECT memory_id FROM memory_changes WHERE change>?)");
        values.push(Sql::Integer(after));
    }
    sql.push_str(" ORDER BY id");
    let kind = if delta { "delta" } else { "full" };
    let now = Position::at(&store_id, last, seq);
    let mut lines = vec![format!("{STAMPS_SINCE}\t{}\t{kind}", now.0)];
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
    if let Some(after) = after {
        let mut changed = read.prepare(&format!(
            "SELECT memory_id,{} FROM memory_changes WHERE change>?1 ORDER BY memory_id",
            origin("memory_changes.memory_id")
        ))?;
        let rows = changed.query_map([after], |r| {
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
    /// no [`Position`].
    pub since: Option<Position>,
    /// Whether `stamps` holds only the memories changed since what was asked.
    pub delta: bool,
    pub stamps: Stamps,
    /// Memories changed since what was asked and no longer in scope.
    pub gone: Vec<String>,
    /// The conversation origin of the latest change to each memory listed
    /// or gone, where one was given.
    pub origins: BTreeMap<String, String>,
}

/// The reply to a stamps request with a [`Position`], or a [`STAMPS`] reply from
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
    let since = Position(fields.next()?.to_owned());
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
    fn reply(conn: &Connection, since: &Position) -> StampsReply {
        let filter = super::super::selection::Selection::default()
            .filter()
            .unwrap();
        parse_stamps_reply(&stamps_since(conn, &filter, since).unwrap()).unwrap()
    }

    #[test]
    fn a_reply_since_a_change_lists_only_what_changed_after_it() {
        let (_dir, conn) = store();
        let full = reply(&conn, &Position::default());
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
        // The position names the store, its change and its latest event,
        // never the raw id.
        let (id, change, seq): (String, i64, i64) = conn
            .query_row(
                "SELECT store_id,last_change,(SELECT coalesce(max(seq),0) FROM memory_events) FROM store_changes",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        let now = moved.since.unwrap();
        assert_eq!(now, Position::at(&id, change, seq));
        assert!(!now.0.contains(&id), "{now:?}");
        // A position the store does not know gets the whole list: another
        // store's, one ahead of what it counted, another format's, a
        // made-up one or none.
        for (case, since) in [
            ("another store", Position::at(&"0".repeat(32), change, seq)),
            ("ahead", Position::at(&id, change + 1, seq)),
            (
                "another format",
                Position(now.0.replacen(POSITION_FORMAT, "2", 1)),
            ),
            ("made up", Position("not a position".into())),
            ("empty", Position::default()),
        ] {
            assert!(!reply(&conn, &since).delta, "{case}");
        }
        // One below what the store dropped too.
        conn.execute("UPDATE store_changes SET pruned_through=?1", [change])
            .unwrap();
        assert!(!reply(&conn, &Position::at(&id, change - 1, seq)).delta);
        // A marker's position names no event: the same change all the same.
        assert!(now.same_change(&Position::at(&id, change, 0)));
        assert!(!now.same_change(&Position::at(&id, change - 1, seq)));
        assert!(!now.same_change(&Position::at(&"0".repeat(32), change, seq)));
        assert!(!now.same_change(&Position("not a position".into())));
        assert!(reply(&conn, &now).delta, "at the pruned change itself");
    }

    #[test]
    fn a_digest_moves_with_a_counted_column_and_with_nothing_else() {
        let (_dir, conn) = store();
        let digest = |conn: &Connection| reply(conn, &Position::default()).stamps[A].digest.clone();
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
