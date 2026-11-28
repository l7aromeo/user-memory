//! Which memories each session used (#123): `memory_use`, one row per
//! memory, project and session origin, with what the session did with it.
//! The index orders by it and session start prints a project's most-used
//! knowledge from it. Rows carry no memory text and move no change marker.

/// One row per memory, the project it counted for and the session (its
/// origin): when the store last wrote it, and whether the session read it,
/// recorded or confirmed it, or was shown a passage of it. `held`: a hold
/// showed it (#118), null until known. `overridden`: whether the agent
/// last held by it ran the held command again, the session's latest value,
/// null until known: one session's flushes reach the store in order, each
/// after the last one's reply, so the latest sent is the latest seen.
/// `passage` is the slot of the passage pushed most in that session (see
/// `index::passages::SLOTS`), null when none was. `seq` is the
/// [`use_clock`](TABLE) value of the row's latest write: the order of
/// writes, which `at`'s whole seconds don't give (#240). Use changes nothing
/// a session reads, so no change counter moves. A forgotten memory's rows
/// go with it. Part of the stored schema since version 12, `seq` since 14.
///
/// `use_clock` is one row, the latest seq given out; none before the first
/// write. Each write of use takes the next value inside its write
/// transaction, so two writers never share one. A counter of its own
/// rather than `max(seq) + 1`: a forget deletes rows by cascade, and the
/// largest left could then be handed out again below a seq that
/// `hold_reset` already points past.
///
/// `hold_reset` is, per memory, the clock's value at its latest change
/// that starts its hold count again (`seq`) and at its latest change that
/// renumbers its passages (`passages`), each null until one happened: use rows with a
/// larger seq came after it. The trigger `memory_use_reset` keeps both.
/// The hold count starts again when what the note asks or where it holds
/// changes, or when it is confirmed: its content, condition, scope or
/// project, or `last_confirmed_at`. A title or reason is wording. A type
/// change is left out: making a note a rule already stops it from ever
/// stopping. A category move doesn't change what it asks. The passage
/// count starts again when the content or context changes. Neither reads
/// `updated_at`: it has whole seconds, so a change in the second the
/// memory was written leaves it as it was. Part of the stored schema since
/// version 14.
pub(super) const TABLE: &str = "CREATE TABLE IF NOT EXISTS memory_use (
    memory TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    project TEXT NOT NULL CHECK(length(project) BETWEEN 1 AND 240),
    origin TEXT NOT NULL CHECK(length(origin) BETWEEN 1 AND 240),
    at TEXT NOT NULL,
    read INTEGER NOT NULL CHECK(read IN (0,1)),
    recorded INTEGER NOT NULL CHECK(recorded IN (0,1)),
    shown INTEGER NOT NULL CHECK(shown IN (0,1)),
    held INTEGER CHECK(held IN (0,1)),
    overridden INTEGER CHECK(overridden IN (0,1)),
    passage INTEGER CHECK(passage >= 0),
    seq INTEGER NOT NULL CHECK(seq > 0),
    PRIMARY KEY(memory, project, origin)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS memory_use_by_project ON memory_use(project, origin);
CREATE TABLE IF NOT EXISTS use_clock (
    id INTEGER PRIMARY KEY CHECK(id = 1),
    seq INTEGER NOT NULL CHECK(seq > 0)
);
CREATE TABLE IF NOT EXISTS hold_reset (
    memory TEXT PRIMARY KEY NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    seq INTEGER CHECK(seq >= 0),
    passages INTEGER CHECK(passages >= 0)
) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS memory_use_reset AFTER UPDATE ON memories
WHEN NEW.content IS NOT OLD.content OR NEW.context IS NOT OLD.context OR NEW.until_kind IS NOT OLD.until_kind OR NEW.until_value IS NOT OLD.until_value OR NEW.until_project IS NOT OLD.until_project OR NEW.scope IS NOT OLD.scope OR NEW.project IS NOT OLD.project OR NEW.last_confirmed_at IS NOT OLD.last_confirmed_at BEGIN
    INSERT INTO hold_reset(memory,seq,passages) SELECT NEW.id,
        CASE WHEN NEW.content IS NOT OLD.content OR NEW.context IS NOT OLD.context OR NEW.until_kind IS NOT OLD.until_kind OR NEW.until_value IS NOT OLD.until_value OR NEW.until_project IS NOT OLD.until_project OR NEW.scope IS NOT OLD.scope OR NEW.project IS NOT OLD.project OR NEW.last_confirmed_at IS NOT OLD.last_confirmed_at THEN clock END,
        CASE WHEN NEW.content IS NOT OLD.content OR NEW.context IS NOT OLD.context THEN clock END
    FROM (SELECT coalesce((SELECT seq FROM use_clock WHERE id=1),0) AS clock) WHERE true
    ON CONFLICT(memory) DO UPDATE SET seq=coalesce(excluded.seq,seq),passages=coalesce(excluded.passages,passages);
END;";

use super::{clean, project_key, schema::connect, schema::write, selection::refuse_local_paths};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};
use uuid::Uuid;

/// The sessions of a project whose use counts: its newest 40 origins, as
/// #125's benchmark set it. At 10,000 memories windows of 20, 40 and 80
/// sessions listed 0.93, 0.95 and 0.95 of later sessions' needs; 40 is one
/// round of its worlds' tasks. Drift, which would make a long window stale,
/// is not modelled there.
pub(crate) const USE_WINDOW: usize = 40;
/// Most rows one request carries: a session that used more sends several.
/// A thousand rows is about 200 KB of JSON, within [`USE_REQUEST_BYTES`].
pub(crate) const USES_PER_REQUEST: usize = 1000;
/// Largest `POST /use` body, refused before it is read: a full request of
/// [`USES_PER_REQUEST`] rows with long project keys fits.
pub(crate) const USE_REQUEST_BYTES: usize = 256 * 1024;
/// Longest origin: the store's limit for a conversation's origin.
const ORIGIN_CHARS: usize = 240;
/// Longest project key: the store's limit for a project key.
const PROJECT_CHARS: usize = 240;

/// One memory a session used, and the project it counts for.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Use {
    pub memory: String,
    pub project: String,
    pub read: bool,
    pub recorded: bool,
    pub shown: bool,
    /// For #118: whether the memory held a command; absent when not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<bool>,
    /// For #118: whether the agent ran the held command again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overridden: Option<bool>,
    /// The slot of the memory's passage shown most in the session; absent
    /// when none was. A request that names one replaces the stored one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passage: Option<u32>,
}

/// One session's use, as `POST /use` takes it and the hooks write it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Uses {
    pub origin: String,
    pub uses: Vec<Use>,
}

/// A row ready to write: the memory's id and the project's key.
struct Row<'a> {
    memory: Uuid,
    project: String,
    given: &'a Use,
}

/// Why `uses` can't be written, in the words a client prints, or its rows.
fn checked(uses: &Uses) -> std::result::Result<(String, Vec<Row<'_>>), String> {
    let origin = clean(&uses.origin, usize::MAX);
    let length = origin.chars().count();
    if length == 0 || length > ORIGIN_CHARS {
        return Err(format!(
            "origin has {length} characters; it must have 1 to {ORIGIN_CHARS}"
        ));
    }
    if uses.uses.is_empty() || uses.uses.len() > USES_PER_REQUEST {
        return Err(format!(
            "uses has {} rows; send 1 to {USES_PER_REQUEST} per request",
            uses.uses.len()
        ));
    }
    let mut rows = Vec::with_capacity(uses.uses.len());
    for given in &uses.uses {
        let Ok(memory) = Uuid::parse_str(given.memory.trim()) else {
            return Err(format!(
                "memory {:?} is not a memory's full id",
                given.memory
            ));
        };
        refuse_local_paths(std::slice::from_ref(&given.project)).map_err(|err| err.to_string())?;
        let project = project_key(&given.project);
        if project.is_empty() || project.chars().count() > PROJECT_CHARS {
            return Err(format!(
                "project {:?} is not a project key of 1 to {PROJECT_CHARS} characters",
                given.project
            ));
        }
        rows.push(Row {
            memory,
            project,
            given,
        });
    }
    Ok((origin, rows))
}

/// What writing a session's use did, or why it was refused.
pub enum Written {
    /// Rows written and skipped, as the reply's JSON.
    Done(Value),
    /// The request is malformed: the reason, in plain text.
    Refused(String),
}

/// Writes one session's use: each row upserted by memory, project and
/// origin, its flags merged with OR (`overridden` takes the latest), its
/// time the store's and its seq the flush's, the next `use_clock` value, so
/// sending a row again only adds to it; then each project written to keeps
/// the [`USE_WINDOW`] origins it wrote last, by seq. A row whose memory isn't stored, such as
/// one forgotten since, or a project memory's row for another project is
/// skipped: a project memory counts for its own project, a global one for
/// any. No change marker moves and no memory changes.
pub fn write_uses(db: &Path, uses: &Uses) -> Result<Written> {
    let (origin, rows) = match checked(uses) {
        Ok(checked) => checked,
        Err(reason) => return Ok(Written::Refused(reason)),
    };
    let conn = connect(db)?;
    let tx = write(&conn)?;
    // The flush's place in the order of writes: the store's write lock is
    // held from here to the commit, so no other writer takes the same seq.
    let seq: i64 = tx.query_row(
        "INSERT INTO use_clock(id,seq) VALUES (1,1) ON CONFLICT(id) DO UPDATE SET seq=seq+1 RETURNING seq",
        [],
        |r| r.get(0),
    )?;
    let mut place = tx.prepare("SELECT scope,project FROM memories WHERE id=?1")?;
    let mut upsert = tx.prepare(
        "INSERT INTO memory_use (memory,project,origin,at,read,recorded,shown,held,overridden,passage,seq) VALUES (?1,?2,?3,CURRENT_TIMESTAMP,?4,?5,?6,?7,?8,?9,?10)
         ON CONFLICT(memory,project,origin) DO UPDATE SET at=excluded.at,seq=excluded.seq,read=max(read,excluded.read),recorded=max(recorded,excluded.recorded),shown=max(shown,excluded.shown),
         held=CASE WHEN excluded.held IS NULL THEN held ELSE max(coalesce(held,0),excluded.held) END,
         overridden=coalesce(excluded.overridden,overridden),
         passage=coalesce(excluded.passage,passage)",
    )?;
    let (mut written, mut skipped) = (0, 0);
    let mut projects = BTreeSet::new();
    for row in &rows {
        let found: Option<(String, String)> = place
            .query_row([row.memory.to_string()], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let counts =
            found.is_some_and(|(scope, project)| scope != "project" || project == row.project);
        if !counts {
            skipped += 1;
            continue;
        }
        upsert.execute(params![
            row.memory.to_string(),
            row.project,
            origin,
            row.given.read,
            row.given.recorded,
            row.given.shown,
            row.given.held,
            row.given.overridden,
            row.given.passage,
            seq,
        ])?;
        written += 1;
        projects.insert(row.project.clone());
    }
    drop((place, upsert));
    let mut prune = tx.prepare(
        "DELETE FROM memory_use WHERE project=?1 AND origin NOT IN (SELECT origin FROM memory_use WHERE project=?1 GROUP BY origin ORDER BY max(seq) DESC LIMIT ?2)",
    )?;
    let window = i64::try_from(USE_WINDOW).expect("USE_WINDOW is a small constant");
    for project in &projects {
        prune.execute(params![project, window])?;
    }
    drop(prune);
    tx.commit()?;
    Ok(Written::Done(
        json!({"written": written, "skipped": skipped}),
    ))
}

/// How much a project's recent sessions used a memory: in how many of
/// them, then in how many it was read or recorded. Greater means used
/// more; ordering by it puts a read or a record before a delivery.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Used {
    pub(super) sessions: i64,
    pub(super) worked: i64,
}

/// The use of each memory in `projects`' kept sessions (each project's
/// newest [`USE_WINDOW`], as the write prunes them). A session that
/// counts for two of the projects counts once. A row that only says a
/// hold showed the memory is no use here: it carries the hold's outcome
/// (#118), and the index and the most-used part stay as reading,
/// recording and pushes made them. Empty without projects, or
/// before any session wrote use.
pub(super) fn used(
    conn: &rusqlite::Connection,
    projects: &[String],
) -> Result<std::collections::HashMap<Uuid, Used>> {
    let mut found = std::collections::HashMap::new();
    if projects.is_empty() {
        return Ok(found);
    }
    let mut statement = conn.prepare(
        "SELECT memory,count(DISTINCT origin),count(DISTINCT CASE WHEN read OR recorded THEN origin END) FROM memory_use WHERE project IN (SELECT value FROM json_each(?1)) AND (read OR recorded OR shown) GROUP BY memory",
    )?;
    let rows = statement.query_map([serde_json::to_string(projects)?], |r| {
        Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?))
    })?;
    for row in rows {
        let (memory, sessions, worked) = row?;
        if let Ok(memory) = Uuid::parse_str(&memory) {
            found.insert(memory, Used { sessions, worked });
        }
    }
    Ok(found)
}

/// Least share of a project's kept sessions that used a memory for it to be
/// printed as most-used knowledge, as #125 set it: the benchmark's
/// essentials are needed by 0.7 of tasks; at a window of 40 and 10,000
/// memories, 0.25 and 0.5 print all of them and 0.75 prints 0.54, and 0.25
/// also prints memories used less often, leaving the index less room.
const ESSENTIAL_SHARE: f64 = 0.5;
/// Most memories the part prints (#125): 2, 5 and 15 print 0.44, 0.92 and
/// 1.00 of the essentials at 10,000 memories, and 15 costs the index 0.01
/// of needs listed.
const ESSENTIALS_MAX: usize = 15;
/// A memory this short is printed whole; a longer one as its title and one
/// passage (#125, where 150, 300 and 600 measured alike).
const ESSENTIAL_WHOLE_CHARS: usize = 300;
/// The part's first line.
pub(super) const MOST_USED_HEADER: &str =
    "Most used in this project's recent sessions (if one applies, read it for the rest first):";
/// Types read as a wish about how to work: an observation of one is about
/// the user, so the part never prints it (push.rs `reads_as_wish`).
const WISH_TYPES: [&str; 5] = ["preference", "constraint", "value", "behavior", "pattern"];

/// Whether `m` may be printed as most-used knowledge: the user's decisions
/// and facts and agents' notes, never a rule, an inference, or an
/// observation about the user.
fn printable(m: &super::Memory) -> bool {
    !super::read::standing_rule(m)
        && match m.source.as_str() {
            "explicit" => !matches!(m.memory_type.as_str(), "behavior" | "pattern"),
            "observed" => !WISH_TYPES.contains(&m.memory_type.as_str()),
            _ => false,
        }
}

/// What `m`'s line says before its text, as a push's does (push.rs
/// `mark`): nothing for the user's own memory, the mark of an agent's
/// note for one.
fn mark(m: &super::Memory) -> &'static str {
    let voice = if m.source == "explicit" {
        super::push::Voice::User
    } else {
        super::push::Voice::Note
    };
    super::push::mark(voice)
}

/// The slot of each of `chosen`'s passages shown in most of `projects`'
/// kept sessions, counting only rows written after the memory last changed
/// (by seq, against `hold_reset.passages`), since an edit renumbers its
/// passages. A memory none of them
/// was shown a passage of has none. Ties go to the earlier passage.
fn delivered(
    conn: &rusqlite::Connection,
    projects: &[String],
    chosen: &[super::Memory],
) -> Result<std::collections::HashMap<Uuid, i64>> {
    let mut statement = conn.prepare(
        "SELECT passage,origin FROM memory_use WHERE memory=?1 AND passage IS NOT NULL AND project IN (SELECT value FROM json_each(?2))
         AND seq > coalesce((SELECT passages FROM hold_reset WHERE memory=?1),0)",
    )?;
    let projects = serde_json::to_string(projects)?;
    let mut found = std::collections::HashMap::new();
    for m in chosen {
        let mut sessions: std::collections::BTreeMap<i64, BTreeSet<String>> =
            std::collections::BTreeMap::new();
        let rows = statement.query_map(params![m.id.to_string(), projects], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (passage, origin) = row?;
            sessions.entry(passage).or_default().insert(origin);
        }
        // The earliest passage among the most shown: max_by_key keeps the
        // last of equals, so the slots are walked from the last.
        if let Some((passage, _)) = sessions
            .iter()
            .rev()
            .max_by_key(|(_, origins)| origins.len())
        {
            found.insert(m.id, *passage);
        }
    }
    Ok(found)
}

/// The text of `m`'s passage in `slot`, as the keyword index numbers them
/// (`index::passages::SLOTS`): a content passage or the context. Empty for
/// a slot that names neither.
fn passage_text(m: &super::Memory, slot: i64) -> String {
    if slot == super::index::passages::CONTEXT_SLOT {
        return super::clean(&m.context, usize::MAX);
    }
    usize::try_from(slot - 1)
        .ok()
        .and_then(|at| {
            super::index::passages::split(&m.content)
                .into_iter()
                .nth(at)
        })
        .map(|place| super::clean(&m.content[place], usize::MAX))
        .unwrap_or_default()
}

/// `m` as the part prints it: whole up to [`ESSENTIAL_WHOLE_CHARS`], else
/// its title and the passage `slot` names, its first when `slot` is none
/// or names no passage of it, marked as part of its text.
fn printed(m: &super::Memory, slot: Option<i64>) -> String {
    let short = &m.id.to_string()[..super::read::SHORT_ID];
    let text = super::clean(&m.content, usize::MAX);
    if text.chars().count() <= ESSENTIAL_WHOLE_CHARS {
        return format!("- {short} {}{text}", mark(m));
    }
    let passage = slot
        .map(|slot| passage_text(m, slot))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| passage_text(m, 1));
    format!("- {short} {}{} — {passage} …", mark(m), m.title)
}

/// The project's most-used knowledge within `room` in `unit`: the memories
/// `filter` covers that are used in at least [`ESSENTIAL_SHARE`] of its
/// projects' kept sessions, the [`ESSENTIALS_MAX`] most used first, as
/// many as fit. Empty without projects, without use, or with none that
/// qualifies.
pub(super) fn most_used(
    conn: &rusqlite::Connection,
    filter: &super::selection::Filter,
    room: usize,
    unit: super::FitUnit,
) -> Result<String> {
    if filter.projects.is_empty() {
        return Ok(String::new());
    }
    let sessions: i64 = conn.query_row(
        "SELECT count(DISTINCT origin) FROM memory_use WHERE project IN (SELECT value FROM json_each(?1)) AND (read OR recorded OR shown)",
        [serde_json::to_string(&filter.projects)?],
        |r| r.get(0),
    )?;
    if sessions == 0 {
        return Ok(String::new());
    }
    let use_of = used(conn, &filter.projects)?;
    let share = |m: &super::Memory| {
        let used = use_of.get(&m.id).map_or(0, |used| used.sessions);
        // Both counts are at most a few hundred sessions: exact as floats.
        #[allow(clippy::cast_precision_loss, reason = "counts of sessions are small")]
        let share = used as f64 / sessions as f64;
        share
    };
    let mut chosen: Vec<super::Memory> = super::selection::candidates(conn, filter)?
        .into_iter()
        .filter(|m| printable(m) && share(m) >= ESSENTIAL_SHARE)
        .collect();
    chosen.sort_by(|a, b| {
        use_of
            .get(&b.id)
            .cmp(&use_of.get(&a.id))
            .then_with(|| super::read::group_order(a, b))
    });
    chosen.truncate(ESSENTIALS_MAX);
    let slots = delivered(conn, &filter.projects, &chosen)?;
    let mut lines = vec![MOST_USED_HEADER.to_owned()];
    let mut used_room = unit.len(MOST_USED_HEADER) + 1;
    for m in &chosen {
        let line = printed(m, slots.get(&m.id).copied());
        let size = unit.len(&line) + 1;
        if used_room + size > room {
            break;
        }
        used_room += size;
        lines.push(line);
    }
    Ok(if lines.len() == 1 {
        String::new()
    } else {
        lines.join("\n")
    })
}

/// The sessions in a row whose holds an agent's note must have been run
/// past before it stops holding a command (#118): three, so one session's
/// misjudged hold or a note right once in a while keeps holding.
pub(crate) const HOLD_OVERRIDES: usize = 3;

/// The agents' notes that no longer hold a command (#118): each one in
/// force that an agent observed whose latest [`HOLD_OVERRIDES`] sessions
/// with a hold of it, all written after it last changed what it asks or was
/// confirmed (`hold_reset.seq`), each ran the held command again, in the
/// order the store wrote them. Counted by session, as `memory_use` keeps
/// one row per session and project; a row's seq is its latest write, so a
/// confirmation between a hold and its session's next flush still counts
/// that session after it. The user's rules and stated memories never stop.
pub fn stopped_holds(db: &Path) -> Result<Vec<String>> {
    Ok(override_runs(&connect(db)?)?
        .into_iter()
        .map(|(id, _)| id)
        .collect())
}

/// Each stopped note in force ([`stopped_holds`]) with its run: how many of its
/// latest sessions with a hold, written after its reset, overrode it in a
/// row, at least [`HOLD_OVERRIDES`] (#238).
pub(super) fn override_runs(conn: &rusqlite::Connection) -> Result<Vec<(String, usize)>> {
    let mut candidates = conn.prepare(
        "SELECT id FROM live_memories
         WHERE source = 'observed'
         AND id IN (SELECT memory FROM memory_use WHERE held = 1)",
    )?;
    let mut sessions = conn.prepare(
        "SELECT max(overridden) FROM memory_use
         WHERE memory = ?1 AND held = 1
         AND seq > coalesce((SELECT seq FROM hold_reset WHERE memory = ?1), 0)
         GROUP BY origin ORDER BY max(seq) DESC",
    )?;
    let found = candidates.query_map([], |r| r.get::<_, String>(0))?;
    let mut stopped = Vec::new();
    for id in found {
        let id = id?;
        let outcomes: Vec<Option<bool>> = sessions
            .query_map([&id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let run = outcomes.iter().take_while(|o| **o == Some(true)).count();
        if run >= HOLD_OVERRIDES {
            stopped.push((id, run));
        }
    }
    Ok(stopped)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The store's resets of `id`: its hold count's and its passages'.
    fn resets(conn: &rusqlite::Connection, id: &str) -> Option<(Option<i64>, Option<i64>)> {
        conn.query_row(
            "SELECT seq,passages FROM hold_reset WHERE memory=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .unwrap()
    }

    // The trigger reads what changed, not `updated_at`, which a change in
    // the second the memory was written leaves as it was: each update here
    // keeps it. A title is wording; a context restarts both counts, a
    // confirmation the hold count only (#240).
    #[test]
    fn what_a_change_touches_restarts_its_counts() {
        let dir = tempfile::tempdir().unwrap();
        let conn = connect(&dir.path().join("memory.sqlite3")).unwrap();
        let id = "e0000001-0000-4000-8000-000000000001";
        conn.execute(
            "INSERT INTO memories (id,memory_type,category,title,content,source,evidence) VALUES (?1,'lesson','workflow','Locks','Keep the locks','observed','seen')",
            [id],
        )
        .unwrap();
        conn.execute("INSERT INTO use_clock VALUES (1,4)", [])
            .unwrap();
        let change = |set: &str| {
            conn.execute(&format!("UPDATE memories SET {set} WHERE id=?1"), [id])
                .unwrap();
        };
        change("title='Lock files'");
        assert_eq!(resets(&conn, id), None, "a title is wording");
        change("content='Keep the lock files'");
        assert_eq!(resets(&conn, id), Some((Some(4), Some(4))));
        conn.execute("UPDATE use_clock SET seq=7", []).unwrap();
        change("last_confirmed_at=CURRENT_TIMESTAMP");
        assert_eq!(resets(&conn, id), Some((Some(7), Some(4))));
        conn.execute("UPDATE use_clock SET seq=9", []).unwrap();
        change("context='In a shared checkout'");
        assert_eq!(resets(&conn, id), Some((Some(9), Some(9))));
        change("memory_type='preference',category='tooling'");
        assert_eq!(
            resets(&conn, id),
            Some((Some(9), Some(9))),
            "type and category"
        );
    }
}
