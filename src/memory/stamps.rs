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

/// Opens a [`STAMPS_SINCE`] reply to a client that also asked for rule
/// prints: after each memory's origin come its kind ([`RULE`], [`AGENT`] or
/// empty), a rule's [`print`], the client and the conversation that wrote
/// the version of a rule in force ([`WRITER`]; the conversation only when
/// it is not the memory's origin), and a rule's or an agent's memory's
/// project (empty for a global one), so the hook knows which projects it
/// has seen; other memories leave these empty.
pub const STAMPS_RULES: &str = "user-memory stamps v3";

/// The kind of a standing rule in a [`STAMPS_RULES`] reply: an explicit,
/// confirmed memory of a rule type, as `read::standing_rule` decides.
pub const RULE: &str = "rule";
/// The kind of a memory an agent recorded: observed or inferred.
pub const AGENT: &str = "agent";

/// Who wrote the version of the memory `{id}` names that is in force: the
/// conversation origin and the client of its latest change, joined by
/// U+001F, which neither holds. Changes an undone import cancelled are
/// passed over, so a memory the undo gave back names who wrote what it
/// says again. An import's client is the importer's, not the author's, so
/// none is given for one.
const WRITER: &str = "coalesce((SELECT origin||char(31)||CASE op WHEN 'import' THEN '' ELSE client END FROM memory_events e WHERE memory_id={id} AND op NOT IN ('confirm','distinct','origin_removed') AND NOT {undone} ORDER BY seq DESC LIMIT 1),char(31))";

/// The columns a rule's print covers: what the rule says and where it
/// applies. Not its project key, so a move or a rekey keeps it, nor its
/// category, type or evidence, which change nothing it asks.
const PRINTED: [&str; 5] = ["title", "content", "context", "reason", "scope"];

/// The way [`print`] hashes rules, which session hooks keep beside the
/// prints they saved. A new way gets a new number, and a hook that kept
/// prints of the old way starts afresh rather than listing every rule as
/// changed; a test pins one print, so the way never changes unnoticed.
pub const PRINT_FORMAT: u32 = 1;

/// A standing rule's fingerprint: [`digest`] of the [`PRINT_FORMAT`] and
/// the [`PRINTED`] columns, quoted and joined as [`printed`] reads them.
fn print(printed: &str) -> String {
    digest(&format!("print {PRINT_FORMAT}\u{1f}{printed}"))
}

/// An SQL expression over a row of `memories` that changes exactly when a
/// [`PRINTED`] column does.
fn printed() -> String {
    PRINTED
        .iter()
        .map(|column| format!("quote({column})"))
        .collect::<Vec<_>>()
        .join("||char(31)||")
}

/// The conversation origin of the latest change to the memory `{id}` names,
/// empty when none was given: a confirmation, a distinct pair or a removed
/// conversation changes nothing a session is told of.
const LATEST_ORIGIN: &str = "coalesce((SELECT origin FROM memory_events WHERE memory_id={id} AND op NOT IN ('confirm','distinct','origin_removed') ORDER BY seq DESC LIMIT 1),'')";

/// The one conversation origin of every change to the memory `{id}` names
/// after the event `{seq}`, empty when there were several or one gave
/// none: a session is spared only a change that was all its own, never one
/// another session made before it. Event numbers are reused once pruning
/// drops the newest event, so on a store with no event for the pruning age,
/// a change landing at or below a session's number is missed once.
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

    /// Whether this position is one of the store `now` is a position of, at
    /// or before `now`: what was read at it holds nothing that store has
    /// not seen. Another store's, or one past `now`, as after a backup was
    /// put back by hand, is not.
    #[cfg(test)]
    pub fn within(&self, now: &Position) -> bool {
        match (self.parts(), now.parts()) {
            (Some((store, change, _)), Some((now_store, now_change, _))) => {
                store == now_store && change <= now_change
            }
            _ => false,
        }
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
/// the store still counts every change since then, else all of them; with
/// `prints`, as [`STAMPS_RULES`] says. One read, so the position it reports
/// is that of what it lists.
pub(super) fn stamps_since(
    conn: &Connection,
    filter: &Filter,
    since: &Position,
    prints: bool,
) -> Result<String> {
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
    // A rule's print and writer are read for rules alone, so the other
    // memories pay only for their kind.
    let rule = super::read::rule_condition();
    let ruled = if prints {
        format!(
            ",CASE WHEN {rule} THEN '{RULE}' WHEN source<>'explicit' THEN '{AGENT}' ELSE '' END,CASE WHEN {rule} THEN {} END,CASE WHEN {rule} THEN {} END,CASE WHEN {rule} OR source<>'explicit' THEN project END",
            printed(),
            WRITER
                .replace("{id}", "memories.id")
                .replace("{undone}", &super::history::undone("e")),
        )
    } else {
        String::new()
    };
    let mut sql = format!(
        "SELECT id,status,title,{},{}{ruled} FROM memories WHERE {condition}",
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
    let head = if prints { STAMPS_RULES } else { STAMPS_SINCE };
    let mut lines = vec![format!("{head}\t{}\t{kind}", now.0)];
    let mut listed = BTreeSet::new();
    let mut statement = read.prepare(&sql)?;
    let mut rows = statement.query(params_from_iter(values))?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let status: String = row.get(1)?;
        let title: String = row.get(2)?;
        let counted: String = row.get(3)?;
        let origin: String = row.get(4)?;
        let digest = digest(&counted);
        lines.push(if prints {
            let kind: String = row.get(5)?;
            let printed: Option<String> = row.get(6)?;
            let writer: Option<String> = row.get(7)?;
            let project: Option<String> = row.get(8)?;
            let (written, client) = writer
                .as_deref()
                .and_then(|w| w.split_once('\u{1f}'))
                .unwrap_or_default();
            // The memory's origin already says it, as for every rule but
            // one an undone import gave back.
            let written = if written == origin { "" } else { written };
            format!(
                "{id}\t{digest}\t{status}\t{origin}\t{kind}\t{}\t{client}\t{written}\t{}\t{title}",
                printed.as_deref().map(print).unwrap_or_default(),
                project.unwrap_or_default(),
            )
        } else {
            format!("{id}\t{digest}\t{status}\t{origin}\t{title}")
        });
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
    /// Whether the reply says which memories are rules and which agents
    /// recorded ([`STAMPS_RULES`]); an older server's does not.
    pub prints: bool,
    /// Each standing rule listed, by id.
    pub rules: BTreeMap<String, Rule>,
    /// The project of each memory listed that an agent recorded, by id;
    /// empty for a global one.
    pub agents: BTreeMap<String, String>,
}

/// What a [`STAMPS_RULES`] reply says of a standing rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// Its fingerprint, which changes with what it says and where it
    /// applies.
    pub print: String,
    /// The client that wrote the version in force, empty when unknown.
    pub client: String,
    /// The conversation that wrote the version in force, as forgetting by
    /// origin names it; empty when unknown.
    pub origin: String,
    /// Its project, empty for a global rule.
    pub project: String,
}

/// The reply to a stamps request with a [`Position`], with rule prints or
/// without, or a [`STAMPS`] reply from a server that knows none; `None` for
/// any other text.
pub fn parse_stamps_reply(text: &str) -> Option<StampsReply> {
    let (head, body) = text.split_once('\n').unwrap_or((text, ""));
    let (state, prints) = match (
        head.strip_prefix(STAMPS_RULES),
        head.strip_prefix(STAMPS_SINCE),
    ) {
        (Some(state), _) => (state, true),
        (None, Some(state)) => (state, false),
        (None, None) => {
            return parse_stamps(text).map(|stamps| StampsReply {
                stamps,
                ..StampsReply::default()
            });
        }
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
        prints,
        ..StampsReply::default()
    };
    let fields_per_line = if prints { 10 } else { 5 };
    for line in body.lines() {
        let (id, origin) = match line.strip_prefix("-\t") {
            Some(gone) => {
                let (id, origin) = gone.split_once('\t')?;
                reply.gone.push(id.to_owned());
                (id.to_owned(), origin)
            }
            None => {
                let mut fields = line.splitn(fields_per_line, '\t');
                let id = fields.next()?.to_owned();
                let digest = fields.next()?.to_owned();
                let status = fields.next()?.to_owned();
                let origin = fields.next()?;
                if prints {
                    let kind = fields.next()?;
                    let print = fields.next()?;
                    let client = fields.next()?;
                    let written = fields.next()?;
                    let project = fields.next()?;
                    match kind {
                        RULE => {
                            let written = if written.is_empty() { origin } else { written };
                            reply.rules.insert(
                                id.clone(),
                                Rule {
                                    print: print.to_owned(),
                                    client: client.to_owned(),
                                    origin: written.to_owned(),
                                    project: project.to_owned(),
                                },
                            );
                        }
                        AGENT => {
                            reply.agents.insert(id.clone(), project.to_owned());
                        }
                        _ => {}
                    }
                }
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

    const A: &str = "e0000002-0000-4000-8000-000000000002";
    const B: &str = "e0000003-0000-4000-8000-000000000003";
    const C: &str = "e0000004-0000-4000-8000-000000000004";

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
        parse_stamps_reply(&stamps_since(conn, &filter, since, false).unwrap()).unwrap()
    }

    /// The reply with rule prints for global memories and those of
    /// `github.com/x/y`, from no position.
    fn ruled(conn: &Connection) -> StampsReply {
        let filter = super::super::selection::Selection {
            projects: vec!["github.com/x/y".into()],
            ..Default::default()
        }
        .filter()
        .unwrap();
        let text = stamps_since(conn, &filter, &Position::default(), true).unwrap();
        assert!(text.starts_with(STAMPS_RULES), "{text}");
        parse_stamps_reply(&text).unwrap()
    }

    /// Inserts a memory with these columns; the others take their defaults.
    fn put(conn: &Connection, id: &str, memory_type: &str, source: &str, status: &str) {
        conn.execute(
            "INSERT INTO memories (id,memory_type,category,title,content,source,status,evidence) VALUES (?1,?2,'tooling','A title','Some content',?3,?4,CASE ?3 WHEN 'observed' THEN 'Seen' ELSE '' END)",
            rusqlite::params![id, memory_type, source, status],
        )
        .unwrap();
    }

    #[test]
    fn a_reply_with_prints_marks_rules_and_agents_and_prints_rules_alone() {
        let dir = tempfile::tempdir().unwrap();
        let conn = super::super::schema::connect(&dir.path().join("m.sqlite3")).unwrap();
        put(&conn, A, "preference", "explicit", "confirmed");
        put(&conn, B, "fact", "observed", "tentative");
        put(&conn, C, "preference", "explicit", "tentative");
        conn.execute(
            "INSERT INTO memory_events (memory_id,op,origin,client) VALUES (?1,'update','claude:session/x','claude')",
            [A],
        )
        .unwrap();
        let reply = ruled(&conn);
        assert!(reply.prints);
        assert_eq!(reply.rules.keys().collect::<Vec<_>>(), [A]);
        assert_eq!(reply.rules[A].client, "claude");
        assert_eq!(reply.rules[A].print.len(), DIGEST_BYTES * 2);
        assert_eq!(reply.agents.keys().collect::<Vec<_>>(), [B]);
        assert_eq!(
            (reply.rules[A].project.as_str(), reply.agents[B].as_str()),
            ("", "")
        );
        // Every memory is still a stamp, with its title whole.
        assert_eq!(reply.stamps.len(), 3);
        assert_eq!(reply.stamps[A].title, "A title");
        let print = |conn: &Connection| ruled(conn).rules[A].print.clone();
        let first = print(&conn);
        // What the rule asks or where it applies moves the print; its
        // project key, category, rule type and evidence do not.
        for (change, moves) in [
            ("project='github.com/x/y',scope='project'", true),
            ("project='github.com/x/z'", false),
            (
                "category='workflow',memory_type='constraint',evidence='Said'",
                false,
            ),
            ("scope='global',project=''", true),
            ("content='Other content'", true),
            ("context='When'", true),
            ("reason='Because'", true),
            ("title='Another title'", true),
        ] {
            let before = print(&conn);
            conn.execute(&format!("UPDATE memories SET {change} WHERE id=?1"), [A])
                .unwrap();
            if change.starts_with("project='github.com/x/z'") {
                // Out of view now; back into it to read its print.
                conn.execute(
                    "UPDATE memories SET project='github.com/x/y' WHERE id=?1",
                    [A],
                )
                .unwrap();
            }
            assert_eq!(print(&conn) != before, moves, "{change}");
            if change.starts_with("project='github.com/x/y'") {
                assert_eq!(ruled(&conn).rules[A].project, "github.com/x/y");
            }
        }
        assert_ne!(print(&conn), first);
        // An import's client is the importer's: none is given.
        conn.execute(
            "INSERT INTO memory_events (memory_id,op,client) VALUES (?1,'import','cli')",
            [A],
        )
        .unwrap();
        assert_eq!(ruled(&conn).rules[A].client, "");
        // A reply without prints says nothing of rules.
        let plain = reply_all(&conn);
        assert!(!plain.prints && plain.rules.is_empty() && plain.agents.is_empty());
    }

    /// A rule an undone import gave back names who wrote it before the
    /// import: the import's own event and the undo's are passed over. The
    /// memory's origin still names its latest change, the undo, so the
    /// session that undid it is not told of it.
    #[test]
    fn a_rule_an_undone_import_gave_back_names_its_earlier_writer() {
        let dir = tempfile::tempdir().unwrap();
        let conn = super::super::schema::connect(&dir.path().join("m.sqlite3")).unwrap();
        put(&conn, A, "preference", "explicit", "confirmed");
        let named = "shared.json@2026-01-01T00:00:00Z";
        for (op, origin, client, imported) in [
            ("update", "claude:session/x", "claude", String::new()),
            ("import", "", "cli", named.to_owned()),
            (
                "update",
                "codex:session/y",
                "codex",
                format!("undo:{named}"),
            ),
        ] {
            conn.execute(
                "INSERT INTO memory_events (memory_id,op,origin,client,imported) VALUES (?1,?2,?3,?4,nullif(?5,''))",
                rusqlite::params![A, op, origin, client, imported],
            )
            .unwrap();
        }
        let reply = ruled(&conn);
        let rule = &reply.rules[A];
        assert_eq!(
            (rule.origin.as_str(), rule.client.as_str()),
            ("claude:session/x", "claude")
        );
        assert_eq!(reply.origins[A], "codex:session/y");
        // Not undone, the import is the latest change: no client.
        conn.execute("DELETE FROM memory_events WHERE imported LIKE 'undo:%'", [])
            .unwrap();
        let reply = ruled(&conn);
        assert_eq!(
            (
                reply.rules[A].origin.as_str(),
                reply.rules[A].client.as_str()
            ),
            ("", "")
        );
    }

    /// The reply without prints for every global memory.
    fn reply_all(conn: &Connection) -> StampsReply {
        reply(conn, &Position::default())
    }

    /// A print of fixed text is pinned: a session hook keeps the prints it
    /// showed, and a print that changed between versions would list every
    /// rule as changed after an upgrade.
    #[test]
    fn a_print_is_the_same_on_every_version() {
        let dir = tempfile::tempdir().unwrap();
        let conn = super::super::schema::connect(&dir.path().join("m.sqlite3")).unwrap();
        conn.execute(
            "INSERT INTO memories (id,memory_type,category,title,content,context,reason,scope) VALUES (?1,'preference','tooling','Use tabs','Indent with tabs, not spaces.','In Go code','gofmt does','global')",
            [A],
        )
        .unwrap();
        assert_eq!(PRINT_FORMAT, 1);
        // SHA-256 of "print 1", then each column quoted as SQLite's quote()
        // does, all joined by U+001F; its first 8 bytes in hex.
        assert_eq!(ruled(&conn).rules[A].print, "a0b34f6246b14fe9");
    }

    /// The rule condition read in SQL holds exactly for the memories
    /// `read::standing_rule` calls rules.
    #[test]
    fn the_rule_condition_agrees_with_standing_rule() {
        let dir = tempfile::tempdir().unwrap();
        let conn = super::super::schema::connect(&dir.path().join("m.sqlite3")).unwrap();
        let mut n = 0;
        for memory_type in ["preference", "constraint", "value", "fact", "pattern"] {
            for source in ["explicit", "observed"] {
                for status in ["confirmed", "tentative"] {
                    n += 1;
                    put(
                        &conn,
                        &format!("e{n:07}-0000-4000-8000-{n:012}"),
                        memory_type,
                        source,
                        status,
                    );
                }
            }
        }
        let filter = super::super::selection::Selection::default()
            .filter()
            .unwrap();
        let rules: BTreeSet<String> = candidates(&conn, &filter)
            .unwrap()
            .iter()
            .filter(|m| super::super::read::standing_rule(m))
            .map(|m| m.id.to_string())
            .collect();
        assert_eq!(rules.len(), 3);
        let reply = ruled(&conn);
        assert_eq!(reply.rules.keys().cloned().collect::<BTreeSet<_>>(), rules);
        assert_eq!(reply.agents.len(), 10);
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
            "{STAMPS}\ne0000001-0000-4000-8000-000000000001\t2026-10-16 10:00:00\tconfirmed\tPrefer Rust for tools"
        );
        let stamps = parse_stamps(&reply).unwrap();
        assert_eq!(
            stamps["e0000001-0000-4000-8000-000000000001"],
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
